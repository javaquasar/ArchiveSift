use archive_sift::{
    app::AppConfig,
    db::{Database, FileDownloadStore},
    fs::StructuredDirectoryStrategy,
    models::{
        AttachmentData, MetadataAttribute, ScrapedItem, TorrentCard, TorrentEdit, TorrentQuery,
    },
    scraper::{ContentScraper, ScraperRegistry},
    services::{torrent_library::TorrentLibrary, ContentDownloadService},
    ui::ConsoleUi,
    AppResult,
};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, RwLock,
    },
};

const TORRENT: &[u8] = b"d4:infod6:lengthi1e4:name4:demoee";
const DOWNLOAD_URL: &str = "https://example.test/download/42";

fn attribute(kind: &str, value: &str) -> MetadataAttribute {
    MetadataAttribute {
        kind: kind.into(),
        label: kind.into(),
        value: value.into(),
        source_url: None,
        is_female: None,
    }
}

fn page() -> ScrapedItem {
    let mut attachment = attribute("attachment", DOWNLOAD_URL);
    attachment.label = "release.torrent".into();
    attachment.source_url = Some(DOWNLOAD_URL.into());
    ScrapedItem {
        site_key: "example-source".into(),
        page_url: "https://example.test/topic?t=42".into(),
        title: "[Example studio] Full release heading [2025, 1080p] — Заглавие".into(),
        raw_html: String::new(),
        images: vec![],
        attributes: vec![
            attachment,
            attribute("content_title", "Short folder title"),
            attribute("studio", "First studio"),
            attribute("description", "Short synopsis."),
            attribute(
                "full_description",
                "Cast: Example\n\nComplete first post.\n\nLast paragraph — конец.",
            ),
        ],
    }
}

struct Adapter {
    page: Arc<RwLock<ScrapedItem>>,
    fetched: Arc<AtomicUsize>,
    login_required: Arc<AtomicBool>,
}

impl ContentScraper for Adapter {
    fn site_key(&self) -> &str {
        "example-source"
    }
    fn supports_url(&self, _: &str) -> bool {
        true
    }
    fn scrape_content<'a>(
        &'a self,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>> {
        Box::pin(async move { Ok(self.page.read().unwrap().clone()) })
    }
    fn fetch_attachment<'a>(
        &'a self,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<AttachmentData>> + Send + 'a>> {
        self.fetched.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let login_required = self.login_required.load(Ordering::SeqCst);
            Ok(AttachmentData {
                bytes: if login_required {
                    b"<html>Login</html>".to_vec()
                } else {
                    TORRENT.to_vec()
                },
                content_type: if login_required {
                    "text/html"
                } else {
                    "application/x-bittorrent"
                }
                .into(),
            })
        })
    }
}

struct Fixture {
    root: tempfile::TempDir,
    page: Arc<RwLock<ScrapedItem>>,
    fetched: Arc<AtomicUsize>,
    login_required: Arc<AtomicBool>,
    config: AppConfig,
    db: Database,
    service: ContentDownloadService,
}

impl Fixture {
    fn new(page: ScrapedItem) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = AppConfig {
            source_url: page.page_url.clone(),
            database_path: root.path().join("archive.sqlite3").display().to_string(),
            download_folder: root.path().join("downloads").display().to_string(),
            subfolder: "collection".into(),
            ..Default::default()
        };
        let page = Arc::new(RwLock::new(page));
        let fetched = Arc::new(AtomicUsize::new(0));
        let login_required = Arc::new(AtomicBool::new(false));
        let mut registry = ScraperRegistry::new();
        registry.register(Box::new(Adapter {
            page: page.clone(),
            fetched: fetched.clone(),
            login_required: login_required.clone(),
        }));
        let db = Database::new(&config.database_path);
        db.initialize().unwrap();
        let service = ContentDownloadService::new(
            registry,
            db.clone(),
            FileDownloadStore::new(),
            ConsoleUi::new(),
            StructuredDirectoryStrategy,
            config.effective_download_dir(),
            root.path().join("thumbs"),
        );
        Self {
            root,
            page,
            fetched,
            login_required,
            config,
            db,
            service,
        }
    }
    fn card(&self) -> TorrentCard {
        self.db
            .search_torrents(&TorrentQuery::default())
            .unwrap()
            .cards
            .remove(0)
    }
    fn description(&self) -> PathBuf {
        let card = self.card();
        let file = card
            .attachments
            .iter()
            .find(|a| a.filename == "description.txt")
            .unwrap();
        self.config
            .effective_download_dir()
            .join(&file.local_file_path)
    }
    fn expected(&self) -> String {
        let page = self.page.read().unwrap();
        format!(
            "{}\n\n{}\n",
            page.title,
            page.attribute_values("full_description").next().unwrap()
        )
    }
}

#[tokio::test]
async fn complete_unicode_heading_and_multiline_text_are_exported_once_and_refreshed() {
    let mut parsed = page();
    parsed
        .title
        .push_str(&" Complete original heading — 日本語".repeat(30));
    let full_text = format!(
        "{}\n\nFinal paragraph: preserved.",
        "First post — строка.\n\n".repeat(2000)
    );
    parsed
        .attributes
        .iter_mut()
        .find(|a| a.kind == "full_description")
        .unwrap()
        .value = full_text;
    let fixture = Fixture::new(parsed);
    fixture.service.run(&fixture.config).await.unwrap();
    let path = fixture.description();
    let directory = fixture.card().directory.unwrap();
    assert_eq!(
        path,
        fixture
            .config
            .effective_download_dir()
            .join(&directory)
            .join("description.txt")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), fixture.expected());
    assert!(path.parent().unwrap().read_dir().unwrap().any(|e| e
        .unwrap()
        .path()
        .extension()
        .is_some_and(|e| e == "torrent")));
    {
        let mut parsed = fixture.page.write().unwrap();
        parsed.title = "Updated complete heading — unabridged".into();
        parsed
            .attributes
            .iter_mut()
            .find(|a| a.kind == "full_description")
            .unwrap()
            .value = "Updated first paragraph.\n\nUpdated final paragraph.".into();
    }
    fixture.service.run(&fixture.config).await.unwrap();
    assert_eq!(fixture.description(), path);
    assert_eq!(fixture.card().directory.as_ref(), Some(&directory));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), fixture.expected());
    assert_eq!(
        fixture
            .card()
            .attachments
            .iter()
            .filter(|a| a.filename == "description.txt")
            .count(),
        1
    );
    assert_eq!(fixture.fetched.load(Ordering::SeqCst), 1);
    std::fs::remove_file(&path).unwrap();
    fixture.service.run(&fixture.config).await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), fixture.expected());
}

#[tokio::test]
async fn absent_or_blank_full_text_falls_back_to_synopsis_or_heading() {
    for full_text in [None, Some(" \n\t")] {
        for synopsis in [
            None,
            Some("Synopsis first paragraph.\n\nSynopsis ending — конец."),
        ] {
            let mut parsed = page();
            parsed.attributes.retain(|a| {
                !matches!(
                    a.kind.as_str(),
                    "description" | "full_description" | "attachment"
                )
            });
            parsed.attributes.push(attribute(
                "magnet",
                "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
            ));
            if let Some(text) = full_text {
                parsed.attributes.push(attribute("full_description", text));
            }
            if let Some(text) = synopsis {
                parsed.attributes.push(attribute("description", text));
            }
            let expected = match synopsis {
                Some(text) => format!("{}\n\n{text}\n", parsed.title),
                None => format!("{}\n", parsed.title),
            };
            let fixture = Fixture::new(parsed);
            fixture.service.run(&fixture.config).await.unwrap();
            assert_eq!(
                std::fs::read_to_string(fixture.description()).unwrap(),
                expected
            );
            assert_eq!(fixture.fetched.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn text_is_retained_when_torrent_download_requires_login() {
    let fixture = Fixture::new(page());
    fixture.login_required.store(true, Ordering::SeqCst);
    assert!(fixture.service.run(&fixture.config).await.is_err());
    assert_eq!(
        std::fs::read_to_string(fixture.description()).unwrap(),
        fixture.expected()
    );
    assert_eq!(fixture.card().attachments.len(), 1);
}

#[tokio::test]
async fn legacy_export_stays_next_to_torrent_and_moves_only_after_confirmation() {
    let fixture = Fixture::new(page());
    let id = fixture
        .db
        .upsert_scraped_item(&fixture.page.read().unwrap())
        .unwrap()
        .page_id;
    let old_directory = PathBuf::from("legacy/release-42");
    let old_torrent = old_directory.join("release.torrent");
    let base = fixture.config.effective_download_dir();
    std::fs::create_dir_all(base.join(&old_directory)).unwrap();
    std::fs::write(base.join(&old_torrent), TORRENT).unwrap();
    fixture
        .db
        .save_page_attachment(
            id,
            DOWNLOAD_URL,
            "release.torrent",
            &old_torrent.to_string_lossy(),
        )
        .unwrap();
    rusqlite::Connection::open(&fixture.config.database_path)
        .unwrap()
        .execute(
            "UPDATE torrent_cards SET directory=NULL WHERE page_id=?1",
            [id],
        )
        .unwrap();
    fixture.service.run(&fixture.config).await.unwrap();
    assert!(fixture.card().directory.is_none());
    let description = fixture.description();
    assert_eq!(
        description,
        base.join(&old_directory).join("description.txt")
    );
    let tracked = fixture
        .card()
        .attachments
        .into_iter()
        .find(|file| file.filename == "description.txt")
        .unwrap()
        .local_file_path;
    #[cfg(windows)]
    assert!(!tracked.contains('/'));
    #[cfg(not(windows))]
    assert!(!tracked.contains('\\'));
    let expected = fixture.expected();
    let db = fixture.db.clone();
    let card = fixture.card();
    let saved = tokio::task::spawn_blocking(move || {
        let library = TorrentLibrary::new(db, base);
        let edit = TorrentEdit {
            primary_studio: "Selected studio".into(),
            ..TorrentEdit::from(&card)
        };
        let plan = library.prepare(id, card.revision, edit, true).unwrap();
        assert!(
            description.is_file(),
            "review must leave the export in place"
        );
        assert_eq!(plan.files.len(), 3);
        library.apply(plan, |_| {}).unwrap()
    })
    .await
    .unwrap();
    let new_path = fixture
        .config
        .effective_download_dir()
        .join(saved.directory.unwrap())
        .join("description.txt");
    assert_eq!(fixture.description(), new_path);
    assert_eq!(std::fs::read_to_string(&new_path).unwrap(), expected);
    assert!(!fixture
        .config
        .effective_download_dir()
        .join(old_directory)
        .join("description.txt")
        .exists());
    fixture.service.run(&fixture.config).await.unwrap();
    assert_eq!(fixture.description(), new_path);
    assert_eq!(std::fs::read_to_string(new_path).unwrap(), expected);
    assert_eq!(fixture.fetched.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn conflicting_user_file_is_preserved_until_it_is_renamed() {
    let fixture = Fixture::new(page());
    fixture
        .db
        .upsert_scraped_item(&fixture.page.read().unwrap())
        .unwrap();
    let directory = fixture
        .config
        .effective_download_dir()
        .join(fixture.card().directory.unwrap());
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("description.txt");
    std::fs::write(&path, "User notes — keep these.").unwrap();
    let error = fixture.service.run(&fixture.config).await.unwrap_err();
    assert!(error.to_string().contains("description.txt already exists"));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "User notes — keep these."
    );
    assert!(fixture.card().attachments.is_empty());
    let notes = directory.join("notes.txt");
    std::fs::rename(&path, &notes).unwrap();
    fixture.service.run(&fixture.config).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.description()).unwrap(),
        fixture.expected()
    );
    assert_eq!(
        std::fs::read_to_string(notes).unwrap(),
        "User notes — keep these."
    );
}

#[tokio::test]
async fn unregistered_export_from_failed_database_write_can_be_retried() {
    let fixture = Fixture::new(page());
    let connection = rusqlite::Connection::open(&fixture.config.database_path).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_description BEFORE INSERT ON page_attachments WHEN NEW.filename='description.txt' BEGIN SELECT RAISE(ABORT,'fixture DB failure'); END;").unwrap();
    assert!(fixture.service.run(&fixture.config).await.is_err());
    let directory = fixture
        .config
        .effective_download_dir()
        .join(fixture.card().directory.unwrap());
    assert_eq!(
        std::fs::read_to_string(directory.join("description.txt")).unwrap(),
        fixture.expected()
    );
    assert!(fixture.card().attachments.is_empty());
    connection
        .execute_batch("DROP TRIGGER fail_description;")
        .unwrap();
    fixture.service.run(&fixture.config).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.description()).unwrap(),
        fixture.expected()
    );
}

#[tokio::test]
async fn non_torrent_pages_have_no_description_export_and_stored_paths_cannot_escape() {
    let mut parsed = page();
    parsed.attributes.retain(|a| a.kind != "attachment");
    let ordinary = Fixture::new(parsed);
    ordinary.service.run(&ordinary.config).await.unwrap();
    assert_eq!(
        ordinary
            .db
            .search_torrents(&TorrentQuery::default())
            .unwrap()
            .total,
        0
    );
    assert!(!ordinary.config.effective_download_dir().exists());

    let fixture = Fixture::new(page());
    fixture.service.run(&fixture.config).await.unwrap();
    let original = fixture.description();
    let outside = fixture
        .root
        .path()
        .join("downloads/outside/description.txt");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, "Keep outside file").unwrap();
    rusqlite::Connection::open(&fixture.config.database_path).unwrap().execute(
        "UPDATE page_attachments SET local_file_path='../outside/description.txt' WHERE filename='description.txt'", [],
    ).unwrap();
    assert!(fixture.service.run(&fixture.config).await.is_err());
    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "Keep outside file"
    );
    assert_eq!(
        std::fs::read_to_string(original).unwrap(),
        fixture.expected()
    );
}

#[cfg(windows)]
#[tokio::test]
async fn failed_atomic_replacement_retains_previous_complete_export() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new(page());
    fixture.service.run(&fixture.config).await.unwrap();
    let path = fixture.description();
    let original = std::fs::read_to_string(&path).unwrap();
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 2)
        .open(&path)
        .unwrap();
    fixture.page.write().unwrap().title = "Updated full heading".into();
    assert!(fixture.service.run(&fixture.config).await.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(!path.parent().unwrap().read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".description.txt.part-")));
    drop(locked);
    fixture.service.run(&fixture.config).await.unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), fixture.expected());
}

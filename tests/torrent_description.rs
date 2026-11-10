use archive_sift::{
    app::AppConfig,
    db::{Database, FileDownloadStore},
    fs::StructuredDirectoryStrategy,
    models::{
        AttachmentData, MetadataAttribute, ScrapedItem, TorrentCard, TorrentEdit, TorrentField,
        TorrentQuery,
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

    async fn save_edit(&self, edit: TorrentEdit) -> TorrentCard {
        let card = self.card();
        let library = TorrentLibrary::new(self.db.clone(), self.config.effective_download_dir());
        tokio::task::spawn_blocking(move || {
            let plan = library
                .prepare(card.page_id, card.revision, edit, false)
                .unwrap();
            library.apply(plan, |_| {}).unwrap()
        })
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn metadata_edits_survive_a_rescrape_and_keep_the_export_in_the_moved_folder() {
    let mut parsed = page();
    parsed.attributes.extend([
        attribute("actor", "Keep Actor"),
        attribute("actor", "Remove Actor"),
        attribute("director", "Old Director"),
    ]);
    let fixture = Fixture::new(parsed.clone());
    fixture.service.run(&fixture.config).await.unwrap();
    let before = fixture.card();
    let original_file = fixture.description();
    let mut other = parsed.clone();
    other.page_url = "https://example.test/topic?t=43".into();
    let other = fixture.db.upsert_scraped_item(&other).unwrap().page_id;
    let mut edit = TorrentEdit::from(&before);
    edit.primary_studio = "New Company".into();
    edit.studios.push("New Company".into());
    edit.actors = Some(vec!["Keep Actor".into()]);
    edit.directors = Some(vec!["OLD DIRECTOR".into(), "New Director".into()]);
    edit.description = Some("Revised synopsis.".into());
    edit.full_description = Some("Edited full description.\n\nSecond paragraph.".into());

    let library = TorrentLibrary::new(fixture.db.clone(), fixture.config.effective_download_dir());
    let revision = before.revision;
    let page_id = before.page_id;
    let saved = tokio::task::spawn_blocking(move || {
        let plan = library.prepare(page_id, revision, edit, false).unwrap();
        assert!(plan.description_export.is_some());
        assert!(!plan.files.is_empty());
        library.apply(plan, |_| {}).unwrap()
    })
    .await
    .unwrap();
    let directory = fixture
        .config
        .effective_download_dir()
        .join(saved.directory.as_ref().unwrap());
    let export = directory.join("description.txt");
    let expected = format!(
        "{}\n\nEdited full description.\n\nSecond paragraph.\n",
        parsed.title
    );
    assert_eq!(std::fs::read_to_string(&export).unwrap(), expected);
    assert!(!original_file.exists());
    assert_eq!(saved.values("director"), ["New Director", "OLD DIRECTOR"]);
    assert_eq!(
        fixture.db.torrent_card(other).unwrap().values("director"),
        ["Old Director"]
    );
    assert_eq!(saved.values("actor"), ["Keep Actor"]);
    assert_eq!(saved.primary_studio, "New Company");
    assert!(saved.values("studio").contains(&"New Company".to_owned()));

    {
        let mut page = fixture.page.write().unwrap();
        page.attributes.retain(|a| {
            !matches!(
                a.kind.as_str(),
                "description" | "full_description" | "director"
            )
        });
        page.attributes.extend([
            attribute("description", "Fresh remote synopsis."),
            attribute("full_description", "Fresh remote description."),
            attribute("director", "Fresh Remote Director"),
            attribute("actor", "Newly Listed Actor"),
        ]);
    }
    fixture.service.run(&fixture.config).await.unwrap();
    let saved = fixture.db.torrent_card(page_id).unwrap();
    assert_eq!(saved.value("description"), "Revised synopsis.");
    assert_eq!(saved.values("actor"), ["Keep Actor", "Newly Listed Actor"]);
    assert_eq!(saved.values("director"), ["New Director", "OLD DIRECTOR"]);
    assert_eq!(std::fs::read_to_string(&export).unwrap(), expected);
    let matching = fixture
        .db
        .search_torrents(&TorrentQuery {
            creators: vec!["new director".into()],
            text: "Edited full description".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(matching.cards.len(), 1);
    assert_eq!(matching.cards[0].page_id, page_id);
}

#[tokio::test]
async fn clearing_the_full_description_falls_back_to_the_synopsis_on_future_downloads() {
    let fixture = Fixture::new(page());
    fixture.service.run(&fixture.config).await.unwrap();
    let before = fixture.card();
    let edit = TorrentEdit {
        full_description: Some(String::new()),
        ..TorrentEdit::from(&before)
    };
    let saved = fixture.save_edit(edit).await;
    assert!(saved.value("full_description").is_empty());
    let expected = format!("{}\n\nShort synopsis.\n", before.title);
    assert_eq!(
        std::fs::read_to_string(fixture.description()).unwrap(),
        expected
    );
    fixture.service.run(&fixture.config).await.unwrap();
    assert!(fixture.card().value("full_description").is_empty());
    assert_eq!(
        std::fs::read_to_string(fixture.description()).unwrap(),
        expected
    );
}

#[tokio::test]
async fn catalog_fields_override_scraped_values_and_rename_only_when_requested() {
    let mut parsed = page();
    parsed.attributes.extend([
        attribute("production_date", "2024-05-12"),
        attribute("release_date", "2025-08-14"),
        attribute("duration", "00:30:15"),
        attribute("actor", "Known Actor"),
    ]);
    let fixture = Fixture::new(parsed);
    fixture.service.run(&fixture.config).await.unwrap();
    let before = fixture.card();
    let original = fixture.description();
    let mut edit = TorrentEdit::from(&before);
    edit.details.extend(
        [
            (TorrentField::Title, "SHORT FOLDER TITLE"),
            (TorrentField::Release, "2000-02-29"),
            (TorrentField::Duration, "01:25:32"),
            (TorrentField::OriginalLanguage, "English"),
            (TorrentField::TranslationLanguage, "German"),
            (TorrentField::Series, "Example collection"),
            (TorrentField::Episode, "Part 2"),
            (TorrentField::StudioCode, "EX-42"),
        ]
        .into_iter()
        .map(|(field, value)| (field, value.to_owned())),
    );
    edit.actors = Some(vec!["KNOWN ACTOR".into(), "Another Actor".into()]);
    let saved = fixture.save_edit(edit).await;
    assert_eq!(saved.directory, before.directory);
    assert_eq!(saved.title, before.title);
    assert_eq!(saved.content_title(), "SHORT FOLDER TITLE");
    assert_eq!(
        fixture
            .db
            .search_torrent_summaries(&TorrentQuery::default())
            .unwrap()
            .cards[0]
            .content_title,
        "SHORT FOLDER TITLE"
    );
    assert_eq!(saved.value("date"), "2000-02-29");
    assert_eq!(saved.value("duration"), "01:25:32");
    assert_eq!(saved.values("actor"), ["Another Actor", "KNOWN ACTOR"]);
    assert_eq!(saved.value("language"), "English");
    assert_eq!(saved.value("translation_language"), "German");
    assert_eq!(saved.value("series"), "Example collection");
    assert_eq!(saved.value("episode"), "Part 2");
    assert_eq!(saved.value("studio_code"), "EX-42");
    assert!(original.is_file());

    let mut edit = TorrentEdit::from(&saved);
    edit.regenerate_folder = true;
    edit.details
        .insert(TorrentField::Title, "Corrected title".into());
    edit.details.insert(TorrentField::Release, "2001".into());
    let renamed = fixture.save_edit(edit).await;
    assert!(renamed
        .directory_name()
        .unwrap()
        .starts_with("Corrected title (2001)"));
    assert!(renamed.folder_name.is_none());
    assert!(!original.exists());
    let export = fixture.description();
    assert!(export.is_file());
    fixture.service.run(&fixture.config).await.unwrap();
    let latest = fixture.card();
    assert_eq!(latest.directory, renamed.directory);
    assert_eq!(latest.content_title(), "Corrected title");
    assert_eq!(latest.value("year"), "2001");
    assert!(latest.value("production_date").is_empty());
    assert!(latest.value("release_date").is_empty());
    assert_eq!(latest.value("duration"), "01:25:32");
    assert_eq!(latest.values("actor"), ["Another Actor", "KNOWN ACTOR"]);
    assert!(std::fs::read_to_string(export)
        .unwrap()
        .starts_with(&before.title));
    for (year, expected) in [("2001", 1), ("2024", 0), ("2025", 0)] {
        let matched = fixture
            .db
            .search_torrent_summaries(&TorrentQuery {
                year: year.into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(matched.total, expected);
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
    let card = fixture.card();
    let library = TorrentLibrary::new(fixture.db.clone(), fixture.config.effective_download_dir());
    let edit = TorrentEdit {
        description: Some("Local edit".into()),
        ..TorrentEdit::from(&card)
    };
    let rejected = tokio::task::spawn_blocking(move || {
        library.prepare(card.page_id, card.revision, edit, false)
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(rejected
        .to_string()
        .contains("unregistered description.txt"));
    assert_eq!(fixture.card().value("description"), "Short synopsis.");
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

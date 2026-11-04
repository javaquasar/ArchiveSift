use archive_sift::{
    app::AppConfig,
    db::{Database, FileDownloadStore},
    fs::StructuredDirectoryStrategy,
    models::{
        AttachmentData, MetadataAttribute, ScrapedImage, ScrapedItem, TorrentEdit, TorrentQuery,
    },
    scraper::{ContentScraper, ScraperRegistry},
    services::{torrent_library::TorrentLibrary, ContentDownloadService},
    ui::ConsoleUi,
    AppResult,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct Adapter {
    html: bool,
    fetched: Arc<AtomicUsize>,
    magnet_only: bool,
    image_url: Option<String>,
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
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>> {
        Box::pin(async move {
            Ok(ScrapedItem {
                site_key: self.site_key().into(),
                page_url: url.into(),
                title: "Attachment example".into(),
                raw_html: String::new(),
                images: self
                    .image_url
                    .iter()
                    .map(|url| ScrapedImage {
                        source_url: url.clone(),
                        preview_url: None,
                        alt_text: None,
                        width: None,
                        height: None,
                        sort_order: 0,
                        local_file_path: None,
                    })
                    .collect(),
                attributes: vec![if self.magnet_only {
                    MetadataAttribute {
                        kind: "magnet".into(),
                        label: "Magnet".into(),
                        value: "magnet:?xt=urn:btih:0123456789012345678901234567890123456789"
                            .into(),
                        source_url: None,
                        is_female: None,
                    }
                } else {
                    MetadataAttribute {
                        kind: "attachment".into(),
                        label: "../../demo.torrent".into(),
                        value: "https://example.test/download?id=42".into(),
                        source_url: Some("https://example.test/download?id=42".into()),
                        is_female: None,
                    }
                }],
            })
        })
    }
    fn fetch_attachment<'a>(
        &'a self,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<AttachmentData>> + Send + 'a>> {
        self.fetched.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(AttachmentData {
                bytes: if self.html {
                    b"<html>Login</html>".to_vec()
                } else {
                    b"d4:infod6:lengthi1e4:name4:demoee".to_vec()
                },
                content_type: if self.html {
                    "text/html"
                } else {
                    "application/x-bittorrent"
                }
                .into(),
            })
        })
    }
}

#[tokio::test]
async fn attachments_without_images_are_persisted_atomically_and_reused() {
    let root = tempfile::tempdir().unwrap();
    let fetched = Arc::new(AtomicUsize::new(0));
    let mut registry = ScraperRegistry::new();
    registry.register(Box::new(Adapter {
        html: false,
        fetched: fetched.clone(),
        magnet_only: false,
        image_url: None,
    }));
    let database = root.path().join("archive.sqlite3");
    let config = AppConfig {
        source_url: "https://example.test/content/42".into(),
        database_path: database.to_string_lossy().into(),
        download_folder: root.path().join("downloads").to_string_lossy().into(),
        ..AppConfig::default()
    };
    let service = ContentDownloadService::new(
        registry,
        Database::new(&database),
        FileDownloadStore::new(),
        ConsoleUi::new(),
        StructuredDirectoryStrategy,
        config.download_folder.clone().into(),
        root.path().join("thumbs"),
    );
    service.run(&config).await.unwrap();
    service.run(&config).await.unwrap();
    assert_eq!(fetched.load(Ordering::SeqCst), 1);
    let db = Database::new(&database);
    let card = db
        .search_torrents(&TorrentQuery::default())
        .unwrap()
        .cards
        .remove(0);
    assert!(card.images.is_empty());
    assert!(card.directory.as_ref().unwrap().starts_with("torrents"));
    let edit = TorrentEdit {
        primary_studio: "Chosen studio".into(),
        tags: vec!["Manual tag".into()],
        categories: vec!["Collection".into()],
        ..TorrentEdit::from(&card)
    };
    let root_dir = config.effective_download_dir();
    let db_clone = db.clone();
    tokio::task::spawn_blocking(move || {
        let library = TorrentLibrary::new(db_clone, root_dir);
        let plan = library
            .prepare(card.page_id, card.revision, edit, false)
            .unwrap();
        library.apply(plan, |_| {}).unwrap();
    })
    .await
    .unwrap();
    service.run(&config).await.unwrap();
    assert_eq!(
        fetched.load(Ordering::SeqCst),
        1,
        "a confirmed move must not cause another attachment download"
    );
    let saved = db
        .search_torrents(&TorrentQuery::default())
        .unwrap()
        .cards
        .remove(0);
    assert_eq!(saved.primary_studio, "Chosen studio");
    assert_eq!(saved.values("tag"), ["Manual tag"]);
    let connection = rusqlite::Connection::open(&database).unwrap();
    let mut q = connection
        .prepare("SELECT filename,local_file_path FROM page_attachments ORDER BY filename")
        .unwrap();
    let files = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 2);
    for (name, relative) in files {
        let file = std::path::Path::new(&config.download_folder)
            .join(&relative)
            .canonicalize()
            .unwrap();
        assert!(file.starts_with(root.path().canonicalize().unwrap()));
        let bytes = std::fs::read(file).unwrap();
        if name == "Magnet" {
            assert!(bytes.starts_with(b"magnet:?xt=urn:btih:"));
        } else {
            assert_eq!(bytes, b"d4:infod6:lengthi1e4:name4:demoee");
        }
    }
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM source_pages", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn login_html_never_becomes_a_torrent_and_magnets_need_no_network() {
    for (html, magnet_only) in [(true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let fetched = Arc::new(AtomicUsize::new(0));
        let mut registry = ScraperRegistry::new();
        registry.register(Box::new(Adapter {
            html,
            fetched: fetched.clone(),
            magnet_only,
            image_url: None,
        }));
        let db = root.path().join("archive.sqlite3");
        let config = AppConfig {
            source_url: "https://example.test/content/42".into(),
            database_path: db.to_string_lossy().into(),
            ..AppConfig::default()
        };
        let service = ContentDownloadService::new(
            registry,
            Database::new(&db),
            FileDownloadStore::new(),
            ConsoleUi::new(),
            StructuredDirectoryStrategy,
            root.path().join("downloads"),
            root.path().join("thumbs"),
        );
        let result = service.run(&config).await;
        assert_eq!(result.is_ok(), magnet_only);
        let connection = rusqlite::Connection::open(db).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM page_attachments", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            if magnet_only { 1 } else { 0 }
        );
        assert_eq!(
            fetched.load(Ordering::SeqCst),
            if magnet_only { 0 } else { 1 }
        );
    }
}

#[tokio::test]
async fn downloaded_images_and_attachments_are_reused_after_a_studio_move() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = tempfile::tempdir().unwrap();
    let fetched = Arc::new(AtomicUsize::new(0));
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let image_url = format!("http://{}/cover.png", listener.local_addr().unwrap());
    let server_hits = hits.clone();
    let server = tokio::spawn(async move {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut input = [0; 4096];
            let _ = socket.read(&mut input).await.unwrap();
            server_hits.fetch_add(1, Ordering::SeqCst);
            let header=format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",png.len());
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(&png).await.unwrap();
        }
    });
    let mut registry = ScraperRegistry::new();
    registry.register(Box::new(Adapter {
        html: false,
        magnet_only: false,
        fetched: fetched.clone(),
        image_url: Some(image_url),
    }));
    let config = AppConfig {
        source_url: "https://example.test/topic?t=9".into(),
        database_path: root.path().join("library.sqlite3").display().to_string(),
        download_folder: root.path().join("downloads").display().to_string(),
        ..Default::default()
    };
    let db = Database::new(&config.database_path);
    let log = archive_sift::ui::UiLog::default();
    let service = ContentDownloadService::new(
        registry,
        db.clone(),
        FileDownloadStore::new(),
        ConsoleUi::from_log(log.clone()),
        StructuredDirectoryStrategy,
        config.effective_download_dir(),
        root.path().join("thumbs"),
    );
    service.run(&config).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let card = db
        .search_torrents(&TorrentQuery::default())
        .unwrap()
        .cards
        .remove(0);
    let original = card.directory.clone().unwrap();
    let db_copy = db.clone();
    let base = config.effective_download_dir();
    tokio::task::spawn_blocking(move || {
        let library = TorrentLibrary::new(db_copy, base);
        let edit = TorrentEdit {
            primary_studio: "Studio selected".into(),
            ..TorrentEdit::from(&card)
        };
        let plan = library
            .prepare(card.page_id, card.revision, edit, false)
            .unwrap();
        assert_eq!(plan.files.len(), 3);
        library.apply(plan, |_| {}).unwrap();
    })
    .await
    .unwrap();
    log.clear();
    service.run(&config).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(fetched.load(Ordering::SeqCst), 1);
    assert!(!log
        .snapshot()
        .iter()
        .any(|s| s.starts_with("Failed to download")));
    assert!(!config.effective_download_dir().join(original).exists());
    let saved = db
        .search_torrents(&TorrentQuery::default())
        .unwrap()
        .cards
        .remove(0);
    let relative = saved.images[0].local_file_path.as_ref().unwrap();
    assert!(relative.starts_with(saved.directory.as_ref().unwrap()));
    assert!(image::open(config.effective_download_dir().join(relative)).is_ok());
    assert!(root.path().join("thumbs/0/1.jpg").is_file());
    server.abort();
}

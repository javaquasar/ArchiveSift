use archive_sift::{
    app::AppConfig,
    db::{Database, FileDownloadStore},
    fs::StructuredDirectoryStrategy,
    models::{AttachmentData, MetadataAttribute, ScrapedItem},
    scraper::{ContentScraper, ScraperRegistry},
    services::ContentDownloadService,
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
                images: vec![],
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

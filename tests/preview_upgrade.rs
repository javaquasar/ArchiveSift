use archive_sift::{
    app::AppConfig,
    db::{Database, FileDownloadStore},
    fs::StructuredDirectoryStrategy,
    models::{MetadataAttribute, ScrapedImage, ScrapedItem, TorrentQuery},
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
        Arc, RwLock,
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Adapter(Arc<RwLock<ScrapedItem>>);
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
        Box::pin(async move { Ok(self.0.read().unwrap().clone()) })
    }
}

#[tokio::test]
async fn original_with_same_basename_is_downloaded_and_reused_without_thumbnail_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let size = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            let side = if request.contains("/full/") { 8 } else { 2 };
            let mut body = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(side, side)
                .write_to(&mut body, image::ImageFormat::Png)
                .unwrap();
            let body = body.into_inner();
            counter.fetch_add(1, Ordering::SeqCst);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
    });
    let preview = format!("{origin}/thumb/image.png");
    let original = format!("{origin}/full/image.png");
    let page = Arc::new(RwLock::new(ScrapedItem {
        site_key: "example-source".into(),
        page_url: format!("{origin}/topic/1"),
        title: "Example release".into(),
        raw_html: String::new(),
        images: vec![ScrapedImage {
            source_url: preview.clone(),
            preview_url: None,
            alt_text: None,
            width: None,
            height: None,
            sort_order: 0,
            local_file_path: None,
        }],
        attributes: vec![MetadataAttribute {
            kind: "magnet".into(),
            label: "Magnet".into(),
            value: "magnet:?xt=urn:btih:0123456789012345678901234567890123456789".into(),
            source_url: None,
            is_female: None,
        }],
    }));
    let config = AppConfig {
        source_url: page.read().unwrap().page_url.clone(),
        database_path: root.path().join("archive.sqlite3").display().to_string(),
        download_folder: root.path().join("files").display().to_string(),
        ..Default::default()
    };
    let mut registry = ScraperRegistry::new();
    registry.register(Box::new(Adapter(page.clone())));
    let db = Database::new(&config.database_path);
    let service = ContentDownloadService::new(
        registry,
        db.clone(),
        FileDownloadStore::new(),
        ConsoleUi::new(),
        StructuredDirectoryStrategy,
        config.effective_download_dir(),
        root.path().join("thumbs"),
    );
    service.run(&config).await.unwrap();
    let before = db
        .search_torrents(&TorrentQuery::default())
        .unwrap()
        .cards
        .remove(0);
    let old_path = config
        .effective_download_dir()
        .join(before.images[0].local_file_path.as_ref().unwrap());
    assert_eq!(image::open(&old_path).unwrap().width(), 2);
    {
        let mut page = page.write().unwrap();
        page.images[0].source_url = original.clone();
        page.images[0].preview_url = Some(preview);
    }
    service.run(&config).await.unwrap();
    let after = db.torrent_card(before.page_id).unwrap();
    assert_eq!(after.images.len(), 1);
    assert_eq!(after.images[0].source_url, original);
    let path = config
        .effective_download_dir()
        .join(after.images[0].local_file_path.as_ref().unwrap());
    assert_ne!(path, old_path);
    assert_eq!(image::open(&path).unwrap().width(), 8);
    assert_eq!(image::open(&old_path).unwrap().width(), 2);
    service.run(&config).await.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(db.torrent_card(before.page_id).unwrap().images.len(), 1);
    server.abort();
}

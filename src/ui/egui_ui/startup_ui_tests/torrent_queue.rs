use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use scraper::{Html, Selector};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::super::{shared_async_runtime, DesktopApp};
use super::{click_label, render, test_app, wait_for_label};
use crate::db::Database;
use crate::models::{MetadataAttribute, QueueFailureKind, ScrapedImage, ScrapedItem};
use crate::scraper::{ContentScraper, ScraperRegistry};
use crate::services::url_queue::QueueJob;
use crate::services::UrlQueueService;
use crate::AppResult;

const TORRENT: &[u8] = b"d4:infod6:lengthi1e4:name4:demoee";
const MAGNET: &str = "magnet:?xt=urn:btih:0123456789012345678901234567890123456789";
const CARD_TITLE: &str = "Saved torrent queue fixture";

struct TorrentFixtureServer {
    page_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl TorrentFixtureServer {
    fn start(with_torrent_link: bool) -> Self {
        let runtime = shared_async_runtime().unwrap();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let page_url = format!("http://{}/topic", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&requests);
        let torrent_link = if with_torrent_link {
            "<a href='/release.torrent'>Download torrent</a>"
        } else {
            ""
        };
        let topic = format!(
            "<h1>Remote title</h1><img src='/cover.png'>
             <a href='/notes.txt'>Notes</a><a href='{MAGNET}'>Magnet</a>{torrent_link}"
        );
        let task = runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                if stream.read_line(&mut line).await.unwrap() == 0 {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                loop {
                    line.clear();
                    if stream.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let (status, content_type, bytes) = match path.as_str() {
                    "/topic" => ("200 OK", "text/html", topic.as_bytes()),
                    "/release.torrent" => ("200 OK", "application/x-bittorrent", TORRENT),
                    _ => ("404 Not Found", "text/plain", b"Unexpected request".as_slice()),
                };
                received.lock().unwrap().push(path);
                let headers = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                stream.get_mut().write_all(headers.as_bytes()).await.unwrap();
                stream.get_mut().write_all(bytes).await.unwrap();
            }
        });
        Self {
            page_url,
            requests,
            task,
        }
    }

    fn assert_requests(&self, expected: &[&str]) {
        assert_eq!(self.requests.lock().unwrap().as_slice(), expected);
    }
}

impl Drop for TorrentFixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct TorrentFixtureScraper {
    page_url: String,
}

impl ContentScraper for TorrentFixtureScraper {
    fn site_key(&self) -> &str {
        "torrent-queue-fixture"
    }

    fn supports_url(&self, url: &str) -> bool {
        url == self.page_url
    }

    fn scrape_content<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>> {
        Box::pin(async move {
            let markup = crate::http::fetch_text(url).await?;
            let html = Html::parse_document(&markup);
            let base = url::Url::parse(url)?;
            let links = Selector::parse("a[href]").unwrap();
            let mut attributes = Vec::new();
            for href in html
                .select(&links)
                .filter_map(|link| link.value().attr("href"))
            {
                if href.starts_with("magnet:") {
                    attributes.push(magnet_attribute());
                } else {
                    attributes.push(MetadataAttribute {
                        kind: "attachment".into(),
                        label: href.trim_start_matches('/').into(),
                        value: String::new(),
                        source_url: Some(base.join(href)?.into()),
                        is_female: None,
                    });
                }
            }
            let title = html.select(&Selector::parse("h1").unwrap()).next().unwrap();
            Ok(ScrapedItem {
                site_key: self.site_key().into(),
                page_url: url.into(),
                title: title.text().collect::<String>(),
                raw_html: markup,
                images: vec![ScrapedImage {
                    source_url: base.join("/cover.png")?.into(),
                    preview_url: None,
                    alt_text: None,
                    width: None,
                    height: None,
                    sort_order: 0,
                    local_file_path: None,
                }],
                attributes,
            })
        })
    }
}

fn magnet_attribute() -> MetadataAttribute {
    MetadataAttribute {
        kind: "magnet".into(),
        label: "Magnet".into(),
        value: MAGNET.into(),
        source_url: None,
        is_female: None,
    }
}

fn saved_card(
    app: &mut DesktopApp,
    root: &tempfile::TempDir,
    server: &TorrentFixtureServer,
) -> (Database, i64) {
    let mut registry = ScraperRegistry::new();
    let scraper = TorrentFixtureScraper {
        page_url: server.page_url.clone(),
    };
    let page = ScrapedItem {
        site_key: scraper.site_key().into(),
        page_url: server.page_url.clone(),
        title: CARD_TITLE.into(),
        raw_html: String::new(),
        images: vec![],
        attributes: vec![magnet_attribute()],
    };
    registry.register(Box::new(scraper));
    app.queue_service = Arc::new(UrlQueueService::with_registry_and_queue_path(
        registry,
        root.path().join("queue.sqlite3"),
    ));
    app.window.config.request_delay_min_ms = 0;
    app.window.config.request_delay_max_ms = 0;
    let db = Database::new(&app.window.config.database_path);
    db.initialize().unwrap();
    let page_id = db.upsert_scraped_item(&page).unwrap().page_id;
    (db, page_id)
}

fn click_torrent_download(app: &mut DesktopApp, context: &egui::Context) {
    let size = egui::vec2(1900.0, 1400.0);
    let output = render(app, context, size, vec![]);
    let output = click_label(app, context, &output, "Search");
    click_label(app, context, &output, "Torrents");
    let output = wait_for_label(app, context, size, CARD_TITLE);
    click_label(app, context, &output, CARD_TITLE);
    let output = wait_for_label(app, context, size, "Download .torrent only");
    click_label(app, context, &output, "Download .torrent only");
    assert!(app
        .window
        .log
        .snapshot()
        .iter()
        .any(|line| line == "Torrent file download queued."));

    let deadline = Instant::now() + Duration::from_secs(10);
    while app.worker_running || app.queue_service.has_pending_work() {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for the torrent queue"
        );
        render(app, context, size, vec![]);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_retry(app: &DesktopApp, url: &str, kind: QueueFailureKind, message: &str) {
    let queue = app.queue_service.snapshot_for_ui(0..0, Some((None, 0)));
    assert_eq!(queue.failed_total, 1);
    let [failure] = queue.failed_preview.as_slice() else {
        panic!("Expected the queued failure in the failed download list");
    };
    assert_eq!(failure.url, url);
    assert_eq!(failure.kind, kind);
    assert!(failure.error.contains(message), "{}", failure.error);
    assert!(app.queue_service.retry_failure(url).unwrap());
    assert_eq!(
        app.queue_service.take_next_job(),
        Some((url.into(), QueueJob::Torrent))
    );
    app.queue_service.mark_finished(url);
}

#[test]
fn torrent_queue_button_downloads_one_file_through_the_worker() {
    let (mut app, root, context) = test_app(true);
    let server = TorrentFixtureServer::start(true);
    let (db, page_id) = saved_card(&mut app, &root, &server);
    let downloads = app.window.config.effective_download_dir();
    std::fs::create_dir(&downloads).unwrap();

    click_torrent_download(&mut app, &context);

    assert_eq!(app.queue_service.snapshot().failed_total, 0);
    let card = db.torrent_card(page_id).unwrap();
    assert_eq!(card.title, CARD_TITLE);
    assert!(card.images.is_empty());
    assert_eq!(card.attachments.len(), 1);
    let attachment = &card.attachments[0];
    assert_eq!(attachment.filename, "release.torrent");
    let folder = downloads.join(card.directory.unwrap());
    assert_eq!(std::fs::read_dir(folder).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(downloads.join(&attachment.local_file_path)).unwrap(),
        TORRENT
    );
    server.assert_requests(&["/topic", "/release.torrent"]);
}

#[test]
fn torrent_queue_keeps_a_retry_when_download_storage_is_missing() {
    let (mut app, root, context) = test_app(true);
    let server = TorrentFixtureServer::start(true);
    let (db, page_id) = saved_card(&mut app, &root, &server);

    click_torrent_download(&mut app, &context);

    assert_retry(
        &app,
        &server.page_url,
        QueueFailureKind::Storage,
        "Library storage unavailable",
    );
    assert!(!app.window.config.effective_download_dir().exists());
    assert!(db.torrent_card(page_id).unwrap().attachments.is_empty());
    server.assert_requests(&[]);
}

#[test]
fn torrent_queue_keeps_a_retry_when_source_has_only_a_magnet() {
    let (mut app, root, context) = test_app(true);
    let server = TorrentFixtureServer::start(false);
    let (db, page_id) = saved_card(&mut app, &root, &server);
    let downloads = app.window.config.effective_download_dir();
    std::fs::create_dir(&downloads).unwrap();

    click_torrent_download(&mut app, &context);

    assert_retry(
        &app,
        &server.page_url,
        QueueFailureKind::Attachment,
        "The source did not provide a .torrent link",
    );
    assert_eq!(std::fs::read_dir(downloads).unwrap().count(), 0);
    assert!(db.torrent_card(page_id).unwrap().attachments.is_empty());
    server.assert_requests(&["/topic"]);
}

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use archive_sift::app::AppConfig;
use archive_sift::db::{Database, FileDownloadStore};
use archive_sift::fs::StructuredDirectoryStrategy;
use archive_sift::models::SearchQuery;
use archive_sift::scraper::ScraperRegistry;
use archive_sift::services::ContentDownloadService;
use archive_sift::ui::ConsoleUi;

const PAGE_PATH: &str = "/demo-site/galleries/random-showcase-004/";
const PAGE: &[u8] = include_bytes!("../demo-site/galleries/random-showcase-004/index.html");
const IMAGES: [(&str, &[u8]); 6] = [
    (
        "/demo-site/media/random-showcase-004/random-001.png",
        include_bytes!("../demo-site/media/random-showcase-004/random-001.png"),
    ),
    (
        "/demo-site/media/random-showcase-004/random-002.png",
        include_bytes!("../demo-site/media/random-showcase-004/random-002.png"),
    ),
    (
        "/demo-site/media/random-showcase-004/random-003.png",
        include_bytes!("../demo-site/media/random-showcase-004/random-003.png"),
    ),
    (
        "/demo-site/media/random-showcase-004/random-004.png",
        include_bytes!("../demo-site/media/random-showcase-004/random-004.png"),
    ),
    (
        "/demo-site/media/random-showcase-004/random-005.png",
        include_bytes!("../demo-site/media/random-showcase-004/random-005.png"),
    ),
    (
        "/demo-site/media/random-showcase-004/random-006.png",
        include_bytes!("../demo-site/media/random-showcase-004/random-006.png"),
    ),
];

fn unique_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after the epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("archivesift_demo_integration_{nanos}"))
}

fn start_demo_server() -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind demo server");
    let address = listener.local_addr().expect("read demo server address");
    let handle = thread::spawn(move || {
        let mut requested_paths = Vec::new();
        for _ in 0..(1 + IMAGES.len()) {
            let (mut stream, _) = listener.accept().expect("accept demo request");
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).expect("read demo request");
            let first_line = String::from_utf8_lossy(&request[..length])
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let path = first_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            requested_paths.push(path.clone());

            let (status, content_type, body) = if path == PAGE_PATH {
                ("200 OK", "text/html; charset=utf-8", PAGE)
            } else if let Some((_, body)) = IMAGES.iter().find(|(candidate, _)| *candidate == path)
            {
                ("200 OK", "image/png", *body)
            } else {
                ("404 Not Found", "text/plain", b"not found" as &[u8])
            };

            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write demo response headers");
            stream.write_all(body).expect("write demo response body");
        }
        requested_paths
    });

    (format!("http://{address}{PAGE_PATH}"), handle)
}

fn assert_attribute(result: &archive_sift::models::SearchImageResult, kind: &str, value: &str) {
    assert!(
        result
            .attributes
            .iter()
            .any(|attribute| attribute.kind == kind && attribute.value == value),
        "missing {kind}={value}"
    );
}

#[tokio::test]
async fn demo_page_downloads_images_persists_metadata_and_supports_filters() {
    let root = unique_root();
    let database_path = root.join("archivesift.sqlite3");
    let download_dir = root.join("downloads");
    let thumbnail_dir = root.join("thumbs");
    let (page_url, server) = start_demo_server();

    let service = ContentDownloadService::new(
        ScraperRegistry::with_public_scrapers(),
        Database::new(&database_path),
        FileDownloadStore::new(),
        ConsoleUi::new(),
        StructuredDirectoryStrategy,
        download_dir.clone(),
        thumbnail_dir,
    );
    let config = AppConfig {
        source_url: page_url.clone(),
        database_path: database_path.to_string_lossy().into_owned(),
        download_folder: download_dir.to_string_lossy().into_owned(),
        max_concurrent_downloads: IMAGES.len(),
        request_delay_min_ms: 0,
        request_delay_max_ms: 0,
        ..AppConfig::default()
    };

    service.run(&config).await.expect("download demo page");
    let requested_paths = server.join().expect("demo server should stop");

    assert_eq!(requested_paths.len(), 1 + IMAGES.len());
    assert!(requested_paths.iter().any(|path| path == PAGE_PATH));
    for (path, _) in IMAGES {
        assert!(requested_paths.iter().any(|requested| requested == path));
    }

    let results = Database::new(&database_path)
        .search_images(&SearchQuery {
            site_key: Some("demo-site".to_string()),
            tags: vec!["generated".to_string()],
            ..SearchQuery::default()
        })
        .expect("search downloaded demo images");

    assert_eq!(results.len(), IMAGES.len());
    assert!(results
        .iter()
        .all(|result| result.page_title() == "Random Pixel Collection"));
    assert!(results.iter().all(|result| result.page_url() == page_url));

    let first = &results[0];
    assert_attribute(first, "actor", "Alice Blue");
    assert_attribute(first, "tag", "random-seed");
    assert_attribute(first, "category", "integration-test");
    assert_attribute(first, "studio", "ArchiveSift Lab");
    assert_attribute(first, "date", "2026-10-28");
    assert_attribute(first, "director", "Seed Generator");
    assert_attribute(first, "duration", "00:06:00");
    assert_attribute(first, "studio_code", "DEMO-RANDOM-004");
    assert_attribute(
        first,
        "description",
        "Six deterministic generated images used to verify metadata parsing, relative URL resolution, concurrent downloads, thumbnails, SQLite persistence, and search filters.",
    );
    assert_attribute(first, "resource_link", "./");

    for index in 1..=IMAGES.len() {
        let file = download_dir
            .join("demo-site")
            .join("alice_blue")
            .join("random_pixel_collection")
            .join(format!("{index:03}_random-{index:03}.png"));
        assert!(file.is_file(), "missing downloaded file {}", file.display());
        assert_eq!(
            &std::fs::read(&file).expect("read downloaded PNG")[..8],
            b"\x89PNG\r\n\x1a\n"
        );
    }
    for result in &results {
        let thumbnail = result
            .thumbnail_path
            .as_deref()
            .map(Path::new)
            .expect("downloaded result should have a thumbnail");
        assert!(
            thumbnail.is_file(),
            "missing thumbnail {}",
            thumbnail.display()
        );
    }

    // The process-wide SQLite runtime cache can retain a Windows file handle
    // until test process shutdown. CI workspaces are ephemeral, so cleanup is
    // best-effort here, matching the database unit tests.
    let _ = std::fs::remove_dir_all(root);
}

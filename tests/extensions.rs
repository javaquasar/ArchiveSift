use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use archive_sift::app::AppConfig;
use archive_sift::db::{Database, FileDownloadStore};
use archive_sift::fs::StructuredDirectoryStrategy;
use archive_sift::models::{PageKind, SearchQuery};
use archive_sift::scraper::extensions::manifest::{
    Limits, PluginManifest, Route, RouteKind, RuntimeKind,
};
use archive_sift::scraper::extensions::ExtensionScraper;
use archive_sift::scraper::{ContentScraper, ScraperRegistry};
use archive_sift::services::ContentDownloadService;
use archive_sift::ui::ConsoleUi;
use sha2::{Digest, Sha256};

const PAGE: &str = include_str!("../demo-site/galleries/random-showcase-004/index.html");
const PNG: &[u8] = include_bytes!("../demo-site/media/random-showcase-004/random-001.png");
type Response = (u16, Vec<(String, String)>, Vec<u8>);

/// Nonblocking server with bounded reads; a failed test cannot hang accept().
struct Server {
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: Option<std::thread::JoinHandle<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Server {
    fn new(handler: impl Fn(&str) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let task = std::thread::spawn(move || {
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    assert!(request.len() < 1024 * 1024);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                received.lock().unwrap().push(request.clone());
                let (status, headers, body) = handler(&request);
                let mut response = format!(
                    "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (name, value) in headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Self {
            origin,
            requests,
            task: Some(task),
            stop,
        }
    }

    fn demo() -> Self {
        Self::new(|request| {
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            if path.contains("/media/") {
                return (
                    200,
                    vec![("Content-Type".into(), "image/png".into())],
                    PNG.to_vec(),
                );
            }
            let body = match path {
                "/galleries/random/" => PAGE,
                "/channels/static/" => {
                    r#"<a class="demo-gallery-link" href="/galleries/random/"></a><a class="demo-next-page" href="/channels/page-2/"></a>"#
                }
                "/channels/lazy/" => r#"<main data-demo-lazy-source="/api/one"></main>"#,
                "/api/one" => {
                    r#"{"items":[{"href":"/galleries/random/"}],"next_payload":"/api/two"}"#
                }
                "/api/two" => {
                    r#"{"items":[{"href":"/galleries/other/"},{"href":"/galleries/random/"}],"next_payload":null}"#
                }
                "/api/post" => "posted",
                _ => "fixture",
            };
            (
                200,
                vec![("Content-Type".into(), "text/html".into())],
                body.as_bytes().to_vec(),
            )
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}

fn fixture(runtime: RuntimeKind) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = match runtime {
        RuntimeKind::Wasm => root.join("target/test-plugins/wasm/adapter.wasm"),
        RuntimeKind::Process => root.join(if cfg!(windows) {
            "target/test-plugins/process/adapter.exe"
        } else {
            "target/test-plugins/process/adapter"
        }),
    };
    assert!(
        path.is_file(),
        "missing real plugin fixture {}; run python scripts/test_extensions.py --prepare-only",
        path.display()
    );
    path
}

fn make_package(
    runtime: RuntimeKind,
    origin: &str,
    limits: Limits,
) -> (tempfile::TempDir, PluginManifest) {
    let root = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(fixture(runtime)).unwrap();
    let entry = match runtime {
        RuntimeKind::Wasm => "adapter.wasm",
        RuntimeKind::Process if cfg!(windows) => "adapter.exe",
        _ => "adapter",
    };
    std::fs::write(root.path().join(entry), &bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.path().join(entry),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let manifest = PluginManifest {
        schema: 1,
        id: "fixture.parser".into(),
        version: "1.0.0".into(),
        api: "1.0".into(),
        display_name: "Fixture extension".into(),
        source_key: "fixture-extension".into(),
        runtime,
        entry: entry.into(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        enabled: true,
        trusted: runtime == RuntimeKind::Process,
        routes: vec![
            Route {
                host: "127.0.0.1".into(),
                path_prefix: "/galleries/".into(),
                kind: RouteKind::Content,
            },
            Route {
                host: "127.0.0.1".into(),
                path_prefix: "/channels/".into(),
                kind: RouteKind::Listing,
            },
        ],
        network_origins: vec![origin.into()],
        allow_http: true,
        limits,
    };
    write_manifest(root.path(), &manifest);
    (root, manifest)
}

fn write_manifest(directory: &Path, manifest: &PluginManifest) {
    std::fs::write(
        directory.join("plugin.json"),
        serde_json::to_vec_pretty(manifest).unwrap(),
    )
    .unwrap();
}

async fn download(runtime: RuntimeKind) {
    let server = Server::demo();
    let (package, _) = make_package(runtime, &server.origin, Limits::default());
    let mut registry = ScraperRegistry::new();
    registry.register(Box::new(ExtensionScraper::load(package.path()).unwrap()));
    let root = tempfile::tempdir().unwrap();
    let database_path = root.path().join("archive.sqlite3");
    let downloads = root.path().join("downloads");
    let url = format!("{}/galleries/random/", server.origin);
    let config = AppConfig {
        source_url: url.clone(),
        database_path: database_path.to_string_lossy().into_owned(),
        download_folder: downloads.to_string_lossy().into_owned(),
        request_delay_min_ms: 0,
        request_delay_max_ms: 0,
        ..AppConfig::default()
    };
    let service = ContentDownloadService::new(
        registry,
        Database::new(&database_path),
        FileDownloadStore::new(),
        ConsoleUi::new(),
        StructuredDirectoryStrategy,
        downloads.clone(),
        root.path().join("thumbnails"),
    );
    service.run(&config).await.unwrap();
    let results = Database::new(&database_path)
        .search_images(&SearchQuery {
            site_key: Some("fixture-extension".into()),
            tags: vec!["generated".into()],
            ..SearchQuery::default()
        })
        .unwrap();
    assert_eq!(results.len(), 6);
    assert!(
        results
            .iter()
            .all(|result| result.page_title() == "Random Pixel Collection"
                && result.page_url() == url)
    );
    let attributes = results[0]
        .attributes
        .iter()
        .map(|attribute| (attribute.kind.as_str(), attribute.value.as_str()))
        .collect::<HashMap<_, _>>();
    for kind in [
        "actor",
        "tag",
        "category",
        "studio",
        "date",
        "duration",
        "studio_code",
        "director",
        "description",
        "resource_link",
    ] {
        assert!(attributes.contains_key(kind), "missing {kind}");
    }
    for result in &results {
        assert!(Path::new(result.thumbnail_path.as_ref().unwrap()).is_file());
        let file = downloads.join(result.local_file_path.as_ref().unwrap());
        assert_eq!(std::fs::read(file).unwrap(), PNG);
    }
    assert_eq!(server.requests.lock().unwrap().len(), 7);
}

#[tokio::test]
async fn wasm_downloads_images_persists_metadata_and_creates_thumbnails() {
    download(RuntimeKind::Wasm).await;
}

#[tokio::test]
async fn process_downloads_images_persists_metadata_and_creates_thumbnails() {
    download(RuntimeKind::Process).await;
}

#[tokio::test]
async fn both_runtimes_support_static_listings_lazy_api_and_post_continuations() {
    let server = Server::demo();
    for runtime in [RuntimeKind::Wasm, RuntimeKind::Process] {
        let (package, _) = make_package(runtime, &server.origin, Limits::default());
        let scraper = ExtensionScraper::load(package.path()).unwrap();
        let listing = scraper
            .scrape_listing(&format!("{}/channels/static/", server.origin))
            .await
            .unwrap();
        assert_eq!(listing.discovered_urls.len(), 1);
        assert_eq!(
            listing.next_page_url,
            Some(format!("{}/channels/page-2/", server.origin))
        );
        let listing = scraper
            .scrape_listing(&format!("{}/channels/lazy/", server.origin))
            .await
            .unwrap();
        assert_eq!(listing.discovered_urls.len(), 2);
        assert_eq!(
            listing.discovered_urls[1].url,
            format!("{}/galleries/other/", server.origin)
        );
        let post = scraper
            .scrape_content(&format!("{}/galleries/__fixture__/post", server.origin))
            .await
            .unwrap();
        assert_eq!(post.title, "POST complete");
    }
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("POST /api/post ")
                && request.contains("{\"query\":\"demo\"}"))
            .count(),
        2
    );
}

#[tokio::test]
async fn both_runtimes_enforce_permissions_result_kinds_errors_and_size_limits() {
    let server = Server::demo();
    for runtime in [RuntimeKind::Wasm, RuntimeKind::Process] {
        let limits = Limits {
            max_output_bytes: 8192,
            ..Limits::default()
        };
        let (package, _) = make_package(runtime, &server.origin, limits);
        let scraper = ExtensionScraper::load(package.path()).unwrap();
        for (mode, expected) in [
            ("fetch-denied", "permission"),
            ("image-denied", "permission"),
            ("wrong-kind", "wrong result kind"),
            ("error", "intentional extension error"),
            ("large-output", "output"),
        ] {
            let error = scraper
                .scrape_content(&format!("{}/galleries/__fixture__/{mode}", server.origin))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(expected) || (mode == "large-output" && error.contains("oversized")),
                "{runtime:?}/{mode}: {error}"
            );
        }
    }
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wasm_bounds_infinite_loops_memory_and_traps_and_recovers() {
    let server = Server::demo();
    let (package, _) = make_package(RuntimeKind::Wasm, &server.origin, Limits::default());
    let scraper = ExtensionScraper::load(package.path()).unwrap();
    for mode in ["loop", "memory", "trap"] {
        assert!(
            scraper
                .scrape_content(&format!("{}/galleries/__fixture__/{mode}", server.origin))
                .await
                .is_err(),
            "accepted {mode}"
        );
    }
    assert_eq!(
        scraper
            .scrape_content(&format!("{}/galleries/random/", server.origin))
            .await
            .unwrap()
            .images
            .len(),
        6
    );
}

#[tokio::test]
async fn process_timeout_and_step_budget_terminate_jobs() {
    let server = Server::demo();
    let limits = Limits {
        timeout_ms: 150,
        ..Limits::default()
    };
    let (package, _) = make_package(RuntimeKind::Process, &server.origin, limits);
    let scraper = ExtensionScraper::load(package.path()).unwrap();
    assert!(scraper
        .scrape_content(&format!("{}/galleries/__fixture__/loop", server.origin))
        .await
        .unwrap_err()
        .to_string()
        .contains("timed out"));
    for runtime in [RuntimeKind::Wasm, RuntimeKind::Process] {
        let limits = Limits {
            max_steps: 2,
            ..Limits::default()
        };
        let (package, _) = make_package(runtime, &server.origin, limits);
        let scraper = ExtensionScraper::load(package.path()).unwrap();
        assert!(scraper
            .scrape_content(&format!(
                "{}/galleries/__fixture__/fetch-repeat",
                server.origin
            ))
            .await
            .unwrap_err()
            .to_string()
            .contains("step budget"));
    }
}

#[tokio::test]
async fn wasm_job_deadline_cancels_cpu_work_and_native_executable_is_reverified() {
    let server = Server::demo();
    let limits = Limits {
        timeout_ms: 10,
        fuel: 500_000_000,
        ..Limits::default()
    };
    let (package, _) = make_package(RuntimeKind::Wasm, &server.origin, limits);
    let scraper = ExtensionScraper::load(package.path()).unwrap();
    let error = scraper
        .scrape_content(&format!("{}/galleries/__fixture__/loop", server.origin))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("timed out"), "{error}");
    let (package, manifest) = make_package(RuntimeKind::Process, &server.origin, Limits::default());
    let scraper = ExtensionScraper::load(package.path()).unwrap();
    std::fs::write(package.path().join(&manifest.entry), b"tampered executable").unwrap();
    assert!(scraper
        .scrape_content(&format!("{}/galleries/random/", server.origin))
        .await
        .unwrap_err()
        .to_string()
        .contains("SHA-256"));
}

#[tokio::test]
async fn broker_rejects_redirect_escape_and_oversized_responses() {
    let server = Server::new(|_| {
        (
            302,
            vec![("Location".into(), "http://ungranted.invalid/secret".into())],
            Vec::new(),
        )
    });
    let (package, _) = make_package(RuntimeKind::Wasm, &server.origin, Limits::default());
    let scraper = ExtensionScraper::load(package.path()).unwrap();
    assert!(scraper
        .scrape_content(&format!("{}/galleries/random/", server.origin))
        .await
        .unwrap_err()
        .to_string()
        .contains("permission"));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let server = Server::new(|_| (200, vec![], vec![b'x'; 2048]));
    let (package, _) = make_package(
        RuntimeKind::Process,
        &server.origin,
        Limits {
            max_response_bytes: 1024,
            ..Limits::default()
        },
    );
    let scraper = ExtensionScraper::load(package.path()).unwrap();
    assert!(scraper
        .scrape_content(&format!("{}/galleries/random/", server.origin))
        .await
        .unwrap_err()
        .to_string()
        .contains("response exceeds"));
}

#[test]
fn registry_routes_real_installed_extensions_and_rejects_tampering() {
    let root = tempfile::tempdir().unwrap();
    let (package, manifest) = make_package(
        RuntimeKind::Wasm,
        "http://127.0.0.1:8080",
        Limits::default(),
    );
    let installed = root.path().join("wasm");
    std::fs::create_dir(&installed).unwrap();
    std::fs::copy(
        package.path().join(&manifest.entry),
        installed.join(&manifest.entry),
    )
    .unwrap();
    write_manifest(&installed, &manifest);
    let registry = ScraperRegistry::with_extensions_from(root.path());
    assert!(registry.extension_statuses()[0].loaded);
    assert_eq!(
        registry.classify_url("http://127.0.0.1:8080/galleries/random/"),
        PageKind::Content
    );
    assert_eq!(
        registry
            .scraper_for_url("http://127.0.0.1:8080/galleries/random/")
            .unwrap()
            .site_key(),
        "fixture-extension"
    );
    std::fs::write(installed.join(&manifest.entry), b"tampered").unwrap();
    let registry = ScraperRegistry::with_extensions_from(root.path());
    assert!(!registry.extension_statuses()[0].loaded);
    assert!(registry.extension_statuses()[0].message.contains("SHA-256"));
}

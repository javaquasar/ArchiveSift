//! Acceptance checks for an installed production WASM parser and the real demo site.
//! No ExtensionScraper is constructed or registered directly in these tests.
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use archive_sift::app::AppConfig;
use archive_sift::db::{Database, FileDownloadStore};
use archive_sift::fs::StructuredDirectoryStrategy;
use archive_sift::models::{MetadataAttribute, PageKind, ScrapedItem, SearchQuery};
use archive_sift::scraper::extensions::manifest::{PluginManifest, RuntimeKind};
use archive_sift::scraper::{ContentScraper, ScraperRegistry};
use archive_sift::services::ContentDownloadService;
use archive_sift::ui::ConsoleUi;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONTENT_PATH: &str = "/galleries/random-showcase-004/";
const DESCRIPTION: &str = "Six deterministic generated images used to verify metadata parsing, relative URL resolution, concurrent downloads, thumbnails, SQLite persistence, and search filters.";

struct DemoSite {
    origin: String,
    mount: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl DemoSite {
    async fn start(mount: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&requests);
        let mount = mount.to_string();
        let prefix = mount.clone();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("demo-site");
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let read = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let mut chunk = [0_u8; 2048];
                    loop {
                        let length = stream.read(&mut chunk).await?;
                        if length == 0 {
                            break;
                        }
                        request.extend_from_slice(&chunk[..length]);
                        if request.windows(4).any(|part| part == b"\r\n\r\n") {
                            break;
                        }
                        if request.len() > 8192 {
                            return Err(std::io::Error::other("oversized test request"));
                        }
                    }
                    Ok::<_, std::io::Error>(())
                })
                .await;
                if !matches!(read, Ok(Ok(()))) {
                    continue;
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                received.lock().unwrap().push(path.to_string());
                let relative = path.strip_prefix(&format!("{prefix}/"));
                let file = relative
                    .filter(|value| {
                        Path::new(value)
                            .components()
                            .all(|part| matches!(part, Component::Normal(_)))
                    })
                    .map(|value| {
                        let path = root.join(value);
                        if path.is_dir() {
                            path.join("index.html")
                        } else {
                            path
                        }
                    });
                let bytes = file.as_ref().and_then(|path| std::fs::read(path).ok());
                let (status, body) = match bytes {
                    Some(bytes) => ("200 OK", bytes),
                    None => ("404 Not Found", b"missing demo fixture".to_vec()),
                };
                let content_type = match file
                    .as_ref()
                    .and_then(|path| path.extension())
                    .and_then(|value| value.to_str())
                {
                    Some("png") => "image/png",
                    Some("json") => "application/json",
                    _ => "text/html; charset=utf-8",
                };
                let headers = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(headers.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });
        Self {
            origin,
            mount,
            requests,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}{path}", self.origin, self.mount)
    }

    fn requested_paths(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for DemoSite {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct InstalledDemo {
    _root: tempfile::TempDir,
    plugin_root: PathBuf,
    package: PathBuf,
    manifest: PluginManifest,
}

impl InstalledDemo {
    fn new(site: &DemoSite) -> Self {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-plugins/demo-wasm");
        assert!(source.join("plugin.json").is_file(), "missing production demo WASM package; run python scripts/test_extensions.py --wasm-demo-only --prepare-only");
        let mut manifest: PluginManifest =
            serde_json::from_slice(&std::fs::read(source.join("plugin.json")).unwrap()).unwrap();
        // Exercise the real packager's output and retain its identity and ABI version.
        manifest.validate().unwrap();
        assert_eq!(manifest.runtime, RuntimeKind::Wasm);
        assert_eq!(manifest.id, "demo.wasm");
        assert_eq!(manifest.source_key, "demo-wasm");
        assert!(!manifest.enabled);
        assert!(!manifest.trusted);
        let module = std::fs::read(source.join(&manifest.entry)).unwrap();
        assert_eq!(
            &module[..8],
            b"\0asm\x0d\0\x01\0",
            "expected a Component Model binary"
        );
        assert_eq!(format!("{:x}", Sha256::digest(&module)), manifest.sha256);

        let root = tempfile::tempdir().unwrap();
        let plugin_root = root.path().join("plugins");
        let package = plugin_root.join(&manifest.id);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join(&manifest.entry), module).unwrap();
        manifest.network_origins = vec![site.origin.clone()];
        manifest.allow_http = true;
        manifest.enabled = true;
        for route in &mut manifest.routes {
            route.host = "127.0.0.1".into();
            route.path_prefix = format!("{}{}", site.mount, route.path_prefix);
        }
        let installed = Self {
            _root: root,
            plugin_root,
            package,
            manifest,
        };
        installed.save();
        installed
    }

    fn save(&self) {
        std::fs::write(
            self.package.join("plugin.json"),
            serde_json::to_vec_pretty(&self.manifest).unwrap(),
        )
        .unwrap();
    }

    fn registry(&self) -> ScraperRegistry {
        let registry = ScraperRegistry::with_extensions_from(&self.plugin_root);
        let statuses = registry.extension_statuses();
        assert_eq!(statuses.len(), 1, "{statuses:?}");
        assert!(statuses[0].loaded, "WASM installation failed: {statuses:?}");
        assert_eq!(statuses[0].id, "demo.wasm");
        assert_eq!(statuses[0].version, self.manifest.version);
        assert_eq!(statuses[0].runtime, Some(RuntimeKind::Wasm));
        assert!(
            registry.list_site_keys().contains(&"demo-site"),
            "built-in fallback should also be present"
        );
        registry
    }
}

fn wasm_for_url<'a>(registry: &'a ScraperRegistry, url: &str) -> &'a dyn ContentScraper {
    let scraper = registry
        .scraper_for_url(url)
        .expect("installed WASM scraper must support the demo URL");
    assert_eq!(
        scraper.site_key(),
        "demo-wasm",
        "built-in demo parser must not satisfy this check"
    );
    scraper
}

fn attributes_by_kind(attributes: &[MetadataAttribute]) -> BTreeMap<&str, Vec<&str>> {
    let mut values = BTreeMap::<_, Vec<_>>::new();
    for attribute in attributes {
        values
            .entry(attribute.kind.as_str())
            .or_default()
            .push(attribute.value.as_str());
    }
    values
}

fn assert_demo_metadata(attributes: &[MetadataAttribute], resource_url: &str) {
    let actual = attributes_by_kind(attributes);
    let expected = BTreeMap::from([
        ("actor", vec!["Alice Blue", "Robin Gray"]),
        ("tag", vec!["generated", "random-seed"]),
        ("category", vec!["integration-test"]),
        ("studio", vec!["ArchiveSift Lab"]),
        ("date", vec!["2026-10-28"]),
        ("director", vec!["Seed Generator"]),
        ("duration", vec!["00:06:00"]),
        ("studio_code", vec!["DEMO-RANDOM-004"]),
        ("description", vec![DESCRIPTION]),
        ("resource_link", vec![resource_url]),
    ]);
    for (kind, expected) in expected {
        assert_eq!(actual.get(kind), Some(&expected), "incorrect {kind}");
    }
    assert_eq!(actual.len(), 10);
}

fn assert_demo_content(page: &ScrapedItem, site: &DemoSite) {
    assert_eq!(page.site_key, "demo-wasm");
    assert_eq!(page.page_url, site.url(CONTENT_PATH));
    assert_eq!(page.title, "Random Pixel Collection");
    assert_eq!(
        page.raw_html,
        include_str!("../demo-site/galleries/random-showcase-004/index.html")
    );
    assert_demo_metadata(&page.attributes, &site.url(CONTENT_PATH));
    assert_eq!(page.images.len(), 6);
    for (index, image) in page.images.iter().enumerate() {
        let number = index + 1;
        let url = site.url(&format!(
            "/media/random-showcase-004/random-{number:03}.png"
        ));
        assert_eq!(image.source_url, url);
        assert_eq!(image.preview_url.as_deref(), Some(url.as_str()));
        assert_eq!(
            image.alt_text,
            Some(format!("Generated random image {number}"))
        );
        assert_eq!(image.sort_order, index as i64);
        assert_eq!(image.local_file_path, None);
    }
}

#[tokio::test]
async fn installed_wasm_demo_connects_through_registry_and_parses_exact_metadata() {
    let site = DemoSite::start("/demo-site").await;
    let installed = InstalledDemo::new(&site);
    let registry = installed.registry();
    let url = site.url(CONTENT_PATH);
    let scraper = wasm_for_url(&registry, &url);
    assert_eq!(registry.classify_url(&url), PageKind::Content);
    let capabilities = registry.capabilities_for_url(&url).unwrap();
    assert!(capabilities.supports_page_kind(PageKind::Content));
    assert!(capabilities.supports_page_kind(PageKind::Listing));
    assert_demo_content(&scraper.scrape_content(&url).await.unwrap(), &site);
    assert_eq!(
        site.requested_paths(),
        [format!("/demo-site{CONTENT_PATH}")]
    );
}

#[tokio::test]
async fn installed_wasm_demo_parses_legacy_content_pages_with_inline_previews() {
    let site = DemoSite::start("/demo-site").await;
    let mut installed = InstalledDemo::new(&site);
    // Legacy fixtures use placeholder full-image URLs; these are parsed, never fetched.
    installed
        .manifest
        .network_origins
        .push("https://demo.example.test".into());
    installed.save();
    let registry = installed.registry();
    for (slug, title, actor, tags, category, studio, count) in [
        (
            "summer-escape-001",
            "Summer Escape",
            "Alice Blue",
            vec!["outdoor", "summer"],
            "editorial",
            "Studio Nova",
            3,
        ),
        (
            "city-lights-002",
            "City Lights",
            "Nina Vale",
            vec!["night", "city"],
            "urban",
            "Studio Nova",
            2,
        ),
        (
            "quiet-morning-003",
            "Quiet Morning",
            "Alice Blue",
            vec!["indoor", "soft-light"],
            "lifestyle",
            "Morning House",
            2,
        ),
    ] {
        let url = site.url(&format!("/galleries/{slug}/"));
        let page = wasm_for_url(&registry, &url)
            .scrape_content(&url)
            .await
            .unwrap();
        assert_eq!(page.site_key, "demo-wasm");
        assert_eq!(page.title, title);
        let metadata = attributes_by_kind(&page.attributes);
        assert_eq!(metadata["actor"], [actor]);
        assert_eq!(metadata["tag"], tags);
        assert_eq!(metadata["category"], [category]);
        assert_eq!(metadata["studio"], [studio]);
        assert_eq!(page.images.len(), count);
        for (index, image) in page.images.iter().enumerate() {
            assert_eq!(
                image.source_url,
                format!(
                    "https://demo.example.test/media/{slug}/image-{:03}.jpg",
                    index + 1
                )
            );
            assert_eq!(
                image.preview_url, None,
                "inline SVG previews are not network image URLs"
            );
        }
    }
    assert_eq!(site.requested_paths().len(), 3);
}

#[tokio::test]
async fn installed_wasm_demo_parses_real_paginated_model_and_lazy_pages() {
    let site = DemoSite::start("/ArchiveSift").await;
    let installed = InstalledDemo::new(&site);
    let registry = installed.registry();
    let channel_url = site.url("/channels/studio-nova/");
    let scraper = wasm_for_url(&registry, &channel_url);
    assert_eq!(registry.classify_url(&channel_url), PageKind::Listing);
    let first = scraper.scrape_listing(&channel_url).await.unwrap();
    assert_eq!(
        first.content_urls().collect::<Vec<_>>(),
        [
            site.url("/galleries/summer-escape-001/"),
            site.url("/galleries/city-lights-002/")
        ]
    );
    assert_eq!(
        first.next_page_url,
        Some(site.url("/channels/studio-nova/page-2/"))
    );
    let second = scraper
        .scrape_listing(first.next_page_url.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        second.content_urls().collect::<Vec<_>>(),
        [site.url("/galleries/quiet-morning-003/")]
    );
    assert_eq!(second.next_page_url, None);
    let model_url = site.url("/models/alice-blue/");
    assert_eq!(registry.classify_url(&model_url), PageKind::Listing);
    let model = wasm_for_url(&registry, &model_url)
        .scrape_listing(&model_url)
        .await
        .unwrap();
    assert_eq!(
        model.content_urls().collect::<Vec<_>>(),
        [
            site.url("/galleries/summer-escape-001/"),
            site.url("/galleries/quiet-morning-003/")
        ]
    );
    let lazy = scraper
        .scrape_listing(&site.url("/channels/studio-nova/lazy/"))
        .await
        .unwrap();
    assert_eq!(
        lazy.content_urls().collect::<Vec<_>>(),
        [
            site.url("/galleries/summer-escape-001/"),
            site.url("/galleries/city-lights-002/"),
            site.url("/galleries/quiet-morning-003/")
        ]
    );
    assert_eq!(lazy.next_page_url, None);
    assert_eq!(
        site.requested_paths(),
        [
            "/ArchiveSift/channels/studio-nova/",
            "/ArchiveSift/channels/studio-nova/page-2/",
            "/ArchiveSift/models/alice-blue/",
            "/ArchiveSift/channels/studio-nova/lazy/",
            "/ArchiveSift/api/lazy/studio-nova-page-1.json",
            "/ArchiveSift/api/lazy/studio-nova-page-2.json",
        ]
    );
}

#[tokio::test]
async fn installed_wasm_demo_downloads_six_distinct_pngs_and_persists_searchable_metadata() {
    let site = DemoSite::start("/ArchiveSift").await;
    let installed = InstalledDemo::new(&site);
    let registry = installed.registry();
    let url = site.url(CONTENT_PATH);
    wasm_for_url(&registry, &url);
    let root = tempfile::tempdir().unwrap();
    let database_path = root.path().join("archive.sqlite3");
    let downloads = root.path().join("downloads");
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
    let database = Database::new(&database_path);
    let query = SearchQuery {
        site_key: Some("demo-wasm".into()),
        tags: vec!["generated".into()],
        ..SearchQuery::default()
    };
    let results = database.search_images(&query).unwrap();
    assert_eq!(results.len(), 6);
    for result in &results {
        assert_eq!(result.page_title(), "Random Pixel Collection");
        assert_eq!(result.page_url(), url);
        assert_demo_metadata(&result.attributes, &url);
        let filename = reqwest::Url::parse(&result.source_url)
            .unwrap()
            .path_segments()
            .unwrap()
            .next_back()
            .unwrap()
            .to_string();
        let expected = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("demo-site/media/random-showcase-004")
                .join(filename),
        )
        .unwrap();
        let relative = Path::new(
            result
                .local_file_path
                .as_ref()
                .expect("WASM image was not downloaded"),
        );
        assert!(relative.starts_with("demo-wasm"));
        assert_eq!(std::fs::read(downloads.join(relative)).unwrap(), expected);
        let thumbnail = result.thumbnail_path.as_ref().expect("missing thumbnail");
        let (width, height) = image::image_dimensions(thumbnail).unwrap();
        assert!(width > 0 && height > 0);
    }
    assert!(
        database
            .search_images(&SearchQuery {
                site_key: Some("demo-site".into()),
                ..query.clone()
            })
            .unwrap()
            .is_empty(),
        "built-in parser must not create these records"
    );
    assert!(database
        .search_images(&SearchQuery {
            tags: vec!["missing-tag".into()],
            ..query
        })
        .unwrap()
        .is_empty());
    let mut requested = site.requested_paths();
    requested.sort();
    let mut expected = vec![format!("/ArchiveSift{CONTENT_PATH}")];
    expected.extend(
        (1..=6)
            .map(|index| format!("/ArchiveSift/media/random-showcase-004/random-{index:03}.png")),
    );
    expected.sort();
    assert_eq!(requested, expected);
}

#[tokio::test]
async fn installed_wasm_demo_reports_disabled_missing_corrupt_and_incompatible_packages() {
    let site = DemoSite::start("/demo-site").await;
    for failure in ["disabled", "missing", "corrupt", "api"] {
        let mut installed = InstalledDemo::new(&site);
        match failure {
            "disabled" => installed.manifest.enabled = false,
            "missing" => {
                std::fs::remove_file(installed.package.join(&installed.manifest.entry)).unwrap()
            }
            "corrupt" => std::fs::write(
                installed.package.join(&installed.manifest.entry),
                b"corrupt module",
            )
            .unwrap(),
            "api" => installed.manifest.api = "2.0".into(),
            _ => unreachable!(),
        }
        installed.save();
        let registry = ScraperRegistry::with_extensions_from(&installed.plugin_root);
        let statuses = registry.extension_statuses();
        assert_eq!(statuses.len(), 1, "{failure}: {statuses:?}");
        assert!(!statuses[0].loaded, "accepted {failure} package");
        assert!(!statuses[0].message.is_empty());
        if failure == "disabled" {
            assert_eq!(statuses[0].message, "Disabled");
        }
        if failure == "corrupt" {
            assert!(statuses[0].message.contains("SHA-256"));
        }
        if failure == "api" {
            assert!(statuses[0].message.contains("API"));
        }
        assert_eq!(registry.list_site_keys(), ["demo-site"]);
    }
    assert!(
        site.requested_paths().is_empty(),
        "package checks must not fetch the site"
    );
}

#[tokio::test]
async fn installed_production_wasm_demo_reports_http_errors_and_recovers() {
    let site = DemoSite::start("/demo-site").await;
    let installed = InstalledDemo::new(&site);
    let registry = installed.registry();
    let url = site.url(CONTENT_PATH);
    let scraper = wasm_for_url(&registry, &url);
    let missing = site.url("/galleries/__fixture__/error");
    let error = scraper
        .scrape_content(&missing)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("HTTP 404"),
        "expected the production parser's HTTP error: {error}"
    );
    assert!(
        !error.contains("intentional extension error"),
        "hostile test feature must not be compiled into the production component"
    );
    let denied = format!("http://ungranted.invalid/demo-site{CONTENT_PATH}");
    assert!(!scraper.supports_url(&denied));
    assert!(scraper.scrape_content(&denied).await.is_err());
    assert_demo_content(&scraper.scrape_content(&url).await.unwrap(), &site);
    assert_eq!(
        site.requested_paths(),
        [
            "/demo-site/galleries/__fixture__/error",
            &format!("/demo-site{CONTENT_PATH}")
        ]
    );
}

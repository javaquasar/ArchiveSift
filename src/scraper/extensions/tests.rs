use super::*;
use manifest::*;
use sha2::{Digest, Sha256};

pub(super) fn manifest() -> PluginManifest {
    PluginManifest {
        schema: 1,
        id: "example.parser".into(),
        version: "1.0.0".into(),
        api: "1.0".into(),
        display_name: "Example".into(),
        source_key: "example".into(),
        runtime: RuntimeKind::Process,
        entry: "adapter.exe".into(),
        sha256: "0".repeat(64),
        enabled: true,
        trusted: true,
        routes: vec![Route {
            host: "example.test".into(),
            path_prefix: "/content/".into(),
            kind: RouteKind::Content,
        }],
        network_origins: vec!["https://example.test".into()],
        allow_http: false,
        limits: Limits::default(),
    }
}

#[test]
fn manifest_rejects_incompatible_versions_and_unsafe_permissions() {
    let base = manifest();
    base.validate().unwrap();
    for field in [
        "schema",
        "api",
        "entry",
        "source_key",
        "sha256",
        "routes",
        "version",
        "timeout",
        "network",
    ] {
        let mut invalid = base.clone();
        match field {
            "schema" => invalid.schema = 2,
            "api" => invalid.api = "2.0".into(),
            "entry" => invalid.entry = "../adapter.exe".into(),
            "source_key" => invalid.source_key = "../outside".into(),
            "sha256" => invalid.sha256 = "invalid".into(),
            "routes" => invalid.routes[0].host = "*.test".into(),
            "version" => invalid.version = "latest".into(),
            "timeout" => invalid.limits.timeout_ms = 1_000_000,
            "network" => invalid.network_origins[0] = "https://example.test/path".into(),
            _ => unreachable!(),
        }
        assert!(invalid.validate().is_err(), "accepted invalid {field}");
    }
}

#[test]
fn routing_matches_exact_hosts_and_path_boundaries_without_guest_calls() {
    let mut manifest = manifest();
    manifest.routes[0].path_prefix = "/content".into();
    assert_eq!(
        manifest.classify("https://example.test/content/one?q=1"),
        PageKind::Content
    );
    for url in [
        "https://example.test.evil/content/one",
        "https://evil.test/content/one",
        "https://example.test/contents/one",
        "https://example.test:8443/content/one",
        "http://example.test/content/one",
        "file:///content/one",
        "https://user:password@example.test/content/one",
    ] {
        assert_eq!(manifest.classify(url), PageKind::Unknown, "accepted {url}");
    }
}

#[test]
fn loader_checks_hash_disabled_packages_and_process_trust() {
    let root = tempfile::tempdir().unwrap();
    let mut manifest = manifest();
    let bytes = b"not an executable";
    std::fs::write(root.path().join(&manifest.entry), bytes).unwrap();
    assert!(manifest.verified_entry(root.path()).is_err());
    manifest.sha256 = format!("{:x}", Sha256::digest(bytes));
    manifest.verified_entry(root.path()).unwrap();
    manifest.trusted = false;
    assert!(ExtensionScraper::from_manifest(root.path(), manifest.clone()).is_err());
    manifest.trusted = true;
    manifest.enabled = false;
    assert!(ExtensionScraper::from_manifest(root.path(), manifest.clone()).is_err());
    std::fs::write(
        root.path().join("plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let parent = tempfile::tempdir().unwrap();
    let disabled = parent.path().join("disabled");
    std::fs::create_dir(&disabled).unwrap();
    std::fs::copy(
        root.path().join("plugin.json"),
        disabled.join("plugin.json"),
    )
    .unwrap();
    let registry = ScraperRegistry::with_extensions_from(parent.path());
    assert_eq!(registry.extension_statuses()[0].message, "Disabled");
    assert_eq!(registry.list_site_keys(), ["demo-site"]);
}

#[test]
fn discovery_reports_bad_packages_and_duplicate_id_without_losing_valid_modules() {
    let root = tempfile::tempdir().unwrap();
    for name in ["a-valid", "b-duplicate", "c-invalid"] {
        let directory = root.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        if name == "c-invalid" {
            std::fs::write(directory.join("plugin.json"), b"{}").unwrap();
            continue;
        }
        let mut manifest = manifest();
        let bytes = b"verified process placeholder";
        manifest.sha256 = format!("{:x}", Sha256::digest(bytes));
        std::fs::write(directory.join(&manifest.entry), bytes).unwrap();
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }
    let registry = ScraperRegistry::with_extensions_from(root.path());
    assert_eq!(registry.list_site_keys(), ["example", "demo-site"]);
    let statuses = registry.extension_statuses();
    assert!(statuses[0].loaded);
    assert!(statuses[1].message.contains("duplicate"));
    assert!(!statuses[2].loaded);
    assert!(registry
        .clone()
        .supports_url("https://example.test/content/one"));
}

#[test]
fn wasm_loader_rejects_core_modules_and_wrong_component_contracts() {
    assert!(WasmBackend::new(
        &ModuleBytes::new(wat::parse_str("(module)").unwrap()),
        Limits::default()
    )
    .is_err());
    assert!(WasmBackend::new(
        &ModuleBytes::new(wat::parse_str("(component)").unwrap()),
        Limits::default()
    )
    .is_err());
}

#[tokio::test]
async fn broker_denies_ungranted_origins_and_forbidden_headers_before_network() {
    let manifest = manifest();
    let broker = HttpBroker::new(&manifest).unwrap();
    let request = archive_sift_scraper_api::FetchRequest::get("https://other.test/");
    assert!(broker
        .fetch(&request)
        .await
        .unwrap_err()
        .to_string()
        .contains("permission"));
    let mut request = archive_sift_scraper_api::FetchRequest::get("https://example.test/");
    request.headers.push(archive_sift_scraper_api::Header {
        name: "host".into(),
        value: "other.test".into(),
    });
    assert!(broker
        .fetch(&request)
        .await
        .unwrap_err()
        .to_string()
        .contains("forbidden"));
}

#[test]
fn normalized_results_keep_storage_identity_in_the_host_and_reject_unsafe_urls() {
    use archive_sift_scraper_api::{Attribute, Content, DiscoveredUrl, Image, Listing, Operation};
    let root = tempfile::tempdir().unwrap();
    let mut manifest = manifest();
    manifest.routes.push(Route {
        host: "example.test".into(),
        path_prefix: "/listing/".into(),
        kind: RouteKind::Listing,
    });
    let bytes = b"process placeholder";
    manifest.sha256 = format!("{:x}", Sha256::digest(bytes));
    std::fs::write(root.path().join(&manifest.entry), bytes).unwrap();
    let extension = ExtensionScraper::from_manifest(root.path(), manifest).unwrap();
    let content = Content {
        title: "Title".into(),
        images: vec![Image {
            url: "../../image.png".into(),
            preview_url: Some("/thumb.png".into()),
            alt_text: None,
            width: Some(10),
            height: Some(20),
        }],
        attributes: vec![Attribute {
            kind: "actor".into(),
            label: "Actor".into(),
            value: "Example".into(),
            source_url: Some("/actor".into()),
            is_female: Some(true),
        }],
    };
    let normalized = extension
        .convert_content(
            content.clone(),
            "https://example.test/content/one",
            "html".into(),
        )
        .unwrap();
    assert_eq!(normalized.site_key, "example");
    assert_eq!(normalized.page_url, "https://example.test/content/one");
    assert_eq!(
        normalized.images[0].source_url,
        "https://example.test/image.png"
    );
    assert_eq!(normalized.images[0].local_file_path, None);
    assert_eq!(normalized.images[0].width, Some(10));
    assert_eq!(normalized.raw_html, "html");
    assert_eq!(
        normalized.attributes[0].source_url.as_deref(),
        Some("https://example.test/actor")
    );
    let mut invalid = content.clone();
    invalid.images[0].url = "file:///secret".into();
    assert!(extension
        .convert_content(invalid, "https://example.test/content/one", String::new())
        .is_err());
    let mut invalid = content;
    invalid.title.clear();
    assert!(extension
        .convert_content(invalid, "https://example.test/content/one", String::new())
        .is_err());
    let invalid = Listing {
        urls: vec![DiscoveredUrl {
            url: "/listing/page".into(),
            operation: Operation::Content,
        }],
        next_page_url: None,
    };
    assert!(extension
        .convert_listing(invalid, "https://example.test/listing/one")
        .is_err());
    let invalid = Listing {
        urls: vec![],
        next_page_url: Some("/content/one".into()),
    };
    assert!(extension
        .convert_listing(invalid, "https://example.test/listing/one")
        .is_err());
}

#[test]
fn discovery_rejects_overlapping_routes_and_keeps_first_deterministic_owner() {
    let root = tempfile::tempdir().unwrap();
    for (name, prefix) in [("a", "/content/"), ("b", "/content/nested/")] {
        let directory = root.path().join(name);
        std::fs::create_dir(&directory).unwrap();
        let mut manifest = manifest();
        manifest.id = name.into();
        manifest.source_key = name.into();
        manifest.routes[0].path_prefix = prefix.into();
        let bytes = b"placeholder";
        manifest.sha256 = format!("{:x}", Sha256::digest(bytes));
        std::fs::write(directory.join(&manifest.entry), bytes).unwrap();
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }
    let registry = ScraperRegistry::with_extensions_from(root.path());
    assert!(registry.extension_statuses()[0].loaded);
    assert!(registry.extension_statuses()[1]
        .message
        .contains("route conflicts"));
    assert_eq!(
        registry
            .scraper_for_url("https://example.test/content/nested/one")
            .unwrap()
            .site_key(),
        "a"
    );
}

#[test]
fn manifest_limits_and_paths_are_checked_before_reading_the_module() {
    for entry in [
        "../outside",
        "C:/outside.exe",
        "/outside.exe",
        "folder/../../outside",
        "adapter.exe:stream",
    ] {
        let mut invalid = manifest();
        invalid.entry = entry.into();
        assert!(invalid.validate().is_err(), "accepted {entry}");
    }
    for key in ["con", "lpt1", "nul.txt", ".hidden", "trailing."] {
        let mut invalid = manifest();
        invalid.source_key = key.into();
        assert!(invalid.validate().is_err(), "accepted {key}");
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("plugin.json"), vec![b'x'; 64 * 1024 + 1]).unwrap();
    assert!(read_manifest(root.path())
        .unwrap_err()
        .to_string()
        .contains("64 KiB"));
}

#[tokio::test]
async fn broker_bounds_chunked_bodies_without_a_content_length() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut input = [0; 2048];
        stream.read(&mut input).await.unwrap();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let body = format!(
            "400\r\n{}\r\n400\r\n{}\r\n0\r\n\r\n",
            "x".repeat(1024),
            "y".repeat(1024)
        );
        let _ = stream.write_all(body.as_bytes()).await;
    });
    let mut manifest = manifest();
    manifest.allow_http = true;
    manifest.network_origins = vec![origin.clone()];
    manifest.limits.max_response_bytes = 1024;
    let broker = HttpBroker::new(&manifest).unwrap();
    let error = broker
        .fetch(&archive_sift_scraper_api::FetchRequest::get(origin))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("response exceeds"));
    server.await.unwrap();
}

#[test]
fn streamed_native_verification_checks_all_chunks_size_and_post_load_modification() {
    use std::io::{Seek, SeekFrom, Write};
    let root = tempfile::tempdir().unwrap();
    let mut manifest = manifest();
    let bytes = vec![0x5a; 3 * 64 * 1024 + 7];
    let entry = root.path().join(&manifest.entry);
    std::fs::write(&entry, &bytes).unwrap();
    manifest.sha256 = format!("{:x}", Sha256::digest(&bytes));
    assert_eq!(
        manifest.verified_process_entry(root.path()).unwrap(),
        entry.canonicalize().unwrap()
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&entry)
        .unwrap();
    file.seek(SeekFrom::Start(2 * 64 * 1024 + 1)).unwrap();
    file.write_all(&[0]).unwrap();
    drop(file);
    assert!(manifest
        .verified_process_entry(root.path())
        .unwrap_err()
        .to_string()
        .contains("SHA-256"));
    std::fs::File::create(&entry)
        .unwrap()
        .set_len(MAX_MODULE_BYTES + 1)
        .unwrap();
    assert!(manifest
        .verified_process_entry(root.path())
        .unwrap_err()
        .to_string()
        .contains("too large"));
}

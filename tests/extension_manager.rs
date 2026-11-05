//! Real Component Model packages, isolated profiles and no external services.
use archive_sift::{
    app::AppConfig,
    db::Database,
    scraper::{
        extensions::{
            manager::{updates::UpdateSource, CheckState, ExtensionManager},
            settings::SettingsCatalog,
        },
        RegistryProvider, ScraperRegistry,
    },
    services::UrlQueueService,
};
use archive_sift_extension_package::{digest, encode, generate_key};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, sync::OnceLock};

fn module() -> &'static Vec<u8> {
    static MODULE: OnceLock<Vec<u8>> = OnceLock::new();
    MODULE.get_or_init(|| {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/test-plugins/demo-wasm/adapter.wasm"),
        )
        .expect("run python scripts/test_extensions.py --prepare-only")
    })
}
fn files(version: &str, id: &str) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("adapter.wasm".into(),module().clone()),
        ("plugin.json".into(),serde_json::to_vec(&json!({"schema":1,"api":"1.0","id":id,"version":version,"display_name":"Example parser","source_key":id.replace('.',"-"),"runtime":"wasm","entry":"adapter.wasm","sha256":digest(module()),"enabled":false,"trusted":false,"routes":[{"host":"example.test","path_prefix":"/galleries/","kind":"content"}],"network_origins":["https://example.test"]})).unwrap()),
        ("self-test.json".into(),include_bytes!("../plugins/demo-parser/self-test.json").to_vec()),
    ])
}
fn manager() -> (tempfile::TempDir, ExtensionManager, RegistryProvider) {
    let temp = tempfile::tempdir().unwrap();
    let provider = RegistryProvider::new(ScraperRegistry::with_public_scrapers());
    let manager = ExtensionManager::new(temp.path().join("plugins"), provider.clone());
    (temp, manager, provider)
}
fn install(
    manager: &ExtensionManager,
    files: BTreeMap<String, Vec<u8>>,
    key: Option<&[u8]>,
    settings: Option<&Value>,
) {
    let prepared = manager.prepare_bytes(encode(&files, key).unwrap()).unwrap();
    manager
        .install(
            &prepared,
            prepared.package.signer.as_deref(),
            key.is_none(),
            false,
            settings,
        )
        .unwrap();
}

#[tokio::test]
async fn signed_install_self_test_hot_swap_disable_rollback_and_restart() {
    let (_temp, manager, provider) = manager();
    let key = generate_key().unwrap();
    let v1 = files("1.0.0", "example.parser");
    let prepared = manager
        .prepare_bytes(encode(&v1, Some(&key)).unwrap())
        .unwrap();
    assert_eq!(
        manager.check_prepared(&prepared, None).await.state,
        CheckState::Ready
    );
    assert!(manager
        .install(&prepared, None, false, false, None)
        .is_err());
    assert!(provider.snapshot().extension_statuses().is_empty());
    manager
        .install(
            &prepared,
            prepared.package.signer.as_deref(),
            false,
            false,
            None,
        )
        .unwrap();
    let old = provider.snapshot();
    assert_eq!(old.extension_statuses()[0].version, "1.0.0");
    let payload = std::fs::read(old.extension_statuses()[0].directory.join("plugin.json")).unwrap();
    assert_eq!(payload, v1["plugin.json"]);
    assert!(
        !serde_json::from_slice::<Value>(&payload).unwrap()["enabled"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        manager.check("example.parser", None).await.state,
        CheckState::Ready
    );
    install(&manager, files("2.0.0", "example.parser"), Some(&key), None);
    assert_eq!(provider.snapshot().extension_statuses()[0].version, "2.0.0");
    assert_eq!(old.extension_statuses()[0].version, "1.0.0");
    assert!(old.supports_url("https://example.test/galleries/check/"));
    manager.set_enabled("example.parser", false).unwrap();
    assert!(!provider
        .snapshot()
        .supports_url("https://example.test/galleries/check/"));
    assert!(old.supports_url("https://example.test/galleries/check/"));
    manager.set_enabled("example.parser", true).unwrap();
    manager.rollback("example.parser", None).unwrap();
    let restarted = ScraperRegistry::with_extensions_from(manager.root());
    assert_eq!(restarted.extension_statuses()[0].version, "1.0.0");
    assert!(restarted.extension_statuses()[0].loaded);
    assert!(old.extension_statuses()[0]
        .directory
        .join("adapter.wasm")
        .exists());
}

#[tokio::test]
async fn conflicting_routes_invalid_abi_and_modified_payload_never_replace_live_registry() {
    let (_temp, manager, provider) = manager();
    install(&manager, files("1.0.0", "example.parser"), None, None);
    let conflict = manager
        .prepare_bytes(encode(&files("1.0.0", "other.parser"), None).unwrap())
        .unwrap();
    assert_eq!(
        manager.check_prepared(&conflict, None).await.state,
        CheckState::Conflict
    );
    assert!(manager.install(&conflict, None, true, false, None).is_err());
    let mut bad = files("2.0.0", "example.parser");
    let bytes = wat::parse_str("(component)").unwrap();
    bad.insert("adapter.wasm".into(), bytes.clone());
    let mut manifest: Value = serde_json::from_slice(&bad["plugin.json"]).unwrap();
    manifest["sha256"] = json!(digest(&bytes));
    bad.insert("plugin.json".into(), serde_json::to_vec(&manifest).unwrap());
    let incompatible = manager.prepare_bytes(encode(&bad, None).unwrap()).unwrap();
    assert_eq!(
        manager.check_prepared(&incompatible, None).await.state,
        CheckState::Incompatible
    );
    assert!(manager
        .install(&incompatible, None, true, false, None)
        .is_err());
    assert_eq!(provider.snapshot().extension_statuses()[0].version, "1.0.0");
    let dir = provider.snapshot().extension_statuses()[0]
        .directory
        .clone();
    std::fs::write(dir.join("self-test.json"), b"{}").unwrap();
    let restarted = ScraperRegistry::with_extensions_from(manager.root());
    assert!(!restarted.extension_statuses()[0].loaded);
    // An already loaded immutable WASM snapshot is still usable.
    assert!(provider
        .snapshot()
        .supports_url("https://example.test/galleries/check/"));
}

#[tokio::test]
async fn diagnostic_reports_missing_settings_and_a_real_parser_self_test_failure() {
    let (_temp, manager, _) = manager();
    let mut f = files("1.0.0", "example.parser");
    f.insert("settings.schema.json".into(),serde_json::to_vec(&json!({"type":"object","additionalProperties":false,"properties":{"label":{"type":"string","minLength":1}},"required":["label"]})).unwrap());
    let p = manager.prepare_bytes(encode(&f, None).unwrap()).unwrap();
    assert_eq!(
        manager.check_prepared(&p, None).await.state,
        CheckState::MissingSettings
    );
    assert_eq!(
        manager
            .check_prepared(&p, Some(&json!({"label":"ready"})))
            .await
            .state,
        CheckState::Ready
    );
    let mut fixture: Value = serde_json::from_slice(&f["self-test.json"]).unwrap();
    fixture["expected"]["Content"]["title"] = json!("wrong");
    f.insert(
        "self-test.json".into(),
        serde_json::to_vec(&fixture).unwrap(),
    );
    let p = manager.prepare_bytes(encode(&f, None).unwrap()).unwrap();
    assert_eq!(
        manager
            .check_prepared(&p, Some(&json!({"label":"ready"})))
            .await
            .state,
        CheckState::Failed
    );
}

#[tokio::test]
async fn local_updates_require_the_same_publisher_and_only_stage_permission_changes() {
    let (temp, manager, provider) = manager();
    let key = generate_key().unwrap();
    let different = generate_key().unwrap();
    install(&manager, files("1.0.0", "example.parser"), Some(&key), None);
    let source = temp.path().join("updates");
    std::fs::create_dir(&source).unwrap();
    manager
        .set_source(
            "example.parser",
            Some(UpdateSource::Directory {
                path: source.clone(),
            }),
        )
        .unwrap();
    std::fs::write(
        source.join("untrusted.asift-plugin"),
        encode(&files("9.0.0", "example.parser"), Some(&different)).unwrap(),
    )
    .unwrap();
    std::fs::write(
        source.join("unsigned.asift-plugin"),
        encode(&files("8.0.0", "example.parser"), None).unwrap(),
    )
    .unwrap();
    assert!(manager.check_update("example.parser").await.is_err());
    let mut f = files("2.0.0", "example.parser");
    let mut manifest: Value = serde_json::from_slice(&f["plugin.json"]).unwrap();
    manifest["network_origins"] = json!(["https://example.test", "https://cdn.example.test"]);
    f.insert("plugin.json".into(), serde_json::to_vec(&manifest).unwrap());
    std::fs::write(
        source.join("new.asift-plugin"),
        encode(&f, Some(&key)).unwrap(),
    )
    .unwrap();
    let candidate = manager.check_update("example.parser").await.unwrap();
    assert_eq!(candidate.manifest.version, "2.0.0");
    assert_eq!(candidate.manifest.network_origins.len(), 2);
    assert_eq!(provider.snapshot().extension_statuses()[0].version, "1.0.0");
    assert!(manager
        .install(&candidate, None, false, false, None)
        .is_err());
}

#[test]
fn queue_uses_current_routes_for_manual_clipboard_and_listing_inputs() {
    let (temp, manager, provider) = manager();
    let queue =
        UrlQueueService::with_provider_and_queue_path(provider, temp.path().join("queue.db"));
    let db = Database::new(temp.path().join("library.db").to_str().unwrap());
    let first = queue
        .enqueue_urls(&db, vec!["https://example.test/galleries/one/".into()])
        .unwrap();
    assert_eq!(first.queued_successfully, 0);
    install(&manager, files("1.0.0", "example.parser"), None, None);
    let next = queue
        .enqueue_urls(&db, vec!["https://example.test/galleries/one/".into()])
        .unwrap();
    assert_eq!(next.queued_successfully, 1);
    manager.set_enabled("example.parser", false).unwrap();
    let last = queue
        .enqueue_urls(&db, vec!["https://example.test/galleries/two/".into()])
        .unwrap();
    assert_eq!(last.queued_successfully, 0);
}

#[test]
fn version_switch_restores_schema_compatible_settings_and_publishes_them_with_the_registry() {
    let (temp, manager, provider) = manager();
    let mut first = files("1.0.0", "example.parser");
    first.insert("settings.schema.json".into(),serde_json::to_vec(&json!({"type":"object","additionalProperties":false,"properties":{"old":{"type":"integer"}},"required":["old"]})).unwrap());
    let old = json!({"old":7});
    install(&manager, first, None, Some(&old));
    let old_key = manager.state().unwrap().plugins["example.parser"]
        .active
        .clone();
    let mut second = files("2.0.0", "example.parser");
    second.insert("settings.schema.json".into(),serde_json::to_vec(&json!({"type":"object","additionalProperties":false,"properties":{"new":{"type":"string"}},"required":["new"]})).unwrap());
    install(&manager, second, None, Some(&old));
    let mut current = AppConfig::default();
    current
        .extension_settings
        .insert("example.parser".into(), json!({"new":"ready"}));
    let config = temp.path().join("config.json");
    current.save_to(&config).unwrap();
    let mut legacy: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    legacy["config_version"] = json!(0);
    std::fs::write(&config, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let current = AppConfig::load_from(&config).unwrap();
    assert!(manager
        .rollback(
            "example.parser",
            current.extension_settings.get("example.parser")
        )
        .is_err());
    let pinned = provider.snapshot();
    let restored = manager
        .switch_and_save("example.parser", &old_key, &current, &config)
        .unwrap();
    assert_eq!(restored.extension_settings["example.parser"], old);
    assert_eq!(
        AppConfig::load_from(&config).unwrap().extension_settings,
        restored.extension_settings
    );
    let mut stale = current.clone();
    let latest = provider.for_job(&mut stale);
    assert_eq!(latest.extension_statuses()[0].version, "1.0.0");
    assert_eq!(stale.extension_settings["example.parser"], old);
    assert_eq!(pinned.extension_statuses()[0].version, "2.0.0");
    let catalog = SettingsCatalog::discover(manager.root());
    assert!(
        catalog.plugins["example.parser"].schema.value()["properties"]
            .get("old")
            .is_some()
    );
    assert!(!manager.root().join("switch-journal.json").exists());
}

#[test]
fn independent_file_writers_cannot_change_the_same_active_pointer_concurrently() {
    let (_temp, manager, provider) = manager();
    install(&manager, files("1.0.0", "example.parser"), None, None);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(manager.root().join(".extension-manager.lock"))
        .unwrap();
    file.lock().unwrap();
    assert!(manager.set_enabled("example.parser", false).is_err());
    assert!(provider.snapshot().extension_statuses()[0].loaded);
    file.unlock().unwrap();
    manager.set_enabled("example.parser", false).unwrap();
    assert!(!provider.snapshot().extension_statuses()[0].loaded);
}

#[tokio::test]
async fn native_packages_require_explicit_approval_and_restore_it_on_version_rollback() {
    let (_temp, manager, provider) = manager();
    let mut f = files("1.0.0", "example.parser");
    let filename = if cfg!(windows) {
        "adapter.exe"
    } else {
        "adapter"
    };
    let bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-plugins/process")
            .join(filename),
    )
    .expect("prepare the process fixture");
    f.remove("adapter.wasm");
    f.remove("self-test.json");
    f.insert(filename.into(), bytes.clone());
    let mut manifest: Value = serde_json::from_slice(&f["plugin.json"]).unwrap();
    manifest["entry"] = json!(filename);
    manifest["runtime"] = json!("process");
    manifest["sha256"] = json!(digest(&bytes));
    f.insert("plugin.json".into(), serde_json::to_vec(&manifest).unwrap());
    let p = manager.prepare_bytes(encode(&f, None).unwrap()).unwrap();
    assert_eq!(
        manager.check_prepared(&p, None).await.state,
        CheckState::Ready
    );
    assert!(manager.install(&p, None, true, false, None).is_err());
    manager.install(&p, None, true, true, None).unwrap();
    let native = provider.snapshot();
    assert_eq!(
        native.extension_statuses()[0].runtime,
        Some(archive_sift::scraper::extensions::manifest::RuntimeKind::Process)
    );
    install(&manager, files("2.0.0", "example.parser"), None, None);
    manager.rollback("example.parser", None).unwrap();
    assert_eq!(
        provider.snapshot().extension_statuses()[0].runtime,
        native.extension_statuses()[0].runtime
    );
    assert!(native.extension_statuses()[0]
        .directory
        .join(filename)
        .is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(native.extension_statuses()[0].directory.join(filename))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[tokio::test]
async fn a_download_already_in_progress_keeps_its_parser_and_image_permissions_after_update() {
    use archive_sift::{
        db::FileDownloadStore, fs::StructuredDirectoryStrategy, services::ContentDownloadService,
        ui::ConsoleUi,
    };
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (temp, manager, provider) = manager();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let arrived = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let (seen, continue_page) = (arrived.clone(), resume.clone());
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = vec![];
            loop {
                let mut b = [0; 2048];
                let n = stream.read(&mut b).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&b[..n]);
                if data.windows(4).any(|b| b == b"\r\n\r\n") {
                    break;
                }
                assert!(data.len() < 8192);
            }
            let image = String::from_utf8_lossy(&data).starts_with("GET /image.png ");
            let (mime, body) = if image {
                (
                    "image/png",
                    include_bytes!("../demo-site/media/random-showcase-004/random-001.png")
                        .to_vec(),
                )
            } else {
                seen.notify_one();
                continue_page.notified().await;
                ("text/html",b"<main data-demo-title='Pinned download'><img data-demo-full='/image.png'></main>".to_vec())
            };
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        }
    });
    let mut old = files("1.0.0", "example.parser");
    let mut manifest: Value = serde_json::from_slice(&old["plugin.json"]).unwrap();
    manifest["routes"][0]["host"] = json!("127.0.0.1");
    manifest["network_origins"] = json!([origin]);
    manifest["allow_http"] = json!(true);
    old.insert("plugin.json".into(), serde_json::to_vec(&manifest).unwrap());
    install(&manager, old, None, None);
    let mut config = AppConfig {
        database_path: temp
            .path()
            .join("db.sqlite3")
            .to_string_lossy()
            .into_owned(),
        download_folder: temp.path().join("files").to_string_lossy().into_owned(),
        thumbnail_folder: Some(temp.path().join("thumbs").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let snapshot = provider.for_job(&mut config);
    let folder = std::path::PathBuf::from(&config.download_folder);
    let url = format!("{origin}/galleries/check/");
    let service = ContentDownloadService::new(
        snapshot,
        Database::new(&config.database_path),
        FileDownloadStore::new(),
        ConsoleUi::default(),
        StructuredDirectoryStrategy,
        folder.clone(),
        config.thumb_dir(),
    );
    let job = tokio::spawn(async move { service.run_for_url(&config, &url).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), arrived.notified())
        .await
        .unwrap();
    install(&manager, files("2.0.0", "example.parser"), None, None);
    assert_ne!(
        provider
            .snapshot()
            .scraper_for_url(&format!("{origin}/galleries/check/"))
            .and_then(|p| p.extension_id()),
        Some("example.parser")
    );
    resume.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(15), job)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.await.unwrap();
    fn downloaded(path: &Path) -> bool {
        std::fs::read_dir(path)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    downloaded(&path)
                } else {
                    std::fs::read(path).is_ok_and(|b| {
                        b == include_bytes!("../demo-site/media/random-showcase-004/random-001.png")
                    })
                }
            })
    }
    assert!(
        downloaded(&folder),
        "the original job must finish downloading its image using the old broker"
    );
}

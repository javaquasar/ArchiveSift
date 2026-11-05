use archive_sift::{
    agent_control::{
        adapter::McpState,
        specs,
        transport::{self, AgentSession},
        wire::{
            ErrorCode, Request, Response, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
        },
    },
    app::AppConfig,
    db::Database,
    models::{MetadataAttribute, ScrapedImage, ScrapedItem},
    scraper::ScraperRegistry,
    services::UrlQueueService,
    ui::UiLog,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
};

struct Fixture {
    _root: tempfile::TempDir,
    profile: PathBuf,
    config: AppConfig,
    queue: Arc<UrlQueueService>,
    log: UiLog,
    page_id: i64,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("agent profile");
        fs::create_dir(&directory).unwrap();
        let profile = directory.join("app-config.json");
        let mut config = AppConfig {
            database_path: root.path().join("library.sqlite3").display().to_string(),
            download_folder: root.path().join("downloads").display().to_string(),
            clipboard_listener_enabled: false,
            ..AppConfig::default()
        };
        config.extension_settings.insert(
            "example.parser".into(),
            json!({"password":"test-fixture-private-value"}),
        );
        config.save_to(&profile).unwrap();
        fs::create_dir(&config.download_folder).unwrap();
        let db = Database::new(&config.database_path);
        db.initialize().unwrap();
        let attribute = |kind: &str, value: &str| MetadataAttribute {
            kind: kind.into(),
            label: kind.into(),
            value: value.into(),
            source_url: None,
            is_female: None,
        };
        let page = ScrapedItem {
            site_key: "example".into(),
            page_url: "https://example.test/topic?t=7&token=test-fixture-private-value".into(),
            title: "Example card".into(),
            raw_html: String::new(),
            images: (0..3)
                .map(|i| ScrapedImage {
                    source_url: format!("https://example.test/image/{i}.jpg"),
                    preview_url: None,
                    alt_text: None,
                    width: None,
                    height: None,
                    sort_order: i,
                    local_file_path: None,
                })
                .collect(),
            attributes: vec![
                attribute("content_title", "Example movie"),
                attribute("director", "Example Creator"),
                attribute("description", "Full description"),
                attribute("api_key", "test-fixture-private-value"),
                attribute(
                    "magnet",
                    "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
                ),
            ],
        };
        let page_id = db.upsert_scraped_item(&page).unwrap().page_id;
        let queue = Arc::new(UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_public_scrapers(),
            root.path().join("queue.sqlite3"),
        ));
        Self {
            _root: root,
            profile,
            config,
            queue,
            log: UiLog::default(),
            page_id,
        }
    }
    fn session(&self) -> (AgentSession, zeroize::Zeroizing<String>) {
        AgentSession::start(
            self.config.clone(),
            Arc::clone(&self.queue),
            &self.profile,
            self.log.clone(),
            Arc::new(|| {}),
        )
        .unwrap()
    }
    fn call(&self, token: &str, name: &str, args: Value) -> Response {
        transport::call(&self.profile, token, name, args).unwrap()
    }
}

#[test]
fn live_session_defaults_off_requires_a_token_and_never_exposes_secrets_or_paths() {
    let fixture = Fixture::new();
    let (session, token) = fixture.session();
    assert!(!session.service().enabled());
    assert_eq!(
        fixture.call(&token, "archivesift_status", json!({})).error,
        Some(ErrorCode::Disabled)
    );
    session.service().activate(false);
    assert_eq!(
        fixture
            .call("invalid", "archivesift_status", json!({}))
            .error,
        Some(ErrorCode::Unauthorized)
    );
    let endpoint = fs::read_to_string(transport::endpoint_path(&fixture.profile).unwrap()).unwrap();
    assert!(!endpoint.contains(token.as_str()));
    let status = fixture.call(&token, "archivesift_status", json!({}));
    assert_eq!(
        status.result.as_ref().unwrap()["grants"],
        json!({"read":true,"downloads":false})
    );
    let images = fixture.call(
        &token,
        "archivesift_search_images",
        json!({"text":"Example","limit":1}),
    );
    let images = images.result.unwrap();
    assert_eq!(images["items"].as_array().unwrap().len(), 1);
    let next = fixture
        .call(
            &token,
            "archivesift_search_images",
            json!({"text":"Example","limit":1,"cursor":images["next_cursor"]}),
        )
        .result
        .unwrap();
    assert_ne!(images["items"][0]["image_id"], next["items"][0]["image_id"]);
    let torrents = fixture
        .call(
            &token,
            "archivesift_search_torrents",
            json!({"creators":["Example Creator"]}),
        )
        .result
        .unwrap();
    assert_eq!(torrents["total"], 1);
    let details = fixture
        .call(
            &token,
            "archivesift_torrent_details",
            json!({"page_id":fixture.page_id}),
        )
        .result
        .unwrap();
    assert_eq!(details["description"], "Full description");
    assert_eq!(details["page_url"], "https://example.test/topic?t=7");
    assert_eq!(details["image_count"], 3);
    for response in [
        serde_json::to_value(status).unwrap(),
        images,
        next,
        torrents,
        details,
    ] {
        let text = response.to_string();
        assert!(!text.contains("test-fixture-private-value"));
        assert!(!text.contains(token.as_str()));
        assert!(!text.contains(&fixture.config.database_path));
        assert!(!text.contains(&fixture.config.download_folder));
    }
    assert!(!fixture
        .log
        .snapshot()
        .join(" ")
        .contains("test-fixture-private-value"));
    session.service().disable();
    assert_eq!(
        fixture.call(&token, "archivesift_status", json!({})).error,
        Some(ErrorCode::Disabled)
    );
}

#[test]
fn queue_mutations_require_a_separate_revocable_grant_and_use_the_existing_queue() {
    let fixture = Fixture::new();
    let (session, token) = fixture.session();
    session.service().activate(false);
    let add = json!({"urls":["https://demo.example.test/galleries/queued/"]});
    assert_eq!(
        fixture
            .call(&token, "archivesift_queue_add", add.clone())
            .error,
        Some(ErrorCode::PermissionDenied)
    );
    assert_eq!(fixture.queue.snapshot_light().pending_total, 0);
    session.service().set_download_permission(true);
    let response = fixture.call(&token, "archivesift_queue_add", add.clone());
    assert_eq!(response.result.unwrap()["queued"], 1);
    assert_eq!(fixture.queue.snapshot_light().pending_total, 1);
    assert!(session.service().take_processor_request());
    assert_eq!(
        fixture
            .call(&token, "archivesift_queue_add", add)
            .result
            .unwrap()["queued"],
        0
    );
    fixture.call(&token, "archivesift_queue_pause", json!({}));
    assert!(fixture.queue.is_paused());
    fixture.call(&token, "archivesift_queue_resume", json!({}));
    assert!(!fixture.queue.is_paused());
    session.service().set_download_permission(false);
    assert_eq!(
        fixture
            .call(&token, "archivesift_queue_pause", json!({}))
            .error,
        Some(ErrorCode::PermissionDenied)
    );
    assert!(fixture
        .call(&token, "archivesift_queue_status", json!({}))
        .error
        .is_none());
    assert_eq!(
        fixture
            .call(&token, "set_agent_access", json!({"enabled":true}))
            .error,
        Some(ErrorCode::InvalidArguments)
    );
    assert_eq!(
        fixture
            .call(&token, "archivesift_status", json!({"confirmed":true}))
            .error,
        Some(ErrorCode::InvalidArguments)
    );
    assert_eq!(
        fixture
            .call(
                &token,
                "archivesift_search_images",
                json!({"database_path":"other.sqlite3"})
            )
            .error,
        Some(ErrorCode::InvalidArguments)
    );
    assert_eq!(
        fixture
            .call(&token, "archivesift_search_images", json!({"limit":51}))
            .error,
        Some(ErrorCode::InvalidArguments)
    );
}

#[test]
fn profile_lock_token_rotation_and_shutdown_prevent_stale_or_competing_sessions() {
    let fixture = Fixture::new();
    let (session, old) = fixture.session();
    session.service().activate(true);
    assert!(matches!(
        AgentSession::start(
            fixture.config.clone(),
            Arc::clone(&fixture.queue),
            &fixture.profile,
            fixture.log.clone(),
            Arc::new(|| {})
        ),
        Err(ErrorCode::Busy)
    ));
    let service = Arc::clone(session.service());
    drop(session);
    assert!(!service.enabled());
    assert!(!transport::endpoint_path(&fixture.profile).unwrap().exists());
    assert!(matches!(
        transport::call(&fixture.profile, &old, "archivesift_status", json!({})),
        Err(ErrorCode::NotRunning)
    ));
    let (next, new) = fixture.session();
    next.service().activate(false);
    assert_eq!(
        fixture.call(&old, "archivesift_status", json!({})).error,
        Some(ErrorCode::Unauthorized)
    );
    assert!(fixture
        .call(&new, "archivesift_status", json!({}))
        .error
        .is_none());
    let mut changed = fixture.config.clone();
    changed.database_path = "other.sqlite3".into();
    assert!(!next.service().bound_to(&changed));
}

#[test]
fn schemas_and_mcp_lifecycle_reject_invalid_or_uninitialized_operations() {
    assert_eq!(specs::tools().len(), 10);
    let mut state = McpState::default();
    let mut calls = 0;
    let mut invoke = |_: &str, _: Value| {
        calls += 1;
        Ok(Response::success("example", json!({"ok":true})))
    };
    let before = state
        .dispatch(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            &mut invoke,
        )
        .unwrap();
    assert!(before["error"].is_object());
    state.dispatch(&json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),&mut invoke).unwrap();
    assert!(state
        .dispatch(
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
            &mut invoke
        )
        .unwrap()["error"]
        .is_object());
    assert!(state
        .dispatch(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            &mut invoke
        )
        .is_none());
    assert_eq!(
        state
            .dispatch(
                &json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}),
                &mut invoke
            )
            .unwrap()["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        10
    );
    let invalid=state.dispatch(&json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"archivesift_queue_add","arguments":{"urls":[]}}}),&mut invoke).unwrap();
    assert_eq!(invalid["error"]["code"], -32602);
    assert!(state.dispatch(&json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"archivesift_queue_resume"}}),&mut invoke).is_none());
    let good=state.dispatch(&json!({"jsonrpc":"2.0","id":"call","method":"tools/call","params":{"name":"archivesift_status"}}),&mut invoke).unwrap();
    assert_eq!(good["result"]["structuredContent"]["ok"], true);
    assert_eq!(calls, 1);
    assert!(specs::find("archivesift_search_torrents")
        .unwrap()
        .validate(&json!({"year":"oops"}))
        .is_err());
}

#[test]
fn real_mcp_executable_uses_json_rpc_only_and_attaches_without_starting_an_engine() {
    let fixture = Fixture::new();
    let (session, token) = fixture.session();
    session.service().activate(false);
    let mut child = Command::new(env!("CARGO_BIN_EXE_archive-sift-mcp"))
        .arg("--config")
        .arg(&fixture.profile)
        .env("ARCHIVESIFT_CONTROL_TOKEN", token.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"archivesift_status"}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"archivesift_queue_resume"}}),
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for message in messages {
            writeln!(stdin, "{message}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    let replies = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 10);
    assert_eq!(
        replies[2]["result"]["structuredContent"]["application"],
        "ArchiveSift"
    );
    assert_eq!(replies[3]["result"]["isError"], true);
    assert!(!text.contains(token.as_str()));
    assert!(!text.contains("test-fixture-private-value"));
    assert_eq!(fixture.queue.snapshot_light().pending_total, 0);
}

#[test]
fn malformed_oversized_and_redirected_ipc_requests_fail_closed() {
    let fixture = Fixture::new();
    let (session, token) = fixture.session();
    session.service().activate(false);
    assert!(matches!(
        transport::call(
            &fixture.profile,
            &token,
            "archivesift_status",
            json!({"large":"x".repeat(MAX_REQUEST_BYTES)})
        ),
        Err(ErrorCode::TooLarge)
    ));
    let endpoint = transport::endpoint_path(&fixture.profile).unwrap();
    let mut details: Value = serde_json::from_slice(&fs::read(&endpoint).unwrap()).unwrap();
    let address = details["address"].as_str().unwrap().to_owned();
    let mut stream = std::net::TcpStream::connect(&address).unwrap();
    stream.write_all(b"not JSON\n").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.len() < MAX_RESPONSE_BYTES);
    assert_eq!(
        serde_json::from_str::<Response>(&response).unwrap().error,
        Some(ErrorCode::InvalidArguments)
    );
    details["address"] = "192.0.2.1:1234".into();
    fs::write(&endpoint, serde_json::to_vec(&details).unwrap()).unwrap();
    assert!(matches!(
        transport::call(&fixture.profile, &token, "archivesift_status", json!({})),
        Err(ErrorCode::NotRunning)
    ));
    // Do not leave the deliberately corrupted discovery record in the fixture.
    details["address"] = address.into();
    fs::write(&endpoint, serde_json::to_vec(&details).unwrap()).unwrap();
}

#[test]
fn missing_destination_and_unavailable_library_return_safe_actionable_errors() {
    let mut fixture = Fixture::new();
    fixture.config.download_folder.clear();
    fixture.config.database_path = fixture
        ._root
        .path()
        .join("missing-fixture-library.sqlite3")
        .display()
        .to_string();
    let (session, token) = fixture.session();
    session.service().activate(true);
    let response = fixture.call(
        &token,
        "archivesift_queue_add",
        json!({"urls":["https://demo.example.test/galleries/queued/"]}),
    );
    assert_eq!(response.error, Some(ErrorCode::DestinationNotConfigured));
    assert_eq!(fixture.queue.snapshot_light().pending_total, 0);
    let response = fixture.call(&token, "archivesift_search_images", json!({}));
    assert_eq!(response.error, Some(ErrorCode::LibraryUnavailable));
    assert!(!serde_json::to_string(&response)
        .unwrap()
        .contains("missing-fixture-library"));
    assert!(!std::path::Path::new(&fixture.config.database_path).exists());
}

#[test]
fn invalid_wire_fields_cannot_grant_permissions() {
    let invalid = json!({"protocol_version":PROTOCOL_VERSION,"instance_id":"example","token":"fixture","tool":"archivesift_status","arguments":{},"confirmed":true});
    assert!(serde_json::from_value::<Request>(invalid).is_err());
}

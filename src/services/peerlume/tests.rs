use super::*;
use client::{ControlTransport, Reply};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const TORRENT: &[u8] =
    b"d4:infod6:lengthi3e4:name4:demo12:piece lengthi16384e6:pieces20:12345678901234567890ee";

pub(crate) fn prepared_for_ui_test() -> PreparedTorrent {
    Fixture::new().prepare()
}

#[path = "mode_tests.rs"]
mod mode_tests;

#[derive(Clone)]
struct Fake {
    state: Arc<Mutex<FakeState>>,
    profile: std::path::PathBuf,
    database: std::path::PathBuf,
}
struct FakeState {
    instance: String,
    manage: bool,
    reporting: bool,
    protocol: u32,
    methods: Vec<Value>,
    torrents: Vec<Value>,
    operations: Vec<Value>,
    add_error: Option<Error>,
    mutation_error: Option<Error>,
    page_error_once: bool,
    pages: bool,
    fail_preview: bool,
}
impl ControlTransport for Fake {
    fn request(&self, instance: Option<&str>, method: Value) -> Result<Reply> {
        let mut state = self.state.lock().unwrap();
        state.methods.push(method.clone());
        if instance.is_some_and(|id| id != state.instance) {
            return Err(Error::OwnerChanged);
        }
        let args = &method["args"];
        let result = match method["name"].as_str().unwrap() {
            "discover" => {
                json!({"protocol_version":state.protocol,"capabilities":["preview","add","operations","list","mutate"],"grants":{"manage":state.manage,"reporting":state.reporting}})
            }
            "preview" => {
                if state.fail_preview {
                    return Err(Error::PathDenied);
                }
                let bytes = std::fs::read(
                    self.profile
                        .join("imports")
                        .join(args["source"].as_str().unwrap()),
                )
                .unwrap();
                let hashes = files::hashes(&bytes).unwrap();
                json!({"total_bytes":3,"file_count":1,"private":true,"info_hash_v1":hashes.v1,"info_hash_v2":hashes.v2})
            }
            "list" => {
                let offset = args["offset"].as_u64().unwrap() as usize;
                if offset > 0 && state.page_error_once {
                    state.page_error_once = false;
                    return Err(Error::OwnerChanged);
                }
                let size = if state.pages { 1 } else { 128 };
                let rows: Vec<_> = state
                    .torrents
                    .iter()
                    .skip(offset)
                    .take(size)
                    .cloned()
                    .collect();
                let next =
                    (offset + rows.len() < state.torrents.len()).then_some(offset + rows.len());
                json!({"revision":7,"controller_available":true,"resync_required":false,"next_offset":next,"torrents":rows})
            }
            "add" => {
                // The server must never see an add before the durable intent.
                let conn = rusqlite::Connection::open(&self.database).unwrap();
                let count: usize = conn
                    .query_row("SELECT COUNT(*) FROM peerlume_handoffs", [], |r| r.get(0))
                    .unwrap();
                assert!(count > 0);
                let op = json!({"operation_id":format!("op-{}",state.operations.len()),"torrent_id":"","command":"add","state":"queued","error":null});
                state.operations.push(op.clone());
                if let Some(error) = state.add_error.take() {
                    return Err(error);
                }
                op
            }
            "operation" => state
                .operations
                .iter()
                .find(|op| op["operation_id"] == args["operation_id"])
                .cloned()
                .ok_or(Error::ReconciliationRequired)?,
            "mutate" => {
                if !state.reporting {
                    return Err(Error::PermissionDenied);
                }
                let index = state
                    .torrents
                    .iter()
                    .position(|row| row["id"] == args["torrent_id"])
                    .unwrap();
                assert_eq!(
                    args["expected_revision"].as_u64().unwrap(),
                    state.torrents[index]["command_revision"]
                        .as_u64()
                        .unwrap_or(0)
                );
                // A mutation intent must already be durable before delivery.
                let conn = rusqlite::Connection::open(&self.database).unwrap();
                let json: String = conn
                    .query_row(
                        "SELECT record FROM peerlume_handoffs ORDER BY rowid DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(json.contains(args["idempotency_key"].as_str().unwrap()));
                let command = args["command"]["kind"].as_str().unwrap();
                let op = json!({"operation_id":format!("op-{}",state.operations.len()),"torrent_id":args["torrent_id"],"command":command,"state":"queued","error":null});
                state.operations.push(op.clone());
                state.torrents[index]["command_revision"] =
                    (args["expected_revision"].as_u64().unwrap() + 1).into();
                if let Some(error) = state.mutation_error.take() {
                    return Err(error);
                }
                op
            }
            "operations" => json!({"operations":state.operations,"next_offset":null}),
            _ => panic!("Unexpected request"),
        };
        Ok(Reply {
            protocol_version: 1,
            instance_id: state.instance.clone(),
            result: Some(result),
            error: None,
        })
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    config: AppConfig,
    fake: Fake,
    id: i64,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("archive");
        let profile = root.path().join("profile");
        std::fs::create_dir(&archive).unwrap();
        std::fs::create_dir(&profile).unwrap();
        let config = AppConfig {
            database_path: root.path().join("library.sqlite3").display().to_string(),
            download_folder: archive.display().to_string(),
            peerlume: PeerLumeConfig {
                enabled: true,
                executable: std::env::current_exe().unwrap().display().to_string(),
                profile: profile.display().to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let database = Database::new(&config.database_path);
        database.initialize().unwrap();
        let page = crate::models::ScrapedItem {
            site_key: "example-source".into(),
            page_url: "https://example.test/release/1".into(),
            title: "Example release".into(),
            raw_html: String::new(),
            images: vec![],
            attributes: vec![crate::models::MetadataAttribute {
                kind: "attachment".into(),
                label: "Torrent".into(),
                value: "release.torrent".into(),
                source_url: Some("https://example.test/release.torrent".into()),
                is_female: None,
            }],
        };
        let id = database.upsert_scraped_item(&page).unwrap().page_id;
        std::fs::write(archive.join("release.torrent"), TORRENT).unwrap();
        database
            .save_page_attachment(
                id,
                "https://example.test/release.torrent",
                "release.torrent",
                "release.torrent",
            )
            .unwrap();
        let fake = Fake {
            state: Arc::new(Mutex::new(FakeState {
                instance: "owner-1".into(),
                manage: true,
                reporting: true,
                protocol: 1,
                methods: vec![],
                torrents: vec![],
                operations: vec![],
                add_error: None,
                mutation_error: None,
                pages: false,
                page_error_once: false,
                fail_preview: false,
            })),
            profile,
            database: config.database_path.clone().into(),
        };
        Self {
            _root: root,
            config,
            fake,
            id,
        }
    }
    fn service(&self) -> HandoffService<Fake> {
        HandoffService::new(self.config.clone(), self.fake.clone()).unwrap()
    }
    fn prepare(&self) -> PreparedTorrent {
        let mut result = self.service().prepare(&[self.id]).unwrap();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        result.torrents.remove(0)
    }
    fn adds(&self) -> usize {
        self.fake
            .state
            .lock()
            .unwrap()
            .methods
            .iter()
            .filter(|v| v["name"] == "add")
            .count()
    }
    fn observed(&self, hash: &InfoHashes, state: &str) -> Value {
        json!({"id":hash.keys().join(";"),"state":state,"downloaded_bytes":2})
    }
}

#[test]
fn peerlume_prepare_stages_exact_bytes_without_starting_a_transfer() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    assert!(prepared.preview.private);
    assert_eq!(prepared.preview.file_count, 1);
    assert_eq!(
        std::fs::read(fixture.fake.profile.join("imports").join(&prepared.source)).unwrap(),
        TORRENT
    );
    assert_eq!(fixture.adds(), 0);
    assert!(fixture.service().history().unwrap().is_empty());
    assert!(std::path::Path::new(&fixture.config.download_folder)
        .join("release.torrent")
        .exists());
}

#[test]
fn peerlume_confirmation_is_durable_paused_and_idempotent_across_restart() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    let record = fixture
        .service()
        .submit(&prepared, "incoming/movies", false)
        .unwrap();
    assert_eq!(record.state, HandoffState::Accepted);
    assert_eq!(record.operation_id.as_deref(), Some("op-0"));
    let again = fixture
        .service()
        .submit(&prepared, "another-destination", true)
        .unwrap();
    assert_eq!(again.id, record.id);
    assert_eq!(again.destination, "incoming/movies");
    assert!(!again.start);
    assert_eq!(fixture.adds(), 1);
    let add = fixture
        .fake
        .state
        .lock()
        .unwrap()
        .methods
        .iter()
        .find(|v| v["name"] == "add")
        .unwrap()
        .clone();
    assert_eq!(add["args"]["idempotency_key"], record.idempotency_key);
    assert_eq!(add["args"]["deadline_unix_ms"], record.deadline_unix_ms);
    assert_eq!(add["args"]["start"], false);
    assert!(!serde_json::to_string(&record).unwrap().contains("magnet:"));
    assert!(!record.idempotency_key.is_empty());
}

#[test]
fn peerlume_link_existing_task_and_do_not_change_its_start_mode() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .fake
        .state
        .lock()
        .unwrap()
        .torrents
        .push(fixture.observed(&prepared.preview.hashes(), "paused"));
    let record = fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    assert_eq!(record.state, HandoffState::Linked);
    assert_eq!(record.transfer, Some(TransferState::Paused));
    assert_eq!(fixture.adds(), 0);
}

#[test]
fn peerlume_ambiguous_delivery_never_replays_and_can_reconcile_by_hash() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture.fake.state.lock().unwrap().add_error = Some(Error::Timeout);
    let record = fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    assert_eq!(record.state, HandoffState::ReconciliationRequired);
    assert_eq!(
        fixture
            .service()
            .submit(&prepared, "incoming", true)
            .unwrap()
            .id,
        record.id
    );
    assert_eq!(fixture.adds(), 1);
    assert_eq!(
        fixture.service().refresh().unwrap()[0].state,
        HandoffState::ReconciliationRequired
    );
    fixture
        .fake
        .state
        .lock()
        .unwrap()
        .torrents
        .push(fixture.observed(&prepared.preview.hashes(), "downloading"));
    let refreshed = fixture.service().refresh().unwrap().remove(0);
    assert_eq!(refreshed.state, HandoffState::Linked);
    assert_eq!(refreshed.downloaded_bytes, 2);
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_lost_acknowledgment_can_recover_the_operation_without_readding() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture.fake.state.lock().unwrap().add_error = Some(Error::Timeout);
    fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    {
        let mut state = fixture.fake.state.lock().unwrap();
        state.operations[0]["torrent_id"] = prepared.preview.hashes().keys().join(";").into();
        state.operations[0]["state"] = "succeeded".into();
    }
    let record = fixture.service().refresh().unwrap().remove(0);
    assert_eq!(record.state, HandoffState::Added);
    assert_eq!(record.operation_id.as_deref(), Some("op-0"));
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_operation_failure_is_distinct_from_acceptance() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    fixture.fake.state.lock().unwrap().operations[0]["state"] = "failed".into();
    fixture.fake.state.lock().unwrap().operations[0]["error"] = "engine_rejected".into();
    let record = fixture.service().refresh().unwrap().remove(0);
    assert_eq!(record.state, HandoffState::Failed);
    assert!(record.error.is_some());
    assert!(fixture.service().refresh().unwrap()[0].error.is_some());
}

#[test]
fn peerlume_confirmed_failure_can_only_be_retried_by_a_new_explicit_review() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    let first = fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    fixture.fake.state.lock().unwrap().operations[0]["state"] = "failed".into();
    fixture.service().refresh().unwrap();
    let again = fixture
        .service()
        .submit(&prepared, "corrected/destination", false)
        .unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(fixture.adds(), 1);
    let retry = fixture
        .service()
        .retry_failed(&fixture.prepare(), "corrected/destination", false)
        .unwrap();
    assert_ne!(retry.id, first.id);
    assert_ne!(retry.idempotency_key, first.idempotency_key);
    assert_eq!(retry.state, HandoffState::Accepted);
    assert_eq!(fixture.adds(), 2);
    assert_eq!(fixture.service().history().unwrap().len(), 2);
    // An explicit retry is still forbidden after uncertain delivery.
    fixture.fake.state.lock().unwrap().operations[1]["state"] = "reconciliation_required".into();
    fixture.service().refresh().unwrap();
    assert_eq!(
        fixture
            .service()
            .retry_failed(&prepared, "incoming", false)
            .unwrap()
            .id,
        retry.id
    );
    assert_eq!(fixture.adds(), 2);
}

#[test]
fn peerlume_observed_transfer_does_not_claim_a_queued_operation_succeeded() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    fixture
        .fake
        .state
        .lock()
        .unwrap()
        .torrents
        .push(fixture.observed(&prepared.preview.hashes(), "awaiting_engine"));
    let record = fixture.service().refresh().unwrap().remove(0);
    assert_eq!(record.state, HandoffState::Accepted);
    assert_eq!(record.transfer, Some(TransferState::AwaitingEngine));
    fixture.fake.state.lock().unwrap().operations[0]["state"] = "succeeded".into();
    assert_eq!(
        fixture.service().refresh().unwrap()[0].state,
        HandoffState::Added
    );
}

#[test]
fn peerlume_removed_torrent_is_marked_missing_without_resubmitting() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    fixture.fake.state.lock().unwrap().operations[0]["state"] = "succeeded".into();
    fixture.fake.state.lock().unwrap().operations[0]["torrent_id"] =
        prepared.preview.hashes().keys().join(";").into();
    fixture.service().refresh().unwrap();
    let record = fixture.service().refresh().unwrap().remove(0);
    assert!(record.missing);
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_owner_change_requires_review_and_does_not_replay_after_restart() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    fixture.fake.state.lock().unwrap().instance = "owner-2".into();
    assert!(matches!(
        fixture.service().submit(&prepared, "incoming", false),
        Err(Error::OwnerChanged)
    ));
    assert_eq!(
        fixture.service().refresh().unwrap()[0].state,
        HandoffState::ReconciliationRequired
    );
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_owner_restart_links_existing_transfers_and_retains_known_completed_add_results() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    fixture.fake.state.lock().unwrap().instance = "owner-2".into();
    fixture
        .fake
        .state
        .lock()
        .unwrap()
        .torrents
        .push(fixture.observed(&prepared.preview.hashes(), "downloading"));
    let record = fixture.service().refresh().unwrap().remove(0);
    assert_eq!(record.state, HandoffState::Linked);
    assert_eq!(record.transfer, Some(TransferState::Downloading));
    fixture.fake.state.lock().unwrap().torrents.clear();
    let record = fixture.service().refresh().unwrap().remove(0);
    assert_eq!(record.state, HandoffState::Linked);
    assert!(record.missing);
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_changed_import_and_stale_review_cannot_be_submitted() {
    let fixture = Fixture::new();
    let mut prepared = fixture.prepare();
    prepared.reviewed_at = 0;
    assert!(matches!(
        fixture.service().submit(&prepared, "incoming", false),
        Err(Error::OwnerChanged)
    ));
    prepared.reviewed_at = now_ms();
    std::fs::write(
        fixture.fake.profile.join("imports").join(&prepared.source),
        b"corrupted",
    )
    .unwrap();
    assert!(matches!(
        fixture.service().submit(&prepared, "incoming", false),
        Err(Error::Staging)
    ));
    assert_eq!(fixture.adds(), 0);
}

#[test]
fn peerlume_batch_keeps_successful_previews_when_another_source_is_invalid() {
    let fixture = Fixture::new();
    let database = Database::new(&fixture.config.database_path);
    database
        .save_page_attachment(
            fixture.id,
            "https://example.test/missing",
            "missing.torrent",
            "missing.torrent",
        )
        .unwrap();
    let result = fixture
        .service()
        .prepare(&[fixture.id, fixture.id, 9999999])
        .unwrap();
    assert_eq!(result.torrents.len(), 1);
    assert_eq!(result.errors.len(), 2);
    assert_eq!(fixture.adds(), 0);
    assert_eq!(
        fixture
            .service()
            .submit(&result.torrents[0], "incoming", true)
            .unwrap()
            .state,
        HandoffState::Accepted
    );
}

#[test]
fn peerlume_connection_checks_grant_and_protocol() {
    let fixture = Fixture::new();
    fixture.fake.state.lock().unwrap().manage = false;
    assert_eq!(
        fixture.service().test_connection(),
        Err(Error::PermissionDenied)
    );
    fixture.fake.state.lock().unwrap().manage = true;
    fixture.fake.state.lock().unwrap().protocol = 2;
    assert_eq!(fixture.service().test_connection(), Err(Error::Protocol));
    assert_eq!(fixture.adds(), 0);
}

#[test]
fn peerlume_profile_identity_separates_credentials_and_journals() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    let mut config = fixture.config.clone();
    let profile = fixture._root.path().join("second-profile");
    std::fs::create_dir(&profile).unwrap();
    config.peerlume.profile = profile.display().to_string();
    assert_ne!(
        config.peerlume.secret_target().unwrap(),
        fixture.config.peerlume.secret_target().unwrap()
    );
    assert!(config
        .peerlume
        .secret_target()
        .unwrap()
        .starts_with(crate::secure_store::HOST_SECRET_NAMESPACE));
    assert!(HandoffService::new(config, fixture.fake.clone())
        .unwrap()
        .history()
        .unwrap()
        .is_empty());
}

#[test]
fn peerlume_concurrent_confirmations_reserve_one_durable_intent() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    let config = fixture.config.clone();
    let fake = fixture.fake.clone();
    let first = prepared.clone();
    let thread = std::thread::spawn(move || {
        HandoffService::new(config, fake)
            .unwrap()
            .submit(&first, "incoming", false)
            .unwrap()
    });
    let second = fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    assert_eq!(thread.join().unwrap().id, second.id);
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_snapshot_restarts_read_only_pagination_on_revision_conflict() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare();
    {
        let mut state = fixture.fake.state.lock().unwrap();
        state.pages = true;
        state.page_error_once = true;
        state.torrents = vec![
            json!({"id":format!("v1:{}","0".repeat(40)),"state":"paused","downloaded_bytes":0}),
            fixture.observed(&prepared.preview.hashes(), "seeding"),
        ];
    }
    let record = fixture
        .service()
        .submit(&prepared, "incoming", false)
        .unwrap();
    assert_eq!(record.transfer, Some(TransferState::Seeding));
    assert_eq!(fixture.adds(), 0);
    let state = fixture.fake.state.lock().unwrap();
    let lists: Vec<_> = state
        .methods
        .iter()
        .filter(|m| m["name"] == "list")
        .collect();
    assert_eq!(lists.len(), 4);
    assert_eq!(lists[1]["args"]["revision"], 7);
    assert!(lists[2]["args"]["revision"].is_null());
}

#[test]
fn peerlume_path_validation_is_portable_and_rejects_unsafe_components() {
    for path in [
        "",
        "../escape",
        "/absolute",
        "C:\\payload",
        "a//b",
        "a/./b",
        "a/..",
        "a:ads",
        "CON",
        "a/LPT1.txt",
        "a/last.",
        "a/trailing ",
        "a/\n",
        "\\\\host\\share",
    ] {
        assert!(relative_destination(path).is_err(), "{path:?}");
    }
    assert_eq!(
        relative_destination("incoming\\movies").unwrap(),
        "incoming/movies"
    );
    assert!(relative_destination("Студия/Film 2026").is_ok());
}

#[test]
fn peerlume_source_bounds_and_hashes_cover_v1_v2_and_hybrid() {
    let v1 = files::hashes(TORRENT).unwrap();
    assert!(v1.v1.is_some() && v1.v2.is_none());
    let v2 = files::hashes(b"d4:infod12:meta versioni2e4:name4:demoee").unwrap();
    assert!(v2.v1.is_none() && v2.v2.is_some());
    let hybrid = files::hashes(b"d4:infod12:meta versioni2e4:name4:demo6:pieces0:ee").unwrap();
    assert!(hybrid.v1.is_some() && hybrid.v2.is_some());
    assert!(files::hashes(b"<html>not metainfo</html>").is_err());
    let fixture = Fixture::new();
    let root = std::path::Path::new(&fixture.config.download_folder);
    assert!(files::read_archive(root, "../outside.torrent").is_err());
    let file = std::fs::File::create(root.join("large.torrent")).unwrap();
    file.set_len(8 * 1024 * 1024 + 1).unwrap();
    assert!(files::read_archive(root, "large.torrent").is_err());
}

#[cfg(unix)]
#[test]
fn peerlume_rejects_links_in_archive_and_import_paths() {
    let fixture = Fixture::new();
    let root = std::path::Path::new(&fixture.config.download_folder);
    std::os::unix::fs::symlink(root.join("release.torrent"), root.join("linked.torrent")).unwrap();
    assert!(files::read_archive(root, "linked.torrent").is_err());
    std::os::unix::fs::symlink(root, fixture.fake.profile.join("imports")).unwrap();
    assert!(files::stage(&fixture.fake.profile, TORRENT).is_err());
}

#[test]
fn peerlume_old_config_and_json_editor_support_the_new_section_without_secrets() {
    let config = AppConfig::default();
    let catalog = crate::scraper::extensions::settings::SettingsCatalog::default();
    let mut document = serde_json::to_value(&config).unwrap();
    document.as_object_mut().unwrap().remove("peerlume");
    assert!(
        !serde_json::from_value::<AppConfig>(document)
            .unwrap()
            .peerlume
            .enabled
    );
    let schema = crate::services::configuration::schema(&catalog).unwrap();
    let props = &schema.value()["properties"]["peerlume"]["properties"];
    assert!(props.get("destination").is_some());
    assert!(props.get("token").is_none());
    let text = "{\"peerlume\": {\"dest";
    let completions =
        crate::services::config_completion::complete(text, text.chars().count(), &schema);
    assert!(completions.iter().any(|c| c.label == "destination"));
    let mut document = serde_json::to_value(config).unwrap();
    document["peerlume"]["token"] = "never-allowed-in-json".into();
    assert!(crate::services::configuration::validate(&document, &catalog).is_err());
}

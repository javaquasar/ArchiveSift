use super::mode::MutationIntent;
use super::*;

fn custom(mode: DownloadMode) -> ModeConfig {
    ModeConfig {
        mode,
        ..Default::default()
    }
}

impl Fixture {
    fn custom_fixture(mode: DownloadMode) -> Self {
        let mut fixture = Self::new();
        fixture.config.peerlume.download_mode = custom(mode);
        fixture
    }
    fn mutations(&self) -> Vec<Value> {
        self.fake
            .state
            .lock()
            .unwrap()
            .methods
            .iter()
            .filter(|m| m["name"] == "mutate")
            .cloned()
            .collect()
    }
    fn confirm_add(&self, prepared: &PreparedTorrent) {
        let mut state = self.fake.state.lock().unwrap();
        state.operations[0]["state"] = "succeeded".into();
        state.operations[0]["torrent_id"] = prepared.preview.hashes().keys().join(";").into();
        let mut row = self.observed(&prepared.preview.hashes(), "paused");
        row["command_revision"] = 3.into();
        row["active_mode"] = "normal".into();
        row["mode_state"] = "active".into();
        state.torrents.push(row);
    }
    fn confirm_mode(&self, config: ModeConfig) {
        let mut state = self.fake.state.lock().unwrap();
        state.operations[1]["state"] = "succeeded".into();
        state.torrents[0]["active_mode"] = serde_json::to_value(config.mode).unwrap();
        state.torrents[0]["active_freeze_after_bytes"] = config.freeze_after_bytes.into();
        state.torrents[0]["active_upload_ratio"] =
            serde_json::to_value(config.upload_ratio).unwrap();
        state.torrents[0]["mode_state"] = "active".into();
    }
}

#[test]
fn peerlume_all_custom_modes_are_applied_before_start_and_paused_choices_stay_paused() {
    for mode in DownloadMode::ALL
        .into_iter()
        .filter(|m| *m != DownloadMode::Normal)
    {
        for start in [false, true] {
            let fixture = Fixture::custom_fixture(mode);
            let prepared = fixture.prepare();
            let record = fixture
                .service()
                .submit(&prepared, "incoming", start)
                .unwrap();
            assert!(record.pending_setup());
            let add = fixture
                .fake
                .state
                .lock()
                .unwrap()
                .methods
                .iter()
                .find(|m| m["name"] == "add")
                .cloned()
                .unwrap();
            assert_eq!(add["args"]["start"], false);
            assert!(fixture.mutations().is_empty());
            // Queued Add is not enough to configure or start anything.
            fixture.service().advance_setups().unwrap();
            assert!(fixture.mutations().is_empty());
            fixture.confirm_add(&prepared);
            fixture.service().advance_setups().unwrap();
            let mutations = fixture.mutations();
            assert_eq!(mutations.len(), 1);
            assert_eq!(mutations[0]["args"]["command"]["kind"], "set_mode");
            assert_eq!(mutations[0]["args"]["expected_revision"], 3);
            assert_eq!(
                mutations[0]["args"]["command"]["value"],
                serde_json::to_value(fixture.config.peerlume.download_mode).unwrap()
            );
            // Even repeated polling cannot start before mode acknowledgement.
            fixture.service().advance_setups().unwrap();
            assert_eq!(fixture.mutations().len(), 1);
            fixture.confirm_mode(fixture.config.peerlume.download_mode);
            let record = fixture.service().advance_setups().unwrap().remove(0);
            if start {
                assert_eq!(record.mode_setup.unwrap().state, SetupState::Starting);
                let mutations = fixture.mutations();
                assert_eq!(mutations.len(), 2);
                assert_eq!(mutations[1]["args"]["command"], json!({"kind":"resume"}));
                assert_eq!(mutations[1]["args"]["expected_revision"], 4);
                fixture.fake.state.lock().unwrap().operations[2]["state"] = "succeeded".into();
                assert_eq!(
                    fixture.service().advance_setups().unwrap()[0]
                        .mode_setup
                        .as_ref()
                        .unwrap()
                        .state,
                    SetupState::Complete
                );
            } else {
                assert_eq!(record.mode_setup.unwrap().state, SetupState::Complete);
                assert_eq!(fixture.mutations().len(), 1);
            }
        }
    }
}

#[test]
fn peerlume_custom_mode_requires_reporting_permission_before_any_add() {
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    fixture.fake.state.lock().unwrap().reporting = false;
    let prepared = fixture.prepare();
    assert!(matches!(
        fixture.service().submit(&prepared, "incoming", true),
        Err(Error::ReportingPermission)
    ));
    assert_eq!(fixture.adds(), 0);
    assert!(fixture.service().history().unwrap().is_empty());
    assert_eq!(
        fixture.service().test_connection(),
        Err(Error::ReportingPermission)
    );
    // A per-batch Normal override works with the existing manage-only grant.
    let record = fixture
        .service()
        .submit_with_mode(&prepared, "incoming", true, false, ModeConfig::default())
        .unwrap();
    assert!(!record.pending_setup());
    assert!(record.mode_setup.is_none());
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_rejects_custom_mode_for_pure_v2_or_empty_metadata_without_fallback() {
    let fixture = Fixture::custom_fixture(DownloadMode::HideDownloaded);
    let mut prepared = fixture.prepare();
    prepared.preview.info_hash_v1 = None;
    prepared.preview.info_hash_v2 = Some("0".repeat(64));
    assert!(matches!(
        fixture.service().submit(&prepared, "incoming", true),
        Err(Error::ModeMetadata)
    ));
    let mut prepared = fixture.prepare();
    prepared.preview.total_bytes = 0;
    assert!(matches!(
        fixture.service().submit(&prepared, "incoming", true),
        Err(Error::ModeMetadata)
    ));
    assert_eq!(fixture.adds(), 0);
}

#[test]
fn peerlume_existing_torrent_mode_and_start_state_are_never_changed() {
    let fixture = Fixture::custom_fixture(DownloadMode::FreezeAfter);
    let prepared = fixture.prepare();
    fixture
        .fake
        .state
        .lock()
        .unwrap()
        .torrents
        .push(fixture.observed(&prepared.preview.hashes(), "downloading"));
    let record = fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    assert_eq!(record.state, HandoffState::Linked);
    assert!(record.mode_setup.is_none());
    fixture.service().advance_setups().unwrap();
    assert!(fixture.mutations().is_empty());
    assert_eq!(fixture.adds(), 0);
}

#[test]
fn peerlume_mode_failure_never_resumes_and_does_not_change_the_saved_default() {
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    fixture.service().advance_setups().unwrap();
    {
        let mut state = fixture.fake.state.lock().unwrap();
        state.operations[1]["state"] = "failed".into();
        state.operations[1]["error"] = "engine_rejected".into();
    }
    let record = fixture.service().advance_setups().unwrap().remove(0);
    assert_eq!(record.mode_setup.unwrap().state, SetupState::Failed);
    fixture.service().advance_setups().unwrap();
    assert_eq!(fixture.mutations().len(), 1);
    assert_eq!(
        fixture.config.peerlume.download_mode.mode,
        DownloadMode::NoProgress
    );
}

#[test]
fn peerlume_ambiguous_mode_delivery_is_durable_and_never_replayed() {
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    fixture.fake.state.lock().unwrap().mutation_error = Some(Error::Timeout);
    let record = fixture.service().advance_setups().unwrap().remove(0);
    let setup = record.mode_setup.as_ref().unwrap();
    assert_eq!(setup.state, SetupState::ReconciliationRequired);
    let intent = setup.mode_request.as_ref().unwrap();
    assert!(intent.operation_id.is_none());
    assert_eq!(intent.deadline_unix_ms, record.deadline_unix_ms);
    let actual = fixture.mutations()[0].clone();
    assert_eq!(actual["args"]["idempotency_key"], intent.idempotency_key);
    fixture.service().advance_setups().unwrap();
    assert_eq!(fixture.mutations().len(), 1);
}

#[test]
fn peerlume_ambiguous_start_delivery_is_not_repeated() {
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    fixture.service().advance_setups().unwrap();
    fixture.confirm_mode(fixture.config.peerlume.download_mode);
    fixture.fake.state.lock().unwrap().mutation_error = Some(Error::Timeout);
    let record = fixture.service().advance_setups().unwrap().remove(0);
    let setup = record.mode_setup.unwrap();
    assert_eq!(setup.state, SetupState::ReconciliationRequired);
    assert!(setup.start_request.unwrap().operation_id.is_none());
    fixture.service().advance_setups().unwrap();
    assert_eq!(fixture.mutations().len(), 2);
    assert_eq!(fixture.adds(), 1);
}

#[test]
fn peerlume_parameter_mismatch_or_revoked_reporting_grant_keeps_a_custom_torrent_paused() {
    for mode in [DownloadMode::FreezeAfter, DownloadMode::UploadRatio] {
        let fixture = Fixture::custom_fixture(mode);
        let prepared = fixture.prepare();
        fixture
            .service()
            .submit(&prepared, "incoming", true)
            .unwrap();
        fixture.confirm_add(&prepared);
        fixture.service().advance_setups().unwrap();
        let mut changed = fixture.config.peerlume.download_mode;
        if mode == DownloadMode::FreezeAfter {
            changed.freeze_after_bytes += 1;
        } else {
            changed.upload_ratio.max_milli += 1;
        }
        fixture.confirm_mode(changed);
        let record = fixture.service().advance_setups().unwrap().remove(0);
        assert_eq!(record.mode_setup.unwrap().state, SetupState::Failed);
        assert_eq!(fixture.mutations().len(), 1);
    }
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    fixture.fake.state.lock().unwrap().reporting = false;
    let record = fixture.service().advance_setups().unwrap().remove(0);
    assert_eq!(record.mode_setup.unwrap().state, SetupState::Failed);
    assert!(fixture.mutations().is_empty());
}

#[test]
fn peerlume_crash_after_recording_a_mode_intent_never_resends_it() {
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    let service = fixture.service();
    let mut record = service.refresh().unwrap().remove(0);
    let setup = record.mode_setup.as_mut().unwrap();
    setup.state = SetupState::ApplyingMode;
    setup.mode_request = Some(MutationIntent {
        expected_revision: 3,
        idempotency_key: random_id().unwrap(),
        deadline_unix_ms: record.deadline_unix_ms,
        operation_id: None,
    });
    service.journal.save(&record).unwrap();
    let record = fixture.service().advance_setups().unwrap().remove(0);
    assert_eq!(
        record.mode_setup.unwrap().state,
        SetupState::ReconciliationRequired
    );
    assert!(fixture.mutations().is_empty());
}

#[test]
fn peerlume_changed_mode_or_owner_never_causes_automatic_start() {
    for owner_change in [false, true] {
        let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
        let prepared = fixture.prepare();
        fixture
            .service()
            .submit(&prepared, "incoming", true)
            .unwrap();
        fixture.confirm_add(&prepared);
        fixture.service().advance_setups().unwrap();
        fixture.confirm_mode(custom(DownloadMode::NoProgress));
        {
            let mut state = fixture.fake.state.lock().unwrap();
            if owner_change {
                state.instance = "owner-2".into();
            } else {
                state.torrents[0]["active_mode"] = "normal".into();
            }
        }
        let record = fixture.service().advance_setups().unwrap().remove(0);
        assert!(matches!(
            record.mode_setup.unwrap().state,
            SetupState::Failed | SetupState::ReconciliationRequired
        ));
        assert_eq!(fixture.mutations().len(), 1);
    }
}

#[test]
fn peerlume_expired_setup_stays_paused_and_defaults_do_not_rewrite_a_confirmed_mode() {
    let mut fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    fixture.config.peerlume.download_mode = custom(DownloadMode::FreezeAfter);
    fixture.service().advance_setups().unwrap();
    assert_eq!(
        fixture.mutations()[0]["args"]["command"]["value"]["mode"],
        "no_progress"
    );
    fixture.confirm_mode(custom(DownloadMode::NoProgress));
    let service = fixture.service();
    let mut record = service.history().unwrap().remove(0);
    record.deadline_unix_ms = 1;
    service.journal.save(&record).unwrap();
    let record = fixture.service().advance_setups().unwrap().remove(0);
    assert_eq!(record.mode_setup.unwrap().state, SetupState::Failed);
    assert_eq!(fixture.mutations().len(), 1);
}

#[test]
fn peerlume_concurrent_mode_workers_send_each_command_only_once() {
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    let config = fixture.config.clone();
    let fake = fixture.fake.clone();
    let worker = std::thread::spawn(move || {
        HandoffService::new(config, fake)
            .unwrap()
            .advance_setups()
            .unwrap()
    });
    fixture.service().advance_setups().unwrap();
    worker.join().unwrap();
    assert_eq!(fixture.mutations().len(), 1);
}

#[test]
fn peerlume_setup_waits_for_initial_file_checking_before_mode_or_start_commands() {
    let fixture = Fixture::custom_fixture(DownloadMode::NoProgress);
    let prepared = fixture.prepare();
    fixture
        .service()
        .submit(&prepared, "incoming", true)
        .unwrap();
    fixture.confirm_add(&prepared);
    fixture.fake.state.lock().unwrap().torrents[0]["state"] = "checking".into();
    let record = fixture.service().advance_setups().unwrap().remove(0);
    assert_eq!(record.mode_setup.unwrap().state, SetupState::AwaitingAdd);
    assert!(fixture.mutations().is_empty());
    fixture.fake.state.lock().unwrap().torrents[0]["state"] = "paused".into();
    fixture.service().advance_setups().unwrap();
    fixture.confirm_mode(fixture.config.peerlume.download_mode);
    fixture.fake.state.lock().unwrap().torrents[0]["state"] = "checking".into();
    let record = fixture.service().advance_setups().unwrap().remove(0);
    assert_eq!(record.mode_setup.unwrap().state, SetupState::ApplyingMode);
    assert_eq!(fixture.mutations().len(), 1);
    fixture.fake.state.lock().unwrap().torrents[0]["state"] = "paused".into();
    fixture.service().advance_setups().unwrap();
    assert_eq!(fixture.mutations().len(), 2);
}

#[test]
fn peerlume_old_config_and_history_default_to_normal_without_setup_commands() {
    let fixture = Fixture::new();
    let record = fixture
        .service()
        .submit(&fixture.prepare(), "incoming", false)
        .unwrap();
    let mut value = serde_json::to_value(record).unwrap();
    value.as_object_mut().unwrap().remove("download_mode");
    value.as_object_mut().unwrap().remove("mode_setup");
    let old: Handoff = serde_json::from_value(value).unwrap();
    assert_eq!(old.download_mode.mode, DownloadMode::Normal);
    assert!(old.mode_setup.is_none());
    let mut config = serde_json::to_value(PeerLumeConfig::default()).unwrap();
    config.as_object_mut().unwrap().remove("download_mode");
    assert_eq!(
        serde_json::from_value::<PeerLumeConfig>(config)
            .unwrap()
            .download_mode
            .mode,
        DownloadMode::Normal
    );
}

#[test]
fn peerlume_mode_schema_completes_and_validates_all_enum_and_parameter_values() {
    let catalog = crate::scraper::extensions::settings::SettingsCatalog::default();
    let schema = crate::services::configuration::schema(&catalog).unwrap();
    let text = "{\"peerlume\":{\"download_mode\":{\"mode\":\"no_";
    let completions =
        crate::services::config_completion::complete(text, text.chars().count(), &schema);
    assert!(completions
        .iter()
        .any(|item| item.insert.contains("no_progress")));
    let original = serde_json::to_value(AppConfig::default()).unwrap();
    for mode in DownloadMode::ALL {
        let mut value = original.clone();
        value["peerlume"]["download_mode"] = serde_json::to_value(custom(mode)).unwrap();
        assert!(crate::services::configuration::validate(&value, &catalog).is_ok());
    }
    for mode in [
        json!({"mode":"invalid"}),
        json!({"mode":"freeze_after","freeze_after_bytes":0}),
        json!({"mode":"upload_ratio_1_5","upload_ratio":{"min_milli":5000,"max_milli":1000}}),
    ] {
        let mut value = original.clone();
        value["peerlume"]["download_mode"] = mode;
        assert!(crate::services::configuration::validate(&value, &catalog).is_err());
    }
}

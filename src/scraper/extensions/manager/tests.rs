use super::*;
#[test]
fn interrupted_config_and_pointer_writes_recover_without_overwriting_external_edits() {
    for committed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.json");
        std::fs::write(&config, b"new").unwrap();
        let state = LocalState {
            schema: 1,
            generation: 1,
            ..Default::default()
        };
        let journal = SwitchJournal {
            next_state: state.clone(),
            config_path: config.clone(),
            old_config: b"old".to_vec(),
            new_config: b"new".to_vec(),
        };
        std::fs::write(
            temp.path().join("switch-journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        if committed {
            std::fs::write(temp.path().join(STATE), serde_json::to_vec(&state).unwrap()).unwrap();
        }
        recover_switch_locked(temp.path()).unwrap();
        assert_eq!(
            std::fs::read(&config).unwrap(),
            if committed { b"new" } else { b"old" }
        );
        assert!(!temp.path().join("switch-journal.json").exists());
        std::fs::write(&config, b"external change").unwrap();
        std::fs::write(
            temp.path().join("switch-journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        assert!(recover_switch_locked(temp.path()).is_err());
        assert_eq!(std::fs::read(&config).unwrap(), b"external change");
    }
}

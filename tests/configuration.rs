use archive_sift::{
    app::AppConfig,
    scraper::extensions::settings::SettingsCatalog,
    services::{config_completion, configuration, SettingsService},
};
use serde_json::{json, Value};

fn catalog(root: &std::path::Path) -> SettingsCatalog {
    let plugin = root.join("example.parser");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.json"),serde_json::to_vec(&json!({"schema":1,"id":"example.parser","version":"1.0.0","api":"1.0","display_name":"Example","source_key":"example-source","runtime":"wasm","entry":"adapter.wasm","sha256":"0".repeat(64),"enabled":true,"routes":[{"host":"example.test","path_prefix":"/items/","kind":"content"}],"network_origins":["https://example.test"]})).unwrap()).unwrap();
    let schema = json!({"type":"object","additionalProperties":false,"x-archivesift-delivery":"state-v1","properties":{
        "quality":{"type":"string","enum":["preview","original"],"default":"original"},
        "max_images":{"type":"integer","minimum":1,"maximum":10,"default":5},
        "api_key_ref":{"type":"string","format":"secret-reference","const":"ArchiveSift/example.parser/api","default":"ArchiveSift/example.parser/api"},
        "nested":{"type":"object","properties":{"enabled":{"type":"boolean","default":true}},"default":{},"additionalProperties":false}
    },"required":["quality","max_images","api_key_ref"]});
    std::fs::write(
        plugin.join("settings.schema.json"),
        serde_json::to_vec(&schema).unwrap(),
    )
    .unwrap();
    let catalog = SettingsCatalog::discover(root);
    assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
    catalog
}

#[test]
fn full_document_describes_every_serialized_setting_and_materializes_plugin_defaults() {
    let root = tempfile::tempdir().unwrap();
    let catalog = catalog(root.path());
    let config = AppConfig::default();
    let document = configuration::document(&config, &catalog).unwrap();
    let schema = configuration::schema(&catalog).unwrap();
    assert!(configuration::validate(&document, &catalog).is_ok());
    for key in serde_json::to_value(config)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
    {
        assert!(
            schema.value()["properties"].get(key).is_some(),
            "missing {key}"
        );
    }
    assert_eq!(
        document["extension_settings"]["example.parser"]["max_images"],
        5
    );
    assert_eq!(
        document["extension_settings"]["example.parser"]["nested"]["enabled"],
        true
    );
    assert!(schema
        .at_path(&[
            "extension_settings".into(),
            "example.parser".into(),
            "quality".into()
        ])
        .is_some());
}

#[test]
fn plugin_sections_reject_unknown_required_type_enum_and_range_errors_with_paths() {
    let root = tempfile::tempdir().unwrap();
    let catalog = catalog(root.path());
    let original = configuration::document(&AppConfig::default(), &catalog).unwrap();
    for (key, value) in [
        ("quality", json!("bad")),
        ("max_images", json!(0)),
        ("max_images", json!("five")),
        ("max_image", json!(5)),
        ("api_key_ref", json!("fixture-sensitive-material")),
    ] {
        let mut document = original.clone();
        document["extension_settings"]["example.parser"][key] = value;
        let issues = configuration::validate(&document, &catalog).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| i.path == format!("/extension_settings/example.parser/{key}")));
        assert!(!format!("{issues:?}").contains("fixture-sensitive-material"));
    }
    let mut document = original;
    document["extension_settings"]["example.parser"]
        .as_object_mut()
        .unwrap()
        .remove("quality");
    assert!(configuration::validate(&document, &catalog)
        .unwrap_err()
        .iter()
        .any(|i| i.path.ends_with("/quality") && i.message.contains("required")));
}

#[test]
fn application_rules_reject_inconsistent_limits_and_paths_before_any_save() {
    let catalog = SettingsCatalog::default();
    let original = configuration::document(&AppConfig::default(), &catalog).unwrap();
    for (pointer, value) in [
        ("/max_parallel_downloads", json!(0)),
        ("/search_columns", json!(11)),
        ("/request_delay_max_ms", json!(1)),
        ("/max_concurrent_downloads", json!(32)),
        ("/subfolder", json!("../outside")),
        ("/subfolder", json!("C:\\outside")),
        ("/thumbnail_folder", json!("")),
        ("/download_folder", json!("X:\rimages")),
        ("/input_mode", json!("unknown")),
        ("/scrape_layout/compact_breakpoint", json!(1)),
        ("/unknown", json!(true)),
    ] {
        let mut document = original.clone();
        if let Some(slot) = document.pointer_mut(pointer) {
            *slot = value;
        } else {
            document
                .as_object_mut()
                .unwrap()
                .insert(pointer[1..].into(), value);
        }
        assert!(
            configuration::validate(&document, &catalog).is_err(),
            "{pointer}"
        );
    }
}

#[test]
fn duplicates_malformed_json_and_oversized_documents_are_rejected() {
    for text in [
        "{\"x\":1,\"x\":2}",
        "{\"outer\":{\"x\":1,\"x\":2}}",
        "{\"x\":}",
        "{\"x\":NaN}",
    ] {
        assert!(configuration::parse_json(text).is_err());
    }
    assert!(configuration::parse_json(&" ".repeat(configuration::MAX_CONFIG_BYTES + 1)).is_err());
    assert_eq!(
        configuration::parse_json("{\"имя\":[null,true,-1,2.5]}").unwrap(),
        json!({"имя":[null,true,-1,2.5]})
    );
}

#[test]
fn applying_a_full_document_preserves_all_sections_and_rejects_external_changes() {
    let root = tempfile::tempdir().unwrap();
    let catalog = catalog(&root.path().join("plugins"));
    let path = root.path().join("app-config.json");
    let current = AppConfig {
        database_path: root.path().join("old.sqlite3").display().to_string(),
        download_folder: root.path().join("old-files").display().to_string(),
        clipboard_listener_enabled: false,
        ..Default::default()
    };
    current.save_to(&path).unwrap();
    let current = AppConfig::load_from(&path).unwrap();
    let mut document = configuration::document(&current, &catalog).unwrap();
    document["preset_tags"] = json!("custom");
    document["search_columns"] = json!(7);
    document["extension_settings"]["example.parser"]["max_images"] = json!(2);
    document["extension_accounts"]["example.parser"] =
        json!({"username":"test-user","remember_password":true});
    document["download_folder"] = json!(root.path().join("new-files").display().to_string());
    document["database_path"] = json!(root.path().join("new.sqlite3").display().to_string());
    let saved = SettingsService::new()
        .apply_configuration(&current, &document, &catalog, &path)
        .unwrap();
    let reloaded = AppConfig::load_from(&path).unwrap();
    assert_eq!(reloaded.preset_tags, "custom");
    assert_eq!(reloaded.search_columns, 7);
    assert_eq!(
        reloaded.extension_settings["example.parser"]["max_images"],
        2
    );
    assert_eq!(
        reloaded.extension_accounts["example.parser"].username,
        "test-user"
    );
    assert!(std::path::Path::new(&saved.database_path).is_file());
    let before = std::fs::read(&path).unwrap();
    let mut invalid = configuration::document(&saved, &catalog).unwrap();
    invalid["extension_settings"]["example.parser"]["max_images"] = json!(0);
    assert!(SettingsService::new()
        .apply_configuration(&saved, &invalid, &catalog, &path)
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut external = reloaded.clone();
    external.search_columns = 8;
    external.save_to(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(SettingsService::new()
        .apply_configuration(&saved, &document, &catalog, &path)
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn uninstalled_plugin_sections_are_preserved_without_loading_any_code() {
    let catalog = SettingsCatalog::default();
    let mut config = AppConfig::default();
    config
        .extension_settings
        .insert("missing.parser".into(), json!({"option":true}));
    let document = configuration::document(&config, &catalog).unwrap();
    let parsed = configuration::validate(&document, &catalog).unwrap();
    assert_eq!(parsed.extension_settings, config.extension_settings);
}

fn completion(text: &str, label: &str, catalog: &SettingsCatalog) -> (String, usize) {
    let cursor = text.find('|').unwrap();
    let input = text.replacen('|', "", 1);
    let chars = text[..cursor].chars().count();
    let schema = configuration::schema(catalog).unwrap();
    let suggestions = config_completion::complete(&input, chars, &schema);
    let item = suggestions
        .iter()
        .find(|s| s.label == label)
        .unwrap_or_else(|| panic!("Missing {label} in {suggestions:?}"));
    config_completion::apply(&input, item)
}

#[test]
fn completion_follows_plugin_ids_nested_objects_enums_and_unicode_cursor_positions() {
    let root = tempfile::tempdir().unwrap();
    let catalog = catalog(root.path());
    let (text, _) = completion(
        "{\"extension_settings\":{\"example.parser\":{\"max_|}}}",
        "max_images",
        &catalog,
    );
    assert!(text.contains("\"max_images\": 5"));
    let (text, _) = completion(
        "{\"extension_settings\":{\"example.parser\":{\"quality\":\"pre|view\"}}}",
        "preview",
        &catalog,
    );
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["extension_settings"]["example.parser"]
            ["quality"],
        "preview"
    );
    let (text,_)=completion("{\"source_url\":\"ёж\",\"extension_settings\":{\"example.parser\":{\"nested\":{\"en|abled\":false}}}}","enabled",&catalog);
    assert!(text.contains("\"enabled\":false"));
    assert_eq!(text.matches("enabled").count(), 1);
    let (text, _) = completion(
        "{\"extension_settings\":{\"example.parser\":{\"nested\":{\"enabled\":f|alse}}}}",
        "false",
        &catalog,
    );
    assert!(serde_json::from_str::<Value>(&text).is_ok());
    let (text, _) = completion(
        "{\"extension_settings\":{\"example.|parser\":{}}}",
        "example.parser",
        &catalog,
    );
    assert!(serde_json::from_str::<Value>(&text).is_ok());
}

#[test]
fn completion_skips_existing_keys_and_does_not_duplicate_colons_or_values() {
    let catalog = SettingsCatalog::default();
    let schema = configuration::schema(&catalog).unwrap();
    let text = "{\"source_url\":\"\", ";
    assert!(
        !config_completion::complete(text, text.chars().count(), &schema)
            .iter()
            .any(|c| c.label == "source_url")
    );
    let (text, _) = completion("{\"search_col|umns\":7}", "search_columns", &catalog);
    assert_eq!(text, "{\"search_columns\":7}");
    let (text, _) = completion(
        "{\"preview_open_mode\":\"single_|click\"}",
        "single_click",
        &catalog,
    );
    assert_eq!(text, "{\"preview_open_mode\":\"single_click\"}");
}

#[test]
fn credential_references_are_host_only_and_secrets_are_not_serialized_or_echoed() {
    use archive_sift::secure_store::{Secret, SecretStore, StoreError};
    struct MemoryStore(std::sync::Mutex<Option<Vec<u8>>>);
    impl SecretStore for MemoryStore {
        fn get(&self, _: &str) -> Result<Option<Secret>, StoreError> {
            Ok(self.0.lock().unwrap().clone().map(Secret::new))
        }
        fn set(&self, _: &str, v: &Secret) -> Result<(), StoreError> {
            *self.0.lock().unwrap() = Some(v.expose_secret().to_vec());
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<(), StoreError> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let catalog = catalog(root.path());
    let plugin = &catalog.plugins["example.parser"];
    let values = plugin.effective(None).unwrap();
    assert!(plugin.guest_settings(&values).get("api_key_ref").is_none());
    let store = MemoryStore(std::sync::Mutex::new(None));
    let secret = Secret::new(b"fixture-sensitive-material".to_vec());
    assert!(catalog
        .store_secret("ArchiveSift/other.parser/api", &secret, &store)
        .is_err());
    assert!(store.0.lock().unwrap().is_none());
    catalog
        .store_secret("ArchiveSift/example.parser/api", &secret, &store)
        .unwrap();
    assert_eq!(
        store.0.lock().unwrap().as_deref(),
        Some(b"fixture-sensitive-material".as_slice())
    );
    let command = archive_sift::ui::UiCommand::StoreSettingSecret {
        target: "ArchiveSift/example.parser/api".into(),
        value: secret,
    };
    assert!(!format!("{command:?}").contains("fixture-sensitive-material"));
    assert!(!configuration::document(&AppConfig::default(), &catalog)
        .unwrap()
        .to_string()
        .contains("fixture-sensitive-material"));
}

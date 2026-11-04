use serde_json::json;
use std::path::Path;

fn package(root: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_archive-sift-plugin-packager"))
        .args(["process"])
        .arg(root.join("input.bin"))
        .arg(root.join("package"))
        .arg("--manifest")
        .arg(root.join("plugin.template.json"))
        .output()
        .unwrap()
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("input.bin"), b"test data, never executed").unwrap();
    std::fs::write(root.path().join("plugin.template.json"),serde_json::to_vec(&json!({"schema":1,"api":"1.0","id":"example.parser","version":"1.0.0","display_name":"Example","source_key":"example-source","routes":[{"host":"example.test","path_prefix":"/items/","kind":"content"}],"network_origins":["https://example.test"]})).unwrap()).unwrap();
    root
}
#[test]
fn declarative_settings_are_packaged_and_removed_when_the_new_template_has_none() {
    let root = fixture();
    let path = root.path().join("settings.schema.json");
    let schema = json!({"type":"object","x-archivesift-delivery":"state-v1","properties":{"limit":{"type":"integer","minimum":1,"default":1}},"additionalProperties":false});
    std::fs::write(&path, serde_json::to_vec(&schema).unwrap()).unwrap();
    let output = package(root.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(root.path().join("package/settings.schema.json")).unwrap()
        )
        .unwrap(),
        schema
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("package/plugin.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["enabled"], false);
    assert_eq!(manifest["trusted"], false);
    std::fs::remove_file(path).unwrap();
    assert!(package(root.path()).status.success());
    assert!(!root.path().join("package/settings.schema.json").exists());
}
#[test]
fn invalid_schemas_are_rejected_before_output_files_are_created() {
    for schema in [
        json!({"type":"object","$ref":"https://example.test/schema"}),
        json!({"type":"object","properties":{"api_key":{"type":"string"}}}),
        json!({"type":"object","properties":{"key_ref":{"type":"string","format":"secret-reference","const":"ArchiveSift/other.parser/api"}}}),
        json!({"type":"object","description":"x".repeat(65536)}),
    ] {
        let root = fixture();
        std::fs::write(
            root.path().join("settings.schema.json"),
            serde_json::to_vec(&schema).unwrap(),
        )
        .unwrap();
        assert!(!package(root.path()).status.success());
        assert!(!root.path().join("package").exists());
    }
}

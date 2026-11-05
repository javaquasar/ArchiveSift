//! Real child-process contract tests; the fixture cannot start a P2P engine.
use archive_sift::{
    secure_store::Secret,
    services::peerlume::{
        client::{CliTransport, ControlTransport},
        Error, PeerLumeConfig,
    },
};
use serde_json::json;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn executable() -> &'static Path {
    static BINARY: OnceLock<(tempfile::TempDir, std::path::PathBuf)> = OnceLock::new();
    &BINARY
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(if cfg!(windows) {
                "peerlume-fixture.exe"
            } else {
                "peerlume-fixture"
            });
            let status = std::process::Command::new(
                std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()),
            )
            .args(["--edition=2021", "--crate-name", "peerlume_control_fixture"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fake_peerlume_control.rs"),
            )
            .arg("-o")
            .arg(&path)
            .status()
            .unwrap();
            assert!(status.success(), "Could not compile the native CLI fixture");
            (directory, path)
        })
        .1
}
fn fixture(mode: &str) -> (tempfile::TempDir, CliTransport) {
    let profile = tempfile::tempdir().unwrap();
    std::fs::write(profile.path().join("mode"), mode).unwrap();
    let config = PeerLumeConfig {
        enabled: true,
        executable: executable().display().to_string(),
        profile: profile.path().display().to_string(),
        ..Default::default()
    };
    let client = CliTransport::new(
        &config,
        Secret::new(b"synthetic-control-credential".to_vec()),
        Duration::from_secs(2),
    )
    .unwrap();
    (profile, client)
}

#[test]
fn peerlume_cli_uses_json_stdin_child_only_credentials_and_pinned_instance() {
    let (_profile, client) = fixture("echo");
    let reply = client
        .request(
            Some("fixture-owner"),
            json!({"name":"preview","args":{"source":"archivesift/test.torrent"}}),
        )
        .unwrap();
    let (_, value) = reply.value(Some("fixture-owner")).unwrap();
    assert_eq!(value["instance_id"], "fixture-owner");
    assert_eq!(value["method"]["name"], "preview");
    assert!(
        std::env::var("PEERLUME_CONTROL_TOKEN").ok().as_deref()
            != Some("synthetic-control-credential")
    );
    assert_eq!(
        client
            .request(None, json!({"name":"discover"}))
            .unwrap()
            .value(Some("other-owner"))
            .err(),
        Some(Error::OwnerChanged)
    );
}

#[test]
fn peerlume_cli_maps_permission_error_without_exposing_child_output() {
    let (_profile, client) = fixture("denied");
    let error = client
        .request(None, json!({"name":"discover"}))
        .unwrap()
        .value(None)
        .unwrap_err();
    assert_eq!(error, Error::PermissionDenied);
    assert!(!error.to_string().contains("synthetic-control"));
}

#[test]
fn peerlume_cli_bounds_stdout_stderr_and_rejects_invalid_json() {
    for (mode, expected) in [
        ("stdout_limit", Error::ResponseTooLarge),
        ("stderr_limit", Error::ResponseTooLarge),
        ("invalid", Error::Protocol),
    ] {
        let (_profile, client) = fixture(mode);
        let error = client
            .request(None, json!({"name":"discover"}))
            .err()
            .unwrap();
        assert_eq!(error, expected, "{mode}");
        assert!(!error.to_string().contains("synthetic-control"));
    }
}

#[test]
fn peerlume_cli_timeout_terminates_and_reaps_the_child() {
    let (profile, client) = fixture("sleep");
    let started = Instant::now();
    assert_eq!(
        client.request(None, json!({"name":"discover"})).err(),
        Some(Error::Timeout)
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    // In particular, no live child keeps its current directory locked on Windows.
    profile.close().unwrap();
}

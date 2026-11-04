#![cfg(windows)]

use archive_sift::secure_store::{Secret, SecretStore, SystemSecretStore};

struct TestCredential(String);
impl Drop for TestCredential {
    fn drop(&mut self) {
        let _ = SystemSecretStore.delete(&self.0);
    }
}

#[test]
#[ignore = "Requires an interactive Windows user credential store"]
fn windows_password_store_roundtrip_uses_only_an_isolated_test_target() {
    let target = format!(
        "ArchiveSift/demo.account/test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let store = SystemSecretStore;
    assert!(store.get(&target).unwrap().is_none());
    let _cleanup = TestCredential(target.clone());
    let result = (|| {
        store.set(&target, &Secret::new(b"fixture-password".to_vec()))?;
        let loaded = store.get(&target)?.expect("test credential exists");
        assert_eq!(loaded.expose_secret(), b"fixture-password");
        assert_eq!(format!("{loaded:?}"), "Secret([redacted])");
        // Replacing an existing entry exercises the same operation as changing a password.
        store.set(&target, &Secret::new(b"updated-fixture-password".to_vec()))?;
        assert_eq!(
            store.get(&target)?.unwrap().expose_secret(),
            b"updated-fixture-password"
        );
        Ok::<_, archive_sift::secure_store::StoreError>(())
    })();
    let cleanup = store.delete(&target);
    cleanup.unwrap();
    result.unwrap();
    assert!(store.get(&target).unwrap().is_none());
    store.delete(&target).unwrap();
}

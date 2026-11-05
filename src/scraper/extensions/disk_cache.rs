//! Only this host's authenticated serialized components may reach deserialize.
//! Publisher packages contain WebAssembly, never accepted native cache artifacts.
use super::manifest::ModuleBytes;
use crate::secure_store::Secret;
#[cfg(not(test))]
use crate::secure_store::{SecretStore, SystemSecretStore};
use ring::hmac;
use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};
use wasmtime::{component::Component, Engine};

const MAX_ARTIFACT: usize = 128 * 1024 * 1024;
fn root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|p| p.join("ArchiveSift").join("wasm-cache"))
}
#[cfg(not(test))]
fn key() -> Option<&'static hmac::Key> {
    use std::sync::OnceLock;
    static KEY: OnceLock<Option<hmac::Key>> = OnceLock::new();
    KEY.get_or_init(|| {
        let target = crate::secure_store::host_secret_target("wasm-cache-hmac-v1");
        let secret = match SystemSecretStore.get(&target).ok()? {
            Some(s) => s,
            None => {
                let mut bytes = [0; 32];
                getrandom::fill(&mut bytes).ok()?;
                let s = encode_key(&bytes);
                SystemSecretStore.set(&target, &s).ok()?;
                s
            }
        };
        let bytes = decode_key(secret.expose_secret())?;
        Some(hmac::Key::new(hmac::HMAC_SHA256, &bytes))
    })
    .as_ref()
}
#[cfg(test)]
fn key() -> Option<&'static hmac::Key> {
    None
}

// secret-tool transports text, so random bytes must not be stored as a line.
fn encode_key(bytes: &[u8; 32]) -> Secret {
    Secret::new(archive_sift_extension_package::hex(bytes).into_bytes())
}
fn decode_key(bytes: &[u8]) -> Option<zeroize::Zeroizing<Vec<u8>>> {
    let decoded = match bytes.len() {
        64 => archive_sift_extension_package::unhex(std::str::from_utf8(bytes).ok()?).ok()?,
        // Accept earlier Windows binary credentials without weakening authentication.
        32 => bytes.to_vec(),
        _ => return None,
    };
    (decoded.len() == 32).then(|| zeroize::Zeroizing::new(decoded))
}

fn identity(engine: &Engine, module: &ModuleBytes) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hash);
    format!(
        "v1-{}-{:016x}-{}",
        env!("CARGO_PKG_VERSION"),
        hash.finish(),
        archive_sift_extension_package::hex(&module.digest())
    )
}
fn read_authenticated(path: &Path, key: &hmac::Key, identity: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = vec![];
    std::fs::File::open(path)
        .ok()?
        .take(MAX_ARTIFACT as u64 + 33)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() < 32 || bytes.len() > MAX_ARTIFACT + 32 {
        return None;
    }
    let (tag, payload) = bytes.split_at(32);
    let mut context = hmac::Context::with_key(key);
    context.update(identity.as_bytes());
    context.update(payload);
    let expected = context.sign();
    if subtle::ConstantTimeEq::ct_eq(expected.as_ref(), tag).unwrap_u8() != 1 {
        return None;
    }
    Some(payload.to_vec())
}
pub(super) fn compile(engine: &Engine, module: &ModuleBytes) -> Result<Component, wasmtime::Error> {
    let disk = root().zip(key());
    Ok(compile_at(
        engine,
        module,
        disk.as_ref().map(|(root, key)| (root.as_path(), *key)),
    )?
    .0)
}
fn compile_at(
    engine: &Engine,
    module: &ModuleBytes,
    disk: Option<(&Path, &hmac::Key)>,
) -> Result<(Component, bool), wasmtime::Error> {
    let identity = identity(engine, module);
    if let Some((root, key)) = disk {
        if let Some(bytes) =
            read_authenticated(&root.join(format!("{identity}.cwasm")), key, &identity)
        {
            // SAFETY: HMAC is checked over these exact owned bytes plus the engine,
            // host and module identity. The secret key is OS-owned, never in the
            // cache or a package; only Component::serialize output is authenticated.
            if let Ok(component) = unsafe { Component::deserialize(engine, &bytes) } {
                return Ok((component, true));
            }
        }
    }
    let component = Component::new(engine, module.bytes())?;
    if let Some((root, key)) = disk {
        if let Ok(bytes) = component.serialize() {
            if bytes.len() <= MAX_ARTIFACT {
                let mut context = hmac::Context::with_key(key);
                context.update(identity.as_bytes());
                context.update(&bytes);
                let mut output = context.sign().as_ref().to_vec();
                output.extend(bytes);
                let _ = crate::persistence::atomic_write(
                    &root.join(format!("{identity}.cwasm")),
                    &output,
                );
                prune(&root);
            }
        }
    }
    Ok((component, false))
}
fn prune(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|e| e == "cwasm"))
        .filter_map(|e| {
            Some((
                e.metadata().ok()?.modified().ok()?,
                e.path(),
                e.metadata().ok()?.len(),
            ))
        })
        .collect::<Vec<_>>();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut total = 0;
    for (index, (_, path, size)) in files.into_iter().enumerate() {
        total += size;
        if index >= 8 || total > 256 * 1024 * 1024 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_keys_survive_text_secret_stores_and_legacy_binary_credentials() {
        let mut raw = [0u8; 32];
        raw[1] = b'\n';
        raw[2] = b'\r';
        raw[3] = 255;
        let stored = encode_key(&raw);
        assert_eq!(stored.expose_secret().len(), 64);
        assert!(stored.expose_secret().is_ascii());
        assert!(!stored.expose_secret().contains(&b'\n'));
        assert_eq!(decode_key(stored.expose_secret()).unwrap().as_slice(), raw);
        assert_eq!(decode_key(&raw).unwrap().as_slice(), raw);
        assert!(decode_key(&[b'z'; 64]).is_none());
        assert!(decode_key(b"short").is_none());
    }
    #[test]
    fn serialized_components_survive_a_new_engine_and_corruption_recompiles() {
        let temp = tempfile::tempdir().unwrap();
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"isolated host key");
        let module = ModuleBytes::new(wat::parse_str("(component)").unwrap());
        let engine = Engine::default();
        assert!(
            !compile_at(&engine, &module, Some((temp.path(), &key)))
                .unwrap()
                .1
        );
        let second = Engine::default();
        assert!(
            compile_at(&second, &module, Some((temp.path(), &key)))
                .unwrap()
                .1
        );
        let path = temp
            .path()
            .join(format!("{}.cwasm", identity(&second, &module)));
        std::fs::write(&path, b"publisher supplied native bytes").unwrap();
        assert!(
            !compile_at(&second, &module, Some((temp.path(), &key)))
                .unwrap()
                .1
        );
        assert!(
            compile_at(&second, &module, Some((temp.path(), &key)))
                .unwrap()
                .1
        );
        let mut different = wasmtime::Config::new();
        different.consume_fuel(true);
        let different = Engine::new(&different).unwrap();
        assert_ne!(identity(&different, &module), identity(&engine, &module));
        assert!(
            !compile_at(&different, &module, Some((temp.path(), &key)))
                .unwrap()
                .1
        );
        assert!(!compile_at(&engine, &module, None).unwrap().1);
    }
    #[test]
    fn authentication_binds_host_engine_module_and_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cache");
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"test secret");
        let mut ctx = hmac::Context::with_key(&key);
        ctx.update(b"identity");
        ctx.update(b"artifact");
        let mut bytes = ctx.sign().as_ref().to_vec();
        bytes.extend(b"artifact");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            read_authenticated(&path, &key, "identity").unwrap(),
            b"artifact"
        );
        assert!(read_authenticated(&path, &key, "different engine or module").is_none());
        bytes[33] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(read_authenticated(&path, &key, "identity").is_none());
    }
}

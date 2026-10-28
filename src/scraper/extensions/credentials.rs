//! Local host-owned credential bindings. No secret is exposed to a WASM guest.
use std::path::{Path, PathBuf};

use reqwest::{header, Url};
use serde::Deserialize;

use super::manifest::PluginManifest;
use crate::AppResult;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    plugin_id: String,
    origin: String,
    path: String,
    header: String,
    target: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    bindings: Vec<Binding>,
}

#[derive(Default)]
pub(super) struct Credentials(Vec<Binding>);

fn config_path() -> Option<PathBuf> {
    std::env::var_os("ARCHIVESIFT_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("APPDATA")
                .map(|p| PathBuf::from(p).join("ArchiveSift/credentials.json"))
        })
}

impl Credentials {
    pub fn load(manifest: &PluginManifest) -> AppResult<Self> {
        match config_path() {
            Some(path) => Self::from_file(manifest, &path),
            None => Ok(Self::default()),
        }
    }

    fn from_file(manifest: &PluginManifest, path: &Path) -> AppResult<Self> {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(_) => return Err("cannot read local credential bindings".into()),
        };
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            return Err("local credential bindings exceed size limit".into());
        }
        Self::parse(manifest, &bytes)
    }

    fn parse(manifest: &PluginManifest, bytes: &[u8]) -> AppResult<Self> {
        let config: Configuration =
            serde_json::from_slice(bytes).map_err(|_| "invalid local credential bindings")?;
        if config.bindings.len() > 128 {
            return Err("too many local credential bindings".into());
        }
        let mut selected = Vec::<Binding>::new();
        for binding in config
            .bindings
            .into_iter()
            .filter(|b| b.plugin_id == manifest.id)
        {
            let url = manifest.allowed_url(&format!("{}{}", binding.origin, binding.path))?;
            let name = header::HeaderName::from_bytes(binding.header.as_bytes())
                .map_err(|_| "invalid credential header name")?;
            if url.scheme() != "https"
                || url.origin().ascii_serialization() != binding.origin
                || url.path() != binding.path
                || url.query().is_some()
                || url.fragment().is_some()
                || !binding.path.starts_with('/')
                || matches!(
                    name.as_str(),
                    "host"
                        | "content-length"
                        | "connection"
                        | "transfer-encoding"
                        | "proxy-authorization"
                        | "upgrade"
                )
                || !binding
                    .target
                    .starts_with(&format!("ArchiveSift/{}/", manifest.id))
                || binding.target.len() > 256
                || selected.iter().any(|b| {
                    b.origin == binding.origin
                        && b.path == binding.path
                        && b.header.eq_ignore_ascii_case(&binding.header)
                })
            {
                return Err("invalid or conflicting credential binding".into());
            }
            selected.push(binding);
        }
        Ok(Self(selected))
    }

    pub fn headers(&self, url: &Url) -> AppResult<header::HeaderMap> {
        self.headers_with(url, read_secret)
    }

    fn headers_with(
        &self,
        url: &Url,
        read: impl Fn(&str) -> AppResult<Vec<u8>>,
    ) -> AppResult<header::HeaderMap> {
        let mut headers = header::HeaderMap::new();
        for binding in &self.0 {
            if url.origin().ascii_serialization() != binding.origin || url.path() != binding.path {
                continue;
            }
            let mut secret = read(&binding.target)?;
            let value = header::HeaderValue::from_bytes(&secret);
            secret.fill(0);
            let mut value = value.map_err(|_| "stored credential is not a valid HTTP header")?;
            value.set_sensitive(true);
            headers.insert(
                header::HeaderName::from_bytes(binding.header.as_bytes())?,
                value,
            );
        }
        Ok(headers)
    }
}

#[cfg(windows)]
fn read_secret(target: &str) -> AppResult<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };
    let target = target.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
    // CredReadW owns the returned allocation until CredFree. Copy only its blob.
    unsafe {
        CredReadW(
            PCWSTR(target.as_ptr()),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
        .map_err(|_| "required extension credential is missing from Windows Credential Manager")?;
        let entry = &*credential;
        let result = if entry.CredentialBlobSize == 0 || entry.CredentialBlobSize > 8192 {
            Err("stored extension credential has an invalid size".into())
        } else {
            Ok(
                std::slice::from_raw_parts(entry.CredentialBlob, entry.CredentialBlobSize as usize)
                    .to_vec(),
            )
        };
        CredFree(credential.cast());
        result
    }
}

#[cfg(not(windows))]
fn read_secret(_: &str) -> AppResult<Vec<u8>> {
    Err("this credential provider requires Windows Credential Manager".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration() -> serde_json::Value {
        serde_json::json!({"bindings":[{"plugin_id":"example.parser","origin":"https://example.test","path":"/api","header":"ApiKey","target":"ArchiveSift/example.parser/api"}]})
    }

    #[test]
    fn secrets_are_attached_only_to_the_bound_origin_and_exact_path() {
        let credentials = Credentials::parse(
            &super::super::tests::manifest(),
            &serde_json::to_vec(&configuration()).unwrap(),
        )
        .unwrap();
        let read = |target: &str| {
            assert_eq!(target, "ArchiveSift/example.parser/api");
            Ok(b"test-value".to_vec())
        };
        let headers = credentials
            .headers_with(&Url::parse("https://example.test/api?q=1").unwrap(), read)
            .unwrap();
        assert_eq!(headers["ApiKey"], "test-value");
        assert!(headers["ApiKey"].is_sensitive());
        for url in [
            "https://example.test/api/other",
            "https://example.test/other",
            "https://other.test/api",
            "http://example.test/api",
            "https://example.test:8443/api",
        ] {
            assert!(credentials
                .headers_with(&Url::parse(url).unwrap(), |_| panic!(
                    "secret must not be read"
                ))
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn rejects_ungranted_origins_unsafe_targets_headers_duplicates_and_embedded_secrets() {
        for (field, value) in [
            ("origin", "https://other.test"),
            ("origin", "http://example.test"),
            ("path", "/api?x=1"),
            ("path", "/api#fragment"),
            ("target", "OtherApp/secret"),
            ("header", "Host"),
            ("header", "bad\r\nname"),
        ] {
            let mut config = configuration();
            config["bindings"][0][field] = value.into();
            assert!(Credentials::parse(
                &super::super::tests::manifest(),
                &serde_json::to_vec(&config).unwrap()
            )
            .is_err());
        }
        let mut config = configuration();
        let duplicate = config["bindings"][0].clone();
        config["bindings"].as_array_mut().unwrap().push(duplicate);
        assert!(Credentials::parse(
            &super::super::tests::manifest(),
            &serde_json::to_vec(&config).unwrap()
        )
        .is_err());
        let mut config = configuration();
        config["bindings"][0]["secret"] = "never-store-here".into();
        assert!(Credentials::parse(
            &super::super::tests::manifest(),
            &serde_json::to_vec(&config).unwrap()
        )
        .is_err());
    }

    #[test]
    fn malformed_secret_errors_do_not_include_the_value() {
        let credentials = Credentials::parse(
            &super::super::tests::manifest(),
            &serde_json::to_vec(&configuration()).unwrap(),
        )
        .unwrap();
        let error = credentials
            .headers_with(&Url::parse("https://example.test/api").unwrap(), |_| {
                Ok(b"private\nvalue".to_vec())
            })
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private"));
        assert!(!error.contains("value"));
    }
}

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
    pub fn include_account(&mut self, account: &super::accounts::AccountProvider) -> AppResult<()> {
        for path in &account.form.session_paths {
            if let Some(binding) = self.0.iter().find(|binding| {
                binding.origin == account.origin()
                    && binding.path == *path
                    && binding.header.eq_ignore_ascii_case("Cookie")
            }) {
                if binding.target != account.session_target() {
                    return Err("account session conflicts with a local credential binding".into());
                }
                continue;
            }
            self.0.push(Binding {
                plugin_id: account.id.clone(),
                origin: account.origin(),
                path: path.clone(),
                header: "Cookie".into(),
                target: account.session_target(),
            });
        }
        Ok(())
    }
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

fn read_secret(target: &str) -> AppResult<Vec<u8>> {
    use crate::secure_store::{SecretStore, SystemSecretStore};
    let secret = SystemSecretStore
        .get(target)?
        .ok_or("required extension credential is missing from the system password store")?;
    Ok(secret.bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_sessions_are_bound_to_declared_exact_paths() {
        let directory = tempfile::tempdir().unwrap();
        let mut manifest = crate::scraper::extensions::tests::manifest();
        manifest.network_origins = vec!["https://example.test".into()];
        std::fs::write(
            directory.path().join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let profile = serde_json::json!({"schema":1,"login_url":"https://example.test/login","check_url":"https://example.test/home","form_selector":"form","username_field":"username","password_field":"password","session_cookie":"session","session_paths":["/download"],"success_selector":"#logout"});
        std::fs::write(
            directory.path().join("account.json"),
            serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
        let account = super::super::accounts::AccountProvider::from_directory(directory.path())
            .unwrap()
            .unwrap();
        let mut credentials = Credentials::default();
        credentials.include_account(&account).unwrap();
        credentials.include_account(&account).unwrap();
        assert_eq!(credentials.0.len(), 1);
        let headers = credentials
            .headers_with(
                &Url::parse("https://example.test/download?id=42").unwrap(),
                |target| {
                    assert_eq!(target, account.session_target());
                    Ok(b"session=fixture-session".to_vec())
                },
            )
            .unwrap();
        assert_eq!(headers.get("Cookie").unwrap(), "session=fixture-session");
        assert!(headers.get("Cookie").unwrap().is_sensitive());
        for url in [
            "https://example.test/download/other",
            "https://example.test/login",
            "https://cdn.example.test/download",
            "http://example.test/download",
        ] {
            assert!(credentials
                .headers_with(&Url::parse(url).unwrap(), |_| panic!(
                    "out-of-scope request read a secret"
                ))
                .unwrap()
                .is_empty());
        }
        credentials.0[0].target = format!("ArchiveSift/{}/other", account.id);
        assert!(credentials.include_account(&account).is_err());
    }

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

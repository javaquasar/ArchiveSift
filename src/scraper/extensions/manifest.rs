use std::path::{Component, Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::models::{PageKind, ScraperCapabilities};
use crate::AppResult;

pub const MAX_MODULE_BYTES: u64 = 32 * 1024 * 1024;

/// Keeps the cache key paired with the exact immutable bytes that were hashed.
pub(super) struct ModuleBytes {
    bytes: Vec<u8>,
    digest: sha2::digest::Output<Sha256>,
}

impl ModuleBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        let digest = Sha256::digest(&bytes);
        Self { bytes, digest }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest.into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Wasm,
    Process,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    Content,
    Listing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Exact hostname; wildcards and suffix matching are intentionally absent.
    pub host: String,
    pub path_prefix: String,
    pub kind: RouteKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub timeout_ms: u64,
    pub memory_mb: usize,
    pub fuel: u64,
    pub max_steps: usize,
    pub max_response_bytes: usize,
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            memory_mb: 128,
            fuel: 50_000_000,
            max_steps: 16,
            max_response_bytes: 2 * 1024 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub schema: u32,
    pub id: String,
    pub version: String,
    pub api: String,
    pub display_name: String,
    pub source_key: String,
    pub runtime: RuntimeKind,
    pub entry: String,
    pub sha256: String,
    #[serde(default)]
    pub enabled: bool,
    /// Enabling native code requires an additional explicit local trust flag.
    #[serde(default)]
    pub trusted: bool,
    pub routes: Vec<Route>,
    /// Exact origins, including scheme and non-default port.
    pub network_origins: Vec<String>,
    #[serde(default)]
    pub allow_http: bool,
    #[serde(default)]
    pub limits: Limits,
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !matches!(
            value.split('.').next().unwrap_or_default(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_.".contains(&c))
}

impl PluginManifest {
    pub fn validate(&self) -> AppResult<()> {
        if self.schema != 1 || self.api != "1.0" {
            return Err("unsupported plugin manifest schema or scraper API".into());
        }
        semver::Version::parse(&self.version)?;
        if !safe_id(&self.id) || !safe_id(&self.source_key) || self.display_name.trim().is_empty() {
            return Err("invalid plugin identity".into());
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("plugin SHA-256 must contain 64 hexadecimal digits".into());
        }
        let entry = Path::new(&self.entry);
        if entry.components().next().is_none()
            || entry
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || self.entry.contains(':')
        {
            return Err("plugin entry must stay inside its package directory".into());
        }
        if self.routes.is_empty() || self.routes.len() > 64 || self.network_origins.len() > 64 {
            return Err("invalid number of routes or network origins".into());
        }
        for route in &self.routes {
            let url = Url::parse(&format!("https://{}/", route.host))?;
            if url.host_str() != Some(route.host.as_str())
                || url.port().is_some()
                || !url.username().is_empty()
                || route.host.contains(['/', '*', '@'])
                || !route.path_prefix.starts_with('/')
                || route.path_prefix.contains(['?', '#'])
            {
                return Err(
                    "routes require an exact lowercase host and absolute path prefix".into(),
                );
            }
        }
        for origin in &self.network_origins {
            let url = Url::parse(origin)?;
            self.check_url_shape(&url)?;
            if url.origin().ascii_serialization() != *origin {
                return Err(
                    "network permission must be a canonical origin without a trailing slash".into(),
                );
            }
        }
        let limits = &self.limits;
        if !(10..=120_000).contains(&limits.timeout_ms)
            || !(8..=256).contains(&limits.memory_mb)
            || !(1..=500_000_000).contains(&limits.fuel)
            || !(1..=64).contains(&limits.max_steps)
            || !(1..=4 * 1024 * 1024).contains(&limits.max_response_bytes)
            || !(1..=8 * 1024 * 1024).contains(&limits.max_output_bytes)
        {
            return Err("plugin resource limits exceed host bounds".into());
        }
        Ok(())
    }

    fn check_url_shape(&self, url: &Url) -> AppResult<()> {
        if (url.scheme() != "https" && !(self.allow_http && url.scheme() == "http"))
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(
                "plugin URL must use an allowed HTTP scheme and no embedded credentials".into(),
            );
        }
        Ok(())
    }

    pub fn allowed_url(&self, value: &str) -> AppResult<Url> {
        let url = Url::parse(value)?;
        self.check_url_shape(&url)?;
        if !self
            .network_origins
            .contains(&url.origin().ascii_serialization())
        {
            return Err(format!(
                "plugin {} has no permission for origin {}",
                self.id,
                url.origin().ascii_serialization()
            )
            .into());
        }
        Ok(url)
    }

    pub fn classify(&self, value: &str) -> PageKind {
        let Ok(url) = self.allowed_url(value) else {
            return PageKind::Unknown;
        };
        self.routes
            .iter()
            .filter(|route| {
                url.host_str() == Some(route.host.as_str())
                    && path_matches(url.path(), &route.path_prefix)
            })
            .max_by_key(|route| route.path_prefix.len())
            .map(|route| match route.kind {
                RouteKind::Content => PageKind::Content,
                RouteKind::Listing => PageKind::Listing,
            })
            .unwrap_or(PageKind::Unknown)
    }

    pub fn capabilities(&self) -> ScraperCapabilities {
        let mut supported_page_kinds = Vec::new();
        if self
            .routes
            .iter()
            .any(|route| route.kind == RouteKind::Content)
        {
            supported_page_kinds.push(PageKind::Content);
        }
        if self
            .routes
            .iter()
            .any(|route| route.kind == RouteKind::Listing)
        {
            supported_page_kinds.push(PageKind::Listing);
        }
        ScraperCapabilities {
            supported_page_kinds,
            ..ScraperCapabilities::default()
        }
    }

    pub(super) fn verified_entry(&self, directory: &Path) -> AppResult<(PathBuf, ModuleBytes)> {
        let entry = self.checked_entry_path(directory)?;
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&entry)?
            .take(MAX_MODULE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MODULE_BYTES {
            return Err("plugin module is too large".into());
        }
        let module = ModuleBytes::new(bytes);
        if format!("{:x}", module.digest) != self.sha256.to_ascii_lowercase() {
            return Err("plugin SHA-256 mismatch".into());
        }
        Ok((entry, module))
    }

    /// Native entries only need verification, not a retained copy of the executable.
    pub(crate) fn verified_process_entry(&self, directory: &Path) -> AppResult<PathBuf> {
        use std::io::Read;
        let entry = self.checked_entry_path(directory)?;
        let file = std::fs::File::open(&entry)?;
        if file.metadata()?.len() > MAX_MODULE_BYTES {
            return Err("plugin module is too large".into());
        }
        let mut file = file.take(MAX_MODULE_BYTES + 1);
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut total = 0_u64;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_MODULE_BYTES {
                return Err("plugin module is too large".into());
            }
            hasher.update(&buffer[..count]);
        }
        if format!("{:x}", hasher.finalize()) != self.sha256.to_ascii_lowercase() {
            return Err("plugin SHA-256 mismatch".into());
        }
        Ok(entry)
    }

    fn checked_entry_path(&self, directory: &Path) -> AppResult<PathBuf> {
        let directory = directory.canonicalize()?;
        let entry = directory.join(&self.entry).canonicalize()?;
        if !entry.starts_with(&directory) || !entry.is_file() {
            return Err("plugin entry escapes its package directory".into());
        }
        Ok(entry)
    }
}

pub(crate) fn path_matches(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| prefix.ends_with('/') || suffix.starts_with('/'))
}

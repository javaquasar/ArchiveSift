//! Manifest discovery, permission enforcement, and adapters for both runtimes.
mod broker;
mod compilation_cache;
pub mod manifest;
mod process;
mod wasm;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use archive_sift_scraper_api::{
    check_json_size, Action, Content, Listing, Operation, Request, API_VERSION,
};

use crate::models::{
    ListingFetchBehavior, ListingPage, ListingPaginationBehavior, ListingUrl, MetadataAttribute,
    PageKind, ScrapedImage, ScrapedItem, ScraperCapabilities,
};
use crate::scraper::{ContentScraper, ScraperRegistry};
use crate::AppResult;
use broker::HttpBroker;
use manifest::{PluginManifest, RuntimeKind};
use process::{ProcessBackend, ProcessSession};
use wasm::WasmBackend;

#[derive(Clone, Debug)]
pub struct ExtensionStatus {
    pub directory: PathBuf,
    pub id: String,
    pub version: String,
    pub runtime: Option<RuntimeKind>,
    pub loaded: bool,
    pub message: String,
}

enum Backend {
    Wasm(Arc<WasmBackend>),
    Process(ProcessBackend),
}

pub struct ExtensionScraper {
    manifest: PluginManifest,
    backend: Backend,
    broker: HttpBroker,
}

enum Output {
    Content(ScrapedItem),
    Listing(ListingPage),
}

impl ExtensionScraper {
    pub fn load(directory: &Path) -> AppResult<Self> {
        let manifest = read_manifest(directory)?;
        Self::from_manifest(directory, manifest)
    }

    fn from_manifest(directory: &Path, manifest: PluginManifest) -> AppResult<Self> {
        manifest.validate()?;
        if !manifest.enabled {
            return Err("extension is disabled".into());
        }
        let backend = match manifest.runtime {
            RuntimeKind::Wasm => {
                let (_, bytes) = manifest.verified_entry(directory)?;
                Backend::Wasm(Arc::new(WasmBackend::new(&bytes, manifest.limits.clone())?))
            }
            RuntimeKind::Process => {
                if !manifest.trusted {
                    return Err("native process extension requires trusted=true".into());
                }
                manifest.verified_process_entry(directory)?;
                Backend::Process(ProcessBackend {
                    directory: directory.canonicalize()?,
                })
            }
        };
        let broker = HttpBroker::new(&manifest)?;
        Ok(Self {
            manifest,
            backend,
            broker,
        })
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn run(&self, operation: Operation, url: &str) -> AppResult<Output> {
        let expected = match operation {
            Operation::Content => PageKind::Content,
            Operation::Listing => PageKind::Listing,
        };
        if self.classify_url(url) != expected {
            return Err("extension does not support this operation for the URL".into());
        }
        tokio::time::timeout(
            Duration::from_millis(self.manifest.limits.timeout_ms),
            self.run_steps(operation, url),
        )
        .await
        .map_err(|_| "extension execution timed out")?
    }

    async fn run_steps(&self, operation: Operation, url: &str) -> AppResult<Output> {
        let mut process = match &self.backend {
            Backend::Process(process) => {
                Some(ProcessSession::spawn(&self.manifest, &process.directory).await?)
            }
            _ => None,
        };
        let mut input = Request {
            api_version: API_VERSION,
            operation: operation.clone(),
            url: url.to_string(),
            response: None,
            state: None,
        };
        let mut raw_html = String::new();
        let mut total_response_bytes = 0;
        let mut fetched_first_response = false;
        for _ in 0..self.manifest.limits.max_steps {
            let action = match &self.backend {
                Backend::Wasm(wasm) => wasm.execute(input).await?,
                Backend::Process(_) => process.as_mut().unwrap().execute(input).await?,
            };
            check_json_size(&action, self.manifest.limits.max_output_bytes)
                .map_err(|_| "extension result exceeds its output limit")?;
            match action {
                Action::Fetch(request) => {
                    if request
                        .state
                        .as_ref()
                        .is_some_and(|state| state.len() > 64 * 1024)
                    {
                        return Err("extension continuation state exceeds its limit".into());
                    }
                    let response = self.broker.fetch(&request).await?;
                    total_response_bytes += response.body.len();
                    if total_response_bytes > 8 * 1024 * 1024 {
                        return Err("extension exceeded total HTTP response budget".into());
                    }
                    if !fetched_first_response && operation == Operation::Content {
                        raw_html = response.body.clone();
                    }
                    fetched_first_response = true;
                    input = Request {
                        api_version: API_VERSION,
                        operation: operation.clone(),
                        url: url.to_string(),
                        response: Some(response),
                        state: request.state,
                    };
                }
                Action::Content(content) if operation == Operation::Content => {
                    return self
                        .convert_content(content, url, raw_html)
                        .map(Output::Content)
                }
                Action::Listing(listing) if operation == Operation::Listing => {
                    return self.convert_listing(listing, url).map(Output::Listing)
                }
                _ => return Err("extension returned the wrong result kind".into()),
            }
        }
        Err("extension exceeded its step budget".into())
    }

    fn resolve_url(&self, base: &str, value: &str) -> AppResult<String> {
        let url = reqwest::Url::parse(base)?.join(value)?;
        self.manifest.allowed_url(url.as_str())?;
        Ok(url.into())
    }

    fn convert_content(
        &self,
        content: Content,
        url: &str,
        raw_html: String,
    ) -> AppResult<ScrapedItem> {
        if content.title.trim().is_empty()
            || content.title.len() > 4096
            || content.images.len() > 5000
            || content.attributes.len() > 1000
        {
            return Err("extension returned invalid title or too many images/attributes".into());
        }
        let images = content
            .images
            .into_iter()
            .enumerate()
            .map(|(index, image)| {
                Ok(ScrapedImage {
                    source_url: self.resolve_url(url, &image.url)?,
                    preview_url: image
                        .preview_url
                        .map(|value| self.resolve_url(url, &value))
                        .transpose()?,
                    alt_text: image.alt_text,
                    width: image.width.map(i64::from),
                    height: image.height.map(i64::from),
                    sort_order: index as i64,
                    local_file_path: None,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        let attributes = content
            .attributes
            .into_iter()
            .map(|attribute| {
                if attribute.kind.is_empty()
                    || attribute.kind.len() > 128
                    || attribute.value.len() > 128 * 1024
                {
                    return Err("extension returned invalid metadata".into());
                }
                Ok(MetadataAttribute {
                    kind: attribute.kind,
                    label: attribute.label,
                    value: attribute.value,
                    source_url: attribute
                        .source_url
                        .map(|value| self.resolve_url(url, &value))
                        .transpose()?,
                    is_female: attribute.is_female,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        Ok(ScrapedItem {
            site_key: self.manifest.source_key.clone(),
            page_url: url.to_string(),
            title: content.title,
            raw_html,
            images,
            attributes,
        })
    }

    fn convert_listing(&self, listing: Listing, base: &str) -> AppResult<ListingPage> {
        if listing.urls.len() > 5000 {
            return Err("extension returned too many listing URLs".into());
        }
        let discovered_urls = listing
            .urls
            .into_iter()
            .map(|entry| {
                let url = self.resolve_url(base, &entry.url)?;
                let expected = match entry.operation {
                    Operation::Content => PageKind::Content,
                    Operation::Listing => PageKind::Listing,
                };
                if self.classify_url(&url) != expected {
                    return Err("extension listing URL does not match its declared route".into());
                }
                Ok(match entry.operation {
                    Operation::Content => ListingUrl::content(url),
                    Operation::Listing => ListingUrl::listing(url),
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        let next_page_url = listing
            .next_page_url
            .map(|value| -> AppResult<String> {
                let url = self.resolve_url(base, &value)?;
                if self.classify_url(&url) != PageKind::Listing {
                    return Err("extension next page is not a listing route".into());
                }
                Ok(url)
            })
            .transpose()?;
        Ok(ListingPage {
            discovered_urls,
            next_page_url,
            total_pages: None,
            pagination_behavior: ListingPaginationBehavior::FollowNextPage,
            fetch_behavior: ListingFetchBehavior::StaticHtml,
            diagnostics: Vec::new(),
        })
    }
}

impl ContentScraper for ExtensionScraper {
    fn site_key(&self) -> &str {
        &self.manifest.source_key
    }
    fn supports_url(&self, url: &str) -> bool {
        self.classify_url(url) != PageKind::Unknown
    }
    fn classify_url(&self, url: &str) -> PageKind {
        self.manifest.classify(url)
    }
    fn capabilities(&self) -> ScraperCapabilities {
        self.manifest.capabilities()
    }
    fn routing_priority(&self) -> u32 {
        1
    }
    fn asset_origins(&self) -> Option<&[String]> {
        Some(&self.manifest.network_origins)
    }

    fn scrape_content<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>> {
        Box::pin(async move {
            match self.run(Operation::Content, url).await? {
                Output::Content(content) => Ok(content),
                _ => unreachable!(),
            }
        })
    }
    fn scrape_listing<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ListingPage>> + Send + 'a>> {
        Box::pin(async move {
            match self.run(Operation::Listing, url).await? {
                Output::Listing(listing) => Ok(listing),
                _ => unreachable!(),
            }
        })
    }
}

pub fn default_plugin_directory() -> PathBuf {
    if let Some(directory) = std::env::var_os("ARCHIVESIFT_PLUGIN_DIR") {
        return directory.into();
    }
    if let Some(directory) = std::env::var_os("APPDATA") {
        return PathBuf::from(directory).join("ArchiveSift/plugins");
    }
    if let Some(directory) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(directory).join("ArchiveSift/plugins");
    }
    if let Some(directory) = std::env::var_os("HOME") {
        return PathBuf::from(directory).join(".local/share/ArchiveSift/plugins");
    }
    PathBuf::from("plugins-local")
}

fn read_manifest(directory: &Path) -> AppResult<PluginManifest> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(directory.join("plugin.json"))?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err("plugin manifest exceeds 64 KiB".into());
    }
    let manifest: PluginManifest = serde_json::from_slice(&bytes)?;
    manifest.validate()?;
    Ok(manifest)
}

/// A bad package is reported without preventing the other packages from loading.
pub fn discover(registry: &mut ScraperRegistry, root: &Path) -> Vec<ExtensionStatus> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            return vec![ExtensionStatus {
                directory: root.into(),
                id: String::new(),
                version: String::new(),
                runtime: None,
                loaded: false,
                message: error.to_string(),
            }]
        }
    };
    let mut directories = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("plugin.json").exists())
        .collect::<Vec<_>>();
    directories.sort();
    let mut statuses = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let mut claimed_routes: Vec<(String, String)> = Vec::new();
    for directory in directories {
        let mut status = ExtensionStatus {
            directory: directory.clone(),
            id: String::new(),
            version: String::new(),
            runtime: None,
            loaded: false,
            message: String::new(),
        };
        let result = (|| -> AppResult<()> {
            let manifest = read_manifest(&directory)?;
            status.id = manifest.id.clone();
            status.version = manifest.version.clone();
            status.runtime = Some(manifest.runtime);
            if !manifest.enabled {
                status.message = "Disabled".into();
                return Ok(());
            }
            if ids.contains(&manifest.id)
                || registry
                    .list_site_keys()
                    .contains(&manifest.source_key.as_str())
            {
                return Err("duplicate extension ID or source key".into());
            }
            for route in &manifest.routes {
                if claimed_routes.iter().any(|(host, path)| {
                    host == &route.host
                        && (manifest::path_matches(path, &route.path_prefix)
                            || manifest::path_matches(&route.path_prefix, path))
                }) {
                    return Err("extension route conflicts with an already loaded extension".into());
                }
            }
            let extension = ExtensionScraper::from_manifest(&directory, manifest.clone())?;
            ids.insert(manifest.id);
            claimed_routes.extend(
                manifest
                    .routes
                    .into_iter()
                    .map(|route| (route.host, route.path_prefix)),
            );
            registry.register(Box::new(extension));
            status.loaded = true;
            status.message = "Loaded".into();
            Ok(())
        })();
        if let Err(error) = result {
            status.message = error.to_string();
        }
        statuses.push(status);
    }
    statuses
}

#[cfg(test)]
mod tests;

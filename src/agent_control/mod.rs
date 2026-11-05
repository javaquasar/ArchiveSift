//! Session-scoped local agent access. The stdio bridge only attaches to a live UI.
pub mod adapter;
pub mod specs;
#[cfg(test)]
mod tests;
pub mod transport;
pub mod wire;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::app::AppConfig;
use crate::db::{Database, QueryCancellation};
use crate::models::{SearchCursor, SearchQuery, TorrentQuery};
use crate::services::UrlQueueService;
use crate::ui::UiLog;
use wire::{ErrorCode, Request, Response, MAX_CONNECTIONS, PROTOCOL_VERSION};

pub use transport::AgentSession;

pub(crate) fn random_secret() -> Result<Zeroizing<String>, ErrorCode> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(bytes.as_mut()).map_err(|_| ErrorCode::OperationFailed)?;
    Ok(Zeroizing::new(
        bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    ))
}

type ActiveQueries = Arc<Mutex<BTreeMap<u64, (QueryCancellation, Instant)>>>;

pub struct AgentService {
    config: AppConfig,
    queue: Arc<UrlQueueService>,
    instance_id: String,
    token_hash: [u8; 32],
    enabled: AtomicBool,
    downloads: AtomicBool,
    processor_requested: AtomicBool,
    requests: AtomicU64,
    clock: AtomicU64,
    active: ActiveQueries,
    log: UiLog,
    wake: Arc<dyn Fn() + Send + Sync>,
}

struct ActiveRequest {
    id: u64,
    active: ActiveQueries,
}
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

impl AgentService {
    pub(crate) fn new(
        config: AppConfig,
        queue: Arc<UrlQueueService>,
        log: UiLog,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(Arc<Self>, Zeroizing<String>), ErrorCode> {
        let token = random_secret()?;
        let instance_id = random_secret()?[..32].to_owned();
        let token_hash = Sha256::digest(token.as_bytes()).into();
        let service = Arc::new(Self {
            config,
            queue,
            instance_id,
            token_hash,
            enabled: AtomicBool::new(false),
            downloads: AtomicBool::new(false),
            processor_requested: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            clock: AtomicU64::new(0),
            active: Arc::new(Mutex::new(BTreeMap::new())),
            log,
            wake,
        });
        Ok((service, token))
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
    pub fn allow_downloads(&self) -> bool {
        self.downloads.load(Ordering::SeqCst)
    }
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
    pub fn activate(&self, downloads: bool) {
        self.downloads.store(downloads, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
    }
    pub fn set_download_permission(&self, downloads: bool) {
        self.downloads.store(downloads, Ordering::SeqCst);
    }
    pub fn take_processor_request(&self) -> bool {
        self.processor_requested.swap(false, Ordering::AcqRel)
    }
    pub fn bound_to(&self, config: &AppConfig) -> bool {
        self.config.database_path == config.database_path
            && self.config.effective_download_dir() == config.effective_download_dir()
    }
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        self.downloads.store(false, Ordering::SeqCst);
        for (token, _) in self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            token.cancel();
        }
    }
    pub(crate) fn expire_queries(&self) {
        for (token, deadline) in self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            if Instant::now() >= *deadline {
                token.cancel();
            }
        }
    }

    pub fn handle(&self, request: &Request) -> Response {
        let fail = |error| Response::error(&self.instance_id, error);
        if !self.enabled() {
            return fail(ErrorCode::Disabled);
        }
        if request.protocol_version != PROTOCOL_VERSION || request.instance_id != self.instance_id {
            return fail(ErrorCode::NotRunning);
        }
        let hash: [u8; 32] = Sha256::digest(request.token.as_bytes()).into();
        if request.token.len() != 64 || !bool::from(hash.ct_eq(&self.token_hash)) {
            return fail(ErrorCode::Unauthorized);
        }
        let tool = match specs::find(&request.tool) {
            Ok(tool) => tool,
            Err(error) => return fail(error),
        };
        if let Err(error) = tool.validate(&request.arguments) {
            return fail(error);
        }
        if !tool.read_only && !self.allow_downloads() {
            return fail(ErrorCode::PermissionDenied);
        }
        let token = QueryCancellation::default();
        let id = self.clock.fetch_add(1, Ordering::Relaxed);
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if active.len() >= MAX_CONNECTIONS {
                return fail(ErrorCode::Busy);
            }
            active.insert(
                id,
                (token.clone(), Instant::now() + Duration::from_secs(15)),
            );
        }
        let _active = ActiveRequest {
            id,
            active: Arc::clone(&self.active),
        };
        let result = self.execute(tool.name, &request.arguments, &token);
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.log.push(format!(
            "Agent request: {} — {}",
            tool.name,
            if result.is_ok() {
                "completed"
            } else {
                "failed"
            }
        ));
        (self.wake)();
        if !self.enabled() {
            return fail(ErrorCode::Disabled);
        }
        match result {
            Ok(value) => Response::success(&self.instance_id, value),
            Err(error) => fail(error),
        }
    }

    fn database(&self) -> Result<Database, ErrorCode> {
        if !std::path::Path::new(&self.config.database_path).is_file() {
            return Err(ErrorCode::LibraryUnavailable);
        }
        Ok(Database::new(&self.config.database_path))
    }

    fn queue_state(&self) -> Value {
        let state = self.queue.snapshot_light();
        json!({"paused":state.paused,"pending":state.pending_total,"failed":state.failed_total,"active_count":state.in_progress.len(),
            "active":state.in_progress.iter().take(16).map(|(url,site)|json!({"url":safe_url(url),"site":clip(site,128)})).collect::<Vec<_>>(),
            "pending_preview":state.pending_preview.iter().map(|url|safe_url(url)).collect::<Vec<_>>()})
    }

    fn execute(
        &self,
        tool: &str,
        arguments: &Value,
        token: &QueryCancellation,
    ) -> Result<Value, ErrorCode> {
        let failed = |_| ErrorCode::OperationFailed;
        Ok(match tool {
            "archivesift_status" => {
                json!({"application":"ArchiveSift","version":env!("CARGO_PKG_VERSION"),"instance_id":self.instance_id,
                "grants":{"read":true,"downloads":self.allow_downloads()},"library_available":std::path::Path::new(&self.config.database_path).is_file(),
                "download_destination_configured":self.config.has_download_folder(),"queue":self.queue_state(),
                "limits":{"request_bytes":wire::MAX_REQUEST_BYTES,"response_bytes":wire::MAX_RESPONSE_BYTES,"connections":MAX_CONNECTIONS,"image_page_size":50,"torrent_page_size":40}})
            }
            "archivesift_extensions" => {
                json!({"items":self.queue.registry().extension_statuses().iter().take(128).map(|plugin|json!({"id":clip(&plugin.id,128),"version":clip(&plugin.version,64),"runtime":plugin.runtime,"loaded":plugin.loaded})).collect::<Vec<_>>()})
            }
            "archivesift_queue_status" => self.queue_state(),
            "archivesift_search_images" => {
                let args: SearchArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| ErrorCode::InvalidArguments)?;
                let query = args.image_query();
                let cursor = args.cursor.map(|cursor| SearchCursor {
                    scraped_at: cursor.scraped_at,
                    page_id: cursor.page_id,
                    sort_order: cursor.sort_order,
                    image_id: cursor.image_id,
                });
                let page = self
                    .database()?
                    .search_images_page_lightweight(
                        &query,
                        cursor.as_ref(),
                        args.limit.unwrap_or(20),
                        token,
                    )
                    .map_err(failed)?;
                json!({"items":page.items.iter().map(|image|json!({"image_id":image.image_id,"page_id":image.page_id(),"title":clip(&image.page.title,512),"page_url":safe_url(&image.page.url),"favorite":image.is_favorite,"has_local_file":image.local_file_path.is_some()})).collect::<Vec<_>>(),
                    "next_cursor":page.next_cursor.map(|cursor|json!({"scraped_at":cursor.scraped_at,"page_id":cursor.page_id,"sort_order":cursor.sort_order,"image_id":cursor.image_id}))})
            }
            "archivesift_search_torrents" => {
                let args: TorrentArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| ErrorCode::InvalidArguments)?;
                let query = TorrentQuery {
                    text: args.text,
                    site_key: args.site,
                    studio: args.studio,
                    creators: args.creators,
                    tags: args.tags,
                    categories: args.categories,
                    year: args.year,
                    offset: args.offset,
                };
                let page = self
                    .database()?
                    .search_torrent_summaries_cancellable(&query, token)
                    .map_err(failed)?;
                json!({"total":page.total,"offset":query.offset,"next_offset":(query.offset+page.cards.len()<page.total).then_some(query.offset+page.cards.len()),
                    "items":page.cards.iter().map(|card|json!({"page_id":card.page_id,"site":clip(&card.site_key,128),"title":clip(&card.content_title,512),"primary_studio":clip(&card.primary_studio,256),"image_count":card.image_count,"attachment_count":card.attachment_count})).collect::<Vec<_>>()})
            }
            "archivesift_torrent_details" => {
                let id = arguments["page_id"]
                    .as_i64()
                    .ok_or(ErrorCode::InvalidArguments)?;
                let card = self
                    .database()?
                    .torrent_card_cancellable(id, token)
                    .map_err(failed)?;
                let allowed = [
                    "actor",
                    "tag",
                    "category",
                    "studio",
                    "date",
                    "year",
                    "release_date",
                    "production_date",
                    "director",
                    "author",
                    "photographer",
                    "creator",
                    "duration",
                    "studio_code",
                    "content_title",
                ];
                json!({"page_id":card.page_id,"title":clip(&card.title,4096),"content_title":clip(card.content_title(),1024),"page_url":safe_url(&card.page_url),
                    "description":card.attributes.iter().find(|a|a.kind=="description").map(|a|clip(&a.value,32768)),
                    "full_description":card.attributes.iter().find(|a|a.kind=="full_description").map(|a|clip(&a.value,32768)),
                    "metadata":card.attributes.iter().filter(|a|allowed.contains(&a.kind.as_str())).take(128).map(|a|json!({"kind":a.kind,"value":clip(&a.value,512)})).collect::<Vec<_>>(),
                    "primary_image_id":card.primary_image().map(|image|image.image_id),"image_count":card.images.len(),"image_ids":card.images.iter().take(256).map(|image|image.image_id).collect::<Vec<_>>(),
                    "attachment_count":card.attachments.len(),"attachments":card.attachments.iter().take(32).map(|file|clip(&file.filename,256)).collect::<Vec<_>>()})
            }
            "archivesift_queue_add" => {
                if !self.config.has_download_folder() {
                    return Err(ErrorCode::DestinationNotConfigured);
                }
                let urls = arguments["urls"]
                    .as_array()
                    .ok_or(ErrorCode::InvalidArguments)?
                    .iter()
                    .map(|url| url.as_str().unwrap_or("").to_owned())
                    .collect::<Vec<_>>();
                let report = self
                    .queue
                    .enqueue_urls(&Database::new(&self.config.database_path), urls)
                    .map_err(failed)?;
                self.processor_requested.store(true, Ordering::Release);
                json!({"queued":report.queued_successfully,"duplicate_or_processed":report.duplicate_or_processed,"invalid":report.empty_or_invalid,"completed":false})
            }
            "archivesift_queue_pause" => {
                self.queue.pause();
                self.queue_state()
            }
            "archivesift_queue_resume" => {
                self.queue.resume();
                self.processor_requested.store(true, Ordering::Release);
                self.queue_state()
            }
            "archivesift_queue_retry" => {
                let count = self.queue.retry_failed();
                self.queue.resume();
                self.processor_requested.store(true, Ordering::Release);
                json!({"retried":count,"completed":false})
            }
            _ => return Err(ErrorCode::InvalidArguments),
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SearchArgs {
    text: String,
    site: String,
    tags: Vec<String>,
    actors: Vec<String>,
    creators: Vec<String>,
    studios: Vec<String>,
    categories: Vec<String>,
    channels: Vec<String>,
    favorites_only: bool,
    page_id: Option<i64>,
    limit: Option<usize>,
    cursor: Option<CursorArgs>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorArgs {
    scraped_at: String,
    page_id: i64,
    sort_order: i64,
    image_id: i64,
}
impl SearchArgs {
    fn image_query(&self) -> SearchQuery {
        SearchQuery {
            text: self.text.clone(),
            site_key: (!self.site.is_empty()).then(|| self.site.clone()),
            tags: self.tags.clone(),
            actors: self.actors.clone(),
            creators: self.creators.clone(),
            studios: self.studios.clone(),
            categories: self.categories.clone(),
            channels: self.channels.clone(),
            favorites_only: self.favorites_only,
            page_id: self.page_id,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TorrentArgs {
    text: String,
    site: String,
    studio: String,
    creators: Vec<String>,
    tags: Vec<String>,
    categories: Vec<String>,
    year: String,
    offset: usize,
}

fn clip(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.into();
    }
    let mut end = maximum.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

fn safe_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return String::new();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return String::new();
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| {
            let key = key.to_ascii_lowercase();
            ![
                "token",
                "password",
                "secret",
                "signature",
                "credential",
                "oauth",
                "bearer",
                "jwt",
                "authorization",
                "cookie",
                "session",
                "api_key",
                "apikey",
                "auth",
                "key",
            ]
            .iter()
            .any(|part| key.contains(part))
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    clip(url.as_str(), 1024)
}

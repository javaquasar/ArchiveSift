use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, RwLock};

use eframe::egui;
use eframe::egui::TextureHandle;

use super::account_settings::AccountSettingsEditor;
use super::creator_filter::CreatorFilter;
use super::download_status::DownloadStatus;
use super::icon::application_viewport;
use super::preview_loader::{
    ImageLoadMode, ImageLoadTimings, OriginalLoader, PreviewLoader, PreviewSource,
    ORIGINAL_MAX_RGBA_BYTES, VIEWER_PROXY_MAX_SIDE,
};
use super::storage_settings::StorageSettingsEditor;
use super::texture_cache::SizedLruCache;
pub(crate) use super::LivePresets;
use crate::app::{AppConfig, PreviewOpenMode};
use crate::fs::structured::actor_folder_name;
use crate::models::{
    DateBrowseDirection, DateGroup, DateGroupCursor, DateGroupPage, DeleteMode, PageKind,
    QueueFailureKind, SearchCursor, SearchImageResult, SearchQuery, SearchResultPage,
    UrlQueueDecision, UrlQueueOutcome, UrlQueueReport,
};
use crate::scraper::ScraperRegistry;
use crate::ui::{truncate, SearchRefresh, UiCommand, UiLog};

const PREVIEW_CACHE_MAX_ENTRIES: usize = 256;
const PREVIEW_CACHE_MAX_BYTES: usize = 96 * 1024 * 1024;
const ORIGINAL_CACHE_MAX_ENTRIES: usize = 6;
/// A 400x400 RGBA preview is at most 640 KiB, so this also bounds new preview
/// texture data to roughly 5 MiB per rendered frame.
const PREVIEW_TEXTURE_UPLOADS_PER_FRAME: usize = 8;
const QUEUE_RANGE_OVERSCAN: usize = 16;
const VIEWER_IMAGE_CROSSFADE_DURATION: std::time::Duration = std::time::Duration::from_millis(220);
const FULL_ORIGINAL_ZOOM_THRESHOLD: f32 = 2.0;
pub(super) const SEARCH_PAGE_SIZE: usize = 120;

// ── Tabs ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tab {
    Scrape,
    Search,
    SqlQuery,
    Extensions,
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsPage {
    Storage,
    Integrations,
    Accounts,
    Appearance,
    AgentAccess,
    Advanced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationTab {
    Queue,
    Failures,
    Activity,
    PeerLume,
}

// ── SQL query results ─────────────────────────────────────────────────────────

/// Result of an arbitrary SQL query: column names and string-formatted rows.
pub(super) struct SqlQueryResult {
    pub(super) columns: Vec<String>,
    pub(super) total_columns: usize,
    pub(super) rows: Vec<Vec<String>>,
    pub(super) truncated: bool,
}

// ── SearchForm ────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct SearchForm {
    text: String,
    site_key: String,
    tags: String,
    actors: String,
    creators: CreatorFilter,
    categories: String,
    studios: String,
    channels: String,
    favorites_only: bool,
}

impl SearchForm {
    fn site_filter(&self) -> Option<String> {
        let key = self.site_key.trim();
        (!key.is_empty()).then(|| key.to_owned())
    }

    fn to_query(&self) -> SearchQuery {
        SearchQuery {
            text: self.text.trim().to_string(),
            site_key: self.site_filter(),
            tags: split_filter_values(&self.tags),
            actors: split_filter_values(&self.actors),
            creators: self.creators.values(),
            categories: split_filter_values(&self.categories),
            studios: split_filter_values(&self.studios),
            channels: split_filter_values(&self.channels),
            favorites_only: self.favorites_only,
            page_id: None,
        }
    }
}

// ── Delete confirmation ───────────────────────────────────────────────────────

struct DeleteConfirmState {
    image_ids: Vec<i64>,
    mode: DeleteMode,
}

struct DescriptionPopup {
    title: String,
    text: String,
}

#[derive(Default)]
struct RescrapePromptQueue {
    pending: VecDeque<Arc<str>>,
    known: HashSet<Arc<str>>,
}

impl RescrapePromptQueue {
    fn push(&mut self, url: String) -> bool {
        let url: Arc<str> = url.into();
        if !self.known.insert(Arc::clone(&url)) {
            return false;
        }
        self.pending.push_back(url);
        true
    }

    fn front(&self) -> Option<&str> {
        self.pending.front().map(Arc::as_ref)
    }

    fn pop_front(&mut self) -> Option<String> {
        let url = self.pending.pop_front()?;
        self.known.remove(url.as_ref());
        Some(url.to_string())
    }
}

#[derive(Clone, Copy, Debug)]
struct ViewerImageTransition {
    image_id: i64,
    from: f32,
    to: f32,
    started_at: std::time::Instant,
}

// ── AppWindow ─────────────────────────────────────────────────────────────────

pub struct AppWindow {
    pub config: AppConfig,
    pub log: UiLog,
    pub pending_command: Option<UiCommand>,
    pub running: bool,
    pub import_running: bool,
    pub(super) queue_worker_running: bool,
    pub error: Option<String>,
    pub queue_status: Option<String>,
    /// Monotonically increasing counter; used to discard stale search responses.
    pub search_seq: u64,

    active_tab: Tab,
    pub(super) agent_access: super::agent_access::AgentAccessPanel,
    storage_settings: StorageSettingsEditor,
    settings_page: SettingsPage,
    appearance_applied: Option<crate::app::AppearanceConfig>,
    scrape_options_open: bool,
    operations_open: bool,
    operation_tab: OperationTab,
    failure_kind: Option<QueueFailureKind>,
    failure_offset: usize,
    retry_batch_size: usize,
    pub(super) queued_card_lookup: Option<String>,
    queue_card_feedback: Option<String>,
    raw_settings: Option<super::raw_settings::RawSettingsEditor>,
    account_settings: Option<AccountSettingsEditor>,
    extensions_manager: super::extensions_manager::ExtensionsManagerUi,
    search_form: SearchForm,
    applied_search: SearchQuery,
    library_sites: Option<Vec<String>>,
    pub(super) download_status: DownloadStatus,
    download_refresh: bool,
    search_filters_open: bool,
    search_detail_open: bool,
    torrent_mode: bool,
    torrent_library: super::torrent_library::TorrentLibraryUi,
    peerlume: super::peerlume::PeerLumeUi,
    /// Immutable snapshot shared with the render closures. Replacing the
    /// snapshot after a search is cheap, and rendering does not deep-clone
    /// every result (including all metadata attributes) on every frame.
    search_results: Arc<Vec<SearchImageResult>>,
    search_next_cursor: Option<SearchCursor>,
    hydrated_image_ids: HashSet<(i64, i64)>,
    metadata_requested_ids: HashSet<i64>,
    selected_result: Option<usize>,
    description_popup: Option<DescriptionPopup>,
    selected_image_ids: HashSet<i64>,

    // Search-filter suggestions
    tag_suggestions: Vec<String>,
    actor_suggestions: Vec<String>,
    category_suggestions: Vec<String>,
    studio_suggestions: Vec<String>,
    last_tag_query: String,
    last_actor_query: String,
    last_category_query: String,
    last_studio_query: String,

    // Bulk-edit inputs
    edit_tag_input: String,
    edit_category_input: String,
    edit_actor_input: String,
    edit_studio_input: String,

    // Bulk-edit suggestions
    edit_tag_suggestions: Vec<String>,
    edit_category_suggestions: Vec<String>,
    edit_actor_suggestions: Vec<String>,
    edit_studio_suggestions: Vec<String>,
    last_edit_tag_query: String,
    last_edit_category_query: String,
    last_edit_actor_query: String,
    last_edit_studio_query: String,

    /// Whether the bulk-select mode is active (checkboxes visible).
    bulk_select_mode: bool,

    /// "Recent pages" mode — number of pages to show and whether this mode is active.
    recent_pages_limit: usize,
    recent_pages_active: bool,

    // Viewer / lightbox
    viewer_open: Option<usize>,
    /// Current zoom level (1.0 = fit-to-window). Reset on navigation.
    viewer_zoom: f32,
    /// Pan offset in pixels from the image centre. Reset on navigation.
    viewer_pan: egui::Vec2,
    /// Image source used by the viewer. Preview keeps navigation fast; Original
    /// decodes the downloaded file at its native dimensions on demand.
    viewer_image_mode: ImageLoadMode,
    /// Current original-image opacity (0 = preview, 1 = original).
    viewer_original_blend: f32,
    viewer_image_transition: Option<ViewerImageTransition>,
    /// Tag input state for the viewer — persists across image navigation.
    viewer_tag_input: String,
    viewer_tag_suggestions: Vec<String>,
    viewer_last_tag_query: String,
    /// Recently used tags (up to 10) — quick-pick chips shown below the input.
    viewer_recent_tags: Vec<String>,

    // Delete confirmation modal (search-grid bulk-select path)
    delete_confirm: Option<DeleteConfirmState>,
    // Delete confirmation modal rendered inside the viewer viewport
    viewer_delete_confirm: Option<DeleteConfirmState>,

    // Already-processed URL prompts (Task 8)
    pending_rescrape_prompts: RescrapePromptQueue,

    // Texture cache for previews
    preview_cache: SizedLruCache<String, TextureHandle>,
    viewer_original_cache: SizedLruCache<String, TextureHandle>,
    preview_loader: PreviewLoader,
    original_loader: OriginalLoader,

    /// Directory next to the database where thumbnails are lazily generated and cached.
    thumb_dir: std::path::PathBuf,

    /// Whether the queue progress window is open.
    pub queue_window_open: bool,
    /// Latest snapshot of the download queue (updated every frame).
    pub queue_snapshot: Arc<crate::models::QueueSnapshot>,
    /// Pending range requested by the virtualized queue window for the next
    /// frame. The queue service clones only this bounded slice.
    pub(super) queue_visible_range: std::ops::Range<usize>,
    /// Shared live presets — updated here on lost_focus, read by the worker per content item.
    live_presets: Arc<RwLock<LivePresets>>,

    // ── SQL Query tab ─────────────────────────────────────────────────────────
    sql_query: String,
    sql_running: bool,
    sql_export_running: bool,
    sql_result: Option<SqlQueryResult>,
    sql_error: Option<String>,
    sql_export_status: Option<String>,

    // ── Date-group browsing ───────────────────────────────────────────────────
    /// Whether date-group browsing mode is active.
    date_browse_active: bool,
    /// Only the current database-backed group is retained in memory.
    date_browse_current: Option<DateGroup>,
    date_browse_total: usize,
    /// Source captured by the latest date-group request.
    date_browse_site_key: Option<String>,
    /// `true` = older first (ascending); `false` = newer first (descending, default).
    date_browse_asc: bool,
    /// When `true`, groups with ≥ `date_browse_fav_threshold` favorites are hidden.
    date_browse_fav_enabled: bool,
    /// Maximum number of favorites a group may have to be included when filter is on.
    date_browse_fav_threshold: u32,
    /// Set while the background load is in flight.
    pub(super) date_browse_loading: bool,
}

impl AppWindow {
    pub(crate) fn new(
        config: AppConfig,
        log: UiLog,
        live_presets: Arc<RwLock<LivePresets>>,
    ) -> Self {
        let thumb_dir = config.thumb_dir();
        let storage_settings = StorageSettingsEditor::new(&config);
        Self {
            config,
            log,
            pending_command: None,
            running: false,
            import_running: false,
            queue_worker_running: false,
            error: None,
            queue_status: None,
            search_seq: 0,
            active_tab: Tab::Scrape,
            agent_access: Default::default(),
            storage_settings,
            settings_page: SettingsPage::Storage,
            appearance_applied: None,
            scrape_options_open: false,
            operations_open: false,
            operation_tab: OperationTab::Queue,
            failure_kind: None,
            failure_offset: 0,
            retry_batch_size: 10,
            queued_card_lookup: None,
            queue_card_feedback: None,
            raw_settings: None,
            account_settings: None,
            extensions_manager: Default::default(),
            search_form: SearchForm::default(),
            applied_search: SearchQuery::default(),
            library_sites: None,
            download_status: DownloadStatus::default(),
            download_refresh: false,
            search_filters_open: false,
            search_detail_open: false,
            torrent_mode: false,
            torrent_library: Default::default(),
            peerlume: Default::default(),
            search_results: Arc::new(Vec::new()),
            search_next_cursor: None,
            hydrated_image_ids: HashSet::new(),
            metadata_requested_ids: HashSet::new(),
            selected_result: None,
            description_popup: None,
            selected_image_ids: HashSet::new(),
            tag_suggestions: Vec::new(),
            actor_suggestions: Vec::new(),
            category_suggestions: Vec::new(),
            studio_suggestions: Vec::new(),
            last_tag_query: String::new(),
            last_actor_query: String::new(),
            last_category_query: String::new(),
            last_studio_query: String::new(),
            edit_tag_input: String::new(),
            edit_category_input: String::new(),
            edit_actor_input: String::new(),
            edit_studio_input: String::new(),
            edit_tag_suggestions: Vec::new(),
            edit_category_suggestions: Vec::new(),
            edit_actor_suggestions: Vec::new(),
            edit_studio_suggestions: Vec::new(),
            last_edit_tag_query: String::new(),
            last_edit_category_query: String::new(),
            last_edit_actor_query: String::new(),
            last_edit_studio_query: String::new(),
            bulk_select_mode: false,
            recent_pages_limit: 10,
            recent_pages_active: false,
            viewer_open: None,
            viewer_zoom: 1.0,
            viewer_pan: egui::Vec2::ZERO,
            viewer_image_mode: ImageLoadMode::Preview,
            viewer_original_blend: 0.0,
            viewer_image_transition: None,
            viewer_tag_input: String::new(),
            viewer_tag_suggestions: Vec::new(),
            viewer_last_tag_query: String::new(),
            viewer_recent_tags: Vec::new(),
            delete_confirm: None,
            viewer_delete_confirm: None,
            pending_rescrape_prompts: RescrapePromptQueue::default(),
            preview_cache: SizedLruCache::new(PREVIEW_CACHE_MAX_ENTRIES, PREVIEW_CACHE_MAX_BYTES),
            viewer_original_cache: SizedLruCache::new(
                ORIGINAL_CACHE_MAX_ENTRIES,
                ORIGINAL_MAX_RGBA_BYTES,
            ),
            preview_loader: PreviewLoader::new(),
            original_loader: OriginalLoader::new(),
            thumb_dir,
            queue_window_open: false,
            queue_snapshot: Arc::new(crate::models::QueueSnapshot::default()),
            queue_visible_range: 0..64,
            live_presets,
            sql_query: String::new(),
            sql_running: false,
            sql_export_running: false,
            sql_result: None,
            sql_error: None,
            sql_export_status: None,
            date_browse_active: false,
            date_browse_current: None,
            date_browse_total: 0,
            date_browse_site_key: None,
            date_browse_asc: false,
            date_browse_fav_enabled: true,
            date_browse_fav_threshold: 3,
            date_browse_loading: false,
        }
    }

    pub fn take_command(&mut self) -> Option<UiCommand> {
        self.pending_command
            .take()
            .or_else(|| self.torrent_library.take_command())
    }
    pub(super) fn review_extension(&mut self, ctx: &egui::Context, path: std::path::PathBuf) {
        self.active_tab = Tab::Extensions;
        self.extensions_manager
            .preview(ctx, path, &self.config, false);
    }

    /// Returns the appropriate refresh mode for the current search state.
    ///
    /// When date-browse mode is active the refresh is scoped to the current
    /// page so that metadata edits (gender, favorites, tags …) do not navigate
    /// away from the group being viewed.
    pub(super) fn search_refresh(&self) -> SearchRefresh {
        if self.recent_pages_active {
            SearchRefresh::RecentPages(self.recent_pages_limit)
        } else {
            if self.date_browse_active {
                SearchRefresh::Query(SearchQuery {
                    page_id: self.date_browse_current.as_ref().map(|group| group.page_id),
                    site_key: self.date_browse_site_key.clone(),
                    ..Default::default()
                })
            } else {
                SearchRefresh::Query(self.applied_search.clone())
            }
        }
    }

    /// Full reset: used after a new search query. Clears selection, bulk-mode, viewer.
    pub fn set_search_results(&mut self, results: Vec<SearchImageResult>) {
        self.preview_loader.advance_generation();
        self.original_loader.advance_generation();
        self.preview_cache.clear();
        self.viewer_original_cache.clear();
        self.hydrated_image_ids = results
            .iter()
            .map(|result| (result.page_id(), result.image_id))
            .collect();
        self.metadata_requested_ids.clear();
        self.search_next_cursor = None;
        self.search_results = Arc::new(results);
        self.selected_result = self.search_results.first().map(|_| 0);
        self.search_detail_open = false;
        self.selected_image_ids.clear();
        self.bulk_select_mode = false;
        self.viewer_open = None;
        self.viewer_original_blend = 0.0;
        self.viewer_image_transition = None;
    }

    pub fn set_search_page(&mut self, page: SearchResultPage, append: bool) {
        if !append {
            self.set_search_results(Vec::new());
        }
        let results = Arc::make_mut(&mut self.search_results);
        if !append {
            results.reserve(page.items.len().max(SEARCH_PAGE_SIZE));
        }
        results.extend(page.items);
        self.search_next_cursor = page.next_cursor;
        if self.selected_result.is_none() && !self.search_results.is_empty() {
            self.selected_result = Some(0);
        }
        if !append
            && !self.date_browse_active
            && !self.recent_pages_active
            && self.applied_search.page_id.is_some()
        {
            self.search_detail_open = self.selected_result.is_some();
        }
    }

    pub fn apply_hydrated_results(
        &mut self,
        requested_ids: &[i64],
        hydrated: Vec<SearchImageResult>,
    ) {
        self.metadata_requested_ids
            .retain(|image_id| !requested_ids.contains(image_id));
        let mut hydrated_by_id = hydrated
            .into_iter()
            .map(|result| ((result.page_id(), result.image_id), result))
            .collect::<std::collections::HashMap<_, _>>();
        if hydrated_by_id.is_empty() {
            return;
        }
        let results = Arc::make_mut(&mut self.search_results);
        for result in results {
            if let Some(hydrated) = hydrated_by_id.remove(&(result.page_id(), result.image_id)) {
                *result = hydrated;
                self.hydrated_image_ids
                    .insert((result.page_id(), result.image_id));
            }
        }
    }

    pub(super) fn release_metadata_requests(&mut self) {
        self.metadata_requested_ids.clear();
    }

    pub(super) fn apply_search_metadata(
        &mut self,
        database_path: &str,
        seq: u64,
        requested_ids: &[i64],
        result: Result<Vec<SearchImageResult>, String>,
    ) {
        if self.search_seq != seq || self.config.database_path != database_path {
            return;
        }
        match result {
            Ok(results) => self.apply_hydrated_results(requested_ids, results),
            Err(error) => {
                self.apply_hydrated_results(requested_ids, Vec::new());
                self.log.push(format!("Metadata load failed: {error}"));
            }
        }
    }

    pub fn patch_search_results(&mut self, updated: Vec<SearchImageResult>) {
        let requested_ids = updated
            .iter()
            .map(|result| result.image_id)
            .collect::<Vec<_>>();
        self.apply_hydrated_results(&requested_ids, updated);
    }

    pub fn remove_search_results(&mut self, removed_ids: &[i64]) {
        let removed = removed_ids.iter().copied().collect::<HashSet<_>>();
        let selected_id = self
            .selected_result
            .and_then(|index| self.search_results.get(index))
            .map(|result| result.image_id);
        let viewer_id = self
            .viewer_open
            .and_then(|index| self.search_results.get(index))
            .map(|result| result.image_id);
        Arc::make_mut(&mut self.search_results)
            .retain(|result| !removed.contains(&result.image_id));
        self.selected_image_ids
            .retain(|image_id| !removed.contains(image_id));
        self.hydrated_image_ids
            .retain(|(_, image_id)| !removed.contains(image_id));
        self.selected_result = selected_id
            .filter(|image_id| !removed.contains(image_id))
            .and_then(|image_id| {
                self.search_results
                    .iter()
                    .position(|result| result.image_id == image_id)
            })
            .or_else(|| (!self.search_results.is_empty()).then_some(0));
        self.search_detail_open &= selected_id.is_some_and(|id| !removed.contains(&id));
        self.viewer_open = viewer_id
            .filter(|image_id| !removed.contains(image_id))
            .and_then(|image_id| {
                self.search_results
                    .iter()
                    .position(|result| result.image_id == image_id)
            });
    }

    /// Soft refresh: used after favorites/gender updates. Preserves the currently
    /// selected image (matched by image_id) and the viewer position.
    pub fn refresh_search_results(&mut self, results: Vec<SearchImageResult>) {
        let previous_selection = self
            .selected_result
            .and_then(|idx| self.search_results.get(idx))
            .map(|r| (r.page_id(), r.image_id));
        let prev_viewer_id = self
            .viewer_open
            .and_then(|idx| self.search_results.get(idx))
            .map(|r| r.image_id);

        self.search_results = Arc::new(results);

        let restored = previous_selection.and_then(|identity| {
            self.search_results
                .iter()
                .position(|r| (r.page_id(), r.image_id) == identity)
        });
        self.search_detail_open &= restored.is_some();
        self.selected_result = restored.or_else(|| self.search_results.first().map(|_| 0));

        // Keep viewer open at the same image if possible.
        if self.viewer_open.is_some() {
            self.viewer_open = prev_viewer_id
                .or(previous_selection.map(|(_, image_id)| image_id))
                .and_then(|id| self.search_results.iter().position(|r| r.image_id == id));
        }
    }

    pub fn refresh_search_page(&mut self, page: SearchResultPage) {
        let hydrated_ids = page
            .items
            .iter()
            .map(|result| (result.page_id(), result.image_id))
            .collect::<HashSet<_>>();
        let next_cursor = page.next_cursor.clone();
        self.refresh_search_results(page.items);
        self.search_next_cursor = next_cursor;
        self.hydrated_image_ids = hydrated_ids;
        self.metadata_requested_ids.clear();
    }

    pub fn set_tag_suggestions(&mut self, query: &str, suggestions: Vec<String>) {
        if last_comma_token(&self.search_form.tags) == query {
            self.tag_suggestions = suggestions.clone();
        }
        if last_comma_token(&self.edit_tag_input) == query {
            self.edit_tag_suggestions = suggestions.clone();
        }
        if last_comma_token(&self.viewer_tag_input) == query {
            self.viewer_tag_suggestions = suggestions;
        }
    }

    pub fn set_actor_suggestions(&mut self, query: &str, suggestions: Vec<String>) {
        self.torrent_library
            .set_actor_suggestions(query, &suggestions);
        if last_comma_token(&self.search_form.actors) == query {
            self.actor_suggestions = suggestions.clone();
        }
        if last_comma_token(&self.edit_actor_input) == query {
            self.edit_actor_suggestions = suggestions;
        }
    }

    pub fn set_category_suggestions(&mut self, query: &str, suggestions: Vec<String>) {
        if last_comma_token(&self.search_form.categories) == query {
            self.category_suggestions = suggestions.clone();
        }
        if last_comma_token(&self.edit_category_input) == query {
            self.edit_category_suggestions = suggestions;
        }
    }

    pub fn set_studio_suggestions(&mut self, query: &str, suggestions: Vec<String>) {
        if last_comma_token(&self.search_form.studios) == query {
            self.studio_suggestions = suggestions.clone();
        }
        if last_comma_token(&self.edit_studio_input) == query {
            self.edit_studio_suggestions = suggestions;
        }
    }

    pub fn set_creator_suggestions(&mut self, database: &str, query: &str, values: Vec<String>) {
        if database == self.config.database_path {
            self.search_form
                .creators
                .set_suggestions(database, query, values.clone());
            self.torrent_library
                .set_creator_suggestions(database, query, values);
        }
    }

    pub fn apply_queue_report(&mut self, report: &UrlQueueReport, origin: &str) {
        self.queue_status = Some(format!(
            "{}: loaded {}, queued {}, skipped invalid/empty {}, skipped duplicate/processed {}",
            origin,
            report.loaded,
            report.queued_successfully,
            report.empty_or_invalid,
            report.duplicate_or_processed
        ));
        for decision in &report.decisions {
            self.log.push(format_queue_decision(decision));
            // When automatic skipping is ON — silently ignore.
            // When OFF — show the popup so the user can choose Skip or Re-scrape.
            if should_show_rescrape_prompt(&decision.outcome, self.config.skip_downloaded) {
                let url = decision
                    .normalized_url
                    .clone()
                    .unwrap_or_else(|| decision.raw_input.trim().to_string());
                self.pending_rescrape_prompts.push(url);
            }
        }
    }

    pub fn apply_metadata_update(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.error = None;
        self.edit_tag_input.clear();
        self.edit_category_input.clear();
        self.edit_actor_input.clear();
        self.edit_studio_input.clear();
        self.edit_tag_suggestions.clear();
        self.edit_category_suggestions.clear();
        self.edit_actor_suggestions.clear();
        self.edit_studio_suggestions.clear();
        self.last_edit_tag_query.clear();
        self.last_edit_category_query.clear();
        self.last_edit_actor_query.clear();
        self.last_edit_studio_query.clear();
        self.log.push(message);
    }

    pub fn render(&mut self, ctx: &egui::Context) {
        if self.appearance_applied != Some(self.config.appearance) {
            super::appearance::apply(ctx, self.config.appearance);
            self.appearance_applied = Some(self.config.appearance);
        }
        self.peerlume.poll(&self.config);
        self.extensions_manager.poll(ctx, &self.config);
        if std::mem::take(&mut self.extensions_manager.changed) {
            self.account_settings = None;
            if let Some(editor) = &mut self.raw_settings {
                editor.refresh_catalog();
            }
        }
        if self.torrent_library.poll() {
            self.torrent_card_saved();
        }
        if let Some(error) = self.torrent_library.startup_error.take() {
            self.log
                .push(format!("Torrent operation recovery failed: {error}"));
            self.error = Some(format!("Torrent operation recovery failed: {error}"));
        }
        self.install_ready_previews(ctx);

        let previous_tab = self.active_tab;
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            if self.torrent_library.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Library operation in progress…");
                });
            }
            ui.add_enabled_ui(
                !self.storage_settings.applying && !self.torrent_library.busy,
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut self.active_tab, Tab::Scrape, "Scrape");
                        ui.selectable_value(&mut self.active_tab, Tab::Search, "Search");
                        ui.selectable_value(&mut self.active_tab, Tab::SqlQuery, "SQL");
                        ui.selectable_value(&mut self.active_tab, Tab::Extensions, "Extensions");
                        ui.selectable_value(&mut self.active_tab, Tab::Settings, "Settings");
                    });
                },
            );
        });
        self.render_status_bar(ctx);
        if self.active_tab == Tab::Settings && previous_tab != Tab::Settings {
            self.storage_settings = StorageSettingsEditor::new(&self.config);
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.active_tab {
            Tab::Settings => self.render_settings(ui, previous_tab != Tab::Settings),
            tab => {
                ui.add_enabled_ui(!self.storage_settings.applying, |ui| match tab {
                    Tab::Scrape => self.render_scrape_tab(ui),
                    Tab::Search => self.render_search_tab(ui, ctx),
                    Tab::SqlQuery => self.render_sql_tab(ui),
                    Tab::Extensions => self.render_extensions_tab(ui),
                    Tab::Settings => unreachable!(),
                });
            }
        });

        // Overlays (rendered outside central panel so they sit on top).
        if let Some((title, text)) = self.torrent_library.take_description() {
            self.description_popup = Some(DescriptionPopup { title, text });
        }
        if let Some(ids) = self.torrent_library.take_handoff() {
            if !self.storage_settings.applying {
                self.peerlume.prepare(ctx, &self.config, ids);
            }
        }
        let storage_busy = self.storage_settings.applying || self.torrent_library.busy;
        if self.pending_command.is_none() {
            self.peerlume.tick(ctx, &self.config, storage_busy);
        }
        self.peerlume.render(ctx, &self.config, storage_busy);
        self.render_operations(ctx);
        if !self.storage_settings.applying {
            self.render_viewer(ctx);
            self.render_description_popup(ctx);
            self.render_delete_confirm(ctx);
            self.render_rescrape_prompts(ctx);
            self.render_queue_window(ctx);
            self.render_scrape_options(ctx);
        }
    }

    pub(super) fn storage_settings_busy(&self) -> bool {
        self.running
            || self.peerlume.blocks_settings()
            || self.torrent_library.busy
            || self.raw_settings.as_ref().is_some_and(|e| e.busy())
            || self.queue_worker_running
            || self.import_running
            || self.sql_running
            || self.sql_export_running
            || self.date_browse_loading
            || self.storage_settings.applying
            || self.extensions_manager.blocks_settings()
            || self
                .account_settings
                .as_ref()
                .is_some_and(|editor| editor.busy())
    }

    pub(super) fn recover_torrent_operations(&mut self, ctx: &egui::Context) {
        self.torrent_library.recover_on_start(ctx, &self.config);
    }
    pub(super) fn extension_manager_busy(&self) -> bool {
        self.extensions_manager.busy()
            || self.account_settings.as_ref().is_some_and(|e| e.busy())
            || self.raw_settings.as_ref().is_some_and(|e| e.busy())
    }
    pub(super) fn extension_version_switched(&mut self) {
        self.storage_settings_saved();
        self.extensions_manager.invalidate();
        self.account_settings = None;
        if let Some(editor) = &mut self.raw_settings {
            editor.refresh_catalog();
        }
    }

    fn torrent_card_saved(&mut self) {
        // Results computed before a transfer must not restore obsolete file paths.
        self.search_seq = self.search_seq.wrapping_add(1);
        self.running = false;
        self.set_search_results(Vec::new());
        self.log
            .push("Torrent card updated; local file paths refreshed.");
    }

    pub(super) fn storage_settings_applying(&self) -> bool {
        self.storage_settings.applying
    }

    pub(super) fn account_finished(
        &mut self,
        id: String,
        result: Result<
            crate::scraper::extensions::accounts::AccountOutcome,
            crate::scraper::extensions::accounts::AccountError,
        >,
    ) {
        let result = super::account_settings::apply_account_result(
            &mut self.config,
            &id,
            result,
            AppConfig::config_path(),
        );
        self.log.push(if result.is_ok() {
            "Extension account updated."
        } else {
            "Extension account operation failed. See Settings for details."
        });
        if let Some(editor) = &mut self.account_settings {
            editor.finish(&id, result);
        }
    }

    pub(super) fn begin_storage_settings(&mut self) {
        self.storage_settings.applying = true;
        self.storage_settings.status = None;
    }

    pub(super) fn storage_settings_failed(&mut self, error: String) {
        self.storage_settings.applying = false;
        if let Some(editor) = &mut self.raw_settings {
            editor.status = Some(error.clone());
        }
        self.storage_settings.status = Some(Err(error));
    }

    pub(super) fn storage_settings_saved(&mut self) {
        self.active_tab = Tab::Settings;
        self.storage_settings.status = Some(Ok(()));
    }

    pub(super) fn apply_storage_config(&mut self, config: AppConfig) {
        let mut replacement = Self::new(config, self.log.clone(), Arc::clone(&self.live_presets));
        replacement.settings_page = self.settings_page;
        if self.settings_page == SettingsPage::Advanced {
            let mut editor = super::raw_settings::RawSettingsEditor::installed(&replacement.config);
            editor.status = Some("Full configuration saved and applied.".into());
            replacement.raw_settings = Some(editor);
        }
        replacement.sql_query = std::mem::take(&mut self.sql_query);
        replacement.extensions_manager = std::mem::take(&mut self.extensions_manager);
        replacement.extensions_manager.invalidate();
        replacement.active_tab = self.active_tab;
        replacement.storage_settings_saved();
        *self = replacement;
    }

    pub(super) fn setting_secret_finished(&mut self, target: &str, result: Result<(), String>) {
        if let Some(editor) = &mut self.raw_settings {
            editor.secret_finished(target, result);
        }
    }

    fn render_settings(&mut self, ui: &mut egui::Ui, entered: bool) {
        let previous_page = self.settings_page;
        ui.horizontal_wrapped(|ui| {
            for (page, label) in [
                (SettingsPage::Storage, "Storage"),
                (SettingsPage::Integrations, "Integrations"),
                (SettingsPage::Accounts, "Accounts"),
                (SettingsPage::Appearance, "Appearance"),
                (SettingsPage::AgentAccess, "Agent access"),
                (SettingsPage::Advanced, "Advanced"),
            ] {
                ui.selectable_value(&mut self.settings_page, page, label);
            }
        });
        ui.separator();
        let busy = self.storage_settings_busy();
        let applying = self.storage_settings.applying;
        if self.settings_page == SettingsPage::Storage {
            if let Some(settings) = self.storage_settings.render(ui, &self.config, busy) {
                self.pending_command = Some(UiCommand::ApplyStorageSettings(settings));
            }
            return;
        }
        egui::ScrollArea::vertical()
            .id_salt(("settings", self.settings_page as u8))
            .show(ui, |ui| {
                let command = match self.settings_page {
                    SettingsPage::Integrations => self.peerlume.settings(ui, &self.config, busy),
                    SettingsPage::Accounts => self
                        .account_settings
                        .get_or_insert_with(|| AccountSettingsEditor::installed(&self.config))
                        .render(ui, applying)
                        .map(UiCommand::Account),
                    SettingsPage::Appearance => {
                        let previous = self.config.appearance;
                        let mut save = false;
                        ui.add_enabled_ui(!applying, |ui| {
                            save = super::appearance::editor(ui, &mut self.config.appearance);
                            egui::ComboBox::from_label("Open image preview")
                                .selected_text(
                                    if self.config.preview_open_mode.opens_on_single_click() {
                                        "Single click"
                                    } else {
                                        "Double click"
                                    },
                                )
                                .show_ui(ui, |ui| {
                                    for (mode, label) in [
                                        (PreviewOpenMode::SingleClick, "Single click"),
                                        (PreviewOpenMode::DoubleClick, "Double click"),
                                    ] {
                                        if ui
                                            .selectable_value(
                                                &mut self.config.preview_open_mode,
                                                mode,
                                                label,
                                            )
                                            .changed()
                                        {
                                            self.save_config_setting("preview open mode");
                                        }
                                    }
                                });
                        });
                        if save {
                            self.save_config_setting("appearance");
                        }
                        if previous != self.config.appearance {
                            ui.ctx().request_repaint();
                        }
                        None
                    }
                    SettingsPage::AgentAccess => self.agent_access.render(ui),
                    SettingsPage::Advanced => {
                        ui.heading("Settings · JSON");
                        let editor = self.raw_settings.get_or_insert_with(|| {
                            super::raw_settings::RawSettingsEditor::installed(&self.config)
                        });
                        if entered || previous_page != SettingsPage::Advanced {
                            editor.refresh_pristine_draft(&self.config);
                        }
                        editor.render(ui, &self.config, busy, applying)
                    }
                    SettingsPage::Storage => unreachable!(),
                };
                if let Some(command) = command {
                    self.pending_command = Some(command);
                }
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            });
    }

    fn render_extensions_tab(&mut self, ui: &mut egui::Ui) {
        let blocked = self.storage_settings.applying
            || self
                .account_settings
                .as_ref()
                .is_some_and(|editor| editor.busy())
            || self
                .raw_settings
                .as_ref()
                .is_some_and(|editor| editor.busy());
        let editor = self
            .raw_settings
            .get_or_insert_with(|| super::raw_settings::RawSettingsEditor::installed(&self.config));
        egui::ScrollArea::vertical()
            .id_salt("extensions_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(command) =
                    self.extensions_manager
                        .render(ui, &self.config, blocked, |ui, id, busy| {
                            editor.render_plugin_credentials(ui, id, busy)
                        })
                {
                    self.pending_command = Some(command);
                }
            });
    }

    fn render_description_popup(&mut self, ctx: &egui::Context) {
        let Some(popup) = self.description_popup.as_ref() else {
            return;
        };
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("scene_description"),
            application_viewport()
                .with_title(format!("Description — {}", truncate(&popup.title, 70)))
                .with_inner_size([800.0, 440.0]),
            |vp_ctx, _class| {
                if vp_ctx.input(|input| {
                    input.viewport().close_requested() || input.key_pressed(egui::Key::Escape)
                }) {
                    close = true;
                }
                egui::CentralPanel::default().show(vp_ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Copy title").clicked() {
                            ui.ctx().copy_text(popup.title.clone());
                        }
                        if ui.button("Copy text").clicked() {
                            ui.ctx().copy_text(popup.text.clone());
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&popup.title).heading())
                                .wrap()
                                .selectable(true),
                        );
                        ui.separator();
                        ui.add(
                            egui::Label::new(popup.text.as_str())
                                .wrap()
                                .selectable(true),
                        );
                    });
                });
            },
        );
        if close {
            self.description_popup = None;
        }
    }

    fn install_ready_previews(&mut self, ctx: &egui::Context) {
        // Viewer originals remain first so an open image is never delayed by a
        // burst of grid thumbnails. Preview uploads are capped per frame.
        let mut ready = self.original_loader.drain_ready();
        let previews = self
            .preview_loader
            .drain_ready_up_to(PREVIEW_TEXTURE_UPLOADS_PER_FRAME);
        let preview_batch_full = previews.len() == PREVIEW_TEXTURE_UPLOADS_PER_FRAME;
        ready.extend(previews);
        for decoded in ready {
            let key = decoded.key;
            let image_id = decoded.image_id;
            let mode = decoded.mode;
            let timings = decoded.timings;
            let estimated_bytes = decoded.estimated_bytes;
            let texture_started = std::time::Instant::now();
            let texture = ctx.load_texture(&key, decoded.image, Default::default());
            let texture_upload = texture_started.elapsed();
            if mode == ImageLoadMode::Original {
                self.log.push(format_original_load_metrics(
                    image_id,
                    timings,
                    texture_upload,
                ));
            }
            match mode {
                ImageLoadMode::Preview => {
                    self.preview_cache.insert(key, texture, estimated_bytes);
                }
                ImageLoadMode::Original => {
                    self.viewer_original_cache
                        .insert(key, texture, estimated_bytes);
                }
            }
        }
        if preview_batch_full {
            ctx.request_repaint();
        }
    }

    fn save_config_setting(&mut self, setting_name: &str) {
        if let Err(error) = self.config.save() {
            self.error = Some(error.to_string());
            self.log.push(format!(
                "Failed to save {} setting: {}",
                setting_name, error
            ));
        }
    }

    // ── Scrape tab ────────────────────────────────────────────────────────────

    fn render_scrape_tab(&mut self, ui: &mut egui::Ui) {
        let configured = self.config.has_download_folder();
        ui.horizontal_wrapped(|ui| {
            let input = ui.add(
                egui::TextEdit::singleline(&mut self.config.source_url)
                    .hint_text("Content or listing URL")
                    .desired_width((ui.available_width() - 170.0).clamp(100.0, 680.0)),
            );
            let can_add = configured && !self.config.source_url.trim().is_empty();
            if ui
                .add_enabled(can_add, egui::Button::new("Add to queue"))
                .clicked()
                || (can_add && input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            {
                self.error = None;
                self.pending_command = Some(UiCommand::QueueUrl(self.config.clone()));
            }
            ui.menu_button("More", |ui| {
                if ui
                    .add_enabled(
                        configured && !self.import_running,
                        egui::Button::new("Import URL file…"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("Text files", &["txt"])
                        .pick_file()
                    {
                        self.pending_command = Some(UiCommand::ImportFile {
                            file_path: file.display().to_string(),
                        });
                    }
                }
                if ui.button("Scrape options…").clicked() {
                    self.scrape_options_open = true;
                    ui.close_menu();
                }
                if ui.button("Open queue in window").clicked() {
                    self.queue_window_open = true;
                    ui.close_menu();
                }
                if ui
                    .add_enabled(
                        self.queue_snapshot.pending_total > 0,
                        egui::Button::new("Clear pending"),
                    )
                    .clicked()
                {
                    self.pending_command = Some(UiCommand::ClearPendingQueue);
                    ui.close_menu();
                }
            });
        });

        ui.horizontal_wrapped(|ui| {
            ui.label("Destination");
            if ui
                .button(if configured {
                    "Change…"
                } else {
                    "Choose folder…"
                })
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.pending_command =
                        Some(UiCommand::SetDownloadFolder(folder.display().to_string()));
                }
            }
            if configured {
                let folder = self.config.effective_download_dir();
                let path = folder.to_string_lossy();
                ui.add_sized(
                    [ui.available_width().max(60.0), ui.spacing().interact_size.y],
                    egui::Label::new(path.as_ref()).truncate(),
                )
                .on_hover_text(path.as_ref());
            } else {
                ui.weak("Choose a download folder to start.");
            }
        });
        if self.import_running {
            ui.weak("Importing URLs…");
        }
        if !self.config.parse_additional_links
            && looks_like_additional_link_source(&self.config.source_url)
        {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Enable listing expansion in Scrape options to queue linked content.",
            );
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.heading("Queue");
            if ui.button("Activity").clicked() {
                self.operation_tab = OperationTab::Activity;
                self.operations_open = true;
            }
            if let Some(status) = &self.queue_status {
                ui.weak(status);
            }
        });
        if self.queue_window_open {
            if ui.button("Show queue here").clicked() {
                self.queue_window_open = false;
            }
        } else {
            self.render_queue_rows(ui);
        }
    }

    fn render_scrape_options(&mut self, ctx: &egui::Context) {
        let mut open = self.scrape_options_open;
        if !open {
            return;
        }
        egui::Window::new("Scrape options")
            .open(&mut open)
            .default_width(440.0)
            .max_width((ctx.screen_rect().width() - 24.0).max(160.0))
            .vscroll(true)
            .show(ctx, |ui| {
                if ui
                    .checkbox(
                        &mut self.config.clipboard_listener_enabled,
                        "Monitor clipboard for supported URLs",
                    )
                    .changed()
                {
                    self.pending_command = Some(UiCommand::SetClipboardListener(
                        self.config.clipboard_listener_enabled,
                    ));
                }
                let mut changed = ui
                    .checkbox(
                        &mut self.config.skip_downloaded,
                        "Skip already downloaded URLs",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.config.parse_additional_links,
                        "Expand listings into content URLs",
                    )
                    .changed();
                ui.separator();
                ui.label("Subfolder");
                changed |= ui
                    .text_edit_singleline(&mut self.config.subfolder)
                    .lost_focus();
                ui.menu_button("Recent folders", |ui| {
                    for folder in &self.config.download_folder_history {
                        if ui
                            .button(truncate(folder, 52))
                            .on_hover_text(folder)
                            .clicked()
                        {
                            self.pending_command =
                                Some(UiCommand::SetDownloadFolder(folder.clone()));
                            ui.close_menu();
                        }
                    }
                });
                if ui.button("Edit storage settings").clicked() {
                    self.active_tab = Tab::Settings;
                    self.settings_page = SettingsPage::Storage;
                    self.storage_settings = StorageSettingsEditor::new(&self.config);
                    self.scrape_options_open = false;
                }
                ui.separator();
                ui.weak("Presets apply to downloaded images. Separate values with commas.");
                for (label, value) in [
                    ("Preset tags", &mut self.config.preset_tags),
                    ("Preset categories", &mut self.config.preset_categories),
                ] {
                    ui.label(label);
                    ui.horizontal_wrapped(|ui| {
                        changed |= ui.text_edit_singleline(value).lost_focus();
                        if ui
                            .add_enabled(!value.is_empty(), egui::Button::new("Clear"))
                            .clicked()
                        {
                            value.clear();
                            changed = true;
                        }
                    });
                }
                ui.collapsing("Download settings", |ui| {
                    for (label, value, maximum) in [
                        (
                            "Parallel pages",
                            &mut self.config.max_parallel_downloads,
                            16,
                        ),
                        (
                            "Concurrent images",
                            &mut self.config.max_concurrent_downloads,
                            32,
                        ),
                    ] {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(label);
                            changed |= ui
                                .add(egui::DragValue::new(value).range(1..=maximum))
                                .changed();
                        });
                    }
                    ui.weak("Random delay before each page request. Set both to zero to disable.");
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Min delay");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.config.request_delay_min_ms)
                                    .range(0..=60_000)
                                    .suffix(" ms"),
                            )
                            .changed()
                        {
                            self.config.request_delay_max_ms = self
                                .config
                                .request_delay_max_ms
                                .max(self.config.request_delay_min_ms);
                            changed = true;
                        }
                        ui.label("Max delay");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.config.request_delay_max_ms)
                                    .range(0..=60_000)
                                    .suffix(" ms"),
                            )
                            .changed()
                        {
                            self.config.request_delay_min_ms = self
                                .config
                                .request_delay_min_ms
                                .min(self.config.request_delay_max_ms);
                            changed = true;
                        }
                    });
                });
                if changed {
                    self.save_scrape_options();
                }
            });
        if !open && self.scrape_options_open {
            self.save_scrape_options();
        }
        self.scrape_options_open &= open;
    }

    fn save_scrape_options(&mut self) {
        if let Ok(mut presets) = self.live_presets.write() {
            presets.tags.clone_from(&self.config.preset_tags);
            presets
                .categories
                .clone_from(&self.config.preset_categories);
        }
        self.save_config_setting("scrape options");
    }

    pub(super) fn needs_queue_rows(&self) -> bool {
        self.queue_window_open || self.active_tab == Tab::Scrape
    }

    pub(super) fn queue_failures(&self) -> Option<(Option<QueueFailureKind>, usize)> {
        (self.operations_open && self.operation_tab == OperationTab::Failures)
            .then_some((self.failure_kind, self.failure_offset))
    }

    pub(super) fn set_library_sites(&mut self, sites: Vec<String>) {
        self.library_sites = Some(sites);
    }

    pub(super) fn begin_library_sites_load(&mut self) -> bool {
        if self.active_tab != Tab::Search || self.library_sites.is_some() {
            return false;
        }
        self.library_sites = Some(Vec::new());
        true
    }

    pub(super) fn invalidate_library_sites(&mut self) {
        self.library_sites = None;
    }

    pub(super) fn page_download_finished(&mut self, url: &str, message: &str, show_result: bool) {
        self.download_status.complete(url, message, show_result);
        self.torrent_library.page_download_finished(url);
        if self
            .search_results
            .iter()
            .any(|image| image.page_url() == url)
        {
            self.download_refresh = true;
        }
    }

    pub(super) fn take_download_refresh(&mut self) -> bool {
        !self.running && !self.date_browse_loading && std::mem::take(&mut self.download_refresh)
    }

    pub(super) fn apply_download_refresh(&mut self, page: SearchResultPage) {
        self.preview_cache.clear();
        self.preview_loader.advance_generation();
        self.original_loader.advance_generation();
        self.viewer_original_cache.clear();
        self.refresh_search_page(page);
        // The background refresh uses the lightweight search projection.
        self.hydrated_image_ids.clear();
    }

    pub(super) fn search_result_limit(&self) -> usize {
        self.search_results.len().max(SEARCH_PAGE_SIZE)
    }

    pub(super) fn queued_card_resolved(
        &mut self,
        ctx: &egui::Context,
        result: Result<Option<(i64, bool)>, String>,
    ) {
        self.queued_card_lookup = None;
        if !self.operations_open {
            return;
        }
        match result {
            Ok(Some((id, torrent))) => {
                self.operations_open = false;
                self.active_tab = Tab::Search;
                self.torrent_mode = torrent;
                if torrent {
                    self.torrent_library.open_card(ctx, &self.config, id);
                } else {
                    self.search_form = SearchForm::default();
                    self.clear_search_suggestions();
                    self.search_detail_open = false;
                    self.run_image_query(SearchQuery {
                        page_id: Some(id),
                        ..Default::default()
                    });
                }
                ctx.request_repaint();
            }
            Ok(None) => {
                self.queue_card_feedback = Some("No card has been saved for this URL yet.".into())
            }
            Err(error) => self.queue_card_feedback = Some(error),
        }
    }

    fn render_queue_rows(&mut self, ui: &mut egui::Ui) {
        let snapshot = clone_queue_snapshot(&self.queue_snapshot);
        if snapshot.in_progress.is_empty() && snapshot.pending_total == 0 {
            ui.weak("Queue is empty. Add a URL to start.");
            return;
        }
        let active = snapshot.in_progress.len();
        let total = active.saturating_add(snapshot.pending_total);
        let row_height = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt("queue_rows")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, total, |ui, rows| {
                let pending = rows.start.saturating_sub(active)..rows.end.saturating_sub(active);
                if !pending.is_empty()
                    && update_queue_visible_range(
                        &mut self.queue_visible_range,
                        queue_range_with_overscan(pending, snapshot.pending_total),
                    )
                {
                    ui.ctx().request_repaint();
                }
                for index in rows {
                    let (label, url) = if let Some((url, _)) = snapshot.in_progress.get(index) {
                        ("Active", url.as_str())
                    } else {
                        (
                            "Pending",
                            queue_snapshot_pending_at(&snapshot, index - active)
                                .unwrap_or("Loading…"),
                        )
                    };
                    ui.horizontal(|ui| {
                        ui.set_min_height(row_height);
                        ui.add_sized([66.0, row_height], egui::Label::new(label));
                        ui.add(egui::Label::new(url).truncate()).on_hover_text(url);
                    });
                }
            });
    }

    fn queue_controls(&mut self, ui: &mut egui::Ui) {
        let paused = self.queue_snapshot.paused;
        if ui.button(if paused { "Resume" } else { "Pause" }).clicked() {
            self.pending_command = Some(if paused {
                UiCommand::ResumeQueue
            } else {
                UiCommand::PauseQueue
            });
        }
    }

    fn render_status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("workspace_status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .selectable_label(self.operations_open, "Operations")
                    .clicked()
                {
                    self.operations_open = !self.operations_open;
                }
                let snapshot = &self.queue_snapshot;
                ui.label(format!(
                    "{} active · {} pending · {} failed",
                    snapshot.in_progress.len(),
                    snapshot.pending_total,
                    snapshot.failed_total,
                ));
                ui.weak(if snapshot.paused {
                    "Paused"
                } else if !snapshot.in_progress.is_empty() {
                    "Running"
                } else {
                    "Idle"
                });
                if snapshot.failed_total > 0 && ui.button("Failed downloads").clicked() {
                    self.operations_open = true;
                    self.operation_tab = OperationTab::Failures;
                }
                ui.add_enabled_ui(!self.storage_settings.applying, |ui| {
                    self.queue_controls(ui)
                });
                if self.agent_access.enabled || self.agent_access.busy {
                    if ui.button("Agent access").clicked() {
                        self.active_tab = Tab::Settings;
                        self.settings_page = SettingsPage::AgentAccess;
                    }
                }
            });
        });
    }

    fn render_operations(&mut self, ctx: &egui::Context) {
        let mut open = self.operations_open;
        if !open {
            return;
        }
        egui::Window::new("Operations")
            .open(&mut open)
            .default_width(540.0)
            .max_width((ctx.screen_rect().width() - 24.0).max(160.0))
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.operation_tab, OperationTab::Queue, "Queue");
                    ui.selectable_value(
                        &mut self.operation_tab,
                        OperationTab::Failures,
                        "Failed downloads",
                    );
                    ui.selectable_value(
                        &mut self.operation_tab,
                        OperationTab::Activity,
                        "Activity",
                    );
                    ui.selectable_value(
                        &mut self.operation_tab,
                        OperationTab::PeerLume,
                        "PeerLume",
                    );
                });
                ui.separator();
                match self.operation_tab {
                    OperationTab::Queue => {
                        if ui
                            .add_enabled(
                                !self.storage_settings.applying,
                                egui::Button::new("Go to queue"),
                            )
                            .clicked()
                        {
                            self.active_tab = Tab::Scrape;
                            self.operations_open = false;
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.add_enabled_ui(!self.storage_settings.applying, |ui| {
                                self.queue_controls(ui)
                            });
                        });
                    }
                    OperationTab::Failures => {
                        let snapshot = clone_queue_snapshot(&self.queue_snapshot);
                        if snapshot.paused {
                            ui.horizontal_wrapped(|ui| {
                                ui.weak("Queue paused");
                                ui.add_enabled_ui(!self.storage_settings.applying, |ui| {
                                    self.queue_controls(ui);
                                });
                            });
                        }
                        if snapshot.failed_total == 0 {
                            ui.weak("No failed downloads.");
                        } else {
                            ui.label(format!("{} failed downloads", snapshot.failed_total));
                            let mut kind = self.failure_kind;
                            ui.horizontal_wrapped(|ui| {
                                egui::ComboBox::from_id_salt("queue_failure_reason")
                                    .selected_text(
                                        kind.map_or("All reasons", QueueFailureKind::label),
                                    )
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut kind, None, "All reasons");
                                        for value in QueueFailureKind::ALL {
                                            ui.selectable_value(
                                                &mut kind,
                                                Some(value),
                                                value.label(),
                                            );
                                        }
                                    });
                                if kind != self.failure_kind {
                                    self.failure_kind = kind;
                                    self.failure_offset = 0;
                                    ui.ctx().request_repaint();
                                } else {
                                    let page_size =
                                        crate::services::url_queue::QUEUE_FAILURE_PAGE_SIZE;
                                    self.failure_offset = self.failure_offset.min(
                                        snapshot.failed_matches.saturating_sub(1) / page_size
                                            * page_size,
                                    );
                                    if ui
                                        .add_enabled(
                                            self.failure_offset > 0,
                                            egui::Button::new("Previous errors"),
                                        )
                                        .clicked()
                                    {
                                        self.failure_offset -= page_size;
                                        ui.ctx().request_repaint();
                                    }
                                    if ui
                                        .add_enabled(
                                            self.failure_offset + page_size
                                                < snapshot.failed_matches,
                                            egui::Button::new("Next errors"),
                                        )
                                        .clicked()
                                    {
                                        self.failure_offset += page_size;
                                        ui.ctx().request_repaint();
                                    }
                                }
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Batch size");
                                ui.add(
                                    egui::DragValue::new(&mut self.retry_batch_size)
                                        .range(1..=1_000_000),
                                );
                                let count = self.retry_batch_size.min(snapshot.failed_matches);
                                let enabled = count > 0
                                    && !self.storage_settings.applying
                                    && self.config.has_download_folder();
                                if ui
                                    .add_enabled(
                                        enabled,
                                        egui::Button::new(format!("Retry {count}")),
                                    )
                                    .on_hover_text(
                                        "Return the oldest matching failures to the queue",
                                    )
                                    .clicked()
                                {
                                    self.pending_command = Some(UiCommand::RetryFailedBatch {
                                        kind: self.failure_kind,
                                        limit: self.retry_batch_size,
                                    });
                                }
                            });
                            if snapshot.failed_preview.is_empty() {
                                ui.weak("No failures match this reason.");
                            }
                            if let Some(feedback) = &self.queue_card_feedback {
                                ui.label(feedback);
                            }
                            egui::ScrollArea::vertical()
                                .max_height(260.0)
                                .show(ui, |ui| {
                                    for failure in &snapshot.failed_preview {
                                        ui.push_id(&failure.url, |ui| {
                                            ui.add(egui::Label::new(&failure.url).truncate())
                                                .on_hover_text(&failure.url);
                                            ui.strong(failure.kind.label());
                                            ui.add(egui::Label::new(&failure.error).wrap());
                                            ui.horizontal_wrapped(|ui| {
                                                let storage_ready = !self.storage_settings.applying;
                                                if ui
                                                    .add_enabled(
                                                        storage_ready
                                                            && self.config.has_download_folder(),
                                                        egui::Button::new("Retry"),
                                                    )
                                                    .clicked()
                                                {
                                                    self.pending_command =
                                                        Some(UiCommand::RetryQueueItem {
                                                            url: failure.url.clone(),
                                                        });
                                                }
                                                if ui
                                                    .add_enabled(
                                                        storage_ready
                                                            && !self.running
                                                            && !self.date_browse_loading
                                                            && self.queued_card_lookup.is_none()
                                                            && self.torrent_library.can_open_card(),
                                                        egui::Button::new("Open card"),
                                                    )
                                                    .clicked()
                                                {
                                                    self.queued_card_lookup =
                                                        Some(failure.url.clone());
                                                    self.queue_card_feedback = None;
                                                    self.pending_command =
                                                        Some(UiCommand::OpenQueuedCard {
                                                            url: failure.url.clone(),
                                                        });
                                                }
                                                ui.hyperlink_to("Open source", &failure.url);
                                            });
                                            ui.separator();
                                        });
                                    }
                                });
                        }
                    }
                    OperationTab::Activity => {
                        if ui.button("Clear log").clicked() {
                            self.log.clear();
                        }
                        let log = self.log.snapshot_shared();
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                if log.is_empty() {
                                    ui.weak("No activity yet.");
                                }
                                for entry in log.iter() {
                                    ui.add(
                                        egui::Label::new(entry.as_ref()).wrap().selectable(true),
                                    );
                                }
                            });
                    }
                    OperationTab::PeerLume => {
                        self.peerlume.render_operations(
                            ui,
                            &self.config,
                            self.storage_settings.applying || self.torrent_library.busy,
                        );
                    }
                }
            });
        self.operations_open &= open;
    }

    fn render_search_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let was_torrent_mode = self.torrent_mode;
        ui.add_enabled_ui(!self.torrent_library.busy && !self.running, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.torrent_mode, false, "Images");
                ui.selectable_value(&mut self.torrent_mode, true, "Torrents");
            });
        });
        if self.torrent_mode && !was_torrent_mode {
            self.viewer_open = None;
            self.delete_confirm = None;
            self.viewer_delete_confirm = None;
        }
        if self.torrent_mode {
            self.torrent_library.render_with_downloads(
                ui,
                &self.config,
                self.library_sites.as_deref().unwrap_or_default(),
                &mut self.download_status,
            );
            return;
        }
        // ── Keyboard shortcuts for date-group navigation ──────────────────────
        if self.date_browse_active && !self.date_browse_loading {
            let go_next = ctx.input(|i| i.key_pressed(egui::Key::PageDown));
            let go_prev = ctx.input(|i| i.key_pressed(egui::Key::PageUp));
            if go_next {
                self.date_browse_next();
            } else if go_prev {
                self.date_browse_prev();
            }
        }

        self.render_image_search_controls(ui);
        self.sync_date_browse_site();
        if self.date_browse_active || self.date_browse_loading {
            self.render_date_browse_row(ui);
        }
        if self.search_filters_open {
            let mut open = self.search_filters_open;
            egui::Window::new("Image filters")
                .open(&mut open)
                .default_width(380.0)
                .max_width((ctx.screen_rect().width() - 24.0).max(160.0))
                .vscroll(true)
                .show(ctx, |ui| {
                    self.render_search_filters(ui);
                    if ui
                        .add_enabled(!self.running, egui::Button::new("Apply filters"))
                        .clicked()
                    {
                        self.start_image_search();
                        self.search_filters_open = false;
                    }
                });
            self.search_filters_open &= open;
        }
        ui.horizontal_wrapped(|ui| {
            // Toggle bulk-select mode.
            let (select_label, select_stroke, select_fill) = if self.bulk_select_mode {
                (
                    "x Cancel Select",
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 140, 200)),
                    egui::Color32::TRANSPARENT,
                    // egui::Color32::from_rgb(80, 30, 30),
                )
            } else {
                (
                    "☑ Select",
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 140, 200)),
                    egui::Color32::TRANSPARENT,
                )
            };
            if ui
                .add_enabled(
                    !self.search_results.is_empty(),
                    egui::Button::new(select_label)
                        .stroke(select_stroke)
                        .fill(select_fill),
                )
                .on_hover_text(if self.bulk_select_mode {
                    "Exit selection mode"
                } else {
                    "Enter selection mode to edit or delete multiple images"
                })
                .clicked()
            {
                self.bulk_select_mode = !self.bulk_select_mode;
                if !self.bulk_select_mode {
                    self.selected_image_ids.clear();
                }
            }
        });

        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }

        if self.bulk_select_mode {
            ui.separator();
            self.render_bulk_edit_panel(ui);

            ui.separator();
            // Delete / Select-all row — only visible in bulk-select mode.
            ui.horizontal(|ui| {
                let n = self.selected_image_ids.len();
                if ui
                    .add_enabled(
                        n > 0 && !self.running,
                        egui::Button::new(format!("🗑 Delete {} selected", n)),
                    )
                    .clicked()
                {
                    self.delete_confirm = Some(DeleteConfirmState {
                        image_ids: self.selected_image_ids.iter().copied().collect(),
                        mode: DeleteMode::DbOnly,
                    });
                }
                if ui.button("Select All").clicked() {
                    for r in self.search_results.iter() {
                        self.selected_image_ids.insert(r.image_id);
                    }
                }
                if ui
                    .add_enabled(n > 0, egui::Button::new("Deselect All"))
                    .clicked()
                {
                    self.selected_image_ids.clear();
                }
                ui.label(format!("{} selected", n));
            });
        }

        ui.separator();

        // ── Layout constants ──────────────────────────────────────────────────
        const IMG_W: f32 = 120.0; // displayed image side length
        const ROW_H: f32 = 205.0; // image(120) + checkbox(20) + 3 text lines + spacing

        // cell_w = exactly the image-button width so nothing sticks out sideways.
        let btn_pad = ui.spacing().button_padding.x;
        let cell_w = IMG_W + btn_pad * 2.0; // ≈ 132 px at default style
        let item_gap = ui.spacing().item_spacing.x;
        let scrollbar_w = ui.spacing().scroll.bar_width
            + ui.spacing().scroll.bar_inner_margin * 2.0
            + ui.spacing().scroll.bar_outer_margin;
        let width = ui.available_width();
        let previous_selection = (self.selected_result, self.search_detail_open);
        let show_details = self.search_detail_open
            && !self.bulk_select_mode
            && self
                .selected_result
                .is_some_and(|index| index < self.search_results.len());
        let split = show_details && width >= 880.0;
        let details_only = show_details && !split;
        let detail_width = if split {
            (width * 0.34).clamp(300.0, 520.0)
        } else {
            0.0
        };
        let results_w =
            (width - detail_width - if split { item_gap * 3.0 + 4.0 } else { 0.0 }).max(1.0);
        let cols = grid_columns(
            results_w,
            cell_w,
            item_gap,
            scrollbar_w,
            self.config.search_columns,
        );

        let results_heading = if self.recent_pages_active {
            format!(
                "Results ({}) — last {} pages",
                self.search_results.len(),
                self.recent_pages_limit
            )
        } else {
            format!("Results ({})", self.search_results.len())
        };

        // Share the immutable snapshot across closures. Arc::clone is O(1),
        // unlike cloning every search result and its metadata on every frame.
        let results = clone_search_results_snapshot(&self.search_results);
        let base_dir = self
            .config
            .effective_download_dir()
            .to_string_lossy()
            .into_owned();
        let mut metadata_to_request = HashSet::new();

        let avail = ui.available_size();
        ui.with_layout(egui::Layout::left_to_right(egui::Align::TOP), |ui| {
            if !details_only {
                ui.allocate_ui_with_layout(
                    egui::vec2(results_w, avail.y),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.set_width(results_w);
                        ui.heading(&results_heading);
                if results.is_empty() && (self.running || self.date_browse_loading) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading images…");
                    });
                } else if results.is_empty() {
                    ui.label("No matching images. Try a broader search.");
                    if self.applied_search != SearchQuery::default() || self.date_browse_active || self.recent_pages_active {
                        if ui.add_enabled(!self.running, egui::Button::new("Reset filters")).clicked() {
                            self.search_form = SearchForm::default();
                            self.clear_search_suggestions();
                            self.start_image_search();
                        }
                    }
                } else {
                    let total_rows = (results.len() + cols - 1) / cols;
                    egui::ScrollArea::vertical()
                        .id_salt("results_scroll")
                        .show_rows(ui, ROW_H, total_rows, |ui, row_range| {
                        for row in row_range {
                            ui.horizontal(|ui| {
                                ui.set_height(ROW_H); // keep row height stable
                                for col in 0..cols {
                                    let index = row * cols + col;
                                    let Some(result) = results.get(index) else {
                                        ui.allocate_space(egui::vec2(cell_w, ROW_H));
                                        continue;
                                    };
                                    if !self.hydrated_image_ids.contains(&(result.page_id(), result.image_id))
                                        && !self.metadata_requested_ids.contains(&result.image_id)
                                    {
                                        metadata_to_request.insert(result.image_id);
                                    }

                                    // Align::Center centers the image (and all content)
                                    // horizontally inside the cell. set_max_width ensures
                                    // text can never push the cell wider than cell_w.
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(cell_w, ROW_H),
                                        egui::Layout::top_down(egui::Align::Center),
                                        |ui| {
                                            ui.set_max_width(cell_w);

                                            if self.bulk_select_mode {
                                                let mut checked = self.selected_image_ids.contains(&result.image_id);
                                                if ui.checkbox(&mut checked, "").changed() {
                                                    if checked { self.selected_image_ids.insert(result.image_id); }
                                                    else { self.selected_image_ids.remove(&result.image_id); }
                                                }
                                            }

                                            // Image button — centred by the Align::Center layout.
                                            if let Some(texture) = load_preview_texture(
                                                ctx,
                                                &mut self.preview_cache,
                                                &mut self.preview_loader,
                                                result,
                                                &base_dir,
                                                &self.thumb_dir,
                                                ImageLoadMode::Preview,
                                            ) {
                                                let img = egui::Image::from_texture(
                                                    (texture.id(), texture.size_vec2()))
                                                    .fit_to_exact_size(egui::vec2(IMG_W, IMG_W));
                                                let resp = ui.add(egui::Button::image(img));
                                                if resp.clicked() {
                                                    self.selected_result = Some(index);
                                                    self.search_detail_open = !self.bulk_select_mode;
                                                    if self.bulk_select_mode {
                                                        if self.selected_image_ids.contains(&result.image_id) { self.selected_image_ids.remove(&result.image_id); }
                                                        else { self.selected_image_ids.insert(result.image_id); }
                                                    }
                                                }
                                                if should_open_preview(self.config.preview_open_mode, &resp) {
                                                    self.viewer_open = Some(index);
                                                }
                                            } else {
                                                let key = image_texture_cache_key(
                                                    ImageLoadMode::Preview, result.image_id, &base_dir,
                                                    &self.thumb_dir, result.local_file_path.as_deref(),
                                                    result.thumbnail_path.as_deref(),
                                                );
                                                let unavailable = self.preview_loader.is_unavailable(&key);
                                                let label = if unavailable { "Image unavailable" } else { "⏳" };
                                                let resp = ui.add_sized([IMG_W, IMG_W], egui::Button::new(label));
                                                let resp = if unavailable {
                                                    resp.on_hover_text("The local image is missing or unreadable. Retry this page in the download queue to fetch missing images.")
                                                } else { resp };
                                                if resp.clicked() {
                                                    self.selected_result = Some(index);
                                                    self.search_detail_open = !self.bulk_select_mode;
                                                    if self.bulk_select_mode {
                                                        if self.selected_image_ids.contains(&result.image_id) { self.selected_image_ids.remove(&result.image_id); }
                                                        else { self.selected_image_ids.insert(result.image_id); }
                                                    }
                                                }
                                            }

                                            // ★ + title — clipped to cell_w, never wider.
                                            ui.horizontal(|ui| {
                                                ui.set_max_width(cell_w);
                                                let star = if result.is_favorite { "★" } else { "☆" };
                                                if ui.small_button(star).clicked() {
                                                    self.pending_command = Some(UiCommand::SetFavorite {
                                                        database_path: self.config.database_path.clone(),
                                                        image_id: result.image_id,
                                                        is_favorite: !result.is_favorite,
                                                        refresh: self.search_refresh(),
                                                    });
                                                }
                                                let selected = self.selected_result == Some(index);
                                                // Use small() text to keep the title within
                                                // the image width. Pre-clip to 16 chars as a
                                                // safety net; hover shows the full title.
                                                let title_text = egui::RichText::new(
                                                    truncate(result.page_title(), 16)
                                                ).small();
                                                if ui.selectable_label(selected, title_text)
                                                    .on_hover_text(result.page_title())
                                                    .clicked()
                                                {
                                                    self.selected_result = Some(index);
                                                    self.search_detail_open = !self.bulk_select_mode;
                                                }
                                            });

                                            // Actors / source — egui truncates to cell_w with "…".
                                            let actors = result.actors();
                                            if !actors.is_empty() {
                                                ui.add(egui::Label::new(
                                                    egui::RichText::new(actors.join(", ")).small()
                                                ).truncate());
                                            }
                                            if let Some(source) = result.display_source() {
                                                ui.add(egui::Label::new(
                                                    egui::RichText::new(source).small()
                                                ).truncate());
                                            }
                                        },
                                    );
                                }
                            });
                        }
                    });
                    if let Some(cursor) = self.search_next_cursor.clone() {
                        if ui
                            .add_enabled(!self.running, egui::Button::new("Load more"))
                            .clicked()
                        {
                            self.search_seq += 1;
                            if self.recent_pages_active {
                                self.pending_command = Some(UiCommand::SearchRecentPages {
                                    database_path: self.config.database_path.clone(),
                                    page_limit: self.recent_pages_limit,
                                    seq: self.search_seq,
                                    cursor: Some(cursor),
                                    append: true,
                                });
                            } else {
                                self.pending_command = Some(UiCommand::Search {
                                    database_path: self.config.database_path.clone(),
                                    query: match self.search_refresh() {
                                        SearchRefresh::Query(query) => query,
                                        SearchRefresh::RecentPages(_) => unreachable!(),
                                    },
                                    seq: self.search_seq,
                                    cursor: Some(cursor),
                                    append: true,
                                });
                            }
                        }
                    }
                }
            });

            }
            if show_details {
                if split {
                    ui.separator();
                }
                let detail_w = ui.available_width().max(1.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(detail_w, avail.y),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.set_width(detail_w);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button(if details_only { "Back to results" } else { "Close details" }).clicked() {
                                self.search_detail_open = false;
                            }
                            ui.heading("Details");
                        });
            egui::ScrollArea::vertical()
                .id_salt("detail_scroll")
                .show(ui, |ui| {
                    if let Some(index) = self.selected_result {
                        if let Some(result) = self.search_results.get(index) {
                            if !self.hydrated_image_ids.contains(&(result.page_id(), result.image_id))
                                && !self.metadata_requested_ids.contains(&result.image_id)
                            {
                                metadata_to_request.insert(result.image_id);
                            }
                            ui.add(egui::Label::new(egui::RichText::new(result.page_title()).heading()).wrap().selectable(true));
                            ui.hyperlink_to("Open scene/page", result.page_url());
                            if self.download_status.render(ui, &self.config, result.page_id(), result.page_url()) {
                                self.pending_command = Some(UiCommand::RepairPageImages { url: result.page_url().to_owned() });
                            }
                            ui.collapsing("Links and paths", |ui| {
                                ui.add(egui::Label::new(result.page_url()).wrap().selectable(true));
                                ui.add(egui::Label::new(&result.source_url).wrap().selectable(true));
                                if let Some(path) = &result.local_file_path {
                                    ui.add(egui::Label::new(path).wrap().selectable(true));
                                }
                            });

                            let actors = result.actors();
                            if !actors.is_empty() {
                                ui.label(format!("Actors: {}", actors.join(", ")));
                            }
                            if let Some(source) = result.display_source() {
                                ui.label(format!("Source: {}", source));
                            }
                            let date = crate::fs::torrent::production_date(&result.attributes).or_else(|| {
                                result.attributes.iter().find(|attribute| attribute.kind == "date").map(|date| date.value.as_str())
                            });
                            if let Some(date) = date {
                                ui.label(format!("Date: {date}"));
                            }
                            if result.attributes.iter().any(|attribute| matches!(attribute.kind.as_str(), "studio_code" | "duration" | "director")) {
                                ui.horizontal_wrapped(|ui| {
                                    for (kind, label) in [
                                        ("studio_code", "Studio Code"),
                                        ("duration", "Duration"),
                                        ("director", "Director"),
                                    ] {
                                        if let Some(attribute) = result.attributes.iter().find(|attribute| attribute.kind == kind) {
                                            ui.label(egui::RichText::new(format!("{label}:")).strong());
                                            ui.label(&attribute.value);
                                            ui.add_space(10.0);
                                        }
                                    }
                                });
                            }
                            for (kind, label) in [("description", "Description"), ("full_description", "Full description")] {
                              if let Some(description) = result.attributes.iter().find(|attribute| attribute.kind == kind) {
                                if ui.button(label).on_hover_text("Open the complete text in a wide window").clicked() {
                                    self.description_popup = Some(DescriptionPopup {
                                        title: result.page_title().to_string(),
                                        text: description.value.clone(),
                                    });
                                }
                              }
                            }

                            if let Some(path) = &result.local_file_path {
                                let full = resolve_preview_path(&base_dir, path);
                                if ui.button("Open in Explorer").clicked() {
                                    open_in_explorer(&full);
                                }
                            }

                            let star = if result.is_favorite { "★ Unfavorite" } else { "☆ Favorite" };
                            if ui.button(star).clicked() {
                                self.pending_command = Some(UiCommand::SetFavorite {
                                    database_path: self.config.database_path.clone(),
                                    image_id: result.image_id,
                                    is_favorite: !result.is_favorite,
                                    refresh: self.search_refresh(),
                                });
                            }

                            if let Some(texture) = load_preview_texture(
                                ctx,
                                &mut self.preview_cache,
                                &mut self.preview_loader,
                                result,
                                &base_dir,
                                &self.thumb_dir,
                                ImageLoadMode::Preview,
                            ) {
                                let size = texture.size_vec2();
                                let max = egui::vec2(260.0, 260.0);
                                let scale = (max.x / size.x).min(max.y / size.y).min(1.0);
                                let response = ui
                                    .add(
                                        egui::Image::from_texture((texture.id(), size))
                                            .fit_to_exact_size(size * scale),
                                    );
                                if should_open_preview(self.config.preview_open_mode, &response) {
                                    self.viewer_open = Some(index);
                                }
                                ui.small(match self.config.preview_open_mode {
                                    PreviewOpenMode::DoubleClick => {
                                        "Double-click image to open viewer"
                                    }
                                    PreviewOpenMode::SingleClick => "Click image to open viewer",
                                });
                            }

                            ui.separator();

                            // Folder name of the current primary actor (from local_file_path).
                            let current_primary_dir = result
                                .local_file_path
                                .as_deref()
                                .and_then(primary_actor_from_path);

                            let has_local_file = result.local_file_path.is_some()
                                && !result.attributes.iter().any(|a| matches!(a.kind.as_str(), "attachment" | "magnet" | "primary_studio"));

                            for attribute in result.attributes.iter() {
                                if matches!(attribute.kind.as_str(), "date" | "description" | "full_description" | "studio_code" | "duration" | "director") {
                                    continue;
                                }
                                if attribute.kind == "resource" {
                                    ui.hyperlink_to(format!("Resource: {}", attribute.value), &attribute.value);
                                    continue;
                                }
                                if attribute.kind == "attachment" {
                                    if let Some(url) = &attribute.source_url {
                                        ui.hyperlink_to(format!("Attachment: {}", attribute.label), url);
                                    }
                                    continue;
                                }
                                if attribute.kind == "magnet" {
                                    if ui.button("Copy magnet link").clicked() {
                                        ui.ctx().copy_text(attribute.value.clone());
                                    }
                                    continue;
                                }
                                if attribute.kind == "actor" {
                                    let gender_badge = match attribute.is_female {
                                        Some(true) => " ♀",
                                        Some(false) => " ♂",
                                        None => "",
                                    };

                                    // Determine whether this actor is currently the primary one.
                                    let this_actor_dir = actor_folder_name(&attribute.value);
                                    let is_primary = current_primary_dir
                                        .as_deref()
                                        .map(|d| d == this_actor_dir)
                                        .unwrap_or(false);
                                    // Compute all self-borrows before closures to avoid conflicts.
                                    let row_refresh = self.search_refresh();
                                    let row_db = self.config.database_path.clone();
                                    let row_actor_name = attribute.value.clone();
                                    let row_page_id = result.page_id();
                                    let is_running = self.running;
                                    let row_attr_label = kind_display_label(&attribute.kind).to_string();
                                    let row_attr_kind = attribute.kind.clone();
                                    let gender_image_ids = image_ids_with_actor(
                                        &self.search_results,
                                        &row_actor_name,
                                    );

                                    ui.horizontal_wrapped(|ui| {
                                        // Primary-actor radio — only shown when a file exists on disk.
                                        if has_local_file {
                                            let hover = if is_primary {
                                                "Current primary actor (folder owner)"
                                            } else {
                                                "Set as primary actor — moves all page images to this actor's folder"
                                            };
                                            let resp = ui
                                                .add_enabled(!is_running, egui::RadioButton::new(is_primary, ""))
                                                .on_hover_text(hover);
                                            if resp.clicked() && !is_primary {
                                                self.pending_command =
                                                    Some(UiCommand::SetPrimaryActor {
                                                        database_path: row_db.clone(),
                                                        download_folder: base_dir.clone(),
                                                        page_id: row_page_id,
                                                        actor_name: row_actor_name.clone(),
                                                        refresh: row_refresh.clone(),
                                                    });
                                            }
                                        }

                                        ui.label(format!(
                                            "{} [{}] = {}{}",
                                            row_attr_label, row_attr_kind, row_actor_name, gender_badge
                                        ));

                                        let refresh = row_refresh.clone();
                                        let database_path = row_db.clone();
                                        let actor_name = row_actor_name.clone();
                                        let selected_text = match attribute.is_female {
                                            Some(true) => "♀",
                                            Some(false) => "♂",
                                            None => "?",
                                        };

                                        egui::ComboBox::from_id_salt(format!(
                                            "actor_gender_{}_{}",
                                            result.image_id, actor_name
                                        ))
                                        .selected_text(selected_text)
                                        .width(36.0)
                                        .show_ui(ui, |ui| {
                                            if ui
                                                .selectable_label(
                                                    attribute.is_female.is_none(),
                                                    "?",
                                                )
                                                .on_hover_text("Unknown")
                                                .clicked()
                                            {
                                                self.pending_command =
                                                    Some(UiCommand::SetActorGender {
                                                        database_path: database_path.clone(),
                                                        actor_name: actor_name.clone(),
                                                        is_female: None,
                                                        image_ids: gender_image_ids.clone(),
                                                        refresh: refresh.clone(),
                                                    });
                                            }
                                            if ui
                                                .selectable_label(
                                                    attribute.is_female == Some(true),
                                                    "♀",
                                                )
                                                .on_hover_text("Female")
                                                .clicked()
                                            {
                                                self.pending_command =
                                                    Some(UiCommand::SetActorGender {
                                                        database_path: database_path.clone(),
                                                        actor_name: actor_name.clone(),
                                                        is_female: Some(true),
                                                        image_ids: gender_image_ids.clone(),
                                                        refresh: refresh.clone(),
                                                    });
                                            }
                                            if ui
                                                .selectable_label(
                                                    attribute.is_female == Some(false),
                                                    "♂",
                                                )
                                                .on_hover_text("Male")
                                                .clicked()
                                            {
                                                self.pending_command =
                                                    Some(UiCommand::SetActorGender {
                                                        database_path,
                                                        actor_name,
                                                        is_female: Some(false),
                                                        image_ids: gender_image_ids.clone(),
                                                        refresh,
                                                    });
                                            }
                                        });
                                    });
                                } else {
                                    // tag / category / studio / channel — right-click chip.
                                    // Pre-compute everything that borrows `self` before
                                    // entering the context_menu closure (borrow-checker).
                                    let chip_kind  = attribute.kind.clone();
                                    let chip_value = attribute.value.clone();
                                    let chip_label = format!(
                                        "{} [{}] = {}",
                                        kind_display_label(&chip_kind),
                                        chip_kind,
                                        chip_value,
                                    );

                                    let db      = self.config.database_path.clone();
                                    let refresh = self.search_refresh();
                                    let img_id  = result.image_id;
                                    let page_id = result.page_id();

                                    // All image-ids belonging to this page (for bulk ops).
                                    let page_image_ids: Vec<i64> = self.search_results
                                        .iter()
                                        .filter(|r| r.page_id() == page_id)
                                        .map(|r| r.image_id)
                                        .collect();
                                    let page_count = page_image_ids.len();

                                    // Build all commands upfront.
                                    let cmd_remove_single = attr_remove_cmd(
                                        &chip_kind, db.clone(), vec![img_id], chip_value.clone(), refresh.clone(),
                                    );
                                    let cmd_remove_page = attr_remove_cmd(
                                        &chip_kind, db.clone(), page_image_ids.clone(), chip_value.clone(), refresh.clone(),
                                    );
                                    let cmd_add_page = attr_add_cmd(
                                        &chip_kind, db.clone(), page_image_ids, chip_value.clone(), refresh.clone(),
                                    );
                                    // For "filter" we need a bool about whether a filter field exists.
                                    let has_filter_field = matches!(
                                        chip_kind.as_str(), "tag" | "category" | "studio" | "channel"
                                    );

                                    let chip_resp = ui.label(egui::RichText::new(&chip_label));

                                    // Collect what the menu wants to do — bools/commands.
                                    let mut do_filter        = false;
                                    let mut pending: Option<UiCommand> = None;

                                    chip_resp.context_menu(|ui| {
                                        ui.label(egui::RichText::new(&chip_value).strong());
                                        ui.separator();

                                        // Filter
                                        if has_filter_field {
                                            if ui.button(format!("🔍  Filter by \"{}\"", chip_value)).clicked() {
                                                do_filter = true;
                                                ui.close_menu();
                                            }
                                        }

                                        ui.separator();

                                        // Remove from this image
                                        if let Some(ref cmd) = cmd_remove_single {
                                            if ui.button("🗑  Remove from this image")
                                                .on_hover_text("Removes the attribute only from this image")
                                                .clicked()
                                            {
                                                pending = Some(cmd.clone());
                                                ui.close_menu();
                                            }
                                        }

                                        // Remove from the whole content item
                                        if let Some(ref cmd) = cmd_remove_page {
                                            if ui.button(format!("🗑  Remove from all {} images of this content item", page_count))
                                                .on_hover_text("Removes the attribute from every image in this content item")
                                                .clicked()
                                            {
                                                pending = Some(cmd.clone());
                                                ui.close_menu();
                                            }
                                        }

                                        ui.separator();

                                        // Add to the whole content item
                                        if let Some(ref cmd) = cmd_add_page {
                                            if ui.button(format!("➕  Add to all {} images of this content item", page_count))
                                                .on_hover_text("Adds this attribute to every image in the content item (idempotent)")
                                                .clicked()
                                            {
                                                pending = Some(cmd.clone());
                                                ui.close_menu();
                                            }
                                        }
                                    });

                                    // Apply results outside the closure.
                                    if do_filter {
                                        match chip_kind.as_str() {
                                            "tag"      => self.search_form.tags       = chip_value.clone(),
                                            "category" => self.search_form.categories = chip_value.clone(),
                                            "studio"   => self.search_form.studios    = chip_value.clone(),
                                            "channel"  => self.search_form.channels   = chip_value.clone(),
                                            _ => {}
                                        }
                                        self.active_tab = Tab::Search;
                                    }
                                    if let Some(cmd) = pending {
                                        self.pending_command = Some(cmd);
                                    }
                                }
                            }
                        }
                    } else {
                        ui.label("Select a result to inspect its details.");
                    }
                }); // ScrollArea
                });
            }
        });
        if previous_selection != (self.selected_result, self.search_detail_open) {
            ctx.request_repaint();
        }

        if self.pending_command.is_none() && !metadata_to_request.is_empty() {
            let image_ids = metadata_to_request.into_iter().collect::<Vec<_>>();
            self.metadata_requested_ids
                .extend(image_ids.iter().copied());
            self.pending_command = Some(UiCommand::HydrateSearchMetadata {
                database_path: self.config.database_path.clone(),
                image_ids,
                seq: self.search_seq,
            });
        }
    }

    // ── Search filters ────────────────────────────────────────────────────────

    fn start_image_search(&mut self) {
        self.run_image_query(self.search_form.to_query());
    }

    fn run_image_query(&mut self, query: SearchQuery) {
        self.error = None;
        if self.applied_search != query || self.date_browse_active || self.recent_pages_active {
            self.set_search_results(Vec::new());
        }
        self.recent_pages_active = false;
        self.date_browse_active = false;
        self.date_browse_loading = false;
        self.search_seq += 1;
        self.applied_search = query.clone();
        self.pending_command = Some(UiCommand::Search {
            database_path: self.config.database_path.clone(),
            query,
            seq: self.search_seq,
            cursor: None,
            append: false,
        });
    }

    fn start_recent_pages(&mut self) {
        self.error = None;
        self.recent_pages_active = true;
        self.date_browse_active = false;
        self.date_browse_loading = false;
        self.search_seq += 1;
        self.pending_command = Some(UiCommand::SearchRecentPages {
            database_path: self.config.database_path.clone(),
            page_limit: self.recent_pages_limit,
            seq: self.search_seq,
            cursor: None,
            append: false,
        });
    }

    fn render_image_search_controls(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_width();
        let draft = self.search_form.to_query();
        let count = image_filter_count(&draft);
        let dirty = draft != self.applied_search;
        ui.horizontal_wrapped(|ui| {
            let text = ui.add(
                egui::TextEdit::singleline(&mut self.search_form.text)
                    .hint_text("Search images")
                    .desired_width((available * 0.32).clamp(115.0, 320.0)),
            );
            site_selector(
                ui,
                "search_site_filter",
                &mut self.search_form.site_key,
                self.library_sites.as_deref().unwrap_or_default(),
            );
            let tags = ui.add(
                egui::TextEdit::singleline(&mut self.search_form.tags)
                    .hint_text("Tags")
                    .desired_width((available * 0.19).clamp(86.0, 175.0)),
            );
            if tags.changed() {
                let query = last_comma_token(&self.search_form.tags);
                self.last_tag_query.clone_from(&query);
                self.tag_suggestions.clear();
                if !query.is_empty() {
                    self.pending_command = Some(UiCommand::SuggestTags {
                        database_path: self.config.database_path.clone(),
                        query,
                    });
                }
            }
            let enter = (text.lost_focus() || tags.lost_focus())
                && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add_enabled(
                    !self.running,
                    egui::Button::new(if dirty { "Apply filters" } else { "Run Search" }),
                )
                .clicked()
                || (!self.running && enter)
            {
                self.start_image_search();
            }
            if ui.button(format!("Filters ({count})")).clicked() {
                self.search_filters_open = !self.search_filters_open;
            }
            ui.menu_button("Browse", |ui| {
                if ui
                    .add_enabled(!self.running, egui::Button::new("By Date"))
                    .clicked()
                {
                    self.request_date_group(DateBrowseDirection::Initial);
                    ui.close_menu();
                }
                ui.label("Recent pages");
                ui.add(
                    egui::DragValue::new(&mut self.recent_pages_limit)
                        .range(1..=200)
                        .suffix(" pages"),
                );
                if ui
                    .add_enabled(!self.running, egui::Button::new("Show recent"))
                    .clicked()
                {
                    self.start_recent_pages();
                    ui.close_menu();
                }
                if (self.recent_pages_active || self.date_browse_active)
                    && ui
                        .add_enabled(!self.running, egui::Button::new("Return to search"))
                        .clicked()
                {
                    self.start_image_search();
                    ui.close_menu();
                }
            });
        });
        if !self.tag_suggestions.is_empty() {
            render_suggestions(ui, &mut self.search_form.tags, &mut self.tag_suggestions);
        }

        let query = match self.search_refresh() {
            SearchRefresh::Query(query) => query,
            SearchRefresh::RecentPages(_) => return,
        };
        let filter_count = image_filter_count(&query) + usize::from(!query.text.is_empty());
        if query.page_id.is_some() && !self.date_browse_active {
            ui.weak("Showing images from one saved card");
        }
        if filter_count > 0 {
            let mut removed = false;
            let chips_enabled = !self.running
                && (self.date_browse_active || self.search_form.to_query() == self.applied_search);
            ui.add_enabled_ui(chips_enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Applied");
                    let mut remaining = 6;
                    if let Some(site) = &query.site_key {
                        if filter_chip(ui, "Site", site, &mut remaining) {
                            self.search_form.site_key.clear();
                            removed = true;
                        }
                    }
                    if !self.date_browse_active {
                        if !query.text.is_empty()
                            && filter_chip(ui, "Text", &query.text, &mut remaining)
                        {
                            self.search_form.text.clear();
                            removed = true;
                        }
                        for (label, field, values) in [
                            ("Tag", &mut self.search_form.tags, &query.tags),
                            ("Actor", &mut self.search_form.actors, &query.actors),
                            (
                                "Category",
                                &mut self.search_form.categories,
                                &query.categories,
                            ),
                            ("Studio", &mut self.search_form.studios, &query.studios),
                            ("Channel", &mut self.search_form.channels, &query.channels),
                        ] {
                            removed |= filter_value_chips(ui, label, field, values, &mut remaining);
                        }
                        for value in &query.creators {
                            if filter_chip(ui, "Creator", value, &mut remaining) {
                                self.search_form.creators.remove(value);
                                removed = true;
                            }
                        }
                        if query.favorites_only
                            && filter_chip(ui, "Favorites", "Only", &mut remaining)
                        {
                            self.search_form.favorites_only = false;
                            removed = true;
                        }
                        if filter_count > 6 {
                            ui.weak(format!("+{}", filter_count - 6));
                        }
                    }
                })
            });
            if removed {
                self.clear_search_suggestions();
                if !self.date_browse_active {
                    self.start_image_search();
                }
            }
        }
    }

    fn clear_search_suggestions(&mut self) {
        self.tag_suggestions.clear();
        self.actor_suggestions.clear();
        self.category_suggestions.clear();
        self.studio_suggestions.clear();
        self.last_tag_query.clear();
        self.last_actor_query.clear();
        self.last_category_query.clear();
        self.last_studio_query.clear();
    }

    fn render_search_filters(&mut self, ui: &mut egui::Ui) {
        let label_width = 112.0;
        let row_height = ui.spacing().interact_size.y;
        let field_width = search_filter_field_width(
            ui.available_width(),
            label_width,
            ui.spacing().item_spacing.x,
        );
        let field_size = [field_width, row_height];

        egui::Grid::new("search_filter_grid")
            .num_columns(2)
            .spacing([ui.spacing().item_spacing.x, 4.0])
            .show(ui, |ui| {
                search_filter_label(ui, "", label_width, row_height);
                ui.checkbox(&mut self.search_form.favorites_only, "Favorites only");
                ui.end_row();

                // Actors
                search_filter_label(ui, "Actors", label_width, row_height);
                let resp = ui.add_sized(
                    field_size,
                    egui::TextEdit::singleline(&mut self.search_form.actors),
                );
                let cur = last_comma_token(&self.search_form.actors);
                if resp.changed() && cur != self.last_actor_query {
                    self.last_actor_query = cur.clone();
                    self.actor_suggestions.clear();
                    if !cur.is_empty() {
                        self.pending_command = Some(UiCommand::SuggestActors {
                            database_path: self.config.database_path.clone(),
                            query: cur,
                        });
                    }
                }
                ui.end_row();
                if !self.actor_suggestions.is_empty() {
                    search_filter_label(ui, "", label_width, row_height);
                    render_suggestions(
                        ui,
                        &mut self.search_form.actors,
                        &mut self.actor_suggestions,
                    );
                    ui.end_row();
                }

                // Categories
                search_filter_label(ui, "Categories", label_width, row_height);
                let resp = ui.add_sized(
                    field_size,
                    egui::TextEdit::singleline(&mut self.search_form.categories),
                );
                let cur = last_comma_token(&self.search_form.categories);
                if resp.changed() && cur != self.last_category_query {
                    self.last_category_query = cur.clone();
                    self.category_suggestions.clear();
                    if !cur.is_empty() {
                        self.pending_command = Some(UiCommand::SuggestCategories {
                            database_path: self.config.database_path.clone(),
                            query: cur,
                        });
                    }
                }
                ui.end_row();
                if !self.category_suggestions.is_empty() {
                    search_filter_label(ui, "", label_width, row_height);
                    render_suggestions(
                        ui,
                        &mut self.search_form.categories,
                        &mut self.category_suggestions,
                    );
                    ui.end_row();
                }

                // Studios (Task 5: autocomplete)
                search_filter_label(ui, "Studios", label_width, row_height);
                let resp = ui.add_sized(
                    field_size,
                    egui::TextEdit::singleline(&mut self.search_form.studios),
                );
                let cur = last_comma_token(&self.search_form.studios);
                if resp.changed() && cur != self.last_studio_query {
                    self.last_studio_query = cur.clone();
                    self.studio_suggestions.clear();
                    if !cur.is_empty() {
                        self.pending_command = Some(UiCommand::SuggestStudios {
                            database_path: self.config.database_path.clone(),
                            query: cur,
                        });
                    }
                }
                ui.end_row();
                if !self.studio_suggestions.is_empty() {
                    search_filter_label(ui, "", label_width, row_height);
                    render_suggestions(
                        ui,
                        &mut self.search_form.studios,
                        &mut self.studio_suggestions,
                    );
                    ui.end_row();
                }

                search_filter_label(ui, "Director / Author", label_width, row_height);
                if let Some(command) =
                    self.search_form
                        .creators
                        .input(ui, field_size, &self.config.database_path)
                {
                    self.pending_command = Some(command);
                }
                ui.end_row();
                if self.search_form.creators.has_suggestions() {
                    search_filter_label(ui, "", label_width, row_height);
                    self.search_form.creators.render_suggestions(ui);
                    ui.end_row();
                }

                // Channels (no autocomplete)
                search_filter_label(ui, "Channels", label_width, row_height);
                ui.add_sized(
                    field_size,
                    egui::TextEdit::singleline(&mut self.search_form.channels),
                );
                ui.end_row();
            });
        let columns = self.config.search_columns;
        ui.horizontal_wrapped(|ui| {
            ui.label("Maximum grid columns");
            ui.add(egui::DragValue::new(&mut self.config.search_columns).range(1..=10));
            if ui.button("Reset filters").clicked() {
                self.search_form = SearchForm::default();
                self.clear_search_suggestions();
            }
        });
        if columns != self.config.search_columns {
            self.save_config_setting("search columns");
        }
    }

    // ── Bulk edit panel ───────────────────────────────────────────────────────

    fn render_bulk_edit_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Edit Selected Images");
        let n = self.selected_image_ids.len();
        ui.label(format!("Selected: {}", n));
        if n == 0 {
            return;
        }

        let db = self.config.database_path.clone();
        let ids: Vec<i64> = self.selected_image_ids.iter().copied().collect();
        let refresh = self.search_refresh();

        self.render_edit_row(
            ui,
            "Tags",
            db.clone(),
            ids.clone(),
            refresh.clone(),
            EditKind::Tag,
        );
        self.render_edit_row(
            ui,
            "Categories",
            db.clone(),
            ids.clone(),
            refresh.clone(),
            EditKind::Category,
        );
        self.render_edit_row(
            ui,
            "Actors",
            db.clone(),
            ids.clone(),
            refresh.clone(),
            EditKind::Actor,
        );
        self.render_edit_row(ui, "Studios", db, ids, refresh, EditKind::Studio);
    }

    fn render_edit_row(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        db: String,
        ids: Vec<i64>,
        refresh: SearchRefresh,
        kind: EditKind,
    ) {
        let (input, suggestions, last_q) = match kind {
            EditKind::Tag => (
                &mut self.edit_tag_input,
                &mut self.edit_tag_suggestions,
                &mut self.last_edit_tag_query,
            ),
            EditKind::Category => (
                &mut self.edit_category_input,
                &mut self.edit_category_suggestions,
                &mut self.last_edit_category_query,
            ),
            EditKind::Actor => (
                &mut self.edit_actor_input,
                &mut self.edit_actor_suggestions,
                &mut self.last_edit_actor_query,
            ),
            EditKind::Studio => (
                &mut self.edit_studio_input,
                &mut self.edit_studio_suggestions,
                &mut self.last_edit_studio_query,
            ),
        };

        ui.horizontal(|ui| {
            ui.label(label);
            let resp = ui.text_edit_singleline(input);
            let cur = last_comma_token(input);
            if resp.changed() && cur != *last_q {
                *last_q = cur.clone();
                suggestions.clear();
                if !cur.is_empty() {
                    self.pending_command = Some(kind.suggest_command(db.clone(), cur));
                }
            }

            let has_val = !input.trim().is_empty();
            if ui.add_enabled(has_val, egui::Button::new("Add")).clicked() {
                let vals = split_filter_values(input);
                self.pending_command =
                    Some(kind.add_command(db.clone(), ids.clone(), vals, refresh.clone()));
            }
            if ui
                .add_enabled(has_val, egui::Button::new("Remove"))
                .clicked()
            {
                let vals = split_filter_values(input);
                self.pending_command =
                    Some(kind.remove_command(db.clone(), ids.clone(), vals, refresh.clone()));
            }
        });
        render_suggestions(ui, input, suggestions);
    }

    // ── Viewer / lightbox (Task 6) ────────────────────────────────────────────
    //
    // Rendered as an independent OS-level window via show_viewport_immediate so
    // it can be freely moved anywhere on screen — including outside the main
    // window and onto secondary monitors.

    fn render_viewer(&mut self, ctx: &egui::Context) {
        let index = match self.viewer_open {
            Some(i) => i,
            None => return,
        };

        let total = self.search_results.len();
        if total == 0 {
            self.viewer_open = None;
            return;
        }
        let index = index.min(total - 1);

        // Clone the data the viewport closure needs so it does not borrow self.
        let result = self.search_results[index].clone();

        // Pre-clone adjacent results for thumbnail preloading inside the closure.
        let prev_result = if index > 0 {
            self.search_results.get(index - 1).cloned()
        } else {
            None
        };
        let next_result = self.search_results.get(index + 1).cloned();

        // All mutation intent collected into locals; applied after the closure.
        let mut nav_to: Option<usize> = None;
        let mut close_viewer = false;
        let mut toggle_fav = false;
        let mut open_description = false;
        let mut trigger_delete = false;
        let mut viewer_delete_confirmed = false;
        let mut viewer_delete_cancelled = false;
        let is_favorite = result.is_favorite;
        let has_description = result
            .attributes
            .iter()
            .any(|attribute| matches!(attribute.kind.as_str(), "description" | "full_description"));
        let db_path = self.config.database_path.clone();
        let viewer_refresh = self.search_refresh();
        let is_running = self.running;

        // Zoom / pan — written back to self after the closure.
        let mut viewer_zoom = self.viewer_zoom;
        let mut viewer_pan = self.viewer_pan;
        let mut viewer_image_mode = self.viewer_image_mode;
        let mut viewer_original_blend = self.viewer_original_blend;
        let mut viewer_image_transition = self.viewer_image_transition;
        let original_available = result.local_file_path.is_some();

        // Tag-input state is cloned into locals so it can be mutated inside the
        // viewport closure, then written back to self after the closure returns.
        let mut viewer_tag_input = self.viewer_tag_input.clone();
        let mut viewer_tag_suggestions = self.viewer_tag_suggestions.clone();
        let mut viewer_last_tag_query = self.viewer_last_tag_query.clone();
        let viewer_recent_tags = self.viewer_recent_tags.clone();
        let mut viewer_add_tags: Option<Vec<String>> = None;
        let mut viewer_suggest_query: Option<String> = None;
        let base_dir = self
            .config
            .effective_download_dir()
            .to_string_lossy()
            .into_owned();
        // We DO need the preview cache and the viewer delete state inside the
        // viewport; borrow them explicitly so the closure captures &mut to the
        // individual fields rather than to all of self.
        let preview_cache = &mut self.preview_cache;
        let viewer_original_cache = &mut self.viewer_original_cache;
        let preview_loader = &mut self.preview_loader;
        let original_loader = &mut self.original_loader;
        let viewer_delete_confirm = &mut self.viewer_delete_confirm;
        let thumb_dir = self.thumb_dir.clone();

        let title = format!(
            "{} — {} / {}",
            truncate(result.page_title(), 60),
            index + 1,
            total
        );

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("image_viewer"),
            application_viewport()
                .with_title(&title)
                .with_inner_size([900.0, 700.0]),
            |vp_ctx, _class| {
                // ── Keyboard + scroll input ───────────────────────────────────
                let wants_keyboard_input = vp_ctx.wants_keyboard_input();
                vp_ctx.input(|i| {
                    // Closing the OS viewport must work independently of widget focus.
                    if i.viewport().close_requested() {
                        close_viewer = true;
                    }

                    if viewer_delete_shortcut_requested(&i.events, wants_keyboard_input) {
                        trigger_delete = true;
                    }

                    // Text fields own all keyboard input while focused. This avoids
                    // navigation, mode changes, favorites, and viewer deletion while
                    // the user edits tags (and protects future text fields as well).
                    if !wants_keyboard_input {
                        // Navigation
                        if i.key_pressed(egui::Key::ArrowLeft)
                            || i.key_pressed(egui::Key::ArrowUp)
                        {
                            if index > 0 { nav_to = Some(index - 1); }
                        }
                        if i.key_pressed(egui::Key::ArrowRight)
                            || i.key_pressed(egui::Key::ArrowDown)
                            || i.key_pressed(egui::Key::Space)
                        {
                            if index + 1 < total { nav_to = Some(index + 1); }
                        }
                        if i.key_pressed(egui::Key::Escape) {
                            close_viewer = true;
                        }
                        if i.key_pressed(egui::Key::F) {
                            toggle_fav = true;
                        }
                        if i.key_pressed(egui::Key::O) && original_available {
                            viewer_image_mode = match viewer_image_mode {
                                ImageLoadMode::Preview => ImageLoadMode::Original,
                                ImageLoadMode::Original => ImageLoadMode::Preview,
                            };
                        }
                        if i.key_pressed(egui::Key::Num0) {
                            viewer_zoom = 1.0;
                            viewer_pan  = egui::Vec2::ZERO;
                        }
                    }

                    // Zoom with mouse wheel (Ctrl+scroll or plain scroll)
                    let scroll_y = i.smooth_scroll_delta.y;
                    if scroll_y != 0.0 {
                        let factor = (1.0 + scroll_y * 0.0025).clamp(0.5, 2.0);
                        viewer_zoom = (viewer_zoom * factor).clamp(0.1, 50.0);
                    }
                });

                // ── Delete key: arm the confirmation modal ────────────────────
                if trigger_delete && viewer_delete_confirm.is_none() {
                    *viewer_delete_confirm = Some(DeleteConfirmState {
                        image_ids: vec![result.image_id],
                        mode: DeleteMode::DbOnly,
                    });
                }

                // ── Delete confirmation modal (shown in viewer window) ─────────
                if viewer_delete_confirm.is_some() {
                    let modal_resp = egui::Modal::new(egui::Id::new("viewer_delete_confirm"))
                        .show(vp_ctx, |ui| {
                            let state = viewer_delete_confirm.as_mut().unwrap();
                            ui.set_min_width(320.0);
                            ui.heading("Delete 1 image?");
                            ui.separator();
                            ui.radio_value(
                                &mut state.mode,
                                DeleteMode::DbOnly,
                                "Remove from database only",
                            );
                            ui.radio_value(
                                &mut state.mode,
                                DeleteMode::DbAndDisk,
                                "Remove from database AND delete files from disk",
                            );
                            if state.mode == DeleteMode::DbAndDisk {
                                ui.add_space(4.0);
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 80, 80),
                                    "⚠ This permanently deletes image files from disk.",
                                );
                            }
                            ui.add_space(8.0);
                            ui.separator();
                            ui.horizontal(|ui| {
                                if ui.button("Cancel").clicked() {
                                    viewer_delete_cancelled = true;
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(
                                                egui::Button::new("🗑 Delete")
                                                    .fill(egui::Color32::from_rgb(160, 30, 30))
                                                    .stroke(egui::Stroke::new(
                                                        1.0,
                                                        egui::Color32::from_rgb(220, 60, 60),
                                                    )),
                                            )
                                            .clicked()
                                        {
                                            viewer_delete_confirmed = true;
                                        }
                                    },
                                );
                            });
                        });
                    if modal_resp.should_close() {
                        viewer_delete_cancelled = true;
                    }
                }

                // ── Tag strip (bottom panel — declared before CentralPanel so it
                //    gets space first and the image fills what remains) ──────────
                egui::TopBottomPanel::bottom("viewer_tag_panel").show(vp_ctx, |ui| {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label("Tags:");
                        let resp = ui.add_sized(
                            [240.0, ui.spacing().interact_size.y],
                            egui::TextEdit::singleline(&mut viewer_tag_input),
                        );
                        let cur = last_comma_token(&viewer_tag_input);
                        if resp.changed() && cur != viewer_last_tag_query {
                            viewer_last_tag_query = cur.clone();
                            viewer_tag_suggestions.clear();
                            if !cur.is_empty() {
                                viewer_suggest_query = Some(cur);
                            }
                        }
                        let has_val = !viewer_tag_input.trim().is_empty();
                        if ui
                            .add_enabled(has_val && !is_running, egui::Button::new("Add"))
                            .on_hover_text("Add tag to this image")
                            .clicked()
                        {
                            viewer_add_tags = Some(split_filter_values(&viewer_tag_input));
                        }
                        if ui
                            .add_enabled(has_val, egui::Button::new("✕"))
                            .on_hover_text("Clear tag input")
                            .clicked()
                        {
                            viewer_tag_input.clear();
                            viewer_tag_suggestions.clear();
                            viewer_last_tag_query.clear();
                        }
                    });
                    render_suggestions(
                        ui,
                        &mut viewer_tag_input,
                        &mut viewer_tag_suggestions,
                    );

                    // ── Recent-tags quick-pick ────────────────────────────────
                    if !viewer_recent_tags.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new("Recent:").small().weak());
                            for tag in &viewer_recent_tags {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(tag.as_str()).small(),
                                        )
                                        .small(),
                                    )
                                    .on_hover_text("Click to insert into tag input")
                                    .clicked()
                                {
                                    // Append to existing input (comma-separated) or just set.
                                    if viewer_tag_input.trim().is_empty() {
                                        viewer_tag_input = tag.clone();
                                    } else {
                                        let base = viewer_tag_input.trim_end_matches([',', ' ']);
                                        viewer_tag_input = format!("{}, {}", base, tag);
                                    }
                                }
                            }
                        });
                    }

                    ui.add_space(2.0);
                });

                egui::CentralPanel::default().show(vp_ctx, |ui| {
                    // ── Navigation bar ────────────────────────────────────────
                    ui.horizontal(|ui| {
                        if ui.add_enabled(index > 0, egui::Button::new("◀ Prev")).clicked() {
                            nav_to = Some(index - 1);
                        }
                        ui.label(format!("{} / {}", index + 1, total));
                        if ui.add_enabled(index + 1 < total, egui::Button::new("Next ▶")).clicked() {
                            nav_to = Some(index + 1);
                        }

                        ui.separator();

                        ui.selectable_value(
                            &mut viewer_image_mode,
                            ImageLoadMode::Preview,
                            "Preview",
                        )
                        .on_hover_text("Use the optimized thumbnail");
                        ui.add_enabled_ui(original_available, |ui| {
                            ui.selectable_value(
                                &mut viewer_image_mode,
                                ImageLoadMode::Original,
                                "Original",
                            )
                            .on_hover_text(
                                "Use the downloaded image at its original resolution (O)",
                            );
                        })
                        .response
                        .on_disabled_hover_text("The original image has not been downloaded");

                        ui.separator();

                        // Zoom indicator + reset button
                        let zoom_pct = (viewer_zoom * 100.0).round() as u32;
                        if ui.button(format!("🔍 {}%", zoom_pct))
                            .on_hover_text("Reset zoom (0)")
                            .clicked()
                        {
                            viewer_zoom = 1.0;
                            viewer_pan  = egui::Vec2::ZERO;
                        }

                        ui.separator();
                        ui.small("← →  Space: next  •  O: preview/original  •  scroll: zoom  •  drag: pan  •  F: ★  •  Esc: close");

                        let fav_label = if is_favorite { "★" } else { "☆" };
                        let fav_hover = if is_favorite { "Remove from favorites (F)" } else { "Add to favorites (F)" };
                        if ui.button(fav_label).on_hover_text(fav_hover).clicked() {
                            toggle_fav = true;
                        }
                        if ui.add_enabled(has_description, egui::Button::new("📄"))
                            .on_hover_text("Open full description")
                            .clicked()
                        {
                            open_description = true;
                        }

                        if let Some(path) = &result.local_file_path {
                            let full = resolve_preview_path(&base_dir, path);
                            if ui
                                .button("Open folder")
                                .on_hover_text("Open the folder containing this image")
                                .clicked()
                            {
                                open_containing_folder(&full);
                            }
                        }
                        if ui.button("✕").on_hover_text("Close (Esc)").clicked() {
                            close_viewer = true;
                        }
                    });

                    ui.separator();

                    // ── Image with zoom / pan / click-to-navigate ─────────────
                    let mut original_fallback = None;
                    let preview_texture = load_preview_texture(
                        vp_ctx,
                        preview_cache,
                        preview_loader,
                        &result,
                        &base_dir,
                        &thumb_dir,
                        ImageLoadMode::Preview,
                    );
                    let original_texture = match viewer_image_mode {
                        ImageLoadMode::Original => match load_original_texture(
                            vp_ctx,
                            viewer_original_cache,
                            original_loader,
                            &result,
                            &base_dir,
                            &thumb_dir,
                            viewer_decode_max_side(viewer_zoom),
                        ) {
                            OriginalTextureState::Ready(texture) => Some(texture),
                            OriginalTextureState::Loading => {
                                original_fallback = Some(OriginalFallbackState::Loading);
                                None
                            }
                            OriginalTextureState::Unavailable => {
                                original_fallback = Some(OriginalFallbackState::Unavailable);
                                None
                            }
                        },
                        ImageLoadMode::Preview
                            if viewer_original_blend > 0.0
                                || viewer_image_transition.is_some() =>
                        {
                            get_cached_original_texture(
                                viewer_original_cache,
                                &result,
                                &base_dir,
                                &thumb_dir,
                            )
                        }
                        ImageLoadMode::Preview => None,
                    };

                    let target_blend = if viewer_image_mode == ImageLoadMode::Original
                        && original_texture.is_some()
                    {
                        1.0
                    } else {
                        0.0
                    };
                    let now = std::time::Instant::now();
                    (viewer_original_blend, viewer_image_transition) =
                        advance_viewer_image_transition(
                            result.image_id,
                            viewer_original_blend,
                            target_blend,
                            viewer_image_transition,
                            now,
                        );
                    if viewer_image_transition.is_some() {
                        vp_ctx.request_repaint();
                    }

                    if let Some(state) = original_fallback {
                        ui.horizontal(|ui| {
                            if state == OriginalFallbackState::Loading {
                                ui.add(egui::Spinner::new().size(14.0));
                            }
                            ui.small(original_fallback_message(state));
                        });
                    }
                    if preview_texture.is_some() || original_texture.is_some() {
                        let layout_texture = original_texture.or(preview_texture).unwrap();
                        let tex_size = layout_texture.size_vec2();
                        let area = ui.available_rect_before_wrap();

                        // Base scale: fit image inside the available area at zoom 1.0.
                        let base_scale = (area.width()  / tex_size.x)
                            .min(area.height() / tex_size.y)
                            .min(1.0);
                        let display_size = tex_size * base_scale * viewer_zoom;
                        let centre = area.center() + viewer_pan;
                        let image_rect = egui::Rect::from_center_size(centre, display_size);

                        // Allocate the full area so drag / click are captured everywhere.
                        let response = ui.allocate_rect(area, egui::Sense::click_and_drag());

                        // Cross-fade the original over the preview. Keeping both
                        // textures in the same rect prevents a visual jump when the
                        // high-resolution texture becomes available.
                        let uv = egui::Rect::from_min_max(
                            egui::pos2(0.0, 0.0),
                            egui::pos2(1.0, 1.0),
                        );
                        let effective_blend = if preview_texture.is_some() {
                            viewer_original_blend
                        } else {
                            1.0
                        };
                        if let Some(texture) = preview_texture {
                            let opacity = 1.0 - effective_blend;
                            if opacity > 0.0 {
                                ui.painter().image(
                                    texture.id(),
                                    image_rect,
                                    uv,
                                    white_with_opacity(opacity),
                                );
                            }
                        }
                        if let Some(texture) = original_texture {
                            if effective_blend > 0.0 {
                                ui.painter().image(
                                    texture.id(),
                                    image_rect,
                                    uv,
                                    white_with_opacity(effective_blend),
                                );
                            }
                        }

                        // Drag to pan (always available, most useful when zoomed).
                        if response.dragged() {
                            viewer_pan += response.drag_delta();
                        }

                        // Double-click: reset zoom/pan.
                        if response.double_clicked() {
                            viewer_zoom = 1.0;
                            viewer_pan  = egui::Vec2::ZERO;
                        }

                        // Single click: left third → prev, right third → next.
                        if response.clicked() && !response.double_clicked() {
                            if let Some(pos) = response.interact_pointer_pos() {
                                let rel = pos.x - area.center().x;
                                let third = area.width() / 3.0;
                                if rel < -third && index > 0 {
                                    nav_to = Some(index - 1);
                                } else if rel > third && index + 1 < total {
                                    nav_to = Some(index + 1);
                                }
                            }
                        }
                    } else {
                        ui.vertical_centered(|ui| {
                            ui.add_space(40.0);
                            ui.label(match viewer_image_mode {
                                ImageLoadMode::Preview => "No local preview available.",
                                ImageLoadMode::Original if original_available => {
                                    "Loading original image, or the local file is unavailable."
                                }
                                ImageLoadMode::Original => {
                                    "The original image has not been downloaded."
                                }
                            });
                            ui.small(&result.source_url);
                        });
                    }

                    // The bounded worker queue preloads adjacent images without
                    // blocking this viewport's render callback.
                    if let Some(ref adj) = prev_result {
                        load_preview_texture(
                            vp_ctx,
                            preview_cache,
                            preview_loader,
                            adj,
                            &base_dir,
                            &thumb_dir,
                            ImageLoadMode::Preview,
                        );
                    }
                    if let Some(ref adj) = next_result {
                        load_preview_texture(
                            vp_ctx,
                            preview_cache,
                            preview_loader,
                            adj,
                            &base_dir,
                            &thumb_dir,
                            ImageLoadMode::Preview,
                        );
                    }
                });
            },
        );

        if open_description {
            if let Some(description) = result
                .attributes
                .iter()
                .find(|attribute| attribute.kind == "full_description")
                .or_else(|| {
                    result
                        .attributes
                        .iter()
                        .find(|attribute| attribute.kind == "description")
                })
            {
                self.description_popup = Some(DescriptionPopup {
                    title: result.page_title().to_string(),
                    text: description.value.clone(),
                });
            }
        }

        // Apply navigation / close / zoom / favorite / tags AFTER closure finishes.
        let viewer_image_mode_changed = self.viewer_image_mode != viewer_image_mode;
        self.viewer_image_mode = viewer_image_mode;
        self.viewer_original_blend = viewer_original_blend;
        self.viewer_image_transition = viewer_image_transition;
        if close_viewer {
            self.viewer_open = None;
            self.viewer_zoom = 1.0;
            self.viewer_pan = egui::Vec2::ZERO;
            self.viewer_original_blend = 0.0;
            self.viewer_image_transition = None;
            self.original_loader.advance_generation();
            self.viewer_original_cache.clear();
        } else if let Some(new_idx) = nav_to {
            self.viewer_open = Some(new_idx);
            self.selected_result = Some(new_idx);
            // Reset zoom and pan when moving to a different image.
            self.viewer_zoom = 1.0;
            self.viewer_pan = egui::Vec2::ZERO;
            self.viewer_original_blend = 0.0;
            self.viewer_image_transition = None;
            if self.viewer_image_mode == ImageLoadMode::Original {
                self.original_loader.advance_generation();
            }
        } else if viewer_image_mode_changed {
            self.viewer_zoom = 1.0;
            self.viewer_pan = egui::Vec2::ZERO;
        } else {
            // Persist zoom/pan changes made this frame.
            self.viewer_zoom = viewer_zoom;
            self.viewer_pan = viewer_pan;
        }
        if toggle_fav {
            self.pending_command = Some(UiCommand::SetFavorite {
                database_path: db_path.clone(),
                image_id: result.image_id,
                is_favorite: !is_favorite,
                refresh: viewer_refresh.clone(),
            });
        }

        // ── Viewer delete confirmation result ─────────────────────────────────
        if viewer_delete_cancelled {
            self.viewer_delete_confirm = None;
        } else if viewer_delete_confirmed {
            if let Some(state) = self.viewer_delete_confirm.take() {
                // Close the viewer immediately — the image is about to disappear.
                self.viewer_open = None;
                self.viewer_zoom = 1.0;
                self.viewer_pan = egui::Vec2::ZERO;
                self.viewer_original_blend = 0.0;
                self.viewer_image_transition = None;
                self.original_loader.advance_generation();
                self.viewer_original_cache.clear();
                self.pending_command = Some(UiCommand::DeleteImages {
                    database_path: db_path.clone(),
                    base_dir: base_dir.clone(),
                    image_ids: state.image_ids,
                    mode: state.mode,
                    refresh: viewer_refresh.clone(),
                });
            }
        }

        // Write tag-input state back (persists across navigation).
        self.viewer_tag_input = viewer_tag_input;
        self.viewer_tag_suggestions = viewer_tag_suggestions;
        self.viewer_last_tag_query = viewer_last_tag_query;

        if let Some(query) = viewer_suggest_query {
            self.pending_command = Some(UiCommand::SuggestTags {
                database_path: db_path.clone(),
                query,
            });
        }
        if let Some(vals) = viewer_add_tags {
            // Push each newly-added tag to the front of viewer_recent_tags,
            // removing any duplicate, then keep only the most recent 10.
            for tag in vals.iter().rev() {
                self.viewer_recent_tags.retain(|t| t != tag);
                self.viewer_recent_tags.insert(0, tag.clone());
            }
            self.viewer_recent_tags.truncate(10);

            self.pending_command = Some(UiCommand::AddTagsToImages {
                database_path: db_path,
                image_ids: vec![result.image_id],
                values: vals,
                refresh: viewer_refresh,
            });
        }
    }

    // ── Delete confirmation modal ─────────────────────────────────────────────

    fn render_delete_confirm(&mut self, ctx: &egui::Context) {
        if self.delete_confirm.is_none() {
            return;
        }

        let mut confirmed = false;
        let mut cancelled = false;

        let modal_resp = egui::Modal::new(egui::Id::new("delete_confirm_modal")).show(ctx, |ui| {
            let state = self.delete_confirm.as_mut().unwrap();
            let image_count = state.image_ids.len();

            ui.set_min_width(320.0);

            ui.heading(format!("Delete {} image(s)?", image_count));
            ui.separator();

            ui.radio_value(
                &mut state.mode,
                DeleteMode::DbOnly,
                "Remove from database only",
            );
            ui.radio_value(
                &mut state.mode,
                DeleteMode::DbAndDisk,
                "Remove from database AND delete files from disk",
            );
            if state.mode == DeleteMode::DbAndDisk {
                ui.add_space(4.0);
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 80),
                    "⚠ This permanently deletes image files from disk.",
                );
            }

            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new("🗑 Delete")
                                .fill(egui::Color32::from_rgb(160, 30, 30))
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgb(220, 60, 60),
                                )),
                        )
                        .clicked()
                    {
                        confirmed = true;
                    }
                });
            });
        });

        if cancelled || modal_resp.should_close() {
            self.delete_confirm = None;
        } else if confirmed {
            if let Some(state) = self.delete_confirm.take() {
                self.pending_command = Some(UiCommand::DeleteImages {
                    database_path: self.config.database_path.clone(),
                    base_dir: self.config.download_folder.clone(),
                    image_ids: state.image_ids,
                    mode: state.mode,
                    refresh: self.search_refresh(),
                });
                self.bulk_select_mode = false;
                self.selected_image_ids.clear();
            }
        }
    }

    // ── Already-processed URL prompts (Task 8) ────────────────────────────────

    fn render_rescrape_prompts(&mut self, ctx: &egui::Context) {
        let Some(url) = self.pending_rescrape_prompts.front() else {
            return;
        };
        let mut skip = false;
        let mut rescrape = false;

        egui::Window::new("Already Processed")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 20.0])
            .show(ctx, |ui| {
                ui.label("This URL was already scraped:");
                ui.small(url);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Skip").clicked() {
                        skip = true;
                    }
                    if ui
                        .button("Re-scrape (update metadata + new images)")
                        .clicked()
                    {
                        rescrape = true;
                    }
                });
            });

        if skip {
            self.pending_rescrape_prompts.pop_front();
        }

        if rescrape {
            let url = self
                .pending_rescrape_prompts
                .pop_front()
                .expect("the visible rescrape prompt must still be queued");
            self.pending_command = Some(UiCommand::RescrapeUrl { url });
        }
    }

    fn render_queue_window(&mut self, ctx: &egui::Context) {
        if !self.queue_window_open {
            return;
        }
        let viewport_id = egui::ViewportId::from_hash_of("queue_window");
        let viewport_builder = application_viewport()
            .with_title("Download Queue")
            .with_inner_size([520.0, 420.0]);

        ctx.show_viewport_immediate(viewport_id, viewport_builder, |ctx, _class| {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.queue_window_open = false;
                return;
            }

            egui::TopBottomPanel::top("detached_queue_controls").show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| self.queue_controls(ui));
            });
            egui::CentralPanel::default().show(ctx, |ui| self.render_queue_rows(ui));
        });
    }
}

// ── EditKind ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum EditKind {
    Tag,
    Category,
    Actor,
    Studio,
}

impl EditKind {
    fn suggest_command(self, db: String, query: String) -> UiCommand {
        match self {
            EditKind::Tag => UiCommand::SuggestTags {
                database_path: db,
                query,
            },
            EditKind::Category => UiCommand::SuggestCategories {
                database_path: db,
                query,
            },
            EditKind::Actor => UiCommand::SuggestActors {
                database_path: db,
                query,
            },
            EditKind::Studio => UiCommand::SuggestStudios {
                database_path: db,
                query,
            },
        }
    }

    fn add_command(
        self,
        db: String,
        ids: Vec<i64>,
        vals: Vec<String>,
        q: SearchRefresh,
    ) -> UiCommand {
        match self {
            EditKind::Tag => UiCommand::AddTagsToImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
            EditKind::Category => UiCommand::AddCategoriesToImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
            EditKind::Actor => UiCommand::AddActorsToImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
            EditKind::Studio => UiCommand::AddStudiosToImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
        }
    }

    fn remove_command(
        self,
        db: String,
        ids: Vec<i64>,
        vals: Vec<String>,
        q: SearchRefresh,
    ) -> UiCommand {
        match self {
            EditKind::Tag => UiCommand::RemoveTagsFromImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
            EditKind::Category => UiCommand::RemoveCategoriesFromImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
            EditKind::Actor => UiCommand::RemoveActorsFromImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
            EditKind::Studio => UiCommand::RemoveStudiosFromImages {
                database_path: db,
                image_ids: ids,
                values: vals,
                refresh: q,
            },
        }
    }
}

// ── Date-group browsing ───────────────────────────────────────────────────────

impl AppWindow {
    pub(super) fn apply_date_groups_loaded(
        &mut self,
        seq: u64,
        result: Result<DateGroupPage, String>,
    ) {
        if seq != self.search_seq || !self.date_browse_active {
            return;
        }
        self.date_browse_loading = false;
        match result {
            Ok(page) => {
                self.date_browse_current = page.group;
                self.date_browse_total = page.total;
                self.date_browse_active = true;
                self.recent_pages_active = false;
                self.date_browse_load_current();
            }
            Err(e) => {
                self.error = Some(format!("Failed to load date groups: {}", e));
            }
        }
    }

    fn date_browse_current_group(&self) -> Option<DateGroup> {
        self.date_browse_current.clone()
    }

    /// Fires a search command for the current group's page_id, if one exists.
    fn date_browse_load_current(&mut self) {
        let Some(page_id) = self.date_browse_current.as_ref().map(|group| group.page_id) else {
            return;
        };
        self.search_seq += 1;
        let seq = self.search_seq;
        self.pending_command = Some(UiCommand::Search {
            database_path: self.config.database_path.clone(),
            query: crate::models::SearchQuery {
                page_id: Some(page_id),
                site_key: self.search_form.site_filter(),
                ..Default::default()
            },
            seq,
            cursor: None,
            append: false,
        });
    }

    /// Navigate to the next group (wraps around).
    fn date_browse_next(&mut self) {
        self.request_date_group(DateBrowseDirection::Next);
    }

    /// Navigate to the previous group (wraps around).
    fn date_browse_prev(&mut self) {
        self.request_date_group(DateBrowseDirection::Previous);
    }

    fn request_date_group(&mut self, direction: DateBrowseDirection) {
        self.date_browse_active = true;
        self.recent_pages_active = false;
        self.date_browse_loading = true;
        let cursor = self
            .date_browse_current
            .as_ref()
            .filter(|_| direction != DateBrowseDirection::Initial)
            .map(|group| DateGroupCursor {
                scraped_at: group.scraped_at.clone(),
                page_id: group.page_id,
            });
        self.date_browse_current = None;
        self.date_browse_total = 0;
        self.date_browse_site_key = self.search_form.site_filter();
        self.search_seq = self.search_seq.wrapping_add(1);
        self.running = false;
        self.error = None;
        self.set_search_results(Vec::new());
        self.pending_command = Some(UiCommand::BrowseDateGroups {
            database_path: self.config.database_path.clone(),
            site_key: self.date_browse_site_key.clone(),
            seq: self.search_seq,
            cursor,
            direction,
            ascending: self.date_browse_asc,
            max_favorites_exclusive: self
                .date_browse_fav_enabled
                .then_some(self.date_browse_fav_threshold as usize),
        });
    }

    fn sync_date_browse_site(&mut self) {
        if self.date_browse_active && self.date_browse_site_key != self.search_form.site_filter() {
            self.request_date_group(DateBrowseDirection::Initial);
        }
    }

    /// Renders the date-browse controls row inside the Search tab.
    fn render_date_browse_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("By Date");
            if self.date_browse_loading {
                ui.spinner();
                return;
            }

            let count = self.date_browse_total;
            ui.separator();

            // ── Sort direction ───────────────────────────────────────────────
            let dir_label = if self.date_browse_asc {
                "↑ Oldest first"
            } else {
                "↓ Newest first"
            };
            if ui
                .button(dir_label)
                .on_hover_text("Toggle sort direction")
                .clicked()
            {
                self.date_browse_asc = !self.date_browse_asc;
                self.date_browse_current = None;
                self.request_date_group(DateBrowseDirection::Initial);
            }

            ui.separator();

            // ── Fav filter ───────────────────────────────────────────────────
            let filter_changed = ui
                .checkbox(&mut self.date_browse_fav_enabled, "Skip ≥")
                .changed();
            let fav_resp = ui.add(
                egui::DragValue::new(&mut self.date_browse_fav_threshold)
                    .range(1..=999)
                    .suffix(" favs"),
            );
            if filter_changed || fav_resp.changed() {
                self.date_browse_current = None;
                self.request_date_group(DateBrowseDirection::Initial);
            }

            if count == 0 {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 160, 0),
                    "No groups match the current filter.",
                );
                return;
            }

            ui.separator();

            // ── Prev / Next navigation ───────────────────────────────────────
            let prev_resp = ui
                .add_enabled(count > 1, egui::Button::new("◀"))
                .on_hover_text("Previous group (PageUp)");
            let next_resp = ui
                .add_enabled(count > 1, egui::Button::new("▶"))
                .on_hover_text("Next group (PageDown)");

            ui.label(format!("{} groups", count));

            // ── Current group info ───────────────────────────────────────────
            if let Some(group) = self.date_browse_current_group() {
                ui.separator();
                ui.label(
                    egui::RichText::new(format!(
                        "{} · {} 🖼  {} ★",
                        group.date_str, group.image_count, group.fav_count,
                    ))
                    .color(egui::Color32::from_rgb(80, 160, 220)),
                );
                ui.separator();
                ui.label(crate::ui::truncate(&group.title, 60));
            }

            // Apply clicks outside the borrow of `self` fields.
            if prev_resp.clicked() {
                self.date_browse_prev();
            } else if next_resp.clicked() {
                self.date_browse_next();
            }
        });
    }
}

// ── SQL Query tab ─────────────────────────────────────────────────────────────

impl AppWindow {
    pub(super) fn apply_sql_result(&mut self, result: Result<SqlQueryResult, String>) {
        self.sql_running = false;
        match result {
            Ok(r) => {
                self.sql_error = None;
                self.sql_result = Some(r);
            }
            Err(e) => {
                self.sql_error = Some(e);
                self.sql_result = None;
            }
        }
    }

    pub(super) fn apply_sql_export_result(
        &mut self,
        output_path: &str,
        result: Result<usize, String>,
    ) {
        self.sql_export_running = false;
        self.sql_export_status = Some(match result {
            Ok(row_count) => format!(
                "Exported {} row{} to {}",
                row_count,
                if row_count == 1 { "" } else { "s" },
                output_path
            ),
            Err(error) => format!("Export failed: {error}"),
        });
    }

    fn render_sql_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("SQL Console");
        ui.small(
            "Execute custom SQL queries against the local database. \
             Read-only SELECT queries are recommended.",
        );
        ui.add_space(4.0);

        // ── Query editor ──────────────────────────────────────────────────────
        egui::ScrollArea::vertical()
            .id_salt("sql_editor")
            .max_height(160.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.sql_query)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(6)
                        .desired_width(f32::INFINITY)
                        .hint_text("SELECT * FROM source_pages LIMIT 20"),
                );
            });

        // Keyboard shortcut check must happen before the button layout.
        let ctrl_enter = ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.ctrl);
        let can_run = !self.sql_running && !self.sql_query.trim().is_empty();

        // ── Toolbar row ───────────────────────────────────────────────────────
        let mut fire_run = false;
        ui.horizontal(|ui| {
            let btn_text = if self.sql_running {
                "⏳  Running…"
            } else {
                "▶  Run"
            };
            if ui
                .add_enabled(can_run, egui::Button::new(btn_text))
                .clicked()
            {
                fire_run = true;
            }
            ui.weak("Ctrl+Enter");

            if let Some(res) = &self.sql_result {
                ui.separator();
                ui.label(format!(
                    "{}{} row{}  ·  {} col{}",
                    res.rows.len(),
                    if res.truncated { "+" } else { "" },
                    if res.rows.len() == 1 { "" } else { "s" },
                    if res.total_columns > res.columns.len() {
                        format!("{}/{}", res.columns.len(), res.total_columns)
                    } else {
                        res.columns.len().to_string()
                    },
                    if res.total_columns == 1 { "" } else { "s" },
                ));
            }
        });

        if (fire_run || ctrl_enter) && can_run {
            self.sql_running = true;
            self.sql_error = None;
            self.sql_export_status = None;
            self.pending_command = Some(UiCommand::RunSqlQuery {
                database_path: self.config.database_path.clone(),
                query: self.sql_query.trim().to_string(),
            });
        }

        // ── Error banner ──────────────────────────────────────────────────────
        if let Some(err) = &self.sql_error.clone() {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), format!("⚠  {}", err));
        }

        ui.separator();

        // ── Results table ─────────────────────────────────────────────────────
        if let Some(result) = &self.sql_result {
            // Export bar
            ui.horizontal(|ui| {
                let export_label = if self.sql_export_running {
                    "Exporting..."
                } else {
                    "⬇  Export CSV"
                };
                if ui
                    .add_enabled(!self.sql_export_running, egui::Button::new(export_label))
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("CSV files", &["csv"])
                        .set_file_name("query_result.csv")
                        .save_file()
                    {
                        self.sql_export_running = true;
                        self.sql_export_status = Some("Exporting...".to_string());
                        self.pending_command = Some(UiCommand::ExportSqlQuery {
                            database_path: self.config.database_path.clone(),
                            query: self.sql_query.trim().to_string(),
                            output_path: path.to_string_lossy().into_owned(),
                        });
                    }
                }
                if let Some(status) = &self.sql_export_status {
                    ui.small(status);
                }
            });

            let displayed = result.rows.len();
            if result.truncated {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 160, 0),
                    format!(
                        "Result preview reached a safety limit ({} rows, {} columns, 64 KiB per cell, 8 MiB total). Export CSV streams the complete result.",
                        super::SQL_QUERY_ROW_LIMIT,
                        super::SQL_QUERY_COLUMN_LIMIT,
                    ),
                );
            }

            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("sql_results_grid")
                        .striped(true)
                        .min_col_width(60.0)
                        .max_col_width(320.0)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            // Header
                            for col_name in &result.columns {
                                ui.strong(col_name);
                            }
                            ui.end_row();

                            // Data rows
                            for row in result.rows.iter().take(displayed) {
                                for cell in row {
                                    ui.label(cell);
                                }
                                ui.end_row();
                            }
                        });
                });
        } else if !self.sql_running && self.sql_error.is_none() {
            ui.add_space(12.0);
            ui.weak("Run a query to see results here.");
            ui.add_space(8.0);
            ui.small("Useful quick queries:");
            for hint in &[
                "SELECT name FROM sqlite_master WHERE type='table';",
                "SELECT COUNT(*) FROM source_pages;",
                "SELECT COUNT(*) FROM images;",
                "SELECT * FROM source_pages ORDER BY scraped_at DESC LIMIT 20;",
            ] {
                if ui
                    .link(egui::RichText::new(*hint).monospace().small())
                    .clicked()
                {
                    self.sql_query = hint.to_string();
                }
            }
        }
    }
}

#[cfg(test)]
fn render_extension_packages(
    ui: &mut egui::Ui,
    extensions: &[crate::scraper::extensions::ExtensionStatus],
) -> egui::scroll_area::ScrollAreaOutput<()> {
    egui::ScrollArea::vertical()
        .id_salt("extension_packages")
        .auto_shrink([false, false])
        .max_height(ui.available_height())
        .show(ui, |ui| {
            for extension in extensions {
                render_extension_card(ui, extension);
            }
        })
}

#[cfg(test)]
fn render_extension_card(
    ui: &mut egui::Ui,
    extension: &crate::scraper::extensions::ExtensionStatus,
) -> egui::Response {
    super::extensions_manager::extension_frame(ui, |ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!("{} {}", extension.id, extension.version)).strong(),
            )
            .wrap(),
        );
        ui.add(egui::Label::new(format!("{:?}: {}", extension.runtime, extension.message)).wrap());
        ui.add(
            egui::Label::new(egui::RichText::new(extension.directory.to_string_lossy()).small())
                .wrap(),
        );
    })
}

// ── Attribute context-menu command builders ───────────────────────────────────

/// Builds a "remove attribute" command for a tag / category / studio.
/// Returns `None` for kinds that don't support single-attribute removal (e.g. actor, channel).
fn attr_remove_cmd(
    kind: &str,
    database_path: String,
    image_ids: Vec<i64>,
    value: String,
    refresh: SearchRefresh,
) -> Option<UiCommand> {
    match kind {
        "tag" => Some(UiCommand::RemoveTagsFromImages {
            database_path,
            image_ids,
            values: vec![value],
            refresh,
        }),
        "category" => Some(UiCommand::RemoveCategoriesFromImages {
            database_path,
            image_ids,
            values: vec![value],
            refresh,
        }),
        "studio" => Some(UiCommand::RemoveStudiosFromImages {
            database_path,
            image_ids,
            values: vec![value],
            refresh,
        }),
        _ => None,
    }
}

/// Builds an "add attribute" command for a tag / category / studio.
fn attr_add_cmd(
    kind: &str,
    database_path: String,
    image_ids: Vec<i64>,
    value: String,
    refresh: SearchRefresh,
) -> Option<UiCommand> {
    match kind {
        "tag" => Some(UiCommand::AddTagsToImages {
            database_path,
            image_ids,
            values: vec![value],
            refresh,
        }),
        "category" => Some(UiCommand::AddCategoriesToImages {
            database_path,
            image_ids,
            values: vec![value],
            refresh,
        }),
        "studio" => Some(UiCommand::AddStudiosToImages {
            database_path,
            image_ids,
            values: vec![value],
            refresh,
        }),
        _ => None,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extracts the actor folder name (path component index 1) from a stored relative path.
/// Path structure: `{site}/{actor}/{content}/{filename}`
fn primary_actor_from_path(local_file_path: &str) -> Option<String> {
    if local_file_path.split(['/', '\\']).next() == Some("torrents") {
        return None;
    }
    std::path::Path::new(local_file_path)
        .components()
        .nth(1)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

fn split_filter_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Returns the last comma-separated token (trimmed).
/// Used to drive autocomplete while the user types in a multi-value field.
/// Example: "blowjob, asi" → "asi"
fn last_comma_token(value: &str) -> String {
    value.rsplit(',').next().unwrap_or(value).trim().to_string()
}

/// Replaces the last token in a multi-value field with `suggestion`.
/// Example: replace_last_token("blowjob, asi", "Asian") → "blowjob, Asian"
fn replace_last_token(value: &str, suggestion: &str) -> String {
    match value.rfind(',') {
        Some(pos) => format!("{}, {}", value[..pos].trim_end(), suggestion),
        None => suggestion.to_string(),
    }
}

/// Maps a stored attribute `kind` to a consistent human-readable display label.
/// Ensures that scraped attributes (label = "Categories:") and manually added
/// ones (label = "Category") are rendered identically.
fn kind_display_label(kind: &str) -> &str {
    match kind {
        "actor" => "Models",
        "channel" => "Channel",
        "studio" => "Studio",
        "category" => "Categories",
        "tag" => "Tags",
        "date" => "Date",
        "resource" => "Resource",
        "content_title" => "Title",
        "director" => "Director",
        "author" => "Author",
        "photographer" => "Photographer",
        "creator" => "Creator",
        other => other,
    }
}

fn format_queue_decision(decision: &UrlQueueDecision) -> String {
    let display = decision
        .normalized_url
        .as_deref()
        .unwrap_or(decision.raw_input.trim());
    match decision.outcome {
        UrlQueueOutcome::Queued => format!("Queued URL: {}", display),
        UrlQueueOutcome::Empty => "Skipped clipboard/file input: empty value".to_string(),
        UrlQueueOutcome::Invalid => format!("Rejected unsupported or invalid URL: {}", display),
        UrlQueueOutcome::DuplicateQueued => format!("Skipped duplicate queued URL: {}", display),
        UrlQueueOutcome::AlreadyProcessed => {
            format!("Already processed: {}", display)
        }
    }
}

/// Whether an `AlreadyProcessed` URL should trigger the re-scrape popup.
/// Extracted as a pure function so it can be unit-tested without egui.
///
/// Rules:
/// - `skip_downloaded = true`  → silently skip, **no** popup.
/// - `skip_downloaded = false` → show popup, let user choose Skip / Re-scrape.
pub(crate) fn should_show_rescrape_prompt(
    outcome: &UrlQueueOutcome,
    skip_downloaded: bool,
) -> bool {
    matches!(outcome, UrlQueueOutcome::AlreadyProcessed) && !skip_downloaded
}

fn viewer_delete_shortcut_requested(events: &[egui::Event], wants_keyboard_input: bool) -> bool {
    !wants_keyboard_input
        && events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::Delete,
                    pressed: true,
                    ..
                }
            )
        })
}

fn format_original_load_metrics(
    image_id: i64,
    timings: ImageLoadTimings,
    texture_upload: std::time::Duration,
) -> String {
    let total = timings.before_texture() + texture_upload;
    let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1_000.0;
    format!(
        "Original image #{image_id}: {:.1} ms (queue {:.1}, header {:.1}, decode {:.1}, transform {:.1}, RGBA {:.1}, prepare {:.1}, texture {:.1})",
        ms(total),
        ms(timings.queue_wait),
        ms(timings.header_read),
        ms(timings.decode),
        ms(timings.transform),
        ms(timings.rgba_conversion),
        ms(timings.color_image_conversion),
        ms(texture_upload),
    )
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MetadataAttribute, SearchPageSummary, UrlQueueOutcome};

    fn test_window() -> AppWindow {
        AppWindow::new(
            AppConfig::default(),
            UiLog::default(),
            Arc::new(RwLock::new(LivePresets::default())),
        )
    }

    fn date_group(page_id: i64) -> DateGroup {
        DateGroup {
            page_id,
            scraped_at: "2026-01-05 00:00:00".into(),
            date_str: "2026-01-05".into(),
            title: format!("Page {page_id}"),
            image_count: 1,
            fav_count: 0,
        }
    }

    #[test]
    fn date_browse_passes_selected_site_to_group_and_image_queries() {
        let mut window = test_window();
        window.search_form.site_key = "source-a".into();
        window.request_date_group(DateBrowseDirection::Initial);
        let seq = window.search_seq;
        assert!(matches!(
            window.take_command(),
            Some(UiCommand::BrowseDateGroups {
                site_key: Some(site), cursor: None, direction: DateBrowseDirection::Initial,
                max_favorites_exclusive: Some(3), seq: request_seq, ..
            }) if site == "source-a" && request_seq == seq
        ));
        window.apply_date_groups_loaded(
            seq,
            Ok(DateGroupPage {
                group: Some(date_group(42)),
                total: 2,
            }),
        );
        match window.take_command().unwrap() {
            UiCommand::Search { query, seq, .. } => {
                assert_eq!(query.site_key.as_deref(), Some("source-a"));
                assert_eq!(query.page_id, Some(42));
                assert_eq!(seq, window.search_seq);
            }
            _ => panic!("expected images for the source-filtered date group"),
        }
        window.date_browse_next();
        assert!(matches!(
            window.take_command(),
            Some(UiCommand::BrowseDateGroups {
                site_key: Some(site), cursor: Some(cursor), direction: DateBrowseDirection::Next, ..
            }) if site == "source-a" && cursor.page_id == 42
        ));
    }

    #[test]
    fn date_browse_site_changes_restart_navigation_and_ignore_stale_results() {
        let mut window = test_window();
        window.search_form.site_key = "source-a".into();
        window.request_date_group(DateBrowseDirection::Initial);
        let old_seq = window.search_seq;
        window.take_command();
        window.set_search_results(vec![search_result(1, Vec::new())]);
        window.running = true;
        window.search_form.site_key = "source-b".into();
        let context = egui::Context::default();
        let render = |window: &mut AppWindow| {
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| window.render_search_tab(ui, ctx));
                },
            );
        };
        render(&mut window);
        let current_seq = window.search_seq;
        assert_ne!(old_seq, current_seq);
        assert!(window.date_browse_loading);
        assert!(!window.running);
        assert!(window.search_results.is_empty());
        assert!(matches!(
            window.take_command(),
            Some(UiCommand::BrowseDateGroups { site_key: Some(site), cursor: None, seq, .. })
                if site == "source-b" && seq == current_seq
        ));
        window.apply_date_groups_loaded(
            old_seq,
            Ok(DateGroupPage {
                group: Some(date_group(1)),
                total: 10,
            }),
        );
        window.apply_date_groups_loaded(old_seq, Err("stale error".into()));
        assert!(window.date_browse_loading);
        assert!(window.date_browse_current.is_none());
        assert!(window.error.is_none());
        assert!(window.take_command().is_none());
        window.apply_date_groups_loaded(
            current_seq,
            Ok(DateGroupPage {
                group: Some(date_group(9)),
                total: 1,
            }),
        );
        assert!(matches!(
            window.take_command(),
            Some(UiCommand::Search { query, .. })
                if query.site_key.as_deref() == Some("source-b") && query.page_id == Some(9)
        ));
        window.set_search_results(vec![search_result(9, Vec::new())]);
        window.search_form = SearchForm::default();
        render(&mut window);
        let all_sites_seq = window.search_seq;
        assert!(matches!(
            window.take_command(),
            Some(UiCommand::BrowseDateGroups { site_key: None, cursor: None, seq, .. })
                if seq == all_sites_seq
        ));
        window.apply_date_groups_loaded(
            all_sites_seq,
            Ok(DateGroupPage {
                group: None,
                total: 0,
            }),
        );
        assert!(!window.date_browse_loading);
        assert!(!window.running);
        assert!(window.date_browse_active);
        assert!(window.date_browse_current.is_none());
        assert_eq!(window.date_browse_total, 0);
        assert!(window.search_results.is_empty());
        assert!(window.selected_result.is_none());
        assert!(window.take_command().is_none());
    }

    #[test]
    fn description_viewport_renders_complete_heading_and_text_with_copy_actions() {
        let mut window = test_window();
        let title = "[Example Network] A complete page heading with all its original metadata [2025, Documentary, 1080p]".to_string();
        let text = format!(
            "First paragraph.\n\n{}Final paragraph marker.",
            "A complete sentence, without truncation. ".repeat(60)
        );
        window.description_popup = Some(DescriptionPopup {
            title: title.clone(),
            text: text.clone(),
        });
        let context = egui::Context::default();
        context.set_embed_viewports(true);
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                ..Default::default()
            },
            |ctx| window.render_description_popup(ctx),
        );
        fn collect(shape: &egui::Shape, text: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(shape) => text.push(shape.galley.job.text.clone()),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, text);
                    }
                }
                _ => {}
            }
        }
        let mut drawn = Vec::new();
        for shape in &output.shapes {
            collect(&shape.shape, &mut drawn);
        }
        for expected in [title.as_str(), text.as_str(), "Copy title", "Copy text"] {
            assert!(
                drawn.iter().any(|actual| actual == expected),
                "missing complete text or copy action"
            );
        }
        assert_eq!(window.description_popup.as_ref().unwrap().text, text);
    }

    #[test]
    fn every_native_child_window_receives_the_application_icon() {
        let mut window = test_window();
        window.set_search_results(vec![search_result(1, Vec::new())]);
        window.viewer_open = Some(0);
        window.description_popup = Some(DescriptionPopup {
            title: "Demo".into(),
            text: "Demo description".into(),
        });
        window.queue_window_open = true;
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests = Arc::clone(&captured);
        egui::Context::set_immediate_viewport_renderer(move |_, mut viewport| {
            requests
                .lock()
                .unwrap()
                .push((viewport.ids.this, viewport.builder.clone()));
            let child_context = egui::Context::default();
            let mut input = egui::RawInput {
                viewport_id: viewport.ids.this,
                ..Default::default()
            };
            input
                .viewports
                .insert(viewport.ids.this, egui::ViewportInfo::default());
            let _ = child_context.run(input, |ctx| (viewport.viewport_ui_cb)(ctx));
        });
        let context = egui::Context::default();
        context.set_embed_viewports(false);
        let _ = context.run(egui::RawInput::default(), |ctx| window.render(ctx));
        let root_icon = super::super::icon::native_options().viewport.icon.unwrap();
        let requests = captured.lock().unwrap();
        for name in ["image_viewer", "scene_description", "queue_window"] {
            let (_, builder) = requests
                .iter()
                .find(|(id, _)| *id == egui::ViewportId::from_hash_of(name))
                .unwrap_or_else(|| panic!("Missing viewport {name}"));
            assert!(
                Arc::ptr_eq(builder.icon.as_ref().unwrap(), &root_icon),
                "Wrong icon for {name}"
            );
            assert_eq!(builder.app_id.as_deref(), Some("archive-sift"));
        }
    }

    #[test]
    fn storage_settings_rebind_thumbnail_paths_and_clear_old_library_views() {
        let mut window = test_window();
        window.sql_query = "SELECT COUNT(*) FROM images".to_string();
        window.set_search_results(vec![search_result(7, Vec::new())]);
        window.viewer_open = Some(0);
        window.tag_suggestions.push("old suggestion".into());
        let next = AppConfig {
            database_path: Path::new("next-library")
                .join("catalog.sqlite3")
                .display()
                .to_string(),
            thumbnail_folder: Some("next-previews".to_string()),
            ..window.config.clone()
        };
        window.apply_storage_config(next);
        assert_eq!(window.thumb_dir, Path::new("next-previews"));
        assert!(window.search_results.is_empty());
        assert!(window.tag_suggestions.is_empty());
        assert!(window.viewer_open.is_none());
        assert_eq!(window.sql_query, "SELECT COUNT(*) FROM images");
        assert_eq!(window.active_tab, Tab::Settings);
    }

    #[test]
    fn extensions_tab_renders_in_a_headless_egui_frame() {
        let mut window = test_window();
        let context = egui::Context::default();
        let output = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| window.render_extensions_tab(ui));
        });
        assert!(!output.shapes.is_empty());
    }

    fn extension_status(id: &str, message: &str) -> crate::scraper::extensions::ExtensionStatus {
        crate::scraper::extensions::ExtensionStatus {
            directory: Path::new("plugins").join(id),
            id: id.to_string(),
            version: "1.0.0".to_string(),
            runtime: Some(crate::scraper::extensions::manifest::RuntimeKind::Wasm),
            loaded: true,
            message: message.to_string(),
        }
    }

    #[test]
    fn extension_cards_share_available_width_and_wrap_long_content() {
        let extensions = [
            extension_status("demo.short", "Loaded"),
            extension_status(
                &format!("demo.{}", "long-name".repeat(40)),
                &"Long diagnostic. ".repeat(60),
            ),
        ];
        for width in [280.0, 640.0, 1200.0] {
            let context = egui::Context::default();
            let mut rects = Vec::new();
            let mut available_width = 0.0;
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 1800.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        available_width = ui.available_width();
                        for extension in &extensions {
                            rects.push(render_extension_card(ui, extension).rect);
                        }
                    });
                },
            );
            for rect in &rects {
                assert!(
                    (rect.width() - available_width).abs() < 1.0,
                    "{rect:?}, available {available_width}"
                );
            }
            assert!(rects[1].height() > rects[0].height());
        }
    }

    #[test]
    fn extension_packages_scroll_when_the_list_exceeds_the_viewport() {
        let extensions = (0..40)
            .map(|index| extension_status(&format!("demo.parser-{index}"), "Loaded"))
            .collect::<Vec<_>>();
        let context = egui::Context::default();
        let run = |time, events| {
            let mut result = None;
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(500.0, 300.0),
                    )),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        result = Some(render_extension_packages(ui, &extensions));
                    });
                },
            );
            result.unwrap()
        };
        let initial = run(0.0, Vec::new());
        assert!(initial.content_size.y > initial.inner_rect.height());
        assert!(initial.inner_rect.height() <= 300.0);
        assert!(initial.content_size.x <= initial.inner_rect.width() + 1.0);

        let scrolled = run(
            0.1,
            vec![
                egui::Event::PointerMoved(initial.inner_rect.center()),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -500.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(scrolled.state.offset.y > initial.state.offset.y);
        let next = run(0.2, Vec::new());
        assert!(
            next.state.offset.y > 0.0,
            "Scroll position should persist between frames"
        );
    }

    fn search_result(image_id: i64, attributes: Vec<MetadataAttribute>) -> SearchImageResult {
        let attributes = Arc::new(attributes);
        SearchImageResult {
            image_id,
            page: Arc::new(SearchPageSummary {
                page_id: 1,
                title: "Page".to_string(),
                url: "https://demo.test/page".to_string(),
                attributes: Arc::clone(&attributes),
            }),
            source_url: format!("https://demo.test/{image_id}.jpg"),
            preview_url: None,
            local_file_path: None,
            thumbnail_path: None,
            is_favorite: false,
            attributes,
        }
    }

    #[test]
    fn metadata_responses_preserve_shared_image_pages_and_ignore_old_searches() {
        let mut window = AppWindow::new(
            AppConfig::default(),
            UiLog::default(),
            Arc::new(RwLock::new(LivePresets::default())),
        );
        let first = search_result(7, Vec::new());
        let mut second = first.clone();
        Arc::make_mut(&mut second.page).page_id = 2;
        window.set_search_page(
            SearchResultPage {
                items: vec![first.clone(), second.clone()],
                next_cursor: None,
            },
            false,
        );
        window.search_seq = 3;
        window.metadata_requested_ids.insert(7);
        let database = window.config.database_path.clone();
        window.apply_search_metadata(&database, 2, &[7], Ok(vec![]));
        window.apply_search_metadata("other.sqlite3", 3, &[7], Err("stale".into()));
        assert!(window.metadata_requested_ids.contains(&7));
        let mut first = first;
        Arc::make_mut(&mut first.page).title = "First page".into();
        Arc::make_mut(&mut second.page).title = "Second page".into();
        window.apply_search_metadata(&database, 3, &[7], Ok(vec![second, first]));
        assert_eq!(window.search_results[0].page.title, "First page");
        assert_eq!(window.search_results[1].page.title, "Second page");
        assert!(window.hydrated_image_ids.contains(&(1, 7)));
        assert!(window.hydrated_image_ids.contains(&(2, 7)));
        assert!(!window.metadata_requested_ids.contains(&7));
        window.metadata_requested_ids.insert(7);
        window.release_metadata_requests();
        assert!(
            window.metadata_requested_ids.is_empty(),
            "cancelled pagination requests must be retryable"
        );
    }

    fn pressed_key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn backspace_never_requests_viewer_image_deletion() {
        let events = [pressed_key(egui::Key::Backspace)];

        assert!(!viewer_delete_shortcut_requested(&events, false));
    }

    #[test]
    fn delete_is_ignored_while_a_text_field_owns_keyboard_input() {
        let events = [pressed_key(egui::Key::Delete)];

        assert!(!viewer_delete_shortcut_requested(&events, true));
    }

    #[test]
    fn delete_requests_viewer_image_deletion_when_canvas_has_keyboard_input() {
        let events = [pressed_key(egui::Key::Delete)];

        assert!(viewer_delete_shortcut_requested(&events, false));
    }

    #[test]
    fn containing_folder_returns_current_image_parent() {
        let image = Path::new("downloads").join("gallery").join("image.jpg");
        let expected = Path::new("downloads").join("gallery");

        assert_eq!(containing_folder(&image), expected.as_path());
    }

    #[test]
    fn torrent_paths_do_not_expose_a_primary_actor_folder() {
        for path in [
            "torrents/source/studio/card/images/file.png",
            "torrents\\source\\studio\\card\\images\\file.png",
        ] {
            assert!(primary_actor_from_path(path).is_none());
        }
        assert_eq!(
            primary_actor_from_path("source/actor/card/image.png").as_deref(),
            Some("actor")
        );
    }

    #[test]
    fn torrent_updates_invalidate_inflight_image_queries_and_obsolete_paths() {
        let mut window = test_window();
        window.set_search_results(vec![search_result(1, Vec::new())]);
        window.viewer_open = Some(0);
        window.search_seq = 7;
        window.running = true;
        window.torrent_card_saved();
        assert_eq!(window.search_seq, 8);
        assert!(!window.running);
        assert!(window.search_results.is_empty());
        assert!(window.viewer_open.is_none());
    }

    #[test]
    fn containing_folder_handles_a_path_without_a_parent() {
        let image = Path::new("image.jpg");

        assert_eq!(containing_folder(image), image);
    }

    #[test]
    fn viewer_crossfade_uses_smooth_midpoint_interpolation() {
        let started_at = std::time::Instant::now();
        let transition = ViewerImageTransition {
            image_id: 7,
            from: 0.0,
            to: 1.0,
            started_at,
        };

        let (blend, active) = advance_viewer_image_transition(
            7,
            0.0,
            1.0,
            Some(transition),
            started_at + VIEWER_IMAGE_CROSSFADE_DURATION / 2,
        );

        assert!((blend - 0.5).abs() < 0.001);
        assert!(active.is_some());
    }

    #[test]
    fn viewer_crossfade_completes_and_releases_animation_state() {
        let started_at = std::time::Instant::now();
        let transition = ViewerImageTransition {
            image_id: 8,
            from: 0.0,
            to: 1.0,
            started_at,
        };

        let (blend, active) = advance_viewer_image_transition(
            8,
            0.0,
            1.0,
            Some(transition),
            started_at + VIEWER_IMAGE_CROSSFADE_DURATION,
        );

        assert_eq!(blend, 1.0);
        assert!(active.is_none());
    }

    #[test]
    fn viewer_crossfade_reverses_from_its_current_opacity() {
        let started_at = std::time::Instant::now();
        let transition = ViewerImageTransition {
            image_id: 9,
            from: 0.0,
            to: 1.0,
            started_at,
        };
        let halfway = started_at + VIEWER_IMAGE_CROSSFADE_DURATION / 2;

        let (blend, reversed) =
            advance_viewer_image_transition(9, 0.0, 0.0, Some(transition), halfway);

        assert!((blend - 0.5).abs() < 0.001);
        let reversed = reversed.expect("reverse transition should remain active");
        assert!((reversed.from - 0.5).abs() < 0.001);
        assert_eq!(reversed.to, 0.0);
        assert_eq!(reversed.started_at, halfway);
    }

    #[test]
    fn viewer_crossfade_discards_transition_from_another_image() {
        let now = std::time::Instant::now();
        let stale = ViewerImageTransition {
            image_id: 10,
            from: 0.0,
            to: 1.0,
            started_at: now,
        };

        let (blend, active) = advance_viewer_image_transition(11, 0.0, 0.0, Some(stale), now);

        assert_eq!(blend, 0.0);
        assert!(active.is_none());
    }

    #[test]
    fn viewer_crossfade_opacity_is_clamped_for_painter_tints() {
        assert_eq!(white_with_opacity(-1.0).a(), 0);
        assert_eq!(white_with_opacity(0.5).a(), 128);
        assert_eq!(white_with_opacity(2.0).a(), 255);
    }

    #[test]
    fn viewer_uses_proxy_until_zoom_requires_source_pixels() {
        assert_eq!(viewer_decode_max_side(1.0), Some(VIEWER_PROXY_MAX_SIDE));
        assert_eq!(viewer_decode_max_side(1.99), Some(VIEWER_PROXY_MAX_SIDE));
        assert_eq!(viewer_decode_max_side(FULL_ORIGINAL_ZOOM_THRESHOLD), None);
        assert_eq!(viewer_decode_max_side(4.0), None);
    }

    #[test]
    fn render_snapshot_shares_search_results_without_deep_clone() {
        let results = Arc::new(Vec::<SearchImageResult>::new());

        let snapshot = clone_search_results_snapshot(&results);

        assert!(Arc::ptr_eq(&results, &snapshot));
        assert_eq!(Arc::strong_count(&results), 2);
    }

    #[test]
    fn paged_search_appends_without_discarding_prior_cards() {
        let mut window = test_window();
        window.set_search_page(
            SearchResultPage {
                items: vec![search_result(1, Vec::new())],
                next_cursor: Some(SearchCursor {
                    scraped_at: "2026-01-01".to_string(),
                    page_id: 1,
                    sort_order: 0,
                    image_id: 1,
                }),
            },
            false,
        );
        window.set_search_page(
            SearchResultPage {
                items: vec![search_result(2, Vec::new())],
                next_cursor: None,
            },
            true,
        );

        assert_eq!(
            window
                .search_results
                .iter()
                .map(|r| r.image_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(window.search_next_cursor.is_none());
    }

    #[test]
    fn uniquely_owned_search_page_reuses_its_buffer_for_append_patch_and_delete() {
        let mut window = test_window();
        window.set_search_page(
            SearchResultPage {
                items: vec![search_result(1, Vec::new())],
                next_cursor: None,
            },
            false,
        );
        let initial_buffer = window.search_results.as_ptr();

        window.set_search_page(
            SearchResultPage {
                items: vec![search_result(2, Vec::new())],
                next_cursor: None,
            },
            true,
        );
        assert_eq!(window.search_results.as_ptr(), initial_buffer);

        window.apply_hydrated_results(&[2], vec![search_result(2, Vec::new())]);
        assert_eq!(window.search_results.as_ptr(), initial_buffer);

        window.remove_search_results(&[1]);
        assert_eq!(window.search_results.as_ptr(), initial_buffer);
        assert_eq!(window.search_results[0].image_id, 2);
    }

    #[test]
    fn filtered_mutation_refresh_replaces_the_pagination_cursor() {
        let mut window = test_window();
        let stale_cursor = SearchCursor {
            scraped_at: "2025-01-01".to_string(),
            page_id: 1,
            sort_order: 0,
            image_id: 1,
        };
        window.set_search_page(
            SearchResultPage {
                items: vec![search_result(1, Vec::new())],
                next_cursor: Some(stale_cursor),
            },
            false,
        );
        let fresh_cursor = SearchCursor {
            scraped_at: "2026-01-01".to_string(),
            page_id: 2,
            sort_order: 3,
            image_id: 9,
        };

        window.refresh_search_page(SearchResultPage {
            items: vec![search_result(2, Vec::new())],
            next_cursor: Some(fresh_cursor.clone()),
        });

        assert_eq!(window.search_next_cursor, Some(fresh_cursor));
        assert_eq!(window.search_results[0].image_id, 2);
        assert!(window.hydrated_image_ids.iter().any(|(_, id)| *id == 2));
    }

    #[test]
    fn hydration_and_deletion_patch_only_targeted_cards() {
        let mut window = test_window();
        window.set_search_page(
            SearchResultPage {
                items: vec![search_result(1, Vec::new()), search_result(2, Vec::new())],
                next_cursor: None,
            },
            false,
        );
        let actor = MetadataAttribute {
            kind: "actor".to_string(),
            label: "Actor".to_string(),
            value: "Alice".to_string(),
            source_url: None,
            is_female: None,
        };
        window.apply_hydrated_results(&[2], vec![search_result(2, vec![actor])]);

        assert!(window.search_results[0].attributes.is_empty());
        assert_eq!(window.search_results[1].actors(), vec!["Alice"]);
        window.remove_search_results(&[1]);
        assert_eq!(window.search_results.len(), 1);
        assert_eq!(window.search_results[0].image_id, 2);
    }

    #[test]
    fn actor_gender_patch_targets_only_hydrated_cards_with_that_actor() {
        let alice = MetadataAttribute {
            kind: "actor".to_string(),
            label: "Actor".to_string(),
            value: "Alice".to_string(),
            source_url: None,
            is_female: None,
        };
        let bob = MetadataAttribute {
            value: "Bob".to_string(),
            ..alice.clone()
        };
        let results = vec![
            search_result(1, vec![alice]),
            search_result(2, vec![bob]),
            search_result(3, Vec::new()),
        ];

        assert_eq!(image_ids_with_actor(&results, "alice"), vec![1]);
    }

    #[test]
    fn render_snapshot_shares_queue_rows_without_deep_clone() {
        let snapshot = Arc::new(crate::models::QueueSnapshot {
            pending_total: 1,
            pending: vec!["https://example.test/one".to_string()],
            ..crate::models::QueueSnapshot::default()
        });

        let render_snapshot = clone_queue_snapshot(&snapshot);

        assert!(Arc::ptr_eq(&snapshot, &render_snapshot));
        assert_eq!(Arc::strong_count(&snapshot), 2);
    }

    #[test]
    fn creator_search_form_keeps_other_filters_and_resets_with_the_form() {
        let mut form = SearchForm::default();
        form.creators.text = "Alice Creator, Bob Photographer".into();
        form.site_key = "example-source".into();
        form.tags = "Outdoor".into();
        let query = form.to_query();
        assert_eq!(query.creators, ["Alice Creator", "Bob Photographer"]);
        assert_eq!(query.site_key.as_deref(), Some("example-source"));
        assert_eq!(query.tags, ["Outdoor"]);
        assert!(SearchForm::default().to_query().creators.is_empty());
    }

    #[test]
    fn search_filter_fields_fill_one_shared_right_aligned_column() {
        let available_width = 400.0;
        let label_width = 72.0;
        let spacing = 8.0;

        let field_width = search_filter_field_width(available_width, label_width, spacing);

        assert_eq!(field_width, 320.0);
        assert_eq!(label_width + spacing + field_width, available_width);
    }

    #[test]
    fn search_filter_field_width_remains_usable_in_narrow_layouts() {
        assert_eq!(search_filter_field_width(100.0, 72.0, 8.0), 80.0);
    }

    #[test]
    fn queue_range_adds_overscan_and_clamps_to_total() {
        assert_eq!(queue_range_with_overscan(30..40, 100), 14..56);
        assert_eq!(queue_range_with_overscan(3..8, 100), 0..24);
        assert_eq!(queue_range_with_overscan(95..100, 100), 79..100);
        assert_eq!(queue_range_with_overscan(0..0, 0), 0..0);
    }

    #[test]
    fn queue_range_update_requests_follow_up_only_when_range_changes() {
        let mut current = 0..64;

        assert!(!update_queue_visible_range(&mut current, 0..64));
        assert!(update_queue_visible_range(&mut current, 48..112));
        assert_eq!(current, 48..112);
    }

    #[test]
    fn queue_snapshot_maps_absolute_row_indices_into_range() {
        let snapshot = crate::models::QueueSnapshot {
            pending_total: 20,
            pending_start: 8,
            pending: vec!["eight".to_string(), "nine".to_string()],
            ..crate::models::QueueSnapshot::default()
        };

        assert_eq!(queue_snapshot_pending_at(&snapshot, 7), None);
        assert_eq!(queue_snapshot_pending_at(&snapshot, 8), Some("eight"));
        assert_eq!(queue_snapshot_pending_at(&snapshot, 9), Some("nine"));
        assert_eq!(queue_snapshot_pending_at(&snapshot, 10), None);
    }

    #[test]
    fn preview_and_original_images_use_distinct_cache_keys() {
        let thumb_dir = std::path::Path::new("thumbs");
        let preview = image_texture_cache_key(
            ImageLoadMode::Preview,
            42,
            "downloads",
            thumb_dir,
            Some("image.jpg"),
            Some("thumb.jpg"),
        );
        let original = image_texture_cache_key(
            ImageLoadMode::Original,
            42,
            "downloads",
            thumb_dir,
            Some("image.jpg"),
            Some("thumb.jpg"),
        );

        assert_ne!(preview, original);
        assert!(preview.starts_with("Preview|"));
        assert!(original.starts_with("Original|"));
    }

    #[test]
    fn original_fallback_status_explicitly_keeps_preview_visible() {
        assert_eq!(
            original_fallback_message(OriginalFallbackState::Loading),
            "Loading original… Showing preview."
        );
        assert_eq!(
            original_fallback_message(OriginalFallbackState::Unavailable),
            "Original unavailable. Showing preview."
        );
    }

    #[test]
    fn original_cache_retains_proxy_and_source_levels_within_the_byte_budget() {
        assert_eq!(ORIGINAL_CACHE_MAX_ENTRIES, 6);
        assert_eq!(ORIGINAL_MAX_RGBA_BYTES, 256 * 1024 * 1024);
    }

    #[test]
    fn original_load_metrics_include_every_measured_stage() {
        let timings = ImageLoadTimings {
            queue_wait: std::time::Duration::from_millis(1),
            header_read: std::time::Duration::from_millis(2),
            decode: std::time::Duration::from_millis(3),
            transform: std::time::Duration::from_millis(4),
            rgba_conversion: std::time::Duration::from_millis(5),
            color_image_conversion: std::time::Duration::from_millis(6),
        };

        let message =
            format_original_load_metrics(42, timings, std::time::Duration::from_millis(7));

        assert!(message.contains("Original image #42: 28.0 ms"));
        assert!(message.contains("queue 1.0"));
        assert!(message.contains("decode 3.0"));
        assert!(message.contains("RGBA 5.0"));
        assert!(message.contains("prepare 6.0"));
        assert!(message.contains("texture 7.0"));
    }

    // ── should_show_rescrape_prompt ───────────────────────────────────────────

    #[test]
    fn popup_hidden_when_skip_downloaded_true() {
        // Checkbox ON → user wants silent skipping → no popup.
        assert!(!should_show_rescrape_prompt(
            &UrlQueueOutcome::AlreadyProcessed,
            true
        ));
    }

    #[test]
    fn popup_shown_when_skip_downloaded_false() {
        // Checkbox OFF → user wants interactive choice → popup must appear.
        assert!(should_show_rescrape_prompt(
            &UrlQueueOutcome::AlreadyProcessed,
            false
        ));
    }

    #[test]
    fn popup_not_shown_for_non_already_processed_outcomes() {
        // Other outcomes never trigger the popup regardless of the flag.
        for outcome in &[
            UrlQueueOutcome::Queued,
            UrlQueueOutcome::Empty,
            UrlQueueOutcome::Invalid,
            UrlQueueOutcome::DuplicateQueued,
        ] {
            assert!(
                !should_show_rescrape_prompt(outcome, false),
                "{:?} should not trigger popup",
                outcome
            );
            assert!(
                !should_show_rescrape_prompt(outcome, true),
                "{:?} should not trigger popup",
                outcome
            );
        }
    }

    #[test]
    fn rescrape_prompt_queue_deduplicates_and_preserves_fifo_order() {
        let mut queue = RescrapePromptQueue::default();

        assert!(queue.push("https://example.test/one".to_string()));
        assert!(queue.push("https://example.test/two".to_string()));
        assert!(!queue.push("https://example.test/one".to_string()));
        assert_eq!(queue.front(), Some("https://example.test/one"));
        assert_eq!(
            queue.pop_front().as_deref(),
            Some("https://example.test/one")
        );
        assert_eq!(queue.front(), Some("https://example.test/two"));

        // Removing an item also removes it from the uniqueness index, so a
        // later independent report may prompt for the same URL again.
        assert!(queue.push("https://example.test/one".to_string()));
        assert_eq!(
            queue.pop_front().as_deref(),
            Some("https://example.test/two")
        );
        assert_eq!(
            queue.pop_front().as_deref(),
            Some("https://example.test/one")
        );
        assert_eq!(queue.pop_front(), None);
    }
}

fn clone_search_results_snapshot(
    results: &Arc<Vec<SearchImageResult>>,
) -> Arc<Vec<SearchImageResult>> {
    Arc::clone(results)
}

fn clone_queue_snapshot(
    snapshot: &Arc<crate::models::QueueSnapshot>,
) -> Arc<crate::models::QueueSnapshot> {
    Arc::clone(snapshot)
}

fn queue_range_with_overscan(
    visible: std::ops::Range<usize>,
    pending_total: usize,
) -> std::ops::Range<usize> {
    let start = visible
        .start
        .min(pending_total)
        .saturating_sub(QUEUE_RANGE_OVERSCAN);
    let end = visible
        .end
        .min(pending_total)
        .saturating_add(QUEUE_RANGE_OVERSCAN)
        .min(pending_total);
    start..end.max(start)
}

fn queue_snapshot_pending_at(
    snapshot: &crate::models::QueueSnapshot,
    queue_index: usize,
) -> Option<&str> {
    let offset = queue_index.checked_sub(snapshot.pending_start)?;
    snapshot.pending.get(offset).map(String::as_str)
}

fn update_queue_visible_range(
    current: &mut std::ops::Range<usize>,
    requested: std::ops::Range<usize>,
) -> bool {
    if *current == requested {
        return false;
    }
    *current = requested;
    true
}

fn grid_columns(width: f32, cell: f32, gap: f32, scrollbar: f32, limit: usize) -> usize {
    (((width - scrollbar + gap) / (cell + gap)).floor() as usize).clamp(1, limit.clamp(1, 10))
}

pub(super) fn filter_chip(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    remaining: &mut usize,
) -> bool {
    if *remaining == 0 {
        return false;
    }
    *remaining -= 1;
    ui.small_button(format!("{label} {} ×", truncate(value, 21)))
        .on_hover_text(format!("Remove {label} filter: {value}"))
        .clicked()
}

fn image_filter_count(query: &SearchQuery) -> usize {
    usize::from(query.site_key.is_some())
        + query.tags.len()
        + query.actors.len()
        + query.creators.len()
        + query.categories.len()
        + query.studios.len()
        + query.channels.len()
        + usize::from(query.favorites_only)
}

pub(super) fn site_selector(ui: &mut egui::Ui, id: &str, selected: &mut String, sites: &[String]) {
    let label = if selected.is_empty() {
        "All sites".to_owned()
    } else {
        truncate(selected, 18)
    };
    egui::ComboBox::from_id_salt(id)
        .width(108.0)
        .selected_text(label)
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, String::new(), "All sites");
            for key in sites {
                ui.selectable_value(selected, key.clone(), key);
            }
        });
}

pub(super) fn filter_value_chips(
    ui: &mut egui::Ui,
    label: &str,
    field: &mut String,
    values: &[String],
    remaining: &mut usize,
) -> bool {
    for (index, value) in values.iter().enumerate() {
        if *remaining == 0 {
            break;
        }
        if filter_chip(ui, label, value, remaining) {
            *field = values
                .iter()
                .enumerate()
                .filter_map(|(i, value)| (i != index).then_some(value.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            return true;
        }
    }
    false
}

fn search_filter_field_width(available_width: f32, label_width: f32, column_spacing: f32) -> f32 {
    (available_width - label_width - column_spacing).max(80.0)
}

fn search_filter_label(ui: &mut egui::Ui, label: &str, width: f32, row_height: f32) {
    ui.add_sized([width, row_height], egui::Label::new(label));
}

fn render_suggestions(ui: &mut egui::Ui, value: &mut String, suggestions: &mut Vec<String>) {
    if suggestions.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.small("Suggestions:");
        for suggestion in suggestions.clone() {
            if ui.small_button(&suggestion).clicked() {
                *value = replace_last_token(value, &suggestion);
                suggestions.clear();
                ui.ctx().request_repaint();
                return;
            }
        }
    });
}

fn should_open_preview(mode: PreviewOpenMode, response: &egui::Response) -> bool {
    match mode {
        PreviewOpenMode::SingleClick => response.clicked(),
        PreviewOpenMode::DoubleClick => response.double_clicked(),
    }
}

fn looks_like_additional_link_source(url: &str) -> bool {
    let registry = ScraperRegistry::with_all_scrapers();
    registry.classify_url(url.trim()) == PageKind::Listing
}

fn resolve_preview_path(base_dir: &str, stored: &str) -> std::path::PathBuf {
    let p = Path::new(stored);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(base_dir).join(p)
    }
}

fn load_preview_texture<'a>(
    ctx: &egui::Context,
    cache: &'a mut SizedLruCache<String, TextureHandle>,
    loader: &mut PreviewLoader,
    result: &SearchImageResult,
    base_dir: &str,
    thumb_dir: &std::path::Path,
    mode: ImageLoadMode,
) -> Option<&'a TextureHandle> {
    let cache_key = image_texture_cache_key(
        mode,
        result.image_id,
        base_dir,
        thumb_dir,
        result.local_file_path.as_deref(),
        result.thumbnail_path.as_deref(),
    );
    if !cache.contains_key(&cache_key) {
        let _ = loader.request(
            PreviewSource {
                key: cache_key.clone(),
                image_id: result.image_id,
                base_dir: std::path::PathBuf::from(base_dir),
                thumb_dir: thumb_dir.to_path_buf(),
                local_file_path: result.local_file_path.clone(),
                thumbnail_path: result.thumbnail_path.clone(),
                mode,
                max_side: None,
            },
            ctx,
        );
    }

    cache.get(&cache_key)
}

enum OriginalTextureState<'a> {
    Ready(&'a TextureHandle),
    Loading,
    Unavailable,
}

fn advance_viewer_image_transition(
    image_id: i64,
    current_blend: f32,
    target_blend: f32,
    transition: Option<ViewerImageTransition>,
    now: std::time::Instant,
) -> (f32, Option<ViewerImageTransition>) {
    let target_blend = target_blend.clamp(0.0, 1.0);
    let mut blend = current_blend.clamp(0.0, 1.0);
    let mut transition = transition.filter(|transition| transition.image_id == image_id);

    if let Some(active) = transition {
        let progress = (now
            .saturating_duration_since(active.started_at)
            .as_secs_f32()
            / VIEWER_IMAGE_CROSSFADE_DURATION.as_secs_f32())
        .clamp(0.0, 1.0);
        let eased = progress * progress * (3.0 - 2.0 * progress);
        blend = active.from + (active.to - active.from) * eased;
        if progress >= 1.0 {
            blend = active.to;
            transition = None;
        }
    }

    let active_target = transition.map(|transition| transition.to);
    if (blend - target_blend).abs() <= f32::EPSILON {
        blend = target_blend;
        transition = None;
    } else if active_target
        .is_none_or(|active_target| (active_target - target_blend).abs() > f32::EPSILON)
    {
        transition = Some(ViewerImageTransition {
            image_id,
            from: blend,
            to: target_blend,
            started_at: now,
        });
    }

    (blend.clamp(0.0, 1.0), transition)
}

fn white_with_opacity(opacity: f32) -> egui::Color32 {
    egui::Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn viewer_decode_max_side(zoom: f32) -> Option<u32> {
    (zoom < FULL_ORIGINAL_ZOOM_THRESHOLD).then_some(VIEWER_PROXY_MAX_SIDE)
}

fn image_ids_with_actor(results: &[SearchImageResult], actor_name: &str) -> Vec<i64> {
    results
        .iter()
        .filter(|result| {
            result.attributes.iter().any(|attribute| {
                attribute.kind == "actor" && attribute.value.eq_ignore_ascii_case(actor_name)
            })
        })
        .map(|result| result.image_id)
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OriginalFallbackState {
    Loading,
    Unavailable,
}

fn original_fallback_message(state: OriginalFallbackState) -> &'static str {
    match state {
        OriginalFallbackState::Loading => "Loading original… Showing preview.",
        OriginalFallbackState::Unavailable => "Original unavailable. Showing preview.",
    }
}

fn load_original_texture<'a>(
    ctx: &egui::Context,
    cache: &'a mut SizedLruCache<String, TextureHandle>,
    loader: &mut OriginalLoader,
    result: &SearchImageResult,
    base_dir: &str,
    thumb_dir: &std::path::Path,
    max_side: Option<u32>,
) -> OriginalTextureState<'a> {
    let mut cache_key = image_texture_cache_key(
        ImageLoadMode::Original,
        result.image_id,
        base_dir,
        thumb_dir,
        result.local_file_path.as_deref(),
        result.thumbnail_path.as_deref(),
    );
    if let Some(max_side) = max_side {
        cache_key.push_str(&format!("|proxy-{max_side}"));
    }
    if cache.contains_key(&cache_key) {
        return OriginalTextureState::Ready(
            cache
                .get(&cache_key)
                .expect("original cache key disappeared"),
        );
    }

    let disposition = loader.request(
        PreviewSource {
            key: cache_key,
            image_id: result.image_id,
            base_dir: std::path::PathBuf::from(base_dir),
            thumb_dir: thumb_dir.to_path_buf(),
            local_file_path: result.local_file_path.clone(),
            thumbnail_path: result.thumbnail_path.clone(),
            mode: ImageLoadMode::Original,
            max_side,
        },
        ctx,
    );
    match disposition {
        super::preview_loader::PreviewRequestDisposition::Unavailable => {
            OriginalTextureState::Unavailable
        }
        super::preview_loader::PreviewRequestDisposition::Scheduled
        | super::preview_loader::PreviewRequestDisposition::AlreadyPending
        | super::preview_loader::PreviewRequestDisposition::QueueFull => {
            OriginalTextureState::Loading
        }
    }
}

fn get_cached_original_texture<'a>(
    cache: &'a mut SizedLruCache<String, TextureHandle>,
    result: &SearchImageResult,
    base_dir: &str,
    thumb_dir: &std::path::Path,
) -> Option<&'a TextureHandle> {
    let cache_key = image_texture_cache_key(
        ImageLoadMode::Original,
        result.image_id,
        base_dir,
        thumb_dir,
        result.local_file_path.as_deref(),
        result.thumbnail_path.as_deref(),
    );
    let proxy_key = format!("{cache_key}|proxy-{VIEWER_PROXY_MAX_SIDE}");
    if cache.contains_key(&proxy_key) {
        cache.get(&proxy_key)
    } else {
        cache.get(&cache_key)
    }
}

fn image_texture_cache_key(
    mode: ImageLoadMode,
    image_id: i64,
    base_dir: &str,
    thumb_dir: &std::path::Path,
    local_file_path: Option<&str>,
    thumbnail_path: Option<&str>,
) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}",
        mode,
        image_id,
        base_dir,
        thumb_dir.to_string_lossy(),
        local_file_path.unwrap_or_default(),
        thumbnail_path.unwrap_or_default(),
    )
}

pub(super) fn open_in_explorer(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        // CommandExt::raw_arg is required so Rust does NOT wrap the argument in
        // extra quotes.  Command::arg() would produce "/select,C:\my path\f.jpg"
        // (outer quotes) which explorer.exe doesn't understand.
        // raw_arg produces the bare string: /select,"C:\my path\f.jpg"
        #[allow(unused_imports)]
        use std::os::windows::process::CommandExt;

        let path_str = path.to_string_lossy();

        // Prefer /select to highlight the file; fall back to the parent folder
        // when the file itself no longer exists on disk.
        let arg = if path.exists() {
            format!("/select,\"{}\"", path_str)
        } else {
            let parent = path.parent().unwrap_or(path);
            format!("\"{}\"", parent.to_string_lossy())
        };

        let _ = std::process::Command::new("explorer.exe")
            .raw_arg(&arg)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .args(["-R", &path.to_string_lossy()])
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let parent = path.parent().unwrap_or(path);
        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
    }
}

fn containing_folder(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(path)
}

fn open_containing_folder(path: &Path) {
    let _ = open_folder(containing_folder(path));
}

pub(super) fn open_folder(folder: &Path) -> std::io::Result<()> {
    let command = if cfg!(target_os = "windows") {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(command).arg(folder).spawn()?;
    Ok(())
}

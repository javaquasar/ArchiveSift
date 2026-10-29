mod app_window;
mod background_executor;
mod coalescing_worker;
mod preview_loader;
mod storage_settings;
mod texture_cache;

pub use app_window::AppWindow;

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;

use eframe::egui;
use futures::StreamExt;

use crate::app::AppConfig;
use crate::clipboard_listener::{spawn_clipboard_watcher, ClipboardEvent};
use crate::db::{Database, FileDownloadStore, ImageLocalPathUpdate, QueryCancellation};
use crate::fs::StructuredDirectoryStrategy;
use crate::models::{
    DateBrowseDirection, DateGroupCursor, DateGroupPage, DeleteMode, SearchCursor,
    SearchImageResult, SearchQuery, SearchResultPage, UrlQueueReport,
};
use crate::scraper::ScraperRegistry;
use crate::services::url_queue;
use crate::services::{
    ContentDownloadService, SearchService, SettingsService, StorageSettings, UrlQueueService,
};
use crate::ui::{ConsoleUi, SearchRefresh, UiBackend, UiCommand, UiLog};
use crate::AppResult;
use background_executor::{shared_async_runtime, spawn_background};
use coalescing_worker::CoalescingWorker;

const SQL_QUERY_ROW_LIMIT: usize = 2_000;
const SQL_QUERY_COLUMN_LIMIT: usize = 256;
const SQL_QUERY_CELL_BYTE_LIMIT: usize = 64 * 1024;
const SQL_QUERY_TOTAL_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const SQL_CELL_TRUNCATED_SUFFIX: &str = "... [truncated]";
const UI_RESULT_CHANNEL_CAPACITY: usize = 16;

// ── EguiBackend ───────────────────────────────────────────────────────────────

/// The native desktop UI backend, implemented with egui/eframe.
pub struct EguiBackend;

impl UiBackend for EguiBackend {
    fn run(&self, config: AppConfig) -> AppResult<()> {
        let options = eframe::NativeOptions::default();
        eframe::run_native(
            "ArchiveSift",
            options,
            Box::new(move |cc| {
                Ok(Box::new(DesktopApp::new(
                    config.clone(),
                    cc.egui_ctx.clone(),
                )))
            }),
        )
        .map_err(|error| error.to_string().into())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SuggestionKind {
    Tag,
    Actor,
    Category,
    Studio,
}

impl SuggestionKind {
    fn database_kind(self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Actor => "actor",
            Self::Category => "category",
            Self::Studio => "studio",
        }
    }
}

struct SuggestionJob {
    database_path: String,
    query: String,
    kind: SuggestionKind,
}

struct SearchMutationResult {
    results: Vec<SearchImageResult>,
    next_cursor: Option<SearchCursor>,
    replace: bool,
}

// ── DesktopApp (eframe::App implementation) ───────────────────────────────────

struct DesktopApp {
    window: AppWindow,
    queue_service: Arc<UrlQueueService>,
    worker_running: bool,
    state_rx: mpsc::Receiver<WorkerState>,
    state_tx: RepaintingSender<WorkerState>,
    suggestion_worker: CoalescingWorker<SuggestionKind, SuggestionJob>,
    search_cancellation: Option<QueryCancellation>,
    repaint_context: egui::Context,
    /// Created on demand when clipboard monitoring is enabled.
    clipboard_rx: Option<mpsc::Receiver<ClipboardEvent>>,
    /// Live preset tags/categories shared with the running queue processor.
    /// Updated by the UI whenever the user changes the preset fields.
    live_presets: Arc<RwLock<LivePresets>>,
}

struct RepaintingSender<T> {
    sender: mpsc::SyncSender<T>,
    repaint_context: egui::Context,
}

fn repainting_channel<T>(
    repaint_context: &egui::Context,
    capacity: usize,
) -> (RepaintingSender<T>, mpsc::Receiver<T>) {
    let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
    (
        RepaintingSender {
            sender,
            repaint_context: repaint_context.clone(),
        },
        receiver,
    )
}

impl<T> Clone for RepaintingSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            repaint_context: self.repaint_context.clone(),
        }
    }
}

impl<T> RepaintingSender<T> {
    fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        self.sender.send(value)?;
        self.repaint_context.request_repaint();
        Ok(())
    }
}

fn background_poll_delay(
    queue_worker_running: bool,
    queue_paused: bool,
) -> Option<std::time::Duration> {
    (queue_worker_running && !queue_paused).then(|| std::time::Duration::from_millis(100))
}

fn try_start_import(import_running: &mut bool) -> bool {
    if *import_running {
        return false;
    }
    *import_running = true;
    true
}

fn clipboard_event_channel(repaint_context: &egui::Context) -> mpsc::Receiver<ClipboardEvent> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let repaint_context = repaint_context.clone();
    spawn_clipboard_watcher(sender, move || repaint_context.request_repaint());
    receiver
}

/// Preset tags and categories that the queue processor reads live for each content item,
/// so changes made in the UI take effect without restarting the processor.
#[derive(Clone, Default)]
pub(crate) struct LivePresets {
    pub tags: String,
    pub categories: String,
}

fn suggestion_worker(
    state_tx: &RepaintingSender<WorkerState>,
) -> CoalescingWorker<SuggestionKind, SuggestionJob> {
    let suggestion_tx = state_tx.clone();
    CoalescingWorker::new(move |job: SuggestionJob| {
        let results = run_suggestions(
            job.database_path,
            job.kind.database_kind(),
            job.query.clone(),
        )
        .unwrap_or_default();
        let state = match job.kind {
            SuggestionKind::Tag => WorkerState::TagSuggestions {
                query: job.query,
                results,
            },
            SuggestionKind::Actor => WorkerState::ActorSuggestions {
                query: job.query,
                results,
            },
            SuggestionKind::Category => WorkerState::CategorySuggestions {
                query: job.query,
                results,
            },
            SuggestionKind::Studio => WorkerState::StudioSuggestions {
                query: job.query,
                results,
            },
        };
        let _ = suggestion_tx.send(state);
    })
}

impl DesktopApp {
    fn new(config: AppConfig, repaint_context: egui::Context) -> Self {
        let (state_tx, state_rx) = repainting_channel(&repaint_context, UI_RESULT_CHANNEL_CAPACITY);
        let suggestion_worker = suggestion_worker(&state_tx);
        let queue_service = Arc::new(UrlQueueService::new());

        let live_presets = Arc::new(RwLock::new(LivePresets {
            tags: config.preset_tags.clone(),
            categories: config.preset_categories.clone(),
        }));

        // Destination setup lives in the Scrape tab. Keep Search, SQL and
        // Extensions accessible even before a download folder is configured.
        let clipboard_rx = config
            .clipboard_listener_enabled
            .then(|| clipboard_event_channel(&repaint_context));
        let window = AppWindow::new(config, UiLog::default(), Arc::clone(&live_presets));

        let app = Self {
            window,
            queue_service,
            worker_running: false,
            state_rx,
            state_tx,
            suggestion_worker,
            search_cancellation: None,
            repaint_context,
            clipboard_rx,
            live_presets,
        };

        // The SQLite-backed queue restores interrupted work itself. Remove
        // entries that were fully persisted before a previous unclean exit.
        let db = Database::new(&app.window.config.database_path);
        let _ = app.queue_service.prune_processed(&db);

        app
    }

    fn enqueue_manual_url(&mut self, config: AppConfig) {
        let window = &mut self.window;
        if !config.has_download_folder() {
            window.error = Some("Select a download directory to enable scraping.".to_string());
            return;
        }
        window.error = None;
        let settings = SettingsService::new();
        let mut config = config;
        let active_folder = config.download_folder.clone();
        settings.set_active_download_dir(&mut config, active_folder);
        let _ = config.save();
        window.config = config.clone();

        let database = Database::new(&config.database_path);
        match self
            .queue_service
            .enqueue_urls(&database, vec![config.source_url.clone()])
        {
            Ok(report) => {
                let window = &mut self.window;
                window.apply_queue_report(&report, "Manual queue");
                self.start_processor_if_needed(config);
            }
            Err(error) => {
                let window = &mut self.window;
                window.error = Some(error.to_string());
                window
                    .log
                    .push(format!("Failed to queue manual URL: {}", error));
            }
        }
    }

    fn start_import_url_file(&mut self, file_path: String) {
        let window = &mut self.window;
        if !window.config.has_download_folder() {
            window.error = Some("Select a download directory to enable scraping.".to_string());
            return;
        }
        if !try_start_import(&mut window.import_running) {
            window.log.push("A URL-file import is already running.");
            return;
        }

        window.error = None;
        window.queue_status = Some(format!("Importing {} in the background…", file_path));
        window.log.push(format!("Import started: {}", file_path));

        let database_path = window.config.database_path.clone();
        let retain_processed_decisions = !window.config.skip_downloaded;
        let state_tx = self.state_tx.clone();
        let queue_service = Arc::clone(&self.queue_service);
        let _ = spawn_import_worker(
            queue_service,
            database_path,
            file_path,
            retain_processed_decisions,
            move |file_path, result| {
                let _ = state_tx.send(WorkerState::ImportFinished { file_path, result });
            },
        );
    }

    fn set_active_download_folder(&mut self, folder: String) {
        let window = &mut self.window;
        let settings = SettingsService::new();
        settings.set_active_download_dir(&mut window.config, &folder);
        if let Err(error) = window.config.save() {
            window.error = Some(error.to_string());
            window
                .log
                .push(format!("Failed to save download folder history: {}", error));
        }
    }

    fn start_storage_settings(&mut self, settings: StorageSettings, config_path: PathBuf) {
        if self.window.storage_settings_applying() {
            return;
        }
        if self.worker_running || self.window.storage_settings_busy() {
            self.window.storage_settings_failed(
                "Wait for active operations to finish before changing storage settings."
                    .to_string(),
            );
            return;
        }
        self.window.begin_storage_settings();
        let config = self.window.config.clone();
        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = SettingsService::new()
                .apply_storage_settings(&config, &settings, &config_path)
                .map_err(|error| error.to_string());
            let _ = state_tx.send(WorkerState::StorageSettingsApplied(result));
        });
    }

    fn finish_storage_settings(&mut self, result: Result<AppConfig, String>) {
        match result {
            Ok(config) => {
                if let Some(cancellation) = self.search_cancellation.take() {
                    cancellation.cancel();
                }
                // Old metadata and suggestion jobs must never populate the new
                // library, even if their database IDs happen to match.
                let (state_tx, state_rx) =
                    repainting_channel(&self.repaint_context, UI_RESULT_CHANNEL_CAPACITY);
                self.suggestion_worker = suggestion_worker(&state_tx);
                self.state_tx = state_tx;
                self.state_rx = state_rx;
                let log = self.window.log.clone();
                log.push("Storage settings saved and applied.");
                self.window.apply_storage_config(config);
            }
            Err(error) => {
                self.window
                    .log
                    .push(format!("Could not apply storage settings: {error}"));
                self.window.storage_settings_failed(error);
            }
        }
    }

    fn set_clipboard_listener_enabled(&mut self, enabled: bool) {
        let window = &mut self.window;
        window.config.clipboard_listener_enabled = enabled;
        if let Err(error) = window.config.save() {
            window.error = Some(error.to_string());
            window.log.push(format!(
                "Failed to save clipboard listener setting: {}",
                error
            ));
            return;
        }
        // Start the watcher on-demand if it was never spawned (e.g. launched in Manual mode).
        if enabled && self.clipboard_rx.is_none() {
            self.clipboard_rx = Some(clipboard_event_channel(&self.repaint_context));
        }
        window.log.push(if enabled {
            "Clipboard listener enabled."
        } else {
            "Clipboard listener disabled."
        });
    }

    fn handle_clipboard_events(&mut self) {
        let rx = match &self.clipboard_rx {
            Some(rx) => rx,
            None => return,
        };

        let window = &mut self.window;

        if !window.config.clipboard_listener_enabled
            || !window.config.has_download_folder()
            || window.storage_settings_applying()
        {
            while rx.try_recv().is_ok() {}
            return;
        }

        let mut to_process: Vec<ClipboardEvent> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            to_process.push(event);
        }

        let config = window.config.clone();
        for event in to_process {
            match event {
                ClipboardEvent::Text(text) => {
                    // Pre-extract URL candidates; if none found, skip silently.
                    let candidates = url_queue::extract_candidate_urls(&text);
                    if candidates.is_empty() {
                        continue;
                    }

                    let database = Database::new(&config.database_path);
                    match self.queue_service.enqueue_text(&database, &text) {
                        Ok(report) => {
                            let window = &mut self.window;
                            // Log each found URL individually, not the raw clipboard text.
                            if candidates.len() == 1 {
                                window.log.push(format!("Clipboard: {}", candidates[0]));
                            } else {
                                window.log.push(format!(
                                    "Clipboard: {} URL{} detected",
                                    candidates.len(),
                                    if candidates.len() == 1 { "" } else { "s" }
                                ));
                                for url in &candidates {
                                    window.log.push(format!("  • {}", url));
                                }
                            }
                            window.apply_queue_report(&report, "Clipboard");
                            if report.queued_successfully > 0 {
                                let cfg = window.config.clone();
                                self.start_processor_if_needed(cfg);
                            }
                        }
                        Err(error) => {
                            let window = &mut self.window;
                            window
                                .log
                                .push(format!("Clipboard processing failed: {}", error));
                        }
                    }
                }
                ClipboardEvent::Error(error) => {
                    let window = &mut self.window;
                    window.log.push(error);
                }
            }
        }
    }

    fn start_search(
        &mut self,
        database_path: String,
        query: SearchQuery,
        seq: u64,
        cursor: Option<SearchCursor>,
        append: bool,
    ) {
        let window = &mut self.window;
        window.running = true;
        window.error = None;

        if let Some(cancellation) = self.search_cancellation.take() {
            cancellation.cancel();
        }
        let cancellation = QueryCancellation::default();
        self.search_cancellation = Some(cancellation.clone());
        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = run_search_page_job(database_path, query, cursor, &cancellation);
            let worker_state = match result {
                Ok(results) => WorkerState::SearchFinished {
                    seq,
                    append,
                    result: Ok(results),
                },
                Err(error) => WorkerState::SearchFinished {
                    seq,
                    append,
                    result: Err(error.to_string()),
                },
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_search_recent_pages(
        &mut self,
        database_path: String,
        page_limit: usize,
        seq: u64,
        cursor: Option<SearchCursor>,
        append: bool,
    ) {
        let window = &mut self.window;
        window.running = true;
        window.error = None;
        if let Some(cancellation) = self.search_cancellation.take() {
            cancellation.cancel();
        }
        let cancellation = QueryCancellation::default();
        self.search_cancellation = Some(cancellation.clone());

        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = run_search_recent_pages(database_path, page_limit, cursor, &cancellation);
            let worker_state = match result {
                Ok(page) => WorkerState::SearchFinished {
                    seq,
                    append,
                    result: Ok(page),
                },
                Err(error) => WorkerState::SearchFinished {
                    seq,
                    append,
                    result: Err(error.to_string()),
                },
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_tag_suggestions(&mut self, database_path: String, query: String) {
        self.suggestion_worker.submit(
            SuggestionKind::Tag,
            SuggestionJob {
                database_path,
                query,
                kind: SuggestionKind::Tag,
            },
        );
    }

    fn start_actor_suggestions(&mut self, database_path: String, query: String) {
        self.suggestion_worker.submit(
            SuggestionKind::Actor,
            SuggestionJob {
                database_path,
                query,
                kind: SuggestionKind::Actor,
            },
        );
    }

    fn start_category_suggestions(&mut self, database_path: String, query: String) {
        self.suggestion_worker.submit(
            SuggestionKind::Category,
            SuggestionJob {
                database_path,
                query,
                kind: SuggestionKind::Category,
            },
        );
    }

    fn start_studio_suggestions(&mut self, database_path: String, query: String) {
        self.suggestion_worker.submit(
            SuggestionKind::Studio,
            SuggestionJob {
                database_path,
                query,
                kind: SuggestionKind::Studio,
            },
        );
    }

    fn start_search_metadata(&self, database_path: String, image_ids: Vec<i64>) {
        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = SearchService::new(Database::new(database_path))
                .load_images_by_ids(&image_ids)
                .map_err(|error| error.to_string());
            let _ = state_tx.send(WorkerState::SearchMetadataLoaded {
                requested_ids: image_ids,
                result,
            });
        });
    }

    fn start_metadata_job(
        &mut self,
        message: impl Into<String>,
        job: impl FnOnce() -> AppResult<SearchMutationResult> + Send + 'static,
    ) {
        let window = &mut self.window;
        window.running = true;
        window.error = None;

        let message = message.into();
        let state_tx = self.state_tx.clone();
        let success_msg = message.clone();
        spawn_background(move || {
            let result = job();
            let worker_state = match result {
                Ok(results) => WorkerState::MetadataUpdated {
                    message: success_msg,
                    results: Ok(results),
                },
                Err(error) => WorkerState::MetadataUpdated {
                    message,
                    results: Err(error.to_string()),
                },
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_set_favorite(
        &mut self,
        database_path: String,
        image_id: i64,
        is_favorite: bool,
        refresh: SearchRefresh,
    ) {
        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = run_set_favorite(database_path, image_id, is_favorite, refresh);
            let worker_state = match result {
                Ok(results) => WorkerState::FavoriteUpdated(Ok(results)),
                Err(error) => WorkerState::FavoriteUpdated(Err(error.to_string())),
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_set_actor_gender(
        &mut self,
        database_path: String,
        actor_name: String,
        is_female: Option<bool>,
        image_ids: Vec<i64>,
        refresh: SearchRefresh,
    ) {
        let window = &mut self.window;
        window.running = true;
        window.error = None;

        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result =
                run_set_actor_gender(database_path, actor_name, is_female, image_ids, refresh);
            let worker_state = match result {
                Ok(results) => WorkerState::ActorGenderUpdated(Ok(results)),
                Err(error) => WorkerState::ActorGenderUpdated(Err(error.to_string())),
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_set_primary_actor(
        &mut self,
        database_path: String,
        download_folder: String,
        page_id: i64,
        actor_name: String,
        refresh: SearchRefresh,
    ) {
        let window = &mut self.window;
        window.running = true;
        window.error = None;

        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result =
                run_set_primary_actor(database_path, download_folder, page_id, actor_name, refresh);
            let worker_state = match result {
                Ok(results) => WorkerState::PrimaryActorUpdated(Ok(results)),
                Err(error) => WorkerState::PrimaryActorUpdated(Err(error.to_string())),
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_delete_images(
        &mut self,
        database_path: String,
        base_dir: String,
        image_ids: Vec<i64>,
        mode: DeleteMode,
        refresh: SearchRefresh,
    ) {
        let window = &mut self.window;
        window.running = true;
        window.error = None;

        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let removed_ids = image_ids.clone();
            let result = run_delete_images(database_path, base_dir, image_ids, mode, refresh)
                .map_err(|error| error.to_string());
            let worker_state = match result {
                Ok(()) => WorkerState::DeleteFinished {
                    image_ids: removed_ids,
                    result: Ok(()),
                },
                Err(error) => WorkerState::DeleteFinished {
                    image_ids: removed_ids,
                    result: Err(error),
                },
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_rescrape(
        &mut self,
        database_path: String,
        download_dir: String,
        url: String,
        max_concurrent_downloads: usize,
    ) {
        let window = &mut self.window;
        if download_dir.trim().is_empty() {
            window.error = Some("Select a download directory to enable scraping.".to_string());
            return;
        }
        window.running = true;
        window.log.push(format!("Re-scraping: {}", url));

        let state_tx = self.state_tx.clone();
        let log = window.log.clone();
        spawn_background(move || {
            let result = run_rescrape(
                database_path,
                download_dir,
                url,
                max_concurrent_downloads,
                log,
            );
            let worker_state = match result {
                Ok(()) => WorkerState::QueueFinished(Ok(())),
                Err(error) => WorkerState::QueueFinished(Err(error.to_string())),
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_processor_if_needed(&mut self, config: AppConfig) {
        if !config.has_download_folder()
            || self.worker_running
            || !self.queue_service.has_pending_work()
        {
            return;
        }

        let window = &mut self.window;
        self.worker_running = true;
        window.running = true;
        window.log.push("Starting queued URL processing...");

        let state_tx = self.state_tx.clone();
        let queue_service = Arc::clone(&self.queue_service);
        let log = window.log.clone();
        let max_parallel = config.effective_parallel_downloads();
        let live_presets = Arc::clone(&self.live_presets);

        spawn_background(move || {
            let result =
                run_queue_processor(config, queue_service, log, max_parallel, live_presets);
            let worker_state = match result {
                Ok(()) => WorkerState::QueueFinished(Ok(())),
                Err(error) => WorkerState::QueueFinished(Err(error.to_string())),
            };
            let _ = state_tx.send(worker_state);
        });
    }
}

impl DesktopApp {
    fn update_ui(&mut self, ctx: &egui::Context) {
        self.handle_clipboard_events();

        while let Ok(state) = self.state_rx.try_recv() {
            match state {
                WorkerState::StorageSettingsApplied(result) => self.finish_storage_settings(result),
                WorkerState::ImportFinished { file_path, result } => {
                    let config_to_start = {
                        let window = &mut self.window;
                        window.import_running = false;
                        match result {
                            Ok(report) => {
                                let should_start = report.queued_successfully > 0;
                                window.apply_queue_report(
                                    &report,
                                    &format!("Imported {}", file_path),
                                );
                                should_start.then(|| window.config.clone())
                            }
                            Err(error) => {
                                window.error = Some(error.clone());
                                window.queue_status = Some(format!("Import failed: {}", file_path));
                                window.log.push(format!(
                                    "Failed to import URL file {}: {}",
                                    file_path, error
                                ));
                                None
                            }
                        }
                    };
                    if let Some(config) = config_to_start {
                        self.start_processor_if_needed(config);
                    }
                }
                WorkerState::QueueFinished(Ok(())) => {
                    self.worker_running = false;
                    {
                        let w = &mut self.window;
                        w.running = false;
                        w.error = None;
                        w.log.push("Queue finished.");
                    }
                    let cfg = self.window.config.clone();
                    self.start_processor_if_needed(cfg);
                }
                WorkerState::QueueFinished(Err(error)) => {
                    self.worker_running = false;
                    {
                        let w = &mut self.window;
                        w.running = false;
                        w.error = Some(error.clone());
                        w.log.push(format!("Queue failed: {}", error));
                    }
                    let cfg = self.window.config.clone();
                    self.start_processor_if_needed(cfg);
                }
                WorkerState::SearchFinished {
                    seq,
                    append,
                    result,
                } => {
                    let w = &mut self.window;
                    if w.search_seq == seq {
                        w.running = false;
                        match result {
                            Ok(page) => {
                                w.error = None;
                                w.set_search_page(page, append);
                            }
                            Err(error) => {
                                w.error = Some(error);
                            }
                        }
                    }
                }
                WorkerState::SearchMetadataLoaded {
                    requested_ids,
                    result,
                } => {
                    let w = &mut self.window;
                    match result {
                        Ok(results) => w.apply_hydrated_results(&requested_ids, results),
                        Err(error) => {
                            w.apply_hydrated_results(&requested_ids, Vec::new());
                            w.log.push(format!("Metadata load failed: {error}"));
                        }
                    }
                }
                WorkerState::TagSuggestions { query, results } => {
                    let w = &mut self.window;
                    w.set_tag_suggestions(&query, results);
                }
                WorkerState::ActorSuggestions { query, results } => {
                    let w = &mut self.window;
                    w.set_actor_suggestions(&query, results);
                }
                WorkerState::CategorySuggestions { query, results } => {
                    let w = &mut self.window;
                    w.set_category_suggestions(&query, results);
                }
                WorkerState::StudioSuggestions { query, results } => {
                    let w = &mut self.window;
                    w.set_studio_suggestions(&query, results);
                }
                WorkerState::MetadataUpdated { message, results } => {
                    let w = &mut self.window;
                    w.running = false;
                    match results {
                        Ok(results) => {
                            w.apply_metadata_update(message);
                            if results.replace {
                                w.refresh_search_page(SearchResultPage {
                                    items: results.results,
                                    next_cursor: results.next_cursor,
                                });
                            } else {
                                w.patch_search_results(results.results);
                            }
                        }
                        Err(error) => {
                            w.error = Some(format!("{}: {}", message, error));
                            w.log.push(format!("{}: {}", message, error));
                        }
                    }
                }
                WorkerState::FavoriteUpdated(result) => {
                    let w = &mut self.window;
                    w.running = false;
                    match result {
                        Ok(results) => {
                            if results.replace {
                                w.refresh_search_page(SearchResultPage {
                                    items: results.results,
                                    next_cursor: results.next_cursor,
                                });
                            } else {
                                w.patch_search_results(results.results);
                            }
                        }
                        Err(error) => {
                            w.error = Some(error.clone());
                            w.log.push(format!("Favorite update failed: {}", error));
                        }
                    }
                }
                WorkerState::ActorGenderUpdated(result) => {
                    let w = &mut self.window;
                    w.running = false;
                    match result {
                        Ok(results) => {
                            w.error = None;
                            w.log.push("Actor gender updated.");
                            w.patch_search_results(results);
                        }
                        Err(error) => {
                            w.error = Some(error.clone());
                            w.log.push(format!("Actor gender update failed: {}", error));
                        }
                    }
                }
                WorkerState::PrimaryActorUpdated(result) => {
                    let w = &mut self.window;
                    w.running = false;
                    match result {
                        Ok(results) => {
                            w.error = None;
                            w.log.push("Primary actor updated — images moved.");
                            w.patch_search_results(results);
                        }
                        Err(error) => {
                            w.error = Some(error.clone());
                            w.log
                                .push(format!("Primary actor update failed: {}", error));
                        }
                    }
                }
                WorkerState::SqlQueryFinished(result) => {
                    let w = &mut self.window;
                    w.apply_sql_result(result);
                }
                WorkerState::SqlExportFinished {
                    output_path,
                    result,
                } => {
                    let w = &mut self.window;
                    w.apply_sql_export_result(&output_path, result);
                }
                WorkerState::DateGroupsLoaded(result) => {
                    let w = &mut self.window;
                    w.apply_date_groups_loaded(result);
                }
                WorkerState::DeleteFinished { image_ids, result } => {
                    let w = &mut self.window;
                    w.running = false;
                    match result {
                        Ok(()) => {
                            w.log.push("Images deleted.");
                            w.remove_search_results(&image_ids);
                        }
                        Err(error) => {
                            w.error = Some(error.clone());
                            w.log.push(format!("Delete failed: {}", error));
                        }
                    }
                }
            }
        }

        // Only clone the full pending URL list when the queue window is actually
        // visible — with 1000+ URLs in the queue, doing this every frame causes
        // noticeable UI lag on every keystroke.
        {
            let window = &mut self.window;
            if window.queue_window_open {
                let range = window.queue_visible_range.clone();
                window.queue_snapshot = Arc::new(self.queue_service.snapshot_range(range));
            } else {
                // Lightweight update: just the paused flag and in-progress items
                // (typically ≤ 16 entries), no full pending clone.
                let s = self.queue_service.snapshot_light();
                window.queue_snapshot = Arc::new(s);
            }
        }

        {
            let window = &mut self.window;
            window.queue_worker_running = self.worker_running;
            window.render(ctx);
        }

        if let Some(command) = self.window.take_command() {
            match command {
                UiCommand::QueueUrl(config) => self.enqueue_manual_url(config),
                UiCommand::ImportFile { file_path } => self.start_import_url_file(file_path),
                UiCommand::SetDownloadFolder(folder) => self.set_active_download_folder(folder),
                UiCommand::ApplyStorageSettings(settings) => {
                    self.start_storage_settings(settings, AppConfig::config_path().to_path_buf())
                }
                UiCommand::SetClipboardListener(enabled) => {
                    self.set_clipboard_listener_enabled(enabled)
                }
                UiCommand::Search {
                    database_path,
                    query,
                    seq,
                    cursor,
                    append,
                } => self.start_search(database_path, query, seq, cursor, append),
                UiCommand::HydrateSearchMetadata {
                    database_path,
                    image_ids,
                } => self.start_search_metadata(database_path, image_ids),
                UiCommand::SearchRecentPages {
                    database_path,
                    page_limit,
                    seq,
                    cursor,
                    append,
                } => self.start_search_recent_pages(database_path, page_limit, seq, cursor, append),
                UiCommand::SuggestTags {
                    database_path,
                    query,
                } => self.start_tag_suggestions(database_path, query),
                UiCommand::SuggestActors {
                    database_path,
                    query,
                } => self.start_actor_suggestions(database_path, query),
                UiCommand::SuggestCategories {
                    database_path,
                    query,
                } => self.start_category_suggestions(database_path, query),
                UiCommand::SuggestStudios {
                    database_path,
                    query,
                } => self.start_studio_suggestions(database_path, query),
                UiCommand::AddTagsToImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Added tags.", move || {
                    run_add_attributes(database_path, image_ids, values, "tag", "Tag", refresh)
                }),
                UiCommand::RemoveTagsFromImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Removed tags.", move || {
                    run_remove_attributes(database_path, image_ids, values, "tag", refresh)
                }),
                UiCommand::AddCategoriesToImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Added categories.", move || {
                    run_add_attributes(
                        database_path,
                        image_ids,
                        values,
                        "category",
                        "Category",
                        refresh,
                    )
                }),
                UiCommand::RemoveCategoriesFromImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Removed categories.", move || {
                    run_remove_attributes(database_path, image_ids, values, "category", refresh)
                }),
                UiCommand::AddActorsToImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Added actors.", move || {
                    run_add_attributes(database_path, image_ids, values, "actor", "Actor", refresh)
                }),
                UiCommand::RemoveActorsFromImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Removed actors.", move || {
                    run_remove_attributes(database_path, image_ids, values, "actor", refresh)
                }),
                UiCommand::AddStudiosToImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Added studios.", move || {
                    run_add_attributes(
                        database_path,
                        image_ids,
                        values,
                        "studio",
                        "Studio",
                        refresh,
                    )
                }),
                UiCommand::RemoveStudiosFromImages {
                    database_path,
                    image_ids,
                    values,
                    refresh,
                } => self.start_metadata_job("Removed studios.", move || {
                    run_remove_attributes(database_path, image_ids, values, "studio", refresh)
                }),
                UiCommand::SetFavorite {
                    database_path,
                    image_id,
                    is_favorite,
                    refresh,
                } => self.start_set_favorite(database_path, image_id, is_favorite, refresh),
                UiCommand::SetActorGender {
                    database_path,
                    actor_name,
                    is_female,
                    image_ids,
                    refresh,
                } => self.start_set_actor_gender(
                    database_path,
                    actor_name,
                    is_female,
                    image_ids,
                    refresh,
                ),
                UiCommand::SetPrimaryActor {
                    database_path,
                    download_folder,
                    page_id,
                    actor_name,
                    refresh,
                } => self.start_set_primary_actor(
                    database_path,
                    download_folder,
                    page_id,
                    actor_name,
                    refresh,
                ),
                UiCommand::DeleteImages {
                    database_path,
                    base_dir,
                    image_ids,
                    mode,
                    refresh,
                } => self.start_delete_images(database_path, base_dir, image_ids, mode, refresh),
                UiCommand::RescrapeUrl {
                    database_path,
                    download_dir,
                    url,
                    max_concurrent_downloads,
                } => {
                    self.start_rescrape(database_path, download_dir, url, max_concurrent_downloads)
                }
                UiCommand::PauseQueue => {
                    self.queue_service.pause();
                    {
                        let w = &mut self.window;
                        w.log.push("Queue paused.");
                    }
                }
                UiCommand::ResumeQueue => {
                    self.queue_service.resume();
                    {
                        let w = &mut self.window;
                        w.log.push("Queue resumed.");
                    }
                    let cfg = self.window.config.clone();
                    self.start_processor_if_needed(cfg);
                }
                UiCommand::RetryFailedQueue => {
                    let retried = self.queue_service.retry_failed();
                    self.queue_service.resume();
                    {
                        let w = &mut self.window;
                        w.log
                            .push(format!("Retrying {retried} failed queue item(s)."));
                    }
                    let cfg = self.window.config.clone();
                    self.start_processor_if_needed(cfg);
                }
                UiCommand::ClearPendingQueue => {
                    self.queue_service.clear_pending();
                    {
                        let w = &mut self.window;
                        w.log.push("Pending queue cleared.");
                    }
                }
                UiCommand::RunSqlQuery {
                    database_path,
                    query,
                } => {
                    self.start_sql_query(database_path, query);
                }
                UiCommand::ExportSqlQuery {
                    database_path,
                    query,
                    output_path,
                } => {
                    self.start_sql_export(database_path, query, output_path);
                }
                UiCommand::BrowseDateGroups {
                    database_path,
                    cursor,
                    direction,
                    ascending,
                    max_favorites_exclusive,
                } => {
                    self.start_browse_date_groups(
                        database_path,
                        cursor,
                        direction,
                        ascending,
                        max_favorites_exclusive,
                    );
                }
            }
        }

        if let Some(delay) =
            background_poll_delay(self.worker_running, self.queue_service.is_paused())
        {
            ctx.request_repaint_after(delay);
        }
    }
}

impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_ui(ctx);
    }
}

impl DesktopApp {
    fn start_sql_query(&self, database_path: String, query: String) {
        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = run_sql_query(database_path, query);
            let worker_state = match result {
                Ok(r) => WorkerState::SqlQueryFinished(Ok(r)),
                Err(e) => WorkerState::SqlQueryFinished(Err(e.to_string())),
            };
            let _ = state_tx.send(worker_state);
        });
    }

    fn start_sql_export(&self, database_path: String, query: String, output_path: String) {
        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = run_sql_export(&database_path, &query, &output_path)
                .map_err(|error| error.to_string());
            let _ = state_tx.send(WorkerState::SqlExportFinished {
                output_path,
                result,
            });
        });
    }

    fn start_browse_date_groups(
        &mut self,
        database_path: String,
        cursor: Option<DateGroupCursor>,
        direction: DateBrowseDirection,
        ascending: bool,
        max_favorites_exclusive: Option<usize>,
    ) {
        {
            let w = &mut self.window;
            w.date_browse_loading = true;
        }
        let state_tx = self.state_tx.clone();
        spawn_background(move || {
            let result = Database::new(database_path).get_date_group_page(
                cursor.as_ref(),
                direction,
                ascending,
                max_favorites_exclusive,
            );
            let worker_state = match result {
                Ok(groups) => WorkerState::DateGroupsLoaded(Ok(groups)),
                Err(e) => WorkerState::DateGroupsLoaded(Err(e.to_string())),
            };
            let _ = state_tx.send(worker_state);
        });
    }
}

// ── Worker state & background jobs ───────────────────────────────────────────

enum WorkerState {
    StorageSettingsApplied(Result<AppConfig, String>),
    ImportFinished {
        file_path: String,
        result: Result<UrlQueueReport, String>,
    },
    QueueFinished(Result<(), String>),
    SearchFinished {
        seq: u64,
        append: bool,
        result: Result<SearchResultPage, String>,
    },
    SearchMetadataLoaded {
        requested_ids: Vec<i64>,
        result: Result<Vec<SearchImageResult>, String>,
    },
    TagSuggestions {
        query: String,
        results: Vec<String>,
    },
    ActorSuggestions {
        query: String,
        results: Vec<String>,
    },
    CategorySuggestions {
        query: String,
        results: Vec<String>,
    },
    StudioSuggestions {
        query: String,
        results: Vec<String>,
    },
    MetadataUpdated {
        message: String,
        results: Result<SearchMutationResult, String>,
    },
    FavoriteUpdated(Result<SearchMutationResult, String>),
    ActorGenderUpdated(Result<Vec<SearchImageResult>, String>),
    PrimaryActorUpdated(Result<Vec<SearchImageResult>, String>),
    DeleteFinished {
        image_ids: Vec<i64>,
        result: Result<(), String>,
    },
    SqlQueryFinished(Result<app_window::SqlQueryResult, String>),
    SqlExportFinished {
        output_path: String,
        result: Result<usize, String>,
    },
    DateGroupsLoaded(Result<DateGroupPage, String>),
}

fn run_queue_processor(
    config: AppConfig,
    queue_service: Arc<UrlQueueService>,
    log: UiLog,
    max_parallel: usize,
    live_presets: Arc<RwLock<LivePresets>>,
) -> AppResult<()> {
    let runtime = shared_async_runtime()
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
    runtime.block_on(async move {
        use futures::stream::FuturesUnordered;

        let base_dir = config.effective_download_dir();
        let concurrency = max_parallel.max(1);

        // Each future returns the processed URL plus any newly-discovered URLs
        // (non-empty only for listing pages).
        type BoxedFut = std::pin::Pin<
            Box<dyn std::future::Future<Output = (String, AppResult<Vec<String>>)> + Send>,
        >;
        let mut active: FuturesUnordered<BoxedFut> = FuturesUnordered::new();

        loop {
            // Fill up to concurrency limit when not paused.
            while !queue_service.is_paused() && active.len() < concurrency {
                match queue_service.take_next_url() {
                    Some(url) => {
                        let mut config = config.clone();
                        // Read the latest preset tags/categories from the shared
                        // RwLock so changes made in the UI take effect immediately
                        // without needing to restart the queue processor.
                        if let Ok(p) = live_presets.read() {
                            config.preset_tags = p.tags.clone();
                            config.preset_categories = p.categories.clone();
                        }
                        let base_dir = base_dir.clone();
                        let log = log.clone();
                        active.push(Box::pin(async move {
                            // ── Random throttle delay ─────────────────────────
                            let delay_ms = sample_delay(
                                config.request_delay_min_ms,
                                config.request_delay_max_ms,
                            );
                            if delay_ms > 0 {
                                tokio::time::sleep(
                                    std::time::Duration::from_millis(delay_ms),
                                )
                                .await;
                            }
                            // ─────────────────────────────────────────────────
                            let thumb_dir = config.thumb_dir();
                            let service = ContentDownloadService::new(
                                ScraperRegistry::with_all_scrapers(),
                                Database::new(&config.database_path),
                                FileDownloadStore::new(),
                                ConsoleUi::from_log(log),
                                StructuredDirectoryStrategy,
                                base_dir,
                                thumb_dir,
                            );
                            let result = service.run_for_url(&config, &url).await;
                            (url, result)
                        }));
                    }
                    None => break,
                }
            }

            if active.is_empty() {
                if queue_service.has_pending_work() {
                    // Paused with items still in queue — wait.
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
                break; // All done.
            }

            // Wait for any task to complete.
            if let Some((url, result)) = active.next().await {
                if result.is_ok() {
                    queue_service.mark_finished(&url);
                }
                match result {
                    Ok(discovered) if !discovered.is_empty() => {
                        // Listing page — re-enqueue discovered content URLs (and next page).
                        log.push(format!(
                            "Listing [{}]: {} URL{} discovered",
                            url,
                            discovered.len(),
                            if discovered.len() == 1 { "" } else { "s" }
                        ));
                        let db = Database::new(&config.database_path);
                        match queue_service.enqueue_urls_summary(&db, discovered, false) {
                            Ok(report) => {
                                log.push(format!(
                                    "Queue update: loaded {}, queued {}, skipped invalid/empty {}, skipped duplicate/processed {}",
                                    report.loaded,
                                    report.queued_successfully,
                                    report.empty_or_invalid,
                                    report.duplicate_or_processed
                                ));
                                debug_assert!(report.decisions.is_empty());
                            }
                            Err(error) => {
                                log.push(format!(
                                    "Failed to enqueue discovered URLs from [{}]: {}",
                                    url, error
                                ));
                            }
                        }
                    }
                    Ok(_) => {} // Content page — images were downloaded directly.
                    Err(error) => {
                        queue_service.mark_failed(&url, &error.to_string());
                        log.push(format!("Download failed [{}]: {}", url, error));
                    }
                }
            }
        }

        Ok(())
    })
}

fn run_search_page_job(
    database_path: String,
    query: SearchQuery,
    cursor: Option<SearchCursor>,
    cancellation: &QueryCancellation,
) -> AppResult<SearchResultPage> {
    let database = Database::new(database_path);
    let service = SearchService::new(database);
    service.search_images_page_lightweight(
        &query,
        cursor.as_ref(),
        app_window::SEARCH_PAGE_SIZE,
        cancellation,
    )
}

fn run_import_job(
    queue_service: &UrlQueueService,
    database_path: &str,
    file_path: &std::path::Path,
    retain_processed_decisions: bool,
) -> AppResult<UrlQueueReport> {
    let database = Database::new(database_path);
    queue_service.import_urls_from_file_summary(&database, file_path, retain_processed_decisions)
}

fn spawn_import_worker<F>(
    queue_service: Arc<UrlQueueService>,
    database_path: String,
    file_path: String,
    retain_processed_decisions: bool,
    on_complete: F,
) -> thread::JoinHandle<()>
where
    F: FnOnce(String, Result<UrlQueueReport, String>) + Send + 'static,
{
    thread::spawn(move || {
        let result = run_import_job(
            &queue_service,
            &database_path,
            PathBuf::from(&file_path).as_path(),
            retain_processed_decisions,
        )
        .map_err(|error| error.to_string());
        on_complete(file_path, result);
    })
}

fn run_search_recent_pages(
    database_path: String,
    page_limit: usize,
    cursor: Option<SearchCursor>,
    cancellation: &QueryCancellation,
) -> AppResult<SearchResultPage> {
    let database = Database::new(database_path);
    let service = SearchService::new(database);
    service.search_recent_pages_page_lightweight(
        page_limit,
        cursor.as_ref(),
        app_window::SEARCH_PAGE_SIZE,
        cancellation,
    )
}

fn run_suggestions(database_path: String, kind: &str, query: String) -> AppResult<Vec<String>> {
    let database = Database::new(database_path);
    database.initialize()?;
    database.suggest_attribute_values(kind, &query, 8)
}

fn run_add_attributes(
    database_path: String,
    image_ids: Vec<i64>,
    values: Vec<String>,
    kind: &str,
    _label: &str,
    refresh: SearchRefresh,
) -> AppResult<SearchMutationResult> {
    let database = Database::new(database_path);
    let service = SearchService::new(database);
    match kind {
        "tag" => service.add_tags_to_images(&image_ids, &values)?,
        "category" => service.add_categories_to_images(&image_ids, &values)?,
        "actor" => service.add_actors_to_images(&image_ids, &values)?,
        "studio" => service.add_studios_to_images(&image_ids, &values)?,
        _ => {}
    }
    mutation_result(
        &service,
        &image_ids,
        &refresh,
        attribute_mutation_affects_filter(kind, &refresh),
    )
}

fn run_remove_attributes(
    database_path: String,
    image_ids: Vec<i64>,
    values: Vec<String>,
    kind: &str,
    refresh: SearchRefresh,
) -> AppResult<SearchMutationResult> {
    let database = Database::new(database_path);
    let service = SearchService::new(database);
    match kind {
        "tag" => service.remove_tags_from_images(&image_ids, &values)?,
        "category" => service.remove_categories_from_images(&image_ids, &values)?,
        "actor" => service.remove_actors_from_images(&image_ids, &values)?,
        "studio" => service.remove_studios_from_images(&image_ids, &values)?,
        _ => {}
    }
    mutation_result(
        &service,
        &image_ids,
        &refresh,
        attribute_mutation_affects_filter(kind, &refresh),
    )
}

fn run_set_favorite(
    database_path: String,
    image_id: i64,
    is_favorite: bool,
    refresh: SearchRefresh,
) -> AppResult<SearchMutationResult> {
    let database = Database::new(&database_path);
    let service = SearchService::new(database);
    service.toggle_favorite(image_id, is_favorite)?;
    let affects_filter = matches!(&refresh, SearchRefresh::Query(query) if query.favorites_only);
    mutation_result(&service, &[image_id], &refresh, affects_filter)
}

fn mutation_result(
    service: &SearchService,
    image_ids: &[i64],
    refresh: &SearchRefresh,
    replace: bool,
) -> AppResult<SearchMutationResult> {
    let (results, next_cursor) = if replace {
        match refresh {
            SearchRefresh::Query(query) => {
                let page = service.search_images_page(query, None, app_window::SEARCH_PAGE_SIZE)?;
                (page.items, page.next_cursor)
            }
            SearchRefresh::RecentPages(limit) => (service.search_recent_pages(*limit)?, None),
        }
    } else {
        (service.load_images_by_ids(image_ids)?, None)
    };
    Ok(SearchMutationResult {
        results,
        next_cursor,
        replace,
    })
}

fn attribute_mutation_affects_filter(kind: &str, refresh: &SearchRefresh) -> bool {
    let SearchRefresh::Query(query) = refresh else {
        return false;
    };
    !query.text.trim().is_empty()
        || match kind {
            "tag" => !query.tags.is_empty(),
            "actor" => !query.actors.is_empty(),
            "category" => !query.categories.is_empty(),
            "studio" => !query.studios.is_empty(),
            "channel" => !query.channels.is_empty(),
            _ => false,
        }
}

fn run_set_actor_gender(
    database_path: String,
    actor_name: String,
    is_female: Option<bool>,
    image_ids: Vec<i64>,
    _refresh: SearchRefresh,
) -> AppResult<Vec<SearchImageResult>> {
    let database = Database::new(&database_path);
    let service = SearchService::new(database);
    service.set_actor_gender_only(&actor_name, is_female)?;
    service.load_images_by_ids(&image_ids)
}

fn run_rescrape(
    database_path: String,
    download_dir: String,
    url: String,
    max_concurrent_downloads: usize,
    log: UiLog,
) -> AppResult<()> {
    let runtime = shared_async_runtime()
        .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
    runtime.block_on(async move {
        let scraper_registry = ScraperRegistry::with_all_scrapers();
        let database = Database::new(&database_path);
        let store = FileDownloadStore::new();
        let ui = ConsoleUi::from_log(log);
        let base_dir = PathBuf::from(&download_dir);
        let mut config = AppConfig::load().unwrap_or_default();
        config.database_path = database_path;
        config.max_concurrent_downloads = max_concurrent_downloads;
        let thumb_dir = config.thumb_dir();
        let service = ContentDownloadService::new(
            scraper_registry,
            database,
            store,
            ui,
            StructuredDirectoryStrategy,
            base_dir,
            thumb_dir,
        );
        service.run_for_url(&config, &url).await.map(|_| ())
    })
}

fn run_delete_images(
    database_path: String,
    base_dir: String,
    image_ids: Vec<i64>,
    mode: DeleteMode,
    _refresh: SearchRefresh,
) -> AppResult<()> {
    let database = Database::new(&database_path);
    let service = SearchService::new(database);
    service.delete_images_only(&image_ids, mode, &base_dir)?;
    Ok(())
}

fn run_set_primary_actor(
    database_path: String,
    download_folder: String,
    page_id: i64,
    actor_name: String,
    _refresh: SearchRefresh,
) -> AppResult<Vec<SearchImageResult>> {
    use crate::fs::structured::actor_folder_name;
    use std::path::{Path, PathBuf};

    let database = Database::new(&database_path);
    let new_actor_dir = actor_folder_name(&actor_name);
    let base = Path::new(&download_folder);

    let images = database.get_page_images_with_paths(page_id)?;
    let page_image_ids = images
        .iter()
        .map(|(image_id, _)| *image_id)
        .collect::<Vec<_>>();
    let mut path_updates = Vec::new();

    for (image_id, rel_path) in images {
        // Expected structure: {site}/{actor}/{content}/{filename}
        let rel = Path::new(&rel_path);
        let parts: Vec<_> = rel.components().collect();
        if parts.len() < 4 {
            continue; // unexpected layout — skip silently
        }

        let site = PathBuf::from(parts[0].as_os_str());
        // parts[1] is the old actor folder — we replace it
        let content_dir = PathBuf::from(parts[2].as_os_str());
        let filename = PathBuf::from(parts[3].as_os_str());

        let new_rel = site.join(&new_actor_dir).join(&content_dir).join(&filename);

        if new_rel == rel {
            continue; // already in the right folder
        }

        let old_abs = base.join(&rel_path);
        let new_abs = base.join(&new_rel);

        if old_abs.exists() {
            if let Some(parent) = new_abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&old_abs, &new_abs)?;
        }

        let new_rel_str = new_rel.to_string_lossy().into_owned();
        path_updates.push(ImageLocalPathUpdate {
            image_id,
            new_path: new_rel_str,
        });
    }

    database.update_image_local_paths(&path_updates)?;

    let service = SearchService::new(Database::new(&database_path));
    service.load_images_by_ids(&page_image_ids)
}

// ── SQL query runner ──────────────────────────────────────────────────────────

fn run_sql_query(database_path: String, query: String) -> AppResult<app_window::SqlQueryResult> {
    use rusqlite::types::ValueRef;

    let conn = rusqlite::Connection::open(&database_path)?;
    conn.execute_batch("PRAGMA busy_timeout = 5000;")?;

    let mut stmt = conn.prepare(&query)?;
    let total_columns = stmt.column_count();
    let displayed_columns = total_columns.min(SQL_QUERY_COLUMN_LIMIT);

    let columns: Vec<String> = (0..displayed_columns)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();
    let mut query_rows = stmt.query([])?;
    let mut rows = Vec::with_capacity(SQL_QUERY_ROW_LIMIT.min(256));
    let mut retained_bytes = columns.iter().map(String::len).sum::<usize>();
    let mut truncated = total_columns > displayed_columns;
    'rows: while let Some(row) = query_rows.next()? {
        if rows.len() == SQL_QUERY_ROW_LIMIT {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(displayed_columns);
        for index in 0..displayed_columns {
            let (cell, cell_truncated) = match row.get_ref(index) {
                Ok(ValueRef::Null) => ("NULL".to_string(), false),
                Ok(ValueRef::Integer(value)) => (value.to_string(), false),
                Ok(ValueRef::Real(value)) => (value.to_string(), false),
                Ok(ValueRef::Text(value)) => bounded_sql_text(value, SQL_QUERY_CELL_BYTE_LIMIT),
                Ok(ValueRef::Blob(value)) => (format!("<blob {} B>", value.len()), false),
                Err(_) => (String::new(), false),
            };
            if retained_bytes.saturating_add(cell.len()) > SQL_QUERY_TOTAL_BYTE_LIMIT {
                truncated = true;
                break 'rows;
            }
            retained_bytes += cell.len();
            truncated |= cell_truncated;
            cells.push(cell);
        }
        rows.push(cells);
    }

    Ok(app_window::SqlQueryResult {
        columns,
        total_columns,
        rows,
        truncated,
    })
}

fn bounded_sql_text(value: &[u8], max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (String::from_utf8_lossy(value).into_owned(), false);
    }

    let payload_limit = max_bytes.saturating_sub(SQL_CELL_TRUNCATED_SUFFIX.len());
    let mut result = String::from_utf8_lossy(&value[..payload_limit.min(value.len())]).into_owned();
    while result.len() > payload_limit {
        result.pop();
    }
    result.push_str(SQL_CELL_TRUNCATED_SUFFIX);
    (result, true)
}

fn run_sql_export(database_path: &str, query: &str, output_path: &str) -> AppResult<usize> {
    use rusqlite::types::ValueRef;

    let connection = rusqlite::Connection::open(database_path)?;
    connection.execute_batch(
        "PRAGMA busy_timeout = 5000; PRAGMA cache_size = -4096; PRAGMA temp_store = FILE;",
    )?;
    let mut statement = connection.prepare(query)?;
    let column_count = statement.column_count();
    let file = std::fs::File::create(output_path)?;
    let mut writer = BufWriter::new(file);

    for index in 0..column_count {
        if index > 0 {
            writer.write_all(b",")?;
        }
        write_csv_field(
            &mut writer,
            statement.column_name(index).unwrap_or("?").as_bytes(),
        )?;
    }
    writer.write_all(b"\n")?;

    let mut rows = statement.query([])?;
    let mut row_count = 0usize;
    while let Some(row) = rows.next()? {
        for index in 0..column_count {
            if index > 0 {
                writer.write_all(b",")?;
            }
            match row.get_ref(index)? {
                ValueRef::Null => writer.write_all(b"NULL")?,
                ValueRef::Integer(value) => write!(writer, "{value}")?,
                ValueRef::Real(value) => write!(writer, "{value}")?,
                ValueRef::Text(value) => {
                    let value = String::from_utf8_lossy(value);
                    write_csv_field(&mut writer, value.as_bytes())?;
                }
                ValueRef::Blob(value) => write!(writer, "<blob {} B>", value.len())?,
            }
        }
        writer.write_all(b"\n")?;
        row_count += 1;
    }
    writer.flush()?;
    Ok(row_count)
}

fn write_csv_field(writer: &mut impl Write, value: &[u8]) -> std::io::Result<()> {
    let needs_quotes = value
        .iter()
        .any(|byte| matches!(*byte, b',' | b'"' | b'\n' | b'\r'));
    if !needs_quotes {
        return writer.write_all(value);
    }

    writer.write_all(b"\"")?;
    let mut chunk_start = 0;
    for (index, byte) in value.iter().enumerate() {
        if *byte == b'"' {
            writer.write_all(&value[chunk_start..index])?;
            writer.write_all(b"\"\"")?;
            chunk_start = index + 1;
        }
    }
    writer.write_all(&value[chunk_start..])?;
    writer.write_all(b"\"")
}

#[cfg(test)]
mod sql_query_tests {
    use super::{
        bounded_sql_text, run_sql_export, run_sql_query, SQL_CELL_TRUNCATED_SUFFIX,
        SQL_QUERY_CELL_BYTE_LIMIT, SQL_QUERY_COLUMN_LIMIT, SQL_QUERY_ROW_LIMIT,
        SQL_QUERY_TOTAL_BYTE_LIMIT,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_root(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!("archivesift_sql_{label}_{nanos}"))
    }

    #[test]
    fn sql_console_retains_only_the_display_budget() {
        let root = unique_test_root("bounded_rows");
        std::fs::create_dir_all(&root).expect("create SQL test root");
        let database_path = root.join("test.sqlite3");
        let result = run_sql_query(
            database_path.to_string_lossy().into_owned(),
            "WITH RECURSIVE seq(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM seq WHERE x < 2105) SELECT x FROM seq"
                .to_string(),
        )
        .expect("run bounded SQL query");

        assert_eq!(result.rows.len(), SQL_QUERY_ROW_LIMIT);
        assert!(result.truncated);
        assert_eq!(result.rows[0], vec!["1"]);
        assert_eq!(
            result.rows.last().expect("last retained row"),
            &vec!["2000"]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sql_console_caps_columns_and_large_text_cells() {
        let root = unique_test_root("bounded_cells");
        std::fs::create_dir_all(&root).expect("create SQL test root");
        let database_path = root.join("test.sqlite3");
        let columns = (0..(SQL_QUERY_COLUMN_LIMIT + 3))
            .map(|index| {
                if index == 0 {
                    "hex(zeroblob(65536)) AS huge".to_string()
                } else {
                    format!("{index} AS c{index}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let result = run_sql_query(
            database_path.to_string_lossy().into_owned(),
            format!("SELECT {columns}"),
        )
        .expect("run bounded SQL query");

        assert_eq!(result.total_columns, SQL_QUERY_COLUMN_LIMIT + 3);
        assert_eq!(result.columns.len(), SQL_QUERY_COLUMN_LIMIT);
        assert!(result.rows[0][0].len() <= SQL_QUERY_CELL_BYTE_LIMIT);
        assert!(result.rows[0][0].ends_with(SQL_CELL_TRUNCATED_SUFFIX));
        assert!(result.truncated);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sql_console_stops_at_the_aggregate_byte_budget() {
        let root = unique_test_root("aggregate_budget");
        std::fs::create_dir_all(&root).expect("create SQL test root");
        let database_path = root.join("test.sqlite3");
        let result = run_sql_query(
            database_path.to_string_lossy().into_owned(),
            "WITH RECURSIVE seq(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM seq WHERE x < 2000) SELECT printf('%010000d', x) AS payload FROM seq".to_string(),
        )
        .expect("run aggregate-bounded SQL query");

        let retained = result.rows.iter().flatten().map(String::len).sum::<usize>();
        assert!(retained <= SQL_QUERY_TOTAL_BYTE_LIMIT);
        assert!(result.rows.len() < SQL_QUERY_ROW_LIMIT);
        assert!(result.truncated);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sql_text_truncation_keeps_utf8_valid() {
        let (value, truncated) = bounded_sql_text("Р–".repeat(100).as_bytes(), 63);
        assert!(truncated);
        assert!(value.len() <= 63);
        assert!(value.ends_with(SQL_CELL_TRUNCATED_SUFFIX));
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
    }

    #[test]
    fn sql_export_reexecutes_and_streams_the_complete_result_as_csv() {
        let root = unique_test_root("stream_export");
        std::fs::create_dir_all(&root).expect("create SQL export root");
        let database_path = root.join("test.sqlite3");
        let output_path = root.join("result.csv");
        let exported = run_sql_export(
            &database_path.to_string_lossy(),
            "WITH RECURSIVE seq(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM seq WHERE x < 2105) SELECT x, CASE WHEN x = 1 THEN 'a,\"b\"' ELSE 'plain' END AS label FROM seq",
            &output_path.to_string_lossy(),
        )
        .expect("stream SQL export");

        assert_eq!(exported, 2105);
        let csv = std::fs::read_to_string(&output_path).expect("read exported CSV");
        assert_eq!(csv.lines().count(), 2106);
        assert!(csv.starts_with("x,label\n1,\"a,\"\"b\"\"\"\n"));
        assert!(csv.ends_with("2105,plain\n"));

        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod startup_ui_tests {
    use super::*;

    fn test_app(configured: bool) -> (DesktopApp, tempfile::TempDir, egui::Context) {
        let root = tempfile::tempdir().unwrap();
        let config = AppConfig {
            clipboard_listener_enabled: false,
            database_path: root.path().join("archive.sqlite3").display().to_string(),
            download_folder: if configured {
                root.path().join("downloads").display().to_string()
            } else {
                String::new()
            },
            preset_tags: "portrait, outdoor".to_string(),
            ..AppConfig::default()
        };
        let context = egui::Context::default();
        (DesktopApp::new(config, context.clone()), root, context)
    }

    fn render(
        app: &mut DesktopApp,
        context: &egui::Context,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| app.update_ui(ctx),
        )
    }

    fn rendered_text(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        fn visit(shape: &egui::Shape, text: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(shape) => text.push((
                    shape.galley.job.text.clone(),
                    egui::Rect::from_min_size(shape.pos, shape.galley.size()),
                )),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        visit(shape, text);
                    }
                }
                _ => {}
            }
        }
        let mut text = Vec::new();
        for shape in &output.shapes {
            visit(&shape.shape, &mut text);
        }
        text
    }

    fn assert_labels(output: &egui::FullOutput, expected: &[&str]) {
        let text = rendered_text(output);
        for expected in expected {
            assert!(
                text.iter().any(|(label, _)| label == expected),
                "Missing {expected:?} in {text:?}"
            );
        }
    }

    fn click_label(
        app: &mut DesktopApp,
        context: &egui::Context,
        output: &egui::FullOutput,
        label: &str,
    ) -> egui::FullOutput {
        let pos = rendered_text(output)
            .into_iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("Missing click target {label:?}"))
            .1
            .center();
        for pressed in [true, false] {
            let _ = render(
                app,
                context,
                egui::vec2(1900.0, 1400.0),
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        render(app, context, egui::vec2(1900.0, 1400.0), Vec::new())
    }

    #[test]
    fn fresh_start_exposes_workspace_settings_queue_and_activity_log() {
        let (mut app, _root, context) = test_app(false);
        app.window.log.push("Download progress is visible here");
        for size in [egui::vec2(800.0, 600.0), egui::vec2(1900.0, 1400.0)] {
            let output = render(&mut app, &context, size, Vec::new());
            assert_labels(
                &output,
                &["Scrape", "Search", "SQL", "Extensions", "Source"],
            );
            if size.x > 1440.0 {
                assert_labels(
                    &output,
                    &[
                        "Destination",
                        "Download folder",
                        "Queue",
                        "Activity",
                        "Download progress is visible here",
                        "Select a download directory to enable scraping.",
                    ],
                );
                let output = click_label(&mut app, &context, &output, "Add URL to Queue");
                assert_labels(&output, &["Source", "Activity"]);
                assert!(!app.worker_running);
                assert!(!app.queue_service.has_pending_work());
                assert!(app.window.error.is_none());
            }
        }
    }

    #[test]
    fn tabs_switch_to_search_filters_sql_and_extensions_without_destination_setup() {
        let (mut app, _root, context) = test_app(false);
        let output = render(&mut app, &context, egui::vec2(1900.0, 1400.0), Vec::new());
        let output = click_label(&mut app, &context, &output, "Search");
        assert_labels(
            &output,
            &["Text", "Site", "Tags", "Actors", "Categories", "Studios"],
        );
        let output = click_label(&mut app, &context, &output, "SQL");
        assert_labels(&output, &["SQL Console", "Ctrl+Enter"]);
        let output = click_label(&mut app, &context, &output, "Extensions");
        assert_labels(&output, &["Extensions", "Scrape", "Search", "SQL"]);
        let output = click_label(&mut app, &context, &output, "Settings");
        assert_labels(
            &output,
            &[
                "Storage",
                "Download folder",
                "Database file",
                "Thumbnail folder",
                "Save and apply",
                "Settings file",
            ],
        );
        let output = click_label(&mut app, &context, &output, "Scrape");
        assert_labels(&output, &["Source", "Destination", "Queue", "Activity"]);
    }

    #[test]
    fn configured_start_preserves_destination_and_presets() {
        let (mut app, root, context) = test_app(true);
        let output = render(&mut app, &context, egui::vec2(1900.0, 1400.0), Vec::new());
        assert_labels(
            &output,
            &["Scrape", "Search", "SQL", "Extensions", "Activity"],
        );
        assert_eq!(
            app.window.config.download_folder,
            root.path().join("downloads").display().to_string()
        );
        assert_eq!(app.live_presets.read().unwrap().tags, "portrait, outdoor");
        assert!(app.window.error.is_none());
        assert!(!app.worker_running);
    }

    #[test]
    fn missing_destination_blocks_manual_import_clipboard_rescrape_and_restored_queue() {
        let (mut app, _root, _context) = test_app(false);
        let config = app.window.config.clone();
        app.enqueue_manual_url(config.clone());
        assert!(app.window.error.is_some());
        app.start_import_url_file("missing.txt".to_string());
        assert!(!app.window.import_running);
        app.start_rescrape(
            config.database_path.clone(),
            String::new(),
            String::new(),
            1,
        );
        assert!(!app.window.running);

        app.window.config.clipboard_listener_enabled = true;
        let (sender, receiver) = mpsc::channel();
        app.clipboard_rx = Some(receiver);
        let url = "https://demo.example.test/galleries/test/";
        sender.send(ClipboardEvent::Text(url.to_string())).unwrap();
        app.handle_clipboard_events();
        assert!(!app.queue_service.has_pending_work());

        let report = app
            .queue_service
            .enqueue_urls(&Database::new(&config.database_path), vec![url.to_string()])
            .unwrap();
        assert_eq!(report.queued_successfully, 1);
        app.start_processor_if_needed(config);
        assert!(app.queue_service.has_pending_work());
        assert!(!app.worker_running);
    }

    #[test]
    fn storage_settings_switch_library_in_background_and_discard_old_job_results() {
        let (mut app, root, context) = test_app(true);
        let old_sender = app.state_tx.clone();
        let config_path = root.path().join("app-config.json");
        let settings = StorageSettings {
            download_folder: root.path().join("new-files").display().to_string(),
            database_path: root
                .path()
                .join("new-library.sqlite3")
                .display()
                .to_string(),
            thumbnail_folder: Some(root.path().join("new-previews").display().to_string()),
        };
        let original_path = app.window.config.database_path.clone();
        app.window.log.push("Previous activity");
        app.start_storage_settings(settings.clone(), config_path.clone());
        assert!(app.window.storage_settings_applying());
        assert_eq!(app.window.config.database_path, original_path);
        let result = app
            .state_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let WorkerState::StorageSettingsApplied(result) = result else {
            panic!("Unexpected worker result");
        };
        app.finish_storage_settings(result);
        assert!(!app.window.storage_settings_applying());
        assert_eq!(StorageSettings::from(&app.window.config), settings);
        assert_eq!(
            StorageSettings::from(&AppConfig::load_from(&config_path).unwrap()),
            settings
        );
        assert!(old_sender
            .send(WorkerState::TagSuggestions {
                query: "old".into(),
                results: vec!["stale".into()]
            })
            .is_err());
        assert!(app
            .window
            .log
            .snapshot()
            .iter()
            .any(|line| line == "Previous activity"));
        let output = render(&mut app, &context, egui::vec2(1900.0, 1400.0), Vec::new());
        assert_labels(
            &output,
            &["Settings", "Storage settings saved and applied."],
        );
    }

    #[test]
    fn storage_settings_do_not_switch_a_busy_or_failed_library() {
        let (mut app, root, _context) = test_app(true);
        let original = StorageSettings::from(&app.window.config);
        let config_path = root.path().join("config.json");
        app.worker_running = true;
        app.start_storage_settings(original.clone(), config_path.clone());
        assert!(!app.window.storage_settings_applying());
        assert!(!config_path.exists());
        app.worker_running = false;
        let mut invalid = original.clone();
        invalid.database_path.clear();
        app.start_storage_settings(invalid, config_path);
        let WorkerState::StorageSettingsApplied(result) = app
            .state_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
        else {
            panic!("Unexpected result");
        };
        assert!(result.is_err());
        app.finish_storage_settings(result);
        assert_eq!(StorageSettings::from(&app.window.config), original);
        assert!(!app.window.storage_settings_applying());
    }
}

#[cfg(test)]
mod repaint_tests {
    use super::{
        attribute_mutation_affects_filter, background_poll_delay, repainting_channel,
        spawn_import_worker, try_start_import,
    };
    use crate::models::SearchQuery;
    use crate::services::UrlQueueService;
    use crate::ui::SearchRefresh;
    use eframe::egui;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn repainting_worker_channel_applies_backpressure_at_capacity() {
        let context = egui::Context::default();
        let (sender, receiver) = repainting_channel(&context, 1);
        sender.send(1usize).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            sender.send(2usize).unwrap();
            done_tx.send(()).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(25)).is_err());
        assert_eq!(receiver.recv().unwrap(), 1);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(receiver.recv().unwrap(), 2);
        worker.join().unwrap();
    }

    #[test]
    fn idle_and_paused_states_do_not_schedule_periodic_repaints() {
        assert_eq!(background_poll_delay(false, false), None);
        assert_eq!(background_poll_delay(true, true), None);
    }

    #[test]
    fn active_queue_worker_keeps_progress_updates_visible() {
        assert_eq!(
            background_poll_delay(true, false),
            Some(std::time::Duration::from_millis(100))
        );
    }

    #[test]
    fn import_guard_allows_only_one_in_flight_job() {
        let mut import_running = false;

        assert!(try_start_import(&mut import_running));
        assert!(import_running);
        assert!(!try_start_import(&mut import_running));

        import_running = false;
        assert!(try_start_import(&mut import_running));
    }

    #[test]
    fn metadata_mutations_refresh_only_when_the_active_filter_can_change() {
        let unfiltered = SearchRefresh::Query(SearchQuery::default());
        assert!(!attribute_mutation_affects_filter("tag", &unfiltered));

        let tag_filtered = SearchRefresh::Query(SearchQuery {
            tags: vec!["portrait".to_string()],
            ..SearchQuery::default()
        });
        assert!(attribute_mutation_affects_filter("tag", &tag_filtered));
        assert!(!attribute_mutation_affects_filter("studio", &tag_filtered));

        let text_filtered = SearchRefresh::Query(SearchQuery {
            text: "alice".to_string(),
            ..SearchQuery::default()
        });
        assert!(attribute_mutation_affects_filter("actor", &text_filtered));
    }

    #[test]
    fn import_worker_delivers_file_errors_asynchronously() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing_path = std::env::temp_dir()
            .join(format!("archivesift_missing_import_{unique}.txt"))
            .to_string_lossy()
            .into_owned();
        let database_path = std::env::temp_dir()
            .join(format!("archivesift_unused_import_{unique}.db"))
            .to_string_lossy()
            .into_owned();
        let (sender, receiver) = mpsc::channel();

        let worker = spawn_import_worker(
            Arc::new(UrlQueueService::new()),
            database_path,
            missing_path.clone(),
            false,
            move |file_path, result| sender.send((file_path, result)).unwrap(),
        );

        let (reported_path, result) = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("background worker should deliver its result");
        assert_eq!(reported_path, missing_path);
        assert!(result.is_err());
        worker.join().unwrap();
    }
}

// ── Throttle helpers ──────────────────────────────────────────────────────────

/// Returns a uniformly-sampled delay in milliseconds within `[min_ms, max_ms]`.
/// Returns 0 when both bounds are 0 (throttling disabled).
///
/// Uses a lightweight xorshift64 seeded from wall-clock nanoseconds so each
/// call produces a different value without requiring the `rand` crate.
fn sample_delay(min_ms: u64, max_ms: u64) -> u64 {
    if min_ms == 0 && max_ms == 0 {
        return 0;
    }
    let hi = max_ms.max(min_ms);
    if min_ms == hi {
        return min_ms;
    }
    let range = hi - min_ms + 1;

    // Cheap entropy: nanosecond bits from the system clock.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(min_ms.wrapping_add(1));

    // One round of xorshift64 to spread the bits.
    let mut x = seed | 1; // ensure non-zero
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;

    min_ms + (x % range)
}

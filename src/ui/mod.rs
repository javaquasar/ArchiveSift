pub mod egui_ui;
pub mod home;

pub use egui_ui::AppWindow;
pub use home::{render_home_screen, HomeScreenState, HomeTransition};

use std::sync::{Arc, Mutex};

use crate::app::AppConfig;
use crate::models::{
    DateBrowseDirection, DateGroupCursor, DeleteMode, MetadataAttribute, SearchCursor, SearchQuery,
};
use crate::AppResult;

// ── UiBackend trait ───────────────────────────────────────────────────────────

/// Abstraction over concrete UI implementations (egui, tauri, …).
///
/// Implement this trait to add a new UI frontend. The `App` in
/// `app::bootstrap` selects which backend to use at startup.
pub trait UiBackend {
    fn run(&self, config: AppConfig) -> AppResult<()>;
}

// ── Shared output/log types ───────────────────────────────────────────────────

/// Maximum number of lines kept in the activity log.
/// Oldest entries are dropped when the buffer is full, keeping memory and
/// per-frame clone cost bounded regardless of how many images are downloaded.
const MAX_LOG_LINES: usize = 500;
const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
const MAX_LOG_TOTAL_BYTES: usize = 256 * 1024;
const LOG_TRUNCATED_SUFFIX: &str = "... [truncated]";

struct UiLogState {
    lines: std::collections::VecDeque<Arc<str>>,
    total_bytes: usize,
    cached_snapshot: Arc<Vec<Arc<str>>>,
    snapshot_dirty: bool,
}

impl Default for UiLogState {
    fn default() -> Self {
        Self {
            lines: std::collections::VecDeque::new(),
            total_bytes: 0,
            cached_snapshot: Arc::new(Vec::new()),
            snapshot_dirty: false,
        }
    }
}

/// A thread-safe, append-only log buffer shared between worker threads and the UI.
///
/// Internally backed by a `VecDeque` so that capping at [`MAX_LOG_LINES`] is
/// O(1): newest entries are pushed to the back, oldest popped from the front.
#[derive(Clone, Default)]
pub struct UiLog {
    state: Arc<Mutex<UiLogState>>,
}

impl UiLog {
    pub fn push(&self, line: impl Into<String>) {
        let line = truncate_log_line(line.into());
        let line_bytes = line.len();
        let line: Arc<str> = line.into();
        let mut state = self.state.lock().unwrap();

        while !state.lines.is_empty()
            && (state.lines.len() >= MAX_LOG_LINES
                || state.total_bytes + line_bytes > MAX_LOG_TOTAL_BYTES)
        {
            if let Some(removed) = state.lines.pop_front() {
                state.total_bytes = state.total_bytes.saturating_sub(removed.len());
            }
        }

        state.total_bytes += line_bytes;
        state.lines.push_back(line);
        state.snapshot_dirty = true;
    }

    /// Returns a point-in-time snapshot as a plain `Vec` (oldest → newest).
    /// Both entry count and total bytes are bounded.
    pub fn snapshot(&self) -> Vec<String> {
        self.snapshot_shared()
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// Returns a cached immutable snapshot without copying log text. A new
    /// vector of `Arc` handles is built only after the log changes.
    pub fn snapshot_shared(&self) -> Arc<Vec<Arc<str>>> {
        let mut state = self.state.lock().unwrap();
        if state.snapshot_dirty {
            state.cached_snapshot = Arc::new(state.lines.iter().cloned().collect());
            state.snapshot_dirty = false;
        }
        Arc::clone(&state.cached_snapshot)
    }

    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.lines.clear();
        state.total_bytes = 0;
        state.snapshot_dirty = true;
    }
}

fn truncate_log_line(mut line: String) -> String {
    if line.len() <= MAX_LOG_LINE_BYTES {
        return line;
    }

    let content_limit = MAX_LOG_LINE_BYTES.saturating_sub(LOG_TRUNCATED_SUFFIX.len());
    let mut boundary = content_limit.min(line.len());
    while boundary > 0 && !line.is_char_boundary(boundary) {
        boundary -= 1;
    }
    line.truncate(boundary);
    line.push_str(LOG_TRUNCATED_SUFFIX);
    line
}

/// Adapter that routes service-layer print calls into a [`UiLog`].
#[derive(Clone, Default)]
pub struct ConsoleUi {
    log: UiLog,
}

impl ConsoleUi {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_log(log: UiLog) -> Self {
        Self { log }
    }

    pub fn print_html(&self, html: &str) {
        self.log.push(format!(
            "Captured page HTML ({} bytes; omitted from activity log).",
            html.len()
        ));
    }

    pub fn print_database_path(&self, path: &str) {
        self.log.push(format!("Database: {}", path));
    }

    pub fn print_page_title(&self, title: &str) {
        self.log.push(format!("Title: {}", title));
    }

    pub fn print_status(&self, message: &str) {
        self.log.push(message.to_string());
    }

    pub fn print_attributes(&self, attributes: &[MetadataAttribute]) {
        for attribute in attributes {
            self.log.push(format!(
                "{} [{}] = {}",
                attribute.label, attribute.kind, attribute.value
            ));
        }
    }

    pub fn print_discovered_link(&self, link: &str) {
        self.log.push(link.to_string());
    }

    pub fn print_found_links(&self, count: usize) {
        self.log.push(format!("Found {} links.", count));
    }

    pub fn print_download_target(&self, url: &str) {
        self.log.push(format!("Downloading {}", url));
    }

    pub fn print_output_file(&self, file_name: &str) {
        self.log.push(format!("Saving to {}", file_name));
    }

    pub fn print_skipped_existing(&self, file_name: &str) {
        self.log.push(format!(
            "File {} already exists. Skipping download.",
            file_name
        ));
    }

    pub fn print_downloaded(&self, url: &str) {
        self.log.push(format!("Downloaded: {}", url));
    }

    pub fn print_download_failed(&self, error: &dyn std::error::Error) {
        self.log.push(format!("Failed to download: {:?}", error));
    }

    pub fn print_warning(&self, message: &str) {
        self.log.push(message.to_string());
    }
}

// ── SearchRefresh ─────────────────────────────────────────────────────────────

/// Describes how to re-run the search after a mutating operation.
/// Keeps background jobs decoupled from the current UI mode.
#[derive(Clone, Debug)]
pub enum SearchRefresh {
    /// Re-run with a standard filter query.
    Query(SearchQuery),
    /// Re-run showing images from the N most-recently-scraped pages.
    RecentPages(usize),
}

// ── UiCommand ────────────────────────────────────────────────────────────────

/// Commands emitted by the UI and consumed by the app orchestration layer.
#[derive(Clone, Debug)]
pub enum UiCommand {
    QueueUrl(AppConfig),
    ImportFile {
        file_path: String,
    },
    SetDownloadFolder(String),
    SetClipboardListener(bool),
    Search {
        database_path: String,
        query: SearchQuery,
        seq: u64,
        cursor: Option<SearchCursor>,
        append: bool,
    },
    HydrateSearchMetadata {
        database_path: String,
        image_ids: Vec<i64>,
    },
    SearchRecentPages {
        database_path: String,
        /// How many most-recently-scraped pages to include.
        page_limit: usize,
        seq: u64,
        cursor: Option<SearchCursor>,
        append: bool,
    },
    SuggestTags {
        database_path: String,
        query: String,
    },
    SuggestActors {
        database_path: String,
        query: String,
    },
    SuggestCategories {
        database_path: String,
        query: String,
    },
    SuggestStudios {
        database_path: String,
        query: String,
    },
    AddTagsToImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    RemoveTagsFromImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    AddCategoriesToImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    RemoveCategoriesFromImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    AddActorsToImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    RemoveActorsFromImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    AddStudiosToImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    RemoveStudiosFromImages {
        database_path: String,
        image_ids: Vec<i64>,
        values: Vec<String>,
        refresh: SearchRefresh,
    },
    SetFavorite {
        database_path: String,
        image_id: i64,
        is_favorite: bool,
        refresh: SearchRefresh,
    },
    SetActorGender {
        database_path: String,
        actor_name: String,
        is_female: Option<bool>,
        image_ids: Vec<i64>,
        refresh: SearchRefresh,
    },
    DeleteImages {
        database_path: String,
        base_dir: String,
        image_ids: Vec<i64>,
        mode: DeleteMode,
        refresh: SearchRefresh,
    },
    /// Move all images of a page into the folder of the chosen primary actor.
    SetPrimaryActor {
        database_path: String,
        download_folder: String,
        page_id: i64,
        actor_name: String,
        refresh: SearchRefresh,
    },
    /// Re-process a URL that was previously scraped (bypasses already-processed check).
    RescrapeUrl {
        database_path: String,
        download_dir: String,
        url: String,
        max_concurrent_downloads: usize,
    },
    PauseQueue,
    ResumeQueue,
    RetryFailedQueue,
    ClearPendingQueue,
    /// Execute an arbitrary SQL query and return column names + rows.
    RunSqlQuery {
        database_path: String,
        query: String,
    },
    /// Re-execute a query and stream every row directly to a CSV file.
    ExportSqlQuery {
        database_path: String,
        query: String,
        output_path: String,
    },
    /// Load all source pages with aggregate stats for date-group browsing.
    BrowseDateGroups {
        database_path: String,
        cursor: Option<DateGroupCursor>,
        direction: DateBrowseDirection,
        ascending: bool,
        max_favorites_exclusive: Option<usize>,
    },
}

// ── Shared utilities ──────────────────────────────────────────────────────────

/// Truncates `value` to at most `max_len` characters, appending `…` if cut.
pub(crate) fn truncate(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        value.to_string()
    } else {
        let mut truncated = value
            .chars()
            .take(max_len.saturating_sub(3))
            .collect::<String>();
        truncated.push_str("...");
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct StubError(&'static str);

    impl std::fmt::Display for StubError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for StubError {}

    #[test]
    fn ui_log_snapshot_preserves_append_order() {
        let log = UiLog::default();
        log.push("first");
        log.push("second");
        log.push("third");

        assert_eq!(log.snapshot(), vec!["first", "second", "third"]);
    }

    #[test]
    fn ui_log_reuses_unchanged_snapshots_and_shared_lines() {
        let log = UiLog::default();
        log.push("first");

        let first = log.snapshot_shared();
        let unchanged = log.snapshot_shared();
        assert!(Arc::ptr_eq(&first, &unchanged));

        log.push("second");
        let updated = log.snapshot_shared();
        assert!(!Arc::ptr_eq(&first, &updated));
        assert!(Arc::ptr_eq(&first[0], &updated[0]));

        log.clear();
        assert!(log.snapshot_shared().is_empty());
    }

    #[test]
    fn ui_log_discards_oldest_entries_after_capacity_limit() {
        let log = UiLog::default();
        for i in 0..(MAX_LOG_LINES + 5) {
            log.push(format!("line-{i}"));
        }

        let snapshot = log.snapshot();
        let expected_last = format!("line-{}", MAX_LOG_LINES + 4);
        assert_eq!(snapshot.len(), MAX_LOG_LINES);
        assert_eq!(snapshot.first().map(String::as_str), Some("line-5"));
        assert_eq!(
            snapshot.last().map(String::as_str),
            Some(expected_last.as_str())
        );
    }

    #[test]
    fn ui_log_truncates_large_utf8_entries_at_a_valid_boundary() {
        let log = UiLog::default();
        log.push("Ж".repeat(MAX_LOG_LINE_BYTES));

        let snapshot = log.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].len() <= MAX_LOG_LINE_BYTES);
        assert!(snapshot[0].ends_with(LOG_TRUNCATED_SUFFIX));
        assert!(std::str::from_utf8(snapshot[0].as_bytes()).is_ok());
    }

    #[test]
    fn ui_log_evicts_entries_to_keep_total_bytes_bounded() {
        let log = UiLog::default();
        for index in 0..100 {
            log.push(format!("{index:03}:{}", "x".repeat(MAX_LOG_LINE_BYTES)));
        }

        let snapshot = log.snapshot();
        let total_bytes = snapshot.iter().map(String::len).sum::<usize>();
        assert!(total_bytes <= MAX_LOG_TOTAL_BYTES);
        assert!(snapshot.len() < 100);
        assert!(snapshot.last().expect("latest entry").starts_with("099:"));
    }

    #[test]
    fn ui_log_clear_resets_entries_and_byte_accounting() {
        let log = UiLog::default();
        log.push("x".repeat(MAX_LOG_LINE_BYTES));
        log.clear();
        log.push("after clear");

        assert_eq!(log.snapshot(), vec!["after clear"]);
        assert_eq!(log.state.lock().unwrap().total_bytes, "after clear".len());
    }

    #[test]
    fn console_ui_omits_raw_html_from_activity_log() {
        let log = UiLog::default();
        let ui = ConsoleUi::from_log(log.clone());
        let html = format!("<html>{}</html>", "large-body".repeat(1_000));

        ui.print_html(&html);

        let snapshot = log.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot[0],
            format!(
                "Captured page HTML ({} bytes; omitted from activity log).",
                html.len()
            )
        );
        assert!(!snapshot[0].contains("large-body"));
    }

    #[test]
    fn console_ui_print_warning_routes_message_into_shared_log() {
        let log = UiLog::default();
        let ui = ConsoleUi::from_log(log.clone());

        ui.print_warning("[warn] thumbnail generation failed");

        assert_eq!(log.snapshot(), vec!["[warn] thumbnail generation failed"]);
    }

    #[test]
    fn console_ui_formats_standard_messages_for_activity_log() {
        let log = UiLog::default();
        let ui = ConsoleUi::from_log(log.clone());

        ui.print_database_path("data/archivesift.sqlite3");
        ui.print_page_title("Summer Escape");
        ui.print_status("Queue resumed");
        ui.print_found_links(3);
        ui.print_download_target("https://cdn.example.test/a.jpg");
        ui.print_output_file("downloads/a.jpg");
        ui.print_downloaded("https://cdn.example.test/a.jpg");
        ui.print_skipped_existing("downloads/existing.jpg");

        assert_eq!(
            log.snapshot(),
            vec![
                "Database: data/archivesift.sqlite3",
                "Title: Summer Escape",
                "Queue resumed",
                "Found 3 links.",
                "Downloading https://cdn.example.test/a.jpg",
                "Saving to downloads/a.jpg",
                "Downloaded: https://cdn.example.test/a.jpg",
                "File downloads/existing.jpg already exists. Skipping download.",
            ]
        );
    }

    #[test]
    fn console_ui_print_attributes_emits_one_line_per_attribute() {
        let log = UiLog::default();
        let ui = ConsoleUi::from_log(log.clone());

        ui.print_attributes(&[
            MetadataAttribute {
                kind: "actor".to_string(),
                label: "Models".to_string(),
                value: "Alice Blue".to_string(),
                source_url: None,
                is_female: Some(true),
            },
            MetadataAttribute {
                kind: "tag".to_string(),
                label: "tag".to_string(),
                value: "outdoor".to_string(),
                source_url: None,
                is_female: None,
            },
        ]);

        assert_eq!(
            log.snapshot(),
            vec!["Models [actor] = Alice Blue", "tag [tag] = outdoor"]
        );
    }

    #[test]
    fn console_ui_print_download_failed_includes_debug_error() {
        let log = UiLog::default();
        let ui = ConsoleUi::from_log(log.clone());
        let error = StubError("network timeout");

        ui.print_download_failed(&error);

        assert_eq!(
            log.snapshot(),
            vec!["Failed to download: StubError(\"network timeout\")"]
        );
    }

    #[test]
    fn truncate_returns_original_when_value_is_short_enough() {
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn truncate_appends_ascii_ellipsis_when_value_is_too_long() {
        assert_eq!(truncate("abcdefghij", 7), "abcd...");
    }
}

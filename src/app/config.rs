use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ConfigError;
use crate::models::InputMode;
use crate::persistence::atomic_write;
use crate::AppResult;

const APP_CONFIG_PATH: &str = "data/app-config.json";
const CURRENT_CONFIG_VERSION: u32 = 2;
const MAX_DOWNLOAD_HISTORY: usize = 10;
const MAX_PARALLEL_PAGE_JOBS: usize = 16;
const MAX_IMAGES_PER_ITEM: usize = 32;
const MAX_TOTAL_CONCURRENT_IMAGE_DOWNLOADS: usize = 32;

fn default_max_parallel() -> usize {
    3
}

fn default_delay_min_ms() -> u64 {
    1_000
}

fn default_delay_max_ms() -> u64 {
    3_000
}

fn default_preview_open_mode() -> PreviewOpenMode {
    PreviewOpenMode::DoubleClick
}

fn default_true() -> bool {
    true
}

fn default_search_columns() -> usize {
    4
}

fn default_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
}

fn default_scrape_compact_breakpoint() -> f32 {
    980.0
}

fn default_scrape_two_column_breakpoint() -> f32 {
    1440.0
}

fn default_scrape_wide_breakpoint() -> f32 {
    1760.0
}

fn default_scrape_medium_primary_ratio() -> f32 {
    0.57
}

fn default_scrape_wide_primary_ratio() -> f32 {
    0.50
}

fn default_scrape_scroll_inset() -> f32 {
    12.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewOpenMode {
    SingleClick,
    DoubleClick,
}

impl PreviewOpenMode {
    pub fn opens_on_single_click(self) -> bool {
        matches!(self, Self::SingleClick)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrapeLayoutConfig {
    #[serde(default = "default_scrape_compact_breakpoint")]
    pub compact_breakpoint: f32,
    #[serde(default = "default_scrape_two_column_breakpoint")]
    pub two_column_breakpoint: f32,
    #[serde(default = "default_scrape_wide_breakpoint")]
    pub wide_breakpoint: f32,
    #[serde(default = "default_scrape_medium_primary_ratio")]
    pub medium_primary_ratio: f32,
    #[serde(default = "default_scrape_wide_primary_ratio")]
    pub wide_primary_ratio: f32,
    #[serde(default = "default_scrape_scroll_inset")]
    pub scroll_inset: f32,
}

impl Default for ScrapeLayoutConfig {
    fn default() -> Self {
        Self {
            compact_breakpoint: default_scrape_compact_breakpoint(),
            two_column_breakpoint: default_scrape_two_column_breakpoint(),
            wide_breakpoint: default_scrape_wide_breakpoint(),
            medium_primary_ratio: default_scrape_medium_primary_ratio(),
            wide_primary_ratio: default_scrape_wide_primary_ratio(),
            scroll_inset: default_scrape_scroll_inset(),
        }
    }
}

impl ScrapeLayoutConfig {
    pub fn normalized(&self) -> Self {
        let compact_breakpoint = self.compact_breakpoint.clamp(640.0, 1400.0);
        let two_column_breakpoint = self
            .two_column_breakpoint
            .clamp(compact_breakpoint + 120.0, 2400.0);
        let wide_breakpoint = self
            .wide_breakpoint
            .clamp(two_column_breakpoint + 120.0, 3200.0);

        Self {
            compact_breakpoint,
            two_column_breakpoint,
            wide_breakpoint,
            medium_primary_ratio: self.medium_primary_ratio.clamp(0.52, 0.72),
            wide_primary_ratio: self.wide_primary_ratio.clamp(0.45, 0.60),
            scroll_inset: self.scroll_inset.clamp(0.0, 32.0),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    pub source_url: String,
    pub download_folder: String,
    #[serde(default)]
    pub subfolder: String,
    pub download_folder_history: Vec<String>,
    pub clipboard_listener_enabled: bool,
    pub database_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_folder: Option<String>,
    pub add_plus: bool,
    pub max_concurrent_downloads: usize,
    #[serde(default = "default_max_parallel")]
    pub max_parallel_downloads: usize,
    #[serde(default)]
    pub input_mode: InputMode,
    #[serde(default = "default_preview_open_mode")]
    pub preview_open_mode: PreviewOpenMode,

    /// Comma-separated tags applied to every image downloaded in this session.
    /// Example: `"outdoor, amateur"`
    #[serde(default)]
    pub preset_tags: String,

    /// Comma-separated categories applied to every image downloaded in this session.
    /// Example: `"portraits, landscapes"`
    #[serde(default)]
    pub preset_categories: String,

    /// When `true` (default), URLs that already exist in the database are silently
    /// skipped during queuing. Set to `false` to re-scrape / re-queue them anyway.
    #[serde(default = "default_true")]
    pub skip_downloaded: bool,

    /// Enables slower parser branches that expand actor/listing pages into
    /// additional content URLs. Disabled by default so direct content links
    /// download immediately without extra crawling.
    #[serde(default)]
    pub parse_additional_links: bool,

    /// Number of image columns in the search results grid. Default: 4.
    #[serde(default = "default_search_columns")]
    pub search_columns: usize,

    /// Minimum random pause (milliseconds) inserted before each page request.
    /// Set both to 0 to disable throttling entirely.
    #[serde(default = "default_delay_min_ms")]
    pub request_delay_min_ms: u64,

    /// Maximum random pause (milliseconds) inserted before each page request.
    /// The actual delay is uniformly sampled from [min, max].
    #[serde(default = "default_delay_max_ms")]
    pub request_delay_max_ms: u64,

    /// Responsive layout tuning for the Scrape tab.
    #[serde(default)]
    pub scrape_layout: ScrapeLayoutConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CURRENT_CONFIG_VERSION,
            source_url: String::new(),
            download_folder: String::new(),
            subfolder: String::new(),
            download_folder_history: Vec::new(),
            clipboard_listener_enabled: true,
            database_path: "data/archivesift.sqlite3".to_string(),
            thumbnail_folder: None,
            add_plus: false,
            max_concurrent_downloads: 5,
            max_parallel_downloads: 3,
            input_mode: InputMode::default(),
            preview_open_mode: PreviewOpenMode::DoubleClick,
            preset_tags: String::new(),
            preset_categories: String::new(),
            skip_downloaded: true,
            parse_additional_links: false,
            search_columns: 4,
            request_delay_min_ms: 1_000,
            request_delay_max_ms: 3_000,
            scrape_layout: ScrapeLayoutConfig::default(),
        }
    }
}

/// Parses a comma-separated string into a trimmed, non-empty list of values.
/// `"portrait, outdoor, "` → `["portrait", "outdoor"]`
pub fn parse_comma_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .collect()
}

impl AppConfig {
    pub fn load() -> AppResult<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> AppResult<Self> {
        let path = path.to_path_buf();
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path).map_err(|source| ConfigError::ReadFile {
            path: path.clone(),
            source,
        })?;
        let raw_value: Value =
            serde_json::from_str(&content).map_err(|source| ConfigError::ParseJson {
                path: path.clone(),
                source,
            })?;
        let had_version = raw_value.get("config_version").is_some();

        let mut config: Self =
            serde_json::from_value(raw_value).map_err(|source| ConfigError::DeserializeConfig {
                path: path.clone(),
                source,
            })?;
        if !had_version || config.config_version < CURRENT_CONFIG_VERSION {
            config.migrate_to_current_version();
        }
        config.normalize_download_history();
        config.normalize_performance_limits();
        config.scrape_layout = config.scrape_layout.normalized();
        Ok(config)
    }

    pub fn save(&self) -> AppResult<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> AppResult<()> {
        let path = path.to_path_buf();
        let mut config = self.clone();
        config.normalize_download_history();
        config.normalize_performance_limits();
        config.scrape_layout = config.scrape_layout.normalized();

        let json = serde_json::to_string_pretty(&config).map_err(|source| {
            ConfigError::SerializeConfig {
                path: path.clone(),
                source,
            }
        })?;
        atomic_write(&path, json.as_bytes()).map_err(|source| ConfigError::PersistConfig {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    pub fn has_download_folder(&self) -> bool {
        !self.download_folder.trim().is_empty()
    }

    /// Custom thumbnail directory, or `thumbs` next to the database by default.
    /// Example: if `database_path` is `data/archivesift.sqlite3` this returns `data/thumbs`.
    pub fn thumb_dir(&self) -> PathBuf {
        if let Some(folder) = self
            .thumbnail_folder
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            return Path::new(folder.trim()).components().collect();
        }
        Path::new(&self.database_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("thumbs")
    }

    pub fn config_path() -> &'static Path {
        Path::new(APP_CONFIG_PATH)
    }

    pub fn set_active_download_folder(&mut self, folder: impl Into<String>) {
        self.download_folder = folder.into();
        self.normalize_download_history();
    }

    /// Returns the effective download root: `download_folder / subfolder` if subfolder is set,
    /// otherwise just `download_folder`.
    pub fn effective_download_dir(&self) -> std::path::PathBuf {
        let base = std::path::Path::new(&self.download_folder);
        let sub = self.subfolder.trim();
        if sub.is_empty() {
            base.to_path_buf()
        } else {
            base.join(sub)
        }
    }

    /// Returns the bounded number of content/listing jobs that may run in parallel.
    ///
    /// This is calculated at the point of use as well as normalized on load/save,
    /// so an in-memory value edited by the UI cannot accidentally disable limits.
    pub fn effective_parallel_downloads(&self) -> usize {
        self.max_parallel_downloads.clamp(1, MAX_PARALLEL_PAGE_JOBS)
    }

    /// Returns the per-item image limit while keeping the combined page/image
    /// workload below [`MAX_TOTAL_CONCURRENT_IMAGE_DOWNLOADS`].
    pub fn effective_concurrent_image_downloads(&self) -> usize {
        let parallel_pages = self.effective_parallel_downloads();
        let combined_cap = (MAX_TOTAL_CONCURRENT_IMAGE_DOWNLOADS / parallel_pages).max(1);
        self.max_concurrent_downloads
            .clamp(1, MAX_IMAGES_PER_ITEM.min(combined_cap))
    }

    fn normalize_download_history(&mut self) {
        let active = self.download_folder.trim().to_string();
        let mut history = Vec::new();

        if !active.is_empty() {
            history.push(active.clone());
        }

        for entry in &self.download_folder_history {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }

            if history
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(trimmed))
            {
                continue;
            }

            history.push(trimmed.to_string());
        }

        history.truncate(MAX_DOWNLOAD_HISTORY);
        self.download_folder_history = history;
        self.download_folder = active;
    }

    fn normalize_performance_limits(&mut self) {
        self.max_parallel_downloads = self.effective_parallel_downloads();
        self.max_concurrent_downloads = self.effective_concurrent_image_downloads();
    }

    fn migrate_to_current_version(&mut self) {
        self.config_version = CURRENT_CONFIG_VERSION;
        self.scrape_layout = self.scrape_layout.normalized();
    }

    fn path() -> PathBuf {
        PathBuf::from(APP_CONFIG_PATH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_current_version() {
        assert_eq!(AppConfig::default().config_version, CURRENT_CONFIG_VERSION);
    }

    #[test]
    fn load_unversioned_config_migrates_to_current_version() {
        let raw = r#"{
          "source_url": "https://demo.example.test/demo-site/galleries/summer-escape-001/",
          "download_folder": "Z:\\demo",
          "subfolder": "",
          "download_folder_history": ["Z:\\demo"],
          "clipboard_listener_enabled": true,
          "database_path": "data/archivesift.sqlite3",
          "add_plus": false,
          "max_concurrent_downloads": 5,
          "max_parallel_downloads": 3,
          "input_mode": "ClipboardMonitor",
          "preview_open_mode": "double_click",
          "preset_tags": "",
          "preset_categories": "",
          "skip_downloaded": true,
          "parse_additional_links": false,
          "search_columns": 4,
          "request_delay_min_ms": 1000,
          "request_delay_max_ms": 3000
        }"#;

        let raw_value: Value = serde_json::from_str(raw).expect("json should parse");
        let had_version = raw_value.get("config_version").is_some();
        let mut config: AppConfig = serde_json::from_value(raw_value).expect("config should parse");
        if !had_version || config.config_version < CURRENT_CONFIG_VERSION {
            config.migrate_to_current_version();
        }
        config.normalize_download_history();

        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.download_folder, "Z:\\demo");
        assert_eq!(config.download_folder_history, vec!["Z:\\demo"]);
        assert_eq!(config.scrape_layout, ScrapeLayoutConfig::default());
    }

    #[test]
    fn load_older_versioned_config_migrates_to_current_version() {
        let raw = r#"{
          "config_version": 0,
          "source_url": "https://demo.example.test/demo-site/galleries/city-lights-002/",
          "download_folder": "Z:\\demo-two",
          "subfolder": "favorites",
          "download_folder_history": ["Z:\\demo-two", "Z:\\archive"],
          "clipboard_listener_enabled": false,
          "database_path": "data/archivesift.sqlite3",
          "add_plus": true,
          "max_concurrent_downloads": 2,
          "max_parallel_downloads": 1,
          "input_mode": "ManualUrl",
          "preview_open_mode": "single_click",
          "preset_tags": "tag-a",
          "preset_categories": "cat-a",
          "skip_downloaded": false,
          "parse_additional_links": true,
          "search_columns": 3,
          "request_delay_min_ms": 0,
          "request_delay_max_ms": 0
        }"#;

        let raw_value: Value = serde_json::from_str(raw).expect("json should parse");
        let had_version = raw_value.get("config_version").is_some();
        let mut config: AppConfig = serde_json::from_value(raw_value).expect("config should parse");
        if !had_version || config.config_version < CURRENT_CONFIG_VERSION {
            config.migrate_to_current_version();
        }
        config.normalize_download_history();

        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.download_folder, "Z:\\demo-two");
        assert_eq!(
            config.download_folder_history,
            vec!["Z:\\demo-two", "Z:\\archive"]
        );
        assert_eq!(config.subfolder, "favorites");
        assert_eq!(config.preview_open_mode, PreviewOpenMode::SingleClick);
        assert_eq!(config.scrape_layout, ScrapeLayoutConfig::default());
    }

    #[test]
    fn normalize_download_history_promotes_active_folder_deduplicates_and_truncates() {
        let mut config = AppConfig {
            download_folder: " Z:\\Active ".to_string(),
            download_folder_history: vec![
                "z:\\active".to_string(),
                "Z:\\Archive".to_string(),
                "".to_string(),
                "Z:\\Archive".to_string(),
                "Z:\\Extra1".to_string(),
                "Z:\\Extra2".to_string(),
                "Z:\\Extra3".to_string(),
                "Z:\\Extra4".to_string(),
                "Z:\\Extra5".to_string(),
                "Z:\\Extra6".to_string(),
                "Z:\\Extra7".to_string(),
                "Z:\\Extra8".to_string(),
                "Z:\\Extra9".to_string(),
                "Z:\\Extra10".to_string(),
                "Z:\\Extra11".to_string(),
            ],
            ..AppConfig::default()
        };

        config.normalize_download_history();

        assert_eq!(config.download_folder, "Z:\\Active");
        assert_eq!(config.download_folder_history.len(), MAX_DOWNLOAD_HISTORY);
        assert_eq!(config.download_folder_history[0], "Z:\\Active");
        assert_eq!(config.download_folder_history[1], "Z:\\Archive");
        assert_eq!(
            config.download_folder_history,
            vec![
                "Z:\\Active",
                "Z:\\Archive",
                "Z:\\Extra1",
                "Z:\\Extra2",
                "Z:\\Extra3",
                "Z:\\Extra4",
                "Z:\\Extra5",
                "Z:\\Extra6",
                "Z:\\Extra7",
                "Z:\\Extra8",
            ]
        );
    }

    #[test]
    fn parse_comma_list_trims_and_discards_empty_values() {
        assert_eq!(
            parse_comma_list(" outdoor,  amateur ,, solo , "),
            vec!["outdoor", "amateur", "solo"]
        );
    }

    #[test]
    fn thumb_dir_uses_database_parent_directory() {
        let config = AppConfig {
            database_path: "data/custom/archivesift.sqlite3".to_string(),
            ..AppConfig::default()
        };

        assert_eq!(config.thumb_dir(), PathBuf::from("data/custom/thumbs"));
    }

    #[test]
    fn effective_download_dir_uses_subfolder_when_present() {
        let config = AppConfig {
            download_folder: "D:\\Downloads".to_string(),
            subfolder: "favorites".to_string(),
            ..AppConfig::default()
        };

        assert_eq!(
            config.effective_download_dir(),
            PathBuf::from("D:\\Downloads").join("favorites")
        );
    }

    #[test]
    fn effective_download_dir_ignores_blank_subfolder() {
        let config = AppConfig {
            download_folder: "D:\\Downloads".to_string(),
            subfolder: "   ".to_string(),
            ..AppConfig::default()
        };

        assert_eq!(
            config.effective_download_dir(),
            PathBuf::from("D:\\Downloads")
        );
    }

    #[test]
    fn set_active_download_folder_promotes_folder_into_history() {
        let mut config = AppConfig {
            download_folder_history: vec!["D:\\Archive".to_string()],
            ..AppConfig::default()
        };

        config.set_active_download_folder(" D:\\Fresh ");

        assert_eq!(config.download_folder, "D:\\Fresh");
        assert_eq!(
            config.download_folder_history,
            vec!["D:\\Fresh", "D:\\Archive"]
        );
    }

    #[test]
    fn preview_open_mode_reports_single_click_behavior() {
        assert!(PreviewOpenMode::SingleClick.opens_on_single_click());
        assert!(!PreviewOpenMode::DoubleClick.opens_on_single_click());
    }

    #[test]
    fn has_download_folder_returns_false_for_blank_values() {
        let config = AppConfig {
            download_folder: "   ".to_string(),
            ..AppConfig::default()
        };

        assert!(!config.has_download_folder());
    }

    #[test]
    fn has_download_folder_returns_true_for_non_blank_values() {
        let config = AppConfig {
            download_folder: "D:\\Downloads".to_string(),
            ..AppConfig::default()
        };

        assert!(config.has_download_folder());
    }

    #[test]
    fn scrape_layout_defaults_cover_three_stage_responsive_breakpoints() {
        let layout = ScrapeLayoutConfig::default();

        assert_eq!(layout.compact_breakpoint, 980.0);
        assert_eq!(layout.two_column_breakpoint, 1440.0);
        assert_eq!(layout.wide_breakpoint, 1760.0);
        assert_eq!(layout.medium_primary_ratio, 0.57);
        assert_eq!(layout.wide_primary_ratio, 0.50);
        assert_eq!(layout.scroll_inset, 12.0);
    }

    #[test]
    fn scrape_layout_normalization_clamps_breakpoints_and_ratios() {
        let normalized = ScrapeLayoutConfig {
            compact_breakpoint: 400.0,
            two_column_breakpoint: 700.0,
            wide_breakpoint: 710.0,
            medium_primary_ratio: 0.90,
            wide_primary_ratio: 0.20,
            scroll_inset: 80.0,
        }
        .normalized();

        assert_eq!(normalized.compact_breakpoint, 640.0);
        assert_eq!(normalized.two_column_breakpoint, 760.0);
        assert_eq!(normalized.wide_breakpoint, 880.0);
        assert_eq!(normalized.medium_primary_ratio, 0.72);
        assert_eq!(normalized.wide_primary_ratio, 0.45);
        assert_eq!(normalized.scroll_inset, 32.0);
    }

    #[test]
    fn performance_limits_replace_zero_with_safe_minimums() {
        let config = AppConfig {
            max_parallel_downloads: 0,
            max_concurrent_downloads: 0,
            ..AppConfig::default()
        };

        assert_eq!(config.effective_parallel_downloads(), 1);
        assert_eq!(config.effective_concurrent_image_downloads(), 1);
    }

    #[test]
    fn performance_limits_cap_nested_download_concurrency() {
        let config = AppConfig {
            max_parallel_downloads: usize::MAX,
            max_concurrent_downloads: usize::MAX,
            ..AppConfig::default()
        };

        assert_eq!(
            config.effective_parallel_downloads(),
            MAX_PARALLEL_PAGE_JOBS
        );
        assert_eq!(config.effective_concurrent_image_downloads(), 2);
        assert!(
            config.effective_parallel_downloads() * config.effective_concurrent_image_downloads()
                <= MAX_TOTAL_CONCURRENT_IMAGE_DOWNLOADS
        );
    }

    #[test]
    fn performance_limits_preserve_safe_default_values() {
        let config = AppConfig::default();

        assert_eq!(config.effective_parallel_downloads(), 3);
        assert_eq!(config.effective_concurrent_image_downloads(), 5);
    }
}

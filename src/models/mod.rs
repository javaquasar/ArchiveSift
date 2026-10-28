#[derive(Clone, Debug)]
pub struct MetadataAttribute {
    pub kind: String,
    pub label: String,
    pub value: String,
    pub source_url: Option<String>,
    /// For `kind == "actor"`, stores the known gender when available.
    /// `Some(true)` = female, `Some(false)` = male, `None` = unknown.
    pub is_female: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ScrapedImage {
    pub source_url: String,
    pub preview_url: Option<String>,
    pub alt_text: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub sort_order: i64,
    pub local_file_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ScrapedItem {
    pub site_key: String,
    pub page_url: String,
    pub title: String,
    pub raw_html: String,
    pub images: Vec<ScrapedImage>,
    pub attributes: Vec<MetadataAttribute>,
}

#[derive(Clone, Debug)]
pub struct DownloadRequest {
    pub image_url: String,
    pub image_id: i64,
    pub page_id: i64,
    /// Path relative to `base_dir`. Stored in the database so `base_dir` can be relocated.
    pub relative_path: std::path::PathBuf,
}

/// Whether URLs are sourced from the clipboard automatically or entered manually.
#[derive(Clone, Debug, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum InputMode {
    /// The app monitors the clipboard and auto-queues copied URLs.
    #[default]
    ClipboardMonitor,
    /// The user pastes or types URLs into the input field manually.
    ManualUrl,
}

/// How images should be removed when the user triggers a delete.
#[derive(Clone, Debug, PartialEq)]
pub enum DeleteMode {
    /// Remove DB records only; leave files on disk.
    DbOnly,
    /// Remove DB records AND delete the files from disk.
    DbAndDisk,
}

#[derive(Clone, Debug)]
pub enum UrlQueueOutcome {
    Queued,
    Empty,
    Invalid,
    DuplicateQueued,
    AlreadyProcessed,
}

#[derive(Clone, Debug)]
pub struct UrlQueueDecision {
    pub raw_input: String,
    pub normalized_url: Option<String>,
    pub outcome: UrlQueueOutcome,
}

#[derive(Clone, Debug, Default)]
pub struct UrlQueueReport {
    pub loaded: usize,
    pub empty_or_invalid: usize,
    pub duplicate_or_processed: usize,
    pub queued_successfully: usize,
    pub decisions: Vec<UrlQueueDecision>,
}

#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    pub text: String,
    /// Exact scraper/site identifier, for example `demo-site`.
    pub site_key: Option<String>,
    pub tags: Vec<String>,
    pub actors: Vec<String>,
    pub categories: Vec<String>,
    pub studios: Vec<String>,
    pub channels: Vec<String>,
    pub favorites_only: bool,
    /// When set, restrict results to images belonging to this specific page.
    pub page_id: Option<i64>,
}

/// A source page record with aggregate statistics, used for date-group browsing.
#[derive(Clone, Debug)]
pub struct DateGroup {
    pub page_id: i64,
    /// ISO datetime from `source_pages.scraped_at`.
    pub scraped_at: String,
    /// Date portion only (YYYY-MM-DD), derived from `scraped_at`.
    pub date_str: String,
    pub title: String,
    pub image_count: usize,
    pub fav_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DateGroupCursor {
    pub scraped_at: String,
    pub page_id: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DateBrowseDirection {
    Initial,
    Next,
    Previous,
}

#[derive(Clone, Debug)]
pub struct DateGroupPage {
    pub group: Option<DateGroup>,
    pub total: usize,
}

#[derive(Clone, Debug)]
pub struct SearchPageSummary {
    pub page_id: i64,
    pub title: String,
    pub url: String,
    /// Base page metadata is retained once per page. Image results without
    /// image-specific metadata point at this same allocation.
    pub attributes: Arc<Vec<MetadataAttribute>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchCursor {
    pub scraped_at: String,
    pub page_id: i64,
    pub sort_order: i64,
    pub image_id: i64,
}

#[derive(Clone, Debug)]
pub struct SearchResultPage {
    pub items: Vec<SearchImageResult>,
    pub next_cursor: Option<SearchCursor>,
}

#[derive(Clone, Debug)]
pub struct SearchImageResult {
    pub image_id: i64,
    pub page: Arc<SearchPageSummary>,
    pub source_url: String,
    pub preview_url: Option<String>,
    pub local_file_path: Option<String>,
    /// Absolute path to the pre-generated thumbnail stored next to the database.
    /// `None` for images downloaded before thumbnail generation was introduced.
    pub thumbnail_path: Option<String>,
    pub is_favorite: bool,
    /// Page-level metadata is shared by every image in the same page. Results
    /// with image-specific metadata receive a copy-on-merge snapshot.
    pub attributes: Arc<Vec<MetadataAttribute>>,
}

impl SearchImageResult {
    pub fn page_id(&self) -> i64 {
        self.page.page_id
    }

    pub fn page_title(&self) -> &str {
        &self.page.title
    }

    pub fn page_url(&self) -> &str {
        &self.page.url
    }

    /// Returns all actor attribute values for this image.
    pub fn actors(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|a| a.kind == "actor")
            .map(|a| a.value.as_str())
            .collect()
    }

    /// Returns the best available "source" label: studio -> channel -> author -> None.
    pub fn display_source(&self) -> Option<&str> {
        for kind in &["studio", "channel", "author"] {
            if let Some(attr) = self.attributes.iter().find(|a| a.kind == *kind) {
                return Some(&attr.value);
            }
        }
        None
    }
}

impl ScrapedItem {
    pub fn attribute_values<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.attributes
            .iter()
            .filter(move |attribute| attribute.kind == kind)
            .map(|attribute| attribute.value.as_str())
    }
}

/// A point-in-time snapshot of the download queue state, used to drive the queue window UI.
#[derive(Clone, Debug, Default)]
pub struct QueueSnapshot {
    pub paused: bool,
    /// URLs currently being downloaded, paired with the connector/site-key label.
    pub in_progress: Vec<(String, String)>,
    /// Total number of URLs waiting to be processed. This remains accurate
    /// even when `pending` contains only a visible range.
    pub pending_total: usize,
    /// URLs retained after a failed processing attempt.
    pub failed_total: usize,
    /// Queue index represented by `pending[0]`.
    pub pending_start: usize,
    /// Requested range of pending URLs, in queue order.
    pub pending: Vec<String>,
    /// Small prefix used by the scrape-tab summary. It is bounded independently
    /// from the visible queue-window range.
    pub pending_preview: Vec<String>,
}

// Page-kind classification

/// Indicates what kind of page a scraper URL points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    /// A content page that can be scraped directly for downloadable assets.
    Content,
    /// A listing / search page containing links to content pages.
    Listing,
    /// The URL is not supported by any registered scraper.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListingUrlKind {
    Content,
    Listing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingPaginationBehavior {
    FollowNextPage,
    ExhaustedInSingleRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListingFetchBehavior {
    StaticHtml,
    LazyLoadApi,
    EagerPagination,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScraperDiagnosticLevel {
    Info,
    Warn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScraperDiagnostic {
    pub level: ScraperDiagnosticLevel,
    pub code: String,
    pub message: String,
}

impl ScraperDiagnostic {
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: ScraperDiagnosticLevel::Info,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn warn(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: ScraperDiagnosticLevel::Warn,
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingUrl {
    pub url: String,
    pub kind: ListingUrlKind,
}

impl ListingUrl {
    pub fn content(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            kind: ListingUrlKind::Content,
        }
    }

    pub fn listing(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            kind: ListingUrlKind::Listing,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScraperCapabilities {
    pub supported_page_kinds: Vec<PageKind>,
    pub listing_behaviors: Vec<ListingFetchBehavior>,
    pub discovers_child_listing_urls: bool,
    pub enriches_content_metadata: bool,
    pub emits_listing_diagnostics: bool,
}

impl Default for ScraperCapabilities {
    fn default() -> Self {
        Self {
            supported_page_kinds: vec![PageKind::Content],
            listing_behaviors: Vec::new(),
            discovers_child_listing_urls: false,
            enriches_content_metadata: false,
            emits_listing_diagnostics: false,
        }
    }
}

impl ScraperCapabilities {
    pub fn supports_page_kind(&self, kind: PageKind) -> bool {
        self.supported_page_kinds.contains(&kind)
    }
}

/// The result of scraping a listing page (channel, actor, tag, category, search ...).
#[derive(Clone, Debug)]
pub struct ListingPage {
    /// URLs discovered on this page that should be re-enqueued into the normal
    /// pipeline. Each entry explicitly describes whether it points at a content
    /// page or another listing page.
    pub discovered_urls: Vec<ListingUrl>,
    /// URL of the next page in the listing sequence, if pagination exists.
    pub next_page_url: Option<String>,
    /// Total number of pages declared by the site (if known).
    pub total_pages: Option<u32>,
    /// Whether the caller should follow `next_page_url`, or treat this result
    /// as already fully expanded by the scraper.
    pub pagination_behavior: ListingPaginationBehavior,
    /// How this listing result was collected.
    pub fetch_behavior: ListingFetchBehavior,
    /// Structured informational/warning notes emitted by the scraper.
    pub diagnostics: Vec<ScraperDiagnostic>,
}

impl ListingPage {
    pub fn queue_urls(&self) -> Vec<String> {
        let mut urls = self
            .discovered_urls
            .iter()
            .map(|entry| entry.url.clone())
            .collect::<Vec<_>>();

        if matches!(
            self.pagination_behavior,
            ListingPaginationBehavior::FollowNextPage
        ) {
            if let Some(next_page_url) = &self.next_page_url {
                urls.push(next_page_url.clone());
            }
        }

        urls
    }

    pub fn content_urls(&self) -> impl Iterator<Item = &str> {
        self.discovered_urls.iter().filter_map(|entry| {
            (entry.kind == ListingUrlKind::Content).then_some(entry.url.as_str())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(kind: &str, value: &str) -> MetadataAttribute {
        MetadataAttribute {
            kind: kind.to_string(),
            label: kind.to_string(),
            value: value.to_string(),
            source_url: None,
            is_female: None,
        }
    }

    fn sample_search_result(attributes: Vec<MetadataAttribute>) -> SearchImageResult {
        let attributes = Arc::new(attributes);
        SearchImageResult {
            image_id: 1,
            page: Arc::new(SearchPageSummary {
                page_id: 2,
                title: "Sample".to_string(),
                url: "https://demo.example.test/page".to_string(),
                attributes: Arc::clone(&attributes),
            }),
            source_url: "https://demo.example.test/image.jpg".to_string(),
            preview_url: None,
            local_file_path: None,
            thumbnail_path: None,
            is_favorite: false,
            attributes,
        }
    }

    #[test]
    fn actors_returns_only_actor_attribute_values_in_order() {
        let result = sample_search_result(vec![
            attribute("tag", "outdoor"),
            attribute("actor", "Alice Blue"),
            attribute("studio", "Studio Nova"),
            attribute("actor", "Bella Rose"),
        ]);

        assert_eq!(result.actors(), vec!["Alice Blue", "Bella Rose"]);
    }

    #[test]
    fn actors_returns_empty_when_no_actor_attributes_exist() {
        let result = sample_search_result(vec![
            attribute("tag", "outdoor"),
            attribute("studio", "Studio Nova"),
        ]);

        assert!(result.actors().is_empty());
    }

    #[test]
    fn cloning_search_result_shares_attribute_storage() {
        let result = sample_search_result(vec![attribute("tag", "outdoor")]);

        let cloned = result.clone();

        assert!(Arc::ptr_eq(&result.attributes, &cloned.attributes));
        assert!(Arc::ptr_eq(&result.page, &cloned.page));
    }

    #[test]
    fn display_source_prefers_studio_over_channel_and_author() {
        let result = sample_search_result(vec![
            attribute("author", "Author A"),
            attribute("channel", "Channel B"),
            attribute("studio", "Studio C"),
        ]);

        assert_eq!(result.display_source(), Some("Studio C"));
    }

    #[test]
    fn display_source_falls_back_from_channel_to_author() {
        let channel_result = sample_search_result(vec![
            attribute("author", "Author A"),
            attribute("channel", "Channel B"),
        ]);
        let author_result = sample_search_result(vec![attribute("author", "Author A")]);
        let empty_result = sample_search_result(vec![attribute("tag", "outdoor")]);

        assert_eq!(channel_result.display_source(), Some("Channel B"));
        assert_eq!(author_result.display_source(), Some("Author A"));
        assert_eq!(empty_result.display_source(), None);
    }

    #[test]
    fn scraped_item_attribute_values_filters_by_kind() {
        let page = ScrapedItem {
            site_key: "demo-site".to_string(),
            page_url: "https://demo.example.test/gallery".to_string(),
            title: "Sample".to_string(),
            raw_html: "<html></html>".to_string(),
            images: Vec::new(),
            attributes: vec![
                attribute("actor", "Alice Blue"),
                attribute("tag", "outdoor"),
                attribute("actor", "Bella Rose"),
            ],
        };

        let actors = page.attribute_values("actor").collect::<Vec<_>>();
        let tags = page.attribute_values("tag").collect::<Vec<_>>();

        assert_eq!(actors, vec!["Alice Blue", "Bella Rose"]);
        assert_eq!(tags, vec!["outdoor"]);
    }

    #[test]
    fn input_mode_default_is_clipboard_monitor() {
        assert_eq!(InputMode::default(), InputMode::ClipboardMonitor);
    }

    #[test]
    fn page_kind_supports_expected_equality_checks() {
        assert_eq!(PageKind::Content, PageKind::Content);
        assert_ne!(PageKind::Content, PageKind::Listing);
        assert_eq!(PageKind::Unknown, PageKind::Unknown);
    }

    #[test]
    fn listing_page_stores_discovered_urls_next_page_and_total_pages() {
        let listing = ListingPage {
            discovered_urls: vec![
                ListingUrl::content("https://demo.example.test/gallery/a"),
                ListingUrl::listing("https://demo.example.test/listing/child"),
            ],
            next_page_url: Some("https://demo.example.test/listing/page/2".to_string()),
            total_pages: Some(4),
            pagination_behavior: ListingPaginationBehavior::FollowNextPage,
            fetch_behavior: ListingFetchBehavior::StaticHtml,
            diagnostics: vec![ScraperDiagnostic::info(
                "demo.child-listing",
                "discovered a nested listing url",
            )],
        };

        assert_eq!(listing.discovered_urls.len(), 2);
        assert_eq!(
            listing.next_page_url.as_deref(),
            Some("https://demo.example.test/listing/page/2")
        );
        assert_eq!(listing.total_pages, Some(4));
        assert_eq!(
            listing.content_urls().collect::<Vec<_>>(),
            vec!["https://demo.example.test/gallery/a"]
        );
        assert_eq!(
            listing.queue_urls(),
            vec![
                "https://demo.example.test/gallery/a".to_string(),
                "https://demo.example.test/listing/child".to_string(),
                "https://demo.example.test/listing/page/2".to_string(),
            ]
        );
    }

    #[test]
    fn queue_snapshot_default_starts_empty_and_unpaused() {
        let snapshot = QueueSnapshot::default();

        assert!(!snapshot.paused);
        assert!(snapshot.in_progress.is_empty());
        assert_eq!(snapshot.pending_total, 0);
        assert_eq!(snapshot.pending_start, 0);
        assert!(snapshot.pending.is_empty());
        assert!(snapshot.pending_preview.is_empty());
    }

    #[test]
    fn scraper_capabilities_default_to_content_only() {
        let capabilities = ScraperCapabilities::default();

        assert!(capabilities.supports_page_kind(PageKind::Content));
        assert!(!capabilities.supports_page_kind(PageKind::Listing));
        assert!(capabilities.listing_behaviors.is_empty());
        assert!(!capabilities.discovers_child_listing_urls);
        assert!(!capabilities.enriches_content_metadata);
        assert!(!capabilities.emits_listing_diagnostics);
    }

    #[test]
    fn listing_page_skips_next_page_when_scraper_fully_expands_listing() {
        let listing = ListingPage {
            discovered_urls: vec![ListingUrl::content("https://demo.example.test/gallery/a")],
            next_page_url: Some("https://demo.example.test/listing/page/2".to_string()),
            total_pages: Some(2),
            pagination_behavior: ListingPaginationBehavior::ExhaustedInSingleRun,
            fetch_behavior: ListingFetchBehavior::EagerPagination,
            diagnostics: Vec::new(),
        };

        assert_eq!(
            listing.queue_urls(),
            vec!["https://demo.example.test/gallery/a".to_string()]
        );
    }
}
use std::sync::Arc;

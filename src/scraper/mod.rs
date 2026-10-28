mod demo;
pub mod extensions;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use crate::error::ScraperError;
use crate::models::{ListingPage, PageKind, ScrapedItem, ScraperCapabilities};
use crate::AppResult;

pub use demo::DemoSiteScraper;

/// A plugin that knows how to scrape a specific website's content pages.
///
/// Implement this trait and register your scraper in [`ScraperRegistry::new`]
/// to add support for a new site. The registry routes each URL to the first
/// scraper whose [`supports_url`](ContentScraper::supports_url) returns `true`.
pub trait ContentScraper: Send + Sync {
    /// A short, stable identifier for the site this scraper handles.
    ///
    /// Used as the top-level directory name in the structured file layout
    /// (e.g. `"example-site"`). Must be unique across all registered scrapers.
    fn site_key(&self) -> &str;

    /// Explicit extension routes take precedence over broad built-in demo routes.
    fn routing_priority(&self) -> u32 {
        0
    }

    /// Restricts host image requests, including redirects, for external modules.
    fn asset_origins(&self) -> Option<&[String]> {
        None
    }

    /// Describes what kinds of pages and listing behaviors this scraper supports.
    fn capabilities(&self) -> ScraperCapabilities {
        ScraperCapabilities::default()
    }

    /// Returns `true` if this scraper can handle the given URL.
    ///
    /// Typically checks the host and optionally the URL path pattern.
    fn supports_url(&self, url: &str) -> bool;

    /// Classifies a supported URL as a content page or a listing/search page.
    ///
    /// The default implementation always returns [`PageKind::Content`], which
    /// preserves backwards compatibility for scrapers that do not distinguish
    /// between the two. Override to enable listing-page support.
    fn classify_url(&self, url: &str) -> PageKind {
        if self.supports_url(url) {
            PageKind::Content
        } else {
            PageKind::Unknown
        }
    }

    /// Scrapes the content page at `url` and returns the structured result.
    ///
    /// The returned [`ScrapedItem`] includes page metadata (title, attributes
    /// such as actors/tags/categories/studios) and all discovered image URLs.
    fn scrape_content<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>>;

    /// Scrapes a listing / search page and returns all discovered content URLs
    /// together with the URL of the next page (if any) for automatic pagination.
    ///
    /// Only called when [`classify_url`](ContentScraper::classify_url) returns
    /// [`PageKind::Listing`]. The default implementation returns an error so
    /// that scrapers without listing support fail loudly rather than silently.
    fn scrape_listing<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = AppResult<ListingPage>> + Send + 'a>> {
        let url = url.to_string();
        Box::pin(async move { Err(ScraperError::ListingNotImplemented { url }.into()) })
    }
}

/// Routes content URLs to the appropriate registered [`ContentScraper`].
///
/// Add new scrapers by implementing [`ContentScraper`] and registering them
/// explicitly through [`ScraperRegistry::register`].
#[derive(Clone)]
pub struct ScraperRegistry {
    scrapers: Arc<Vec<Arc<dyn ContentScraper>>>,
    extension_statuses: Arc<Vec<extensions::ExtensionStatus>>,
}

impl ScraperRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            scrapers: Arc::new(Vec::new()),
            extension_statuses: Arc::new(Vec::new()),
        }
    }

    /// Creates a registry pre-loaded with the application's public scrapers.
    ///
    /// Keeping public registration separate from [`ScraperRegistry::new`]
    /// makes it possible to publish the platform with only demo scrapers,
    /// while wiring private site adapters in local or private builds.
    pub fn with_public_scrapers() -> Self {
        let mut registry = Self::new();
        register_public_scrapers(&mut registry);
        registry
    }

    /// Creates a registry containing the scrapers shipped with this build.
    pub fn with_all_scrapers() -> Self {
        static REGISTRY: OnceLock<ScraperRegistry> = OnceLock::new();
        REGISTRY
            .get_or_init(|| Self::with_extensions_from(&extensions::default_plugin_directory()))
            .clone()
    }

    /// Creates a deterministic startup snapshot. Restart to apply package changes.
    pub fn with_extensions_from(directory: &std::path::Path) -> Self {
        let mut registry = Self::with_public_scrapers();
        registry.extension_statuses = Arc::new(extensions::discover(&mut registry, directory));
        registry
    }

    pub fn extension_statuses(&self) -> &[extensions::ExtensionStatus] {
        &self.extension_statuses
    }

    /// Registers one scraper implementation.
    pub fn register(&mut self, scraper: Box<dyn ContentScraper>) {
        let scrapers = Arc::make_mut(&mut self.scrapers);
        scrapers.push(Arc::from(scraper));
        // Stable sorting preserves registration order for equal priorities.
        scrapers.sort_by_key(|scraper| std::cmp::Reverse(scraper.routing_priority()));
    }

    /// Returns the scraper that supports `url`, or `None` if no match.
    pub fn scraper_for_url(&self, url: &str) -> Option<&dyn ContentScraper> {
        self.scrapers
            .iter()
            .find(|scraper| scraper.supports_url(url))
            .map(|scraper| scraper.as_ref())
    }

    /// Returns `true` if any registered scraper supports `url`.
    pub fn supports_url(&self, url: &str) -> bool {
        self.scraper_for_url(url).is_some()
    }

    /// Classifies `url` as a content, listing, or unknown page.
    pub fn classify_url(&self, url: &str) -> PageKind {
        self.scraper_for_url(url)
            .map(|s| s.classify_url(url))
            .unwrap_or(PageKind::Unknown)
    }

    /// Returns the `site_key` of every registered scraper, in registration order.
    pub fn list_site_keys(&self) -> Vec<&str> {
        self.scrapers.iter().map(|s| s.site_key()).collect()
    }

    /// Returns a snapshot of the capabilities of every registered scraper.
    pub fn list_capabilities(&self) -> Vec<(&str, ScraperCapabilities)> {
        self.scrapers
            .iter()
            .map(|scraper| (scraper.site_key(), scraper.capabilities()))
            .collect()
    }

    /// Returns the capabilities of the scraper that supports `url`, if any.
    pub fn capabilities_for_url(&self, url: &str) -> Option<ScraperCapabilities> {
        self.scraper_for_url(url)
            .map(|scraper| scraper.capabilities())
    }
}

/// Registers the public scrapers that are safe to ship in the open-source build.
///
/// Keeping this list in one place makes it straightforward to support several
/// scrapers at once in the desktop app while still allowing public/private
/// builds to customize which adapters are included.
pub fn register_public_scrapers(registry: &mut ScraperRegistry) {
    registry.register(Box::new(DemoSiteScraper::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ContentOnlyScraper {
        site_key: &'static str,
        host: &'static str,
    }

    impl ContentScraper for ContentOnlyScraper {
        fn site_key(&self) -> &'static str {
            self.site_key
        }

        fn supports_url(&self, url: &str) -> bool {
            url.contains(self.host)
        }

        fn scrape_content<'a>(
            &'a self,
            _url: &'a str,
        ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>> {
            Box::pin(async move { Err("not used in routing test".into()) })
        }
    }

    struct ListingScraper {
        site_key: &'static str,
        host: &'static str,
    }

    impl ContentScraper for ListingScraper {
        fn site_key(&self) -> &'static str {
            self.site_key
        }

        fn supports_url(&self, url: &str) -> bool {
            url.contains(self.host)
        }

        fn capabilities(&self) -> ScraperCapabilities {
            ScraperCapabilities {
                supported_page_kinds: vec![PageKind::Content, PageKind::Listing],
                listing_behaviors: vec![crate::models::ListingFetchBehavior::StaticHtml],
                discovers_child_listing_urls: false,
                enriches_content_metadata: false,
                emits_listing_diagnostics: true,
            }
        }

        fn classify_url(&self, _url: &str) -> PageKind {
            PageKind::Listing
        }

        fn scrape_content<'a>(
            &'a self,
            _url: &'a str,
        ) -> Pin<Box<dyn Future<Output = AppResult<ScrapedItem>> + Send + 'a>> {
            Box::pin(async move { Err("not used in routing test".into()) })
        }
    }

    #[test]
    fn registry_routes_multiple_sites_by_url() {
        let mut registry = ScraperRegistry::new();
        registry.register(Box::new(ContentOnlyScraper {
            site_key: "alpha",
            host: "alpha.test",
        }));
        registry.register(Box::new(ListingScraper {
            site_key: "beta",
            host: "beta.test",
        }));

        let alpha = registry
            .scraper_for_url("https://alpha.test/gallery/1/")
            .expect("alpha scraper should be found");
        let beta = registry
            .scraper_for_url("https://beta.test/listing/")
            .expect("beta scraper should be found");

        assert_eq!(alpha.site_key(), "alpha");
        assert_eq!(beta.site_key(), "beta");
        assert_eq!(
            registry.classify_url("https://beta.test/listing/"),
            PageKind::Listing
        );
        assert!(registry.supports_url("https://alpha.test/gallery/1/"));
        assert!(registry.supports_url("https://beta.test/listing/"));
    }

    #[test]
    fn public_registry_contains_demo_scraper_only() {
        let registry = ScraperRegistry::with_public_scrapers();

        assert!(registry
            .supports_url("https://demo.example.test/demo-site/galleries/summer-escape-001/"));
        assert!(!registry.supports_url("https://unsupported.example/gallery/123/"));
        assert_eq!(registry.list_site_keys(), vec!["demo-site"]);
        let capabilities = registry
            .capabilities_for_url("https://demo.example.test/demo-site/channels/studio-nova/")
            .expect("demo capabilities should exist");
        assert!(capabilities.supports_page_kind(PageKind::Listing));
        assert!(capabilities
            .listing_behaviors
            .contains(&crate::models::ListingFetchBehavior::LazyLoadApi));
    }

    #[test]
    fn all_scrapers_registry_reuses_the_same_startup_snapshot() {
        let registry = ScraperRegistry::with_all_scrapers();
        let clone = ScraperRegistry::with_all_scrapers();

        assert!(registry
            .supports_url("https://demo.example.test/demo-site/galleries/summer-escape-001/"));
        assert_eq!(registry.list_site_keys(), clone.list_site_keys());
        assert!(std::ptr::eq(
            registry.scrapers[0].as_ref(),
            clone.scrapers[0].as_ref()
        ));
        assert!(Arc::ptr_eq(&registry.scrapers, &clone.scrapers));
        assert!(Arc::ptr_eq(
            &registry.extension_statuses,
            &clone.extension_statuses
        ));
    }

    #[test]
    fn registering_on_a_clone_keeps_existing_registry_snapshots_unchanged() {
        let original = ScraperRegistry::with_public_scrapers();
        let mut clone = original.clone();
        assert!(Arc::ptr_eq(&original.scrapers, &clone.scrapers));
        clone.register(Box::new(ContentOnlyScraper {
            site_key: "extra",
            host: "extra.test",
        }));
        assert_eq!(original.list_site_keys(), ["demo-site"]);
        assert_eq!(clone.list_site_keys(), ["demo-site", "extra"]);
        assert!(!Arc::ptr_eq(&original.scrapers, &clone.scrapers));
        assert!(Arc::ptr_eq(
            &original.extension_statuses,
            &clone.extension_statuses
        ));
        assert!(!original.supports_url("https://extra.test/content"));
        assert!(clone.supports_url("https://extra.test/content"));
    }

    #[test]
    fn registry_lists_scraper_capabilities_in_registration_order() {
        let mut registry = ScraperRegistry::new();
        registry.register(Box::new(ContentOnlyScraper {
            site_key: "alpha",
            host: "alpha.test",
        }));
        registry.register(Box::new(ListingScraper {
            site_key: "beta",
            host: "beta.test",
        }));

        let capabilities = registry.list_capabilities();

        assert_eq!(capabilities.len(), 2);
        assert_eq!(capabilities[0].0, "alpha");
        assert_eq!(capabilities[1].0, "beta");
        assert_eq!(capabilities[0].1, ScraperCapabilities::default());
        assert!(capabilities[1].1.supports_page_kind(PageKind::Listing));
    }
}

use std::path::PathBuf;

use futures::{stream, StreamExt};

use crate::app::{parse_comma_list, AppConfig};
use crate::db::{Database, FileDownloadStore, ImageArtifactUpdate};
use crate::error::ScraperError;
use crate::fs::{DirectoryStrategy, StructuredDirectoryStrategy};
use crate::http::fetch_response;
use crate::models::{DownloadRequest, PageKind, ScrapedImage, ScrapedItem, ScraperDiagnosticLevel};
use crate::scraper::ScraperRegistry;
use crate::services::ThumbnailService;
use crate::ui::ConsoleUi;
use crate::AppResult;

pub struct ContentDownloadService<
    R = ScraperRegistry,
    D = Database,
    F = FileDownloadStore,
    U = ConsoleUi,
    S = StructuredDirectoryStrategy,
> {
    scraper_registry: R,
    database: D,
    file_store: F,
    ui: U,
    strategy: S,
    base_dir: PathBuf,
    thumbnail_service: ThumbnailService,
}

impl<R, D, F, U, S> ContentDownloadService<R, D, F, U, S> {
    pub fn new(
        scraper_registry: R,
        database: D,
        file_store: F,
        ui: U,
        strategy: S,
        base_dir: PathBuf,
        thumb_dir: PathBuf,
    ) -> Self {
        Self {
            scraper_registry,
            database,
            file_store,
            ui,
            strategy,
            base_dir,
            thumbnail_service: ThumbnailService::new(thumb_dir),
        }
    }
}

impl
    ContentDownloadService<
        ScraperRegistry,
        Database,
        FileDownloadStore,
        ConsoleUi,
        StructuredDirectoryStrategy,
    >
{
    pub async fn run(&self, config: &AppConfig) -> AppResult<()> {
        self.run_for_url(config, &config.source_url)
            .await
            .map(|_| ())
    }

    /// Processes one URL.
    ///
    /// * **Content page** -> downloads images, returns `Ok(vec![])`.
    /// * **Listing page** -> extracts content links and next-page URL, returns
    ///   `Ok(discovered_urls)` for the caller to re-enqueue.
    pub async fn run_for_url(
        &self,
        config: &AppConfig,
        source_url: &str,
    ) -> AppResult<Vec<String>> {
        self.database.initialize()?;
        self.ui.print_database_path(&config.database_path);

        let scraper = self
            .scraper_registry
            .scraper_for_url(source_url)
            .ok_or_else(|| ScraperError::NoScraperRegistered {
                url: source_url.to_string(),
            })?;

        match scraper.classify_url(source_url) {
            PageKind::Listing => self.process_listing_url(scraper, config, source_url).await,
            PageKind::Content | PageKind::Unknown => {
                self.process_content_url(scraper, config, source_url).await
            }
        }
    }

    async fn process_listing_url(
        &self,
        scraper: &dyn crate::scraper::ContentScraper,
        config: &AppConfig,
        source_url: &str,
    ) -> AppResult<Vec<String>> {
        if !config.parse_additional_links {
            self.ui.print_status(&format!(
                "Skipping listing expansion for {} because additional content links are disabled.",
                source_url
            ));
            return Ok(vec![]);
        }

        let listing = scraper.scrape_listing(source_url).await?;
        for diagnostic in &listing.diagnostics {
            let message = format!("[{}] {}", diagnostic.code, diagnostic.message);
            match diagnostic.level {
                ScraperDiagnosticLevel::Info => self.ui.print_status(&message),
                ScraperDiagnosticLevel::Warn => self.ui.print_warning(&message),
            }
        }

        let discovered = listing.queue_urls();
        self.ui.print_found_links(discovered.len());
        Ok(discovered)
    }

    async fn process_content_url(
        &self,
        scraper: &dyn crate::scraper::ContentScraper,
        config: &AppConfig,
        source_url: &str,
    ) -> AppResult<Vec<String>> {
        let mut page = scraper.scrape_content(source_url).await?;
        self.log_content_discovery(&page);

        if page.images.is_empty() {
            return Ok(vec![]);
        }

        let persisted = self.database.upsert_scraped_item(&page)?;
        // The diagnostic HTML has been archived transactionally. Release the
        // potentially large allocation before image downloads start.
        page.raw_html = String::new();
        self.apply_preset_attributes(
            config,
            &persisted
                .image_ids_by_url
                .values()
                .copied()
                .collect::<Vec<_>>(),
        )?;

        let requests =
            self.build_download_requests(&page, persisted.page_id, &persisted.image_ids_by_url);
        self.execute_downloads(requests, config.effective_concurrent_image_downloads())
            .await?;

        Ok(vec![])
    }

    fn log_content_discovery(&self, page: &ScrapedItem) {
        self.ui.print_page_title(&page.title);
        self.ui.print_html(&page.raw_html);
        self.ui.print_attributes(&page.attributes);

        for image in &page.images {
            self.ui.print_discovered_link(&image.source_url);
        }
        self.ui.print_found_links(page.images.len());
    }

    fn apply_preset_attributes(&self, config: &AppConfig, image_ids: &[i64]) -> AppResult<()> {
        let preset_tags = parse_comma_list(&config.preset_tags);
        let preset_categories = parse_comma_list(&config.preset_categories);
        self.database.add_image_attribute_groups(
            image_ids,
            &[
                ("tag", "tag", preset_tags.as_slice()),
                ("category", "category", preset_categories.as_slice()),
            ],
        )
    }

    fn build_download_requests(
        &self,
        page: &ScrapedItem,
        page_id: i64,
        image_ids_by_url: &std::collections::HashMap<String, i64>,
    ) -> Vec<DownloadRequest> {
        page.images
            .iter()
            .filter_map(|image| {
                let image_id = image_ids_by_url.get(&image.source_url)?;
                Some(build_download_request(
                    image,
                    page,
                    page_id,
                    *image_id,
                    &self.strategy,
                ))
            })
            .collect()
    }

    async fn execute_downloads(
        &self,
        requests: Vec<DownloadRequest>,
        max_concurrent_downloads: usize,
    ) -> AppResult<()> {
        let results = stream::iter(requests)
            .map(|request| self.download_one(request))
            .buffer_unordered(max_concurrent_downloads.max(1))
            .collect::<Vec<_>>()
            .await;
        let mut artifacts = Vec::with_capacity(results.len());
        for result in results {
            match result {
                Ok(artifact) => artifacts.push(artifact),
                Err(error) => {
                    self.ui.print_download_failed(error.as_ref());
                }
            }
        }

        self.database.update_image_artifacts_batch(&artifacts)?;
        Ok(())
    }

    async fn download_one(&self, mut request: DownloadRequest) -> AppResult<ImageArtifactUpdate> {
        self.ui.print_download_target(&request.image_url);

        // Some image URLs end in an opaque ID without a file extension. Reuse
        // any previously downloaded extension before fetching.
        if request.relative_path.extension().is_none() {
            for extension in ["jpg", "jpeg", "png", "webp", "gif", "avif"] {
                let candidate = request.relative_path.with_extension(extension);
                if self
                    .file_store
                    .exists(&self.base_dir.join(&candidate).to_string_lossy())
                {
                    request.relative_path = candidate;
                    break;
                }
            }
        }
        let mut absolute = self.base_dir.join(&request.relative_path);
        let absolute_str = absolute.to_string_lossy();

        if self.file_store.exists(&absolute_str) {
            self.ui.print_output_file(&absolute_str);
            self.ui.print_skipped_existing(&absolute_str);
            return Ok(self.prepare_local_artifact(&request, &absolute).await);
        }

        let limited_response = fetch_response(&request.image_url).await?;
        let (response, request_permit) = limited_response.into_parts();
        if request.relative_path.extension().is_none() {
            let extension = image_extension_from_content_type(
                response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
            );
            request.relative_path.set_extension(extension);
            absolute = self.base_dir.join(&request.relative_path);
        }
        let absolute_str = absolute.to_string_lossy();
        self.ui.print_output_file(&absolute_str);
        let write_result = self
            .file_store
            .write_stream_atomic(&absolute_str, response.bytes_stream())
            .await;
        drop(request_permit);
        write_result?;
        self.ui.print_downloaded(&request.image_url);
        Ok(self.prepare_local_artifact(&request, &absolute).await)
    }

    async fn prepare_local_artifact(
        &self,
        request: &DownloadRequest,
        absolute_path: &std::path::Path,
    ) -> ImageArtifactUpdate {
        let thumbnail_path = self
            .thumbnail_service
            .ensure_with_diagnostics(&self.ui, request.image_id, absolute_path)
            .await;
        let thumbnail_path = thumbnail_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        ImageArtifactUpdate {
            page_id: request.page_id,
            image_id: request.image_id,
            file_path: request.relative_path.to_string_lossy().into_owned(),
            thumbnail_path,
        }
    }
}

fn image_extension_from_content_type(content_type: Option<&str>) -> &'static str {
    match content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
    {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/avif" => "avif",
        "image/jpeg" => "jpg",
        _ => "jpg",
    }
}

fn build_download_request<S: DirectoryStrategy>(
    image: &ScrapedImage,
    page: &ScrapedItem,
    page_id: i64,
    image_id: i64,
    strategy: &S,
) -> DownloadRequest {
    let normalized_url = image.source_url.replace(" :", "").replace("  ", " ");
    let path_without_query = normalized_url.split('?').next().unwrap_or(&normalized_url);
    let original_filename = path_without_query
        .split('/')
        .filter(|segment| !segment.is_empty())
        .next_back()
        .unwrap_or("image");

    let relative_path = strategy.relative_path(page, image, original_filename);

    DownloadRequest {
        image_url: image.source_url.clone(),
        image_id,
        page_id,
        relative_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensionless_images_use_response_mime_type() {
        assert_eq!(
            image_extension_from_content_type(Some("image/png; charset=binary")),
            "png"
        );
        assert_eq!(
            image_extension_from_content_type(Some("image/webp")),
            "webp"
        );
        assert_eq!(image_extension_from_content_type(None), "jpg");
    }

    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use image::{ImageBuffer, Rgb};
    use rusqlite::params;

    struct FixedStrategy {
        relative_path: PathBuf,
    }

    impl DirectoryStrategy for FixedStrategy {
        fn relative_path(
            &self,
            _page: &ScrapedItem,
            _image: &ScrapedImage,
            _original_filename: &str,
        ) -> PathBuf {
            self.relative_path.clone()
        }
    }

    struct FilenameStrategy;

    impl DirectoryStrategy for FilenameStrategy {
        fn relative_path(
            &self,
            _page: &ScrapedItem,
            _image: &ScrapedImage,
            original_filename: &str,
        ) -> PathBuf {
            PathBuf::from(original_filename)
        }
    }

    fn sample_page() -> ScrapedItem {
        ScrapedItem {
            site_key: "demo-site".to_string(),
            page_url: "https://demo.example.test/galleries/sample/".to_string(),
            title: "Sample".to_string(),
            raw_html: "<html></html>".to_string(),
            images: Vec::new(),
            attributes: Vec::new(),
        }
    }

    fn sample_image(source_url: &str) -> ScrapedImage {
        ScrapedImage {
            source_url: source_url.to_string(),
            preview_url: None,
            alt_text: None,
            width: None,
            height: None,
            sort_order: 0,
            local_file_path: None,
        }
    }

    fn unique_test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!("archivesift_content_{label}_{nanos}"))
    }

    #[test]
    fn build_download_request_preserves_strategy_relative_path() {
        let page = sample_page();
        let image = sample_image("https://cdn.example.test/media/sample.jpg");
        let strategy = FixedStrategy {
            relative_path: PathBuf::from("actors/alice/sample.jpg"),
        };

        let request = build_download_request(&image, &page, 10, 20, &strategy);

        assert_eq!(request.page_id, 10);
        assert_eq!(request.image_id, 20);
        assert_eq!(request.image_url, image.source_url);
        assert_eq!(request.relative_path, Path::new("actors/alice/sample.jpg"));
    }

    #[test]
    fn build_download_request_uses_last_path_segment_as_filename() {
        let page = sample_page();
        let image = sample_image("https://cdn.example.test/media/folder/demo-image.webp");

        let request = build_download_request(&image, &page, 1, 2, &FilenameStrategy);

        assert_eq!(request.relative_path, Path::new("demo-image.webp"));
    }

    #[test]
    fn build_download_request_applies_existing_filename_normalization_rules() {
        let page = sample_page();
        let image = sample_image("https://cdn.example.test/media/odd :name  .jpg");

        let request = build_download_request(&image, &page, 1, 2, &FilenameStrategy);

        assert_eq!(request.relative_path, Path::new("oddname .jpg"));
    }

    #[test]
    fn build_download_request_ignores_image_url_query_when_naming_file() {
        let page = sample_page();
        let image = sample_image("https://cdn.example.test/media/cover.jpg?width=1280");
        let request = build_download_request(&image, &page, 1, 2, &FilenameStrategy);
        assert_eq!(request.relative_path, Path::new("cover.jpg"));
    }

    #[test]
    fn build_download_requests_skips_images_without_persisted_ids() {
        let service = ContentDownloadService::new(
            ScraperRegistry::with_public_scrapers(),
            Database::new(std::env::temp_dir().join("content_download_request_skip.db")),
            FileDownloadStore::new(),
            ConsoleUi::new(),
            StructuredDirectoryStrategy,
            PathBuf::from("downloads"),
            PathBuf::from("thumbs"),
        );
        let page = ScrapedItem {
            site_key: "demo-site".to_string(),
            page_url: "https://demo.example.test/galleries/sample/".to_string(),
            title: "Sample".to_string(),
            raw_html: "<html></html>".to_string(),
            images: vec![
                sample_image("https://cdn.example.test/media/known.jpg"),
                sample_image("https://cdn.example.test/media/missing.jpg"),
            ],
            attributes: Vec::new(),
        };
        let image_ids_by_url = std::collections::HashMap::from([(
            "https://cdn.example.test/media/known.jpg".to_string(),
            41,
        )]);

        let requests = service.build_download_requests(&page, 11, &image_ids_by_url);

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].page_id, 11);
        assert_eq!(requests[0].image_id, 41);
        assert_eq!(
            requests[0].image_url,
            "https://cdn.example.test/media/known.jpg"
        );
    }

    #[tokio::test]
    async fn existing_content_files_are_finalized_through_the_batch_path() {
        let root = unique_test_root("batch");
        let database_path = root.join("gallery.db");
        let base_dir = root.join("downloads");
        let thumb_dir = root.join("thumbs");
        let database = Database::new(&database_path);
        database.initialize().expect("initialize database");
        let connection = rusqlite::Connection::open(&database_path).expect("open database");
        connection
            .execute(
                "INSERT INTO source_pages (site_key, page_url, title, raw_html) VALUES ('demo', 'https://demo.test/gallery', 'Demo', '')",
                [],
            )
            .expect("insert page");
        let page_id = connection.last_insert_rowid();
        let mut requests = Vec::new();
        for index in 0..2 {
            let source_url = format!("https://demo.test/image-{index}.png");
            connection
                .execute(
                    "INSERT INTO images (source_url) VALUES (?1)",
                    params![source_url],
                )
                .expect("insert image");
            let image_id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO page_images (page_id, image_id, sort_order) VALUES (?1, ?2, ?3)",
                    params![page_id, image_id, index],
                )
                .expect("link image");
            let relative_path = PathBuf::from(format!("gallery/image-{index}.png"));
            let absolute_path = base_dir.join(&relative_path);
            std::fs::create_dir_all(absolute_path.parent().expect("image parent"))
                .expect("create image directory");
            ImageBuffer::from_pixel(8, 8, Rgb([index as u8, 10, 20]))
                .save(&absolute_path)
                .expect("save local image");
            requests.push(DownloadRequest {
                page_id,
                image_id,
                image_url: source_url,
                relative_path,
            });
        }
        drop(connection);

        let service = ContentDownloadService::new(
            ScraperRegistry::with_public_scrapers(),
            database,
            FileDownloadStore::new(),
            ConsoleUi::new(),
            StructuredDirectoryStrategy,
            base_dir,
            thumb_dir,
        );
        service
            .execute_downloads(requests, 2)
            .await
            .expect("finalize existing files");

        let connection = rusqlite::Connection::open(&database_path).expect("reopen database");
        let finalized: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM page_images WHERE page_id = ?1 AND local_file_path IS NOT NULL AND thumbnail_path IS NOT NULL AND downloaded_at IS NOT NULL",
                params![page_id],
                |row| row.get(0),
            )
            .expect("count finalized files");
        assert_eq!(finalized, 2);

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }
}

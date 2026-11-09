use std::path::PathBuf;

use futures::{stream, StreamExt};

use crate::app::{parse_comma_list, AppConfig};
use crate::db::{Database, FileDownloadStore, ImageArtifactUpdate};
use crate::error::ScraperError;
use crate::fs::{DirectoryStrategy, StructuredDirectoryStrategy};
use crate::http::{fetch_response, OriginRestrictedClient};
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
        self.run_page(config, source_url, false).await
    }

    pub(crate) async fn repair_page_images(
        &self,
        config: &AppConfig,
        source_url: &str,
    ) -> AppResult<Vec<String>> {
        self.run_page(config, source_url, true).await
    }

    async fn run_page(
        &self,
        config: &AppConfig,
        source_url: &str,
        images_only: bool,
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
            PageKind::Listing if images_only => {
                Err("An image repair requires a saved content page.".into())
            }
            PageKind::Listing => self.process_listing_url(scraper, config, source_url).await,
            PageKind::Content | PageKind::Unknown => {
                self.process_content_url(scraper, config, source_url, images_only)
                    .await
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

        let settings = scraper
            .extension_id()
            .and_then(|id| config.extension_settings.get(id));
        let listing = scraper
            .scrape_listing_configured(source_url, settings)
            .await?;
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
        images_only: bool,
    ) -> AppResult<Vec<String>> {
        let settings = scraper
            .extension_id()
            .and_then(|id| config.extension_settings.get(id));
        let mut page = scraper
            .scrape_content_configured(source_url, settings)
            .await?;
        if images_only && page.page_url != source_url {
            return Err("The extension returned a different page for this image repair.".into());
        }
        let content_lock = self.database.content_lock(&page.page_url);
        let _guard = content_lock.lock_owned().await;
        let recovery_database = self.database.clone();
        let recovery_root = self.base_dir.clone();
        let recovery_url = page.page_url.clone();
        let _lease = tokio::task::spawn_blocking(move || -> AppResult<std::fs::File> {
            let lease = recovery_database
                .try_content_lease(&recovery_url)?
                .ok_or("this page is being updated by another application instance")?;
            super::torrent_library::TorrentLibrary::new(recovery_database, recovery_root)
                .recover_page_locked(&recovery_url)?;
            Ok(lease)
        })
        .await??;
        self.log_content_discovery(&page);

        let mut persisted = if images_only {
            self.database.upsert_repair_images(&page)?
        } else {
            self.database.upsert_scraped_item(&page)?
        };
        // The diagnostic HTML has been archived transactionally. Release the
        // potentially large allocation before image downloads start.
        page.raw_html = String::new();
        if images_only {
            // A fresh parse can discover more images or refresh signed URLs.
            // Keep retrying saved images that the parser omitted this time.
            for (id, image) in self.database.page_repair_images(persisted.page_id)? {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    persisted.image_ids_by_url.entry(image.source_url.clone())
                {
                    entry.insert(id);
                    page.images.push(image);
                }
            }
        } else {
            self.apply_preset_attributes(
                config,
                &persisted
                    .image_ids_by_url
                    .values()
                    .copied()
                    .collect::<Vec<_>>(),
            )?;

            super::attachments::save_description(
                &page,
                persisted.page_id,
                &self.database,
                &self.base_dir,
            )
            .await?;
        }

        let requests =
            self.build_download_requests(&page, persisted.page_id, &persisted.image_ids_by_url)?;
        let image_result = self
            .execute_downloads(
                requests,
                config.effective_concurrent_image_downloads(),
                scraper.asset_origins(),
                images_only,
            )
            .await;

        if images_only {
            return image_result.map(|()| Vec::new());
        }
        let attachment_result = super::attachments::download(
            &page,
            persisted.page_id,
            scraper,
            &self.database,
            &self.base_dir,
        )
        .await;

        match (image_result, attachment_result) {
            (Ok(()), result) => result?,
            (Err(error), Ok(())) => return Err(error),
            (Err(images), Err(attachments)) => {
                return Err(
                    format!("{images} Attachment download also failed: {attachments}").into(),
                );
            }
        }

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
    ) -> AppResult<Vec<DownloadRequest>> {
        let directory = self.database.torrent_directory(page_id)?;
        let existing = self
            .database
            .get_page_images_with_paths(page_id)?
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        Ok(page
            .images
            .iter()
            .filter_map(|image| {
                let image_id = image_ids_by_url.get(&image.source_url)?;
                let mut request =
                    build_download_request(image, page, page_id, *image_id, &self.strategy);
                let existing_path = existing.get(image_id).filter(|p| !p.is_empty());
                if let Some(path) = existing_path {
                    request.relative_path = path.into();
                } else if let Some(directory) = &directory {
                    let filename = request.relative_path.file_name()?.to_owned();
                    request.relative_path = directory.join("images").join(filename);
                }
                if existing_path.is_none()
                    && image
                        .preview_url
                        .as_deref()
                        .is_some_and(|preview| preview != image.source_url)
                    && self.base_dir.join(&request.relative_path).exists()
                {
                    // A newly resolved original may share the preview's basename.
                    // Give it a stable separate path instead of accepting old pixels.
                    let mut name = request.relative_path.file_stem()?.to_os_string();
                    name.push(format!("_{image_id}"));
                    if let Some(extension) = request.relative_path.extension() {
                        name.push(".");
                        name.push(extension);
                    }
                    request.relative_path.set_file_name(name);
                }
                Some(request)
            })
            .collect())
    }

    async fn execute_downloads(
        &self,
        requests: Vec<DownloadRequest>,
        max_concurrent_downloads: usize,
        asset_origins: Option<&[String]>,
        repair_empty: bool,
    ) -> AppResult<()> {
        let restricted_client = asset_origins.map(OriginRestrictedClient::new).transpose()?;
        let results = stream::iter(requests)
            .map(|request| self.download_one(request, restricted_client.as_ref(), repair_empty))
            .buffer_unordered(max_concurrent_downloads.max(1))
            .collect::<Vec<_>>()
            .await;
        let total = results.len();
        let mut artifacts = Vec::with_capacity(total);
        let mut failed = 0;
        let mut cause = None;
        for result in results {
            match result {
                Ok(artifact) => artifacts.push(artifact),
                Err(error) => {
                    failed += 1;
                    self.ui.print_download_failed(error.as_ref());
                    if cause.is_none() {
                        cause = Some(error);
                    }
                }
            }
        }

        self.database.update_image_artifacts_batch(&artifacts)?;
        if let Some(cause) = cause {
            return Err(crate::error::IncompleteImages {
                failed,
                total,
                cause,
            }
            .into());
        }
        Ok(())
    }

    async fn download_one(
        &self,
        mut request: DownloadRequest,
        restricted_client: Option<&OriginRestrictedClient>,
        repair_empty: bool,
    ) -> AppResult<ImageArtifactUpdate> {
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
            use tokio::io::AsyncReadExt;
            let mut file = tokio::fs::File::open(&absolute).await?;
            let mut header = [0; 32];
            let size = file.read(&mut header).await?;
            if size > 0 || !repair_empty {
                if image::guess_format(&header[..size]).is_err() {
                    return Err("existing image file has an invalid image header".into());
                }
                self.ui.print_output_file(&absolute_str);
                self.ui.print_skipped_existing(&absolute_str);
                return Ok(self.prepare_local_artifact(&request, &absolute).await);
            }
        }

        let limited_response = match restricted_client {
            Some(client) => client.fetch(&request.image_url).await?,
            None => fetch_response(&request.image_url).await?,
        };
        let (response, request_permit) = limited_response.into_parts();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if matches!(
            content_type.as_str(),
            "text/html" | "application/xhtml+xml" | "application/json"
        ) {
            return Err("image download returned a document instead of an image".into());
        }
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
        // Sniff only a small prefix; replay received chunks without copying the
        // full response, preserving streaming and atomic file finalization.
        let mut stream = response.bytes_stream();
        let mut chunks = Vec::new();
        let mut header = Vec::with_capacity(32);
        while header.len() < 32 {
            let Some(chunk) = stream.next().await else {
                break;
            };
            let chunk = chunk?;
            header.extend(chunk.iter().take(32 - header.len()));
            chunks.push(chunk);
        }
        if image::guess_format(&header).is_err() {
            return Err("image response has an invalid image header".into());
        }
        let stream = stream::iter(chunks.into_iter().map(Ok::<_, reqwest::Error>)).chain(stream);
        let write_result = self
            .file_store
            .write_stream_atomic(&absolute_str, stream)
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
        let root = tempfile::tempdir().unwrap();
        let database = Database::new(root.path().join("archive.sqlite3"));
        database.initialize().unwrap();
        let service = ContentDownloadService::new(
            ScraperRegistry::with_public_scrapers(),
            database,
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

        let requests = service
            .build_download_requests(&page, 11, &image_ids_by_url)
            .unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].page_id, 11);
        assert_eq!(requests[0].image_id, 41);
        assert_eq!(
            requests[0].image_url,
            "https://cdn.example.test/media/known.jpg"
        );
    }

    #[tokio::test]
    async fn document_empty_and_mislabelled_responses_never_create_image_files() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (content_type, body) in [
            ("text/html", b"<html>not an image</html>".as_slice()),
            ("image/jpeg", b"<html>not an image</html>".as_slice()),
            ("application/octet-stream", b"".as_slice()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                let header = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(body).await;
            });
            let service = ContentDownloadService::new(
                ScraperRegistry::new(),
                Database::new(root.path().join("archive.sqlite3")),
                FileDownloadStore::new(),
                ConsoleUi::new(),
                StructuredDirectoryStrategy,
                root.path().join("downloads"),
                root.path().join("thumbs"),
            );
            let request = DownloadRequest {
                image_url: format!("http://{address}/image.jpg"),
                image_id: 1,
                page_id: 1,
                relative_path: "scene/image.jpg".into(),
            };
            assert!(service.download_one(request, None, false).await.is_err());
            assert!(!root.path().join("downloads/scene/image.jpg").exists());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn existing_document_is_rejected_without_overwriting_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("image.jpg");
        let data = b"<html>an old error page</html>";
        std::fs::write(&path, data).unwrap();
        let service = ContentDownloadService::new(
            ScraperRegistry::new(),
            Database::new(root.path().join("archive.sqlite3")),
            FileDownloadStore::new(),
            ConsoleUi::new(),
            StructuredDirectoryStrategy,
            root.path().to_path_buf(),
            root.path().join("thumbs"),
        );
        let request = DownloadRequest {
            image_url: "https://unrequested.invalid/image.jpg".into(),
            image_id: 1,
            page_id: 1,
            relative_path: "image.jpg".into(),
        };
        assert!(service.download_one(request, None, false).await.is_err());
        assert_eq!(std::fs::read(path).unwrap(), data);
    }

    struct RetryFixtureScraper {
        page: ScrapedItem,
        attachment_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        attachment_fails: bool,
    }

    impl crate::scraper::ContentScraper for RetryFixtureScraper {
        fn site_key(&self) -> &str {
            "demo-site"
        }
        fn supports_url(&self, url: &str) -> bool {
            url == self.page.page_url
        }
        fn scrape_content<'a>(
            &'a self,
            _: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<ScrapedItem>> + Send + 'a>>
        {
            Box::pin(async move { Ok(self.page.clone()) })
        }
        fn fetch_attachment<'a>(
            &'a self,
            _: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = AppResult<crate::models::AttachmentData>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.attachment_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.attachment_fails {
                    return Err("fixture attachment unavailable".into());
                }
                Ok(crate::models::AttachmentData {
                    bytes: b"fixture sidecar".to_vec(),
                    content_type: "application/octet-stream".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn partial_downloads_report_failure_keep_attachments_and_retry_only_missing_files() {
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for (attachment_fails, images_only) in [(false, false), (true, false), (false, true)] {
            let root = tempfile::tempdir().unwrap();
            let fixture = root.path().join("fixture.png");
            ImageBuffer::from_pixel(8, 8, Rgb([1u8, 2, 3]))
                .save(&fixture)
                .unwrap();
            let bytes = std::fs::read(fixture).unwrap();
            let recovered = Arc::new(AtomicBool::new(false));
            let good_calls = Arc::new(AtomicUsize::new(0));
            let retry_calls = Arc::new(AtomicUsize::new(0));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let server = {
                let recovered = recovered.clone();
                let good_calls = good_calls.clone();
                let retry_calls = retry_calls.clone();
                tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let mut request = [0; 2048];
                        let len = stream.read(&mut request).await.unwrap();
                        let retry =
                            String::from_utf8_lossy(&request[..len]).starts_with("GET /retry.png ");
                        if retry {
                            retry_calls.fetch_add(1, Ordering::SeqCst);
                        } else {
                            good_calls.fetch_add(1, Ordering::SeqCst);
                        }
                        let ready = !retry || recovered.load(Ordering::SeqCst);
                        let status = if ready {
                            "200 OK"
                        } else {
                            "503 Service Unavailable"
                        };
                        let body = if ready {
                            bytes.as_slice()
                        } else {
                            b"unavailable"
                        };
                        let headers = format!("HTTP/1.1 {status}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
                        stream.write_all(headers.as_bytes()).await.unwrap();
                        stream.write_all(body).await.unwrap();
                    }
                })
            };
            let mut page = sample_page();
            page.images = ["ready.png", "retry.png"]
                .iter()
                .enumerate()
                .map(|(index, name)| {
                    let mut image = sample_image(&format!("{origin}/{name}"));
                    image.sort_order = index as i64;
                    image
                })
                .collect();
            page.attributes.push(crate::models::MetadataAttribute {
                kind: "attachment".into(),
                label: "sidecar.bin".into(),
                value: "Sidecar".into(),
                source_url: Some("https://demo.example.test/sidecar.bin".into()),
                is_female: None,
            });
            let attachment_calls = Arc::new(AtomicUsize::new(0));
            let mut scraper = RetryFixtureScraper {
                page: page.clone(),
                attachment_calls: attachment_calls.clone(),
                attachment_fails,
            };
            let database = Database::new(root.path().join("library.db"));
            database.initialize().unwrap();
            let config = AppConfig::default();
            let mut service = ContentDownloadService::new(
                ScraperRegistry::new(),
                database,
                FileDownloadStore::new(),
                ConsoleUi::new(),
                StructuredDirectoryStrategy,
                root.path().join("downloads"),
                root.path().join("thumbs"),
            );
            let error = service
                .process_content_url(&scraper, &config, &page.page_url, false)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("1 of 2 images could not be downloaded"),
                "{error}"
            );
            assert_eq!(
                error.contains("Attachment download also failed"),
                attachment_fails
            );
            assert_eq!(attachment_calls.load(Ordering::SeqCst), 1);
            assert!(!service.database.has_processed_url(&page.page_url).unwrap());
            let connection = rusqlite::Connection::open(root.path().join("library.db")).unwrap();
            let finalized:i64 = connection.query_row("SELECT COUNT(*) FROM page_images WHERE local_file_path IS NOT NULL AND thumbnail_path IS NOT NULL",[],|row|row.get(0)).unwrap();
            assert_eq!(finalized, 1);
            if !attachment_fails {
                let stored: String = connection
                    .query_row(
                        "SELECT local_file_path FROM page_attachments WHERE filename='sidecar.bin'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    std::fs::read(service.base_dir.join(stored)).unwrap(),
                    b"fixture sidecar"
                );
            }
            recovered.store(true, Ordering::SeqCst);
            let retried = if images_only {
                let (page_id, image_id): (i64, i64) = connection
                    .query_row(
                        "SELECT page_id, image_id FROM page_images WHERE local_file_path IS NULL",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                let folder = service
                    .database
                    .torrent_directory(page_id)
                    .unwrap()
                    .unwrap();
                let empty = folder.join("images").join("empty.png");
                std::fs::write(service.base_dir.join(&empty), []).unwrap();
                service
                    .database
                    .update_image_file_location(page_id, image_id, &empty.to_string_lossy())
                    .unwrap();

                let read_attachments = || {
                    service
                        .database
                        .torrent_card(page_id)
                        .unwrap()
                        .attachments
                        .into_iter()
                        .map(|attachment| {
                            let bytes =
                                std::fs::read(service.base_dir.join(&attachment.local_file_path))
                                    .unwrap();
                            (attachment.local_file_path, bytes)
                        })
                        .collect::<std::collections::BTreeMap<_, _>>()
                };
                let saved_attachments = read_attachments();

                // Repair preserves the saved title and sidecars and also retries
                // the known image even when a fresh parse omits its URL.
                scraper.page.title = "Changed remote title".into();
                scraper.page.attributes.clear();
                scraper.page.images.pop();
                service.scraper_registry.register(Box::new(scraper));
                let result = service.repair_page_images(&config, &page.page_url).await;
                let card = service.database.torrent_card(page_id).unwrap();
                assert_eq!(card.title, page.title);
                assert_eq!(saved_attachments.len(), 2);
                assert_eq!(read_attachments(), saved_attachments);
                assert_eq!(attachment_calls.load(Ordering::SeqCst), 1);
                assert!(
                    std::fs::metadata(service.base_dir.join(empty))
                        .unwrap()
                        .len()
                        > 0
                );
                result
            } else {
                service
                    .process_content_url(&scraper, &config, &page.page_url, false)
                    .await
            };
            if attachment_fails {
                assert!(retried
                    .unwrap_err()
                    .to_string()
                    .contains("fixture attachment unavailable"));
            } else {
                retried.unwrap();
            }
            assert!(service.database.has_processed_url(&page.page_url).unwrap());
            let finalized:i64 = connection.query_row("SELECT COUNT(*) FROM page_images WHERE local_file_path IS NOT NULL AND thumbnail_path IS NOT NULL",[],|row|row.get(0)).unwrap();
            assert_eq!(finalized, 2);
            assert_eq!(good_calls.load(Ordering::SeqCst), 1);
            assert_eq!(retry_calls.load(Ordering::SeqCst), 2);
            server.abort();
        }
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
            .execute_downloads(requests, 2, None, false)
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

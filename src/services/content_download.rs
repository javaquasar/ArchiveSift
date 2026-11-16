use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;

use futures::{stream, StreamExt};

use crate::app::{parse_comma_list, AppConfig};
use crate::db::{Database, FileDownloadStore, ImageArtifactUpdate};
use crate::error::ScraperError;
use crate::fs::{DirectoryStrategy, StructuredDirectoryStrategy};
use crate::http::{fetch_response, OriginRestrictedClient};
use crate::image_access::{ImageAccessIssue, ImagePermissionRequired, ISSUE_KIND};
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

    pub(crate) async fn download_torrent_file(
        &self,
        config: &AppConfig,
        source_url: &str,
    ) -> AppResult<Vec<String>> {
        self.database.initialize()?;
        let Some((page_id, true, saved_url)) = self.database.saved_page_location(source_url)?
        else {
            return Err("Save the torrent card before requesting its file.".into());
        };
        let _lease = self.lock_page(&saved_url).await?;
        if self
            .database
            .torrent_card(page_id)?
            .has_downloaded_torrent(&self.base_dir)
        {
            return Ok(Vec::new());
        }
        let scraper = self
            .scraper_registry
            .scraper_for_url(&saved_url)
            .ok_or_else(|| ScraperError::NoScraperRegistered {
                url: saved_url.clone(),
            })?;
        if scraper.classify_url(&saved_url) == PageKind::Listing {
            return Err("A torrent file download requires a saved content page.".into());
        }
        let settings = scraper
            .extension_id()
            .and_then(|id| config.extension_settings.get(id));
        let page = scraper
            .scrape_content_configured(&saved_url, settings)
            .await?;
        if page.page_url != saved_url
            && crate::db::collection_url_alternate(&saved_url).as_deref() != Some(&page.page_url)
        {
            return Err("The extension returned a different page for this torrent file.".into());
        }
        super::attachments::download_torrent_file(
            &page,
            page_id,
            scraper,
            &self.database,
            &self.base_dir,
        )
        .await?;
        Ok(Vec::new())
    }

    async fn lock_page(
        &self,
        url: &str,
    ) -> AppResult<(tokio::sync::OwnedMutexGuard<()>, std::fs::File)> {
        let guard = self.database.content_lock(url).lock_owned().await;
        let database = self.database.clone();
        let root = self.base_dir.clone();
        let url = url.to_owned();
        let lease = tokio::task::spawn_blocking(move || -> AppResult<std::fs::File> {
            let lease = database
                .try_content_lease(&url)?
                .ok_or("this page is being updated by another application instance")?;
            super::torrent_library::TorrentLibrary::new(database, root)
                .recover_page_locked(&url)?;
            Ok(lease)
        })
        .await??;
        Ok((guard, lease))
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
        let extension_id = scraper.extension_id();
        let settings = extension_id.and_then(|id| config.extension_settings.get(id));
        if extension_id.is_some() {
            self.database
                .seed_legacy_image_access(&self.scraper_registry)?;
        }
        let grants = extension_id
            .map(|id| self.database.image_origin_grants(id))
            .transpose()?
            .unwrap_or_default();
        let mut page = scraper
            .scrape_content_with_image_origins(source_url, settings, &grants)
            .await?;
        if images_only && page.page_url != source_url {
            if crate::db::collection_url_alternate(source_url).as_deref() != Some(&page.page_url) {
                return Err(
                    "The extension returned a different page for this image repair.".into(),
                );
            }
            page.page_url = source_url.to_owned();
        }
        let _lease = self.lock_page(&page.page_url).await?;
        let mut permission_issues = take_image_access_issues(&mut page);
        let image_sections = take_image_sections(&mut page);
        self.log_content_discovery(&page);

        let mut persisted = if images_only {
            self.database.upsert_repair_images(&page)?
        } else {
            self.database.upsert_scraped_item(&page)?
        };
        if let Some(id) = extension_id.filter(|_| !permission_issues.is_empty()) {
            self.database.update_image_access_issues(
                persisted.page_id,
                id,
                &permission_issues,
                false,
            )?;
        }
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

            super::attachments::save_description(persisted.page_id, &self.database, &self.base_dir)
                .await?;
        }

        let sections_by_id: HashMap<_, _> = image_sections
            .into_iter()
            .filter_map(|(url, folder)| {
                persisted.image_ids_by_url.get(&url).map(|id| (*id, folder))
            })
            .collect();
        let sections_by_id = if sections_by_id.is_empty() {
            sections_by_id
        } else {
            let library = super::torrent_library::TorrentLibrary::new(
                self.database.clone(),
                self.base_dir.clone(),
            );
            let page_id = persisted.page_id;
            tokio::task::spawn_blocking(move || -> AppResult<_> {
                library.organize_image_sections_locked(page_id, &sections_by_id)?;
                Ok(sections_by_id)
            })
            .await??
        };
        let requests = self.build_download_requests(
            &page,
            persisted.page_id,
            &persisted.image_ids_by_url,
            &sections_by_id,
            images_only,
        )?;
        let restricted_client = scraper
            .asset_origins()
            .map(|origins| OriginRestrictedClient::with_grants(origins, &grants))
            .transpose()?;
        let image_result = self
            .execute_downloads(
                requests,
                config.effective_concurrent_image_downloads(),
                restricted_client.as_ref(),
                images_only,
                &mut permission_issues,
            )
            .await;
        if let Some(id) = extension_id {
            let replace = image_result.is_ok() && !page.images.is_empty();
            if replace || !permission_issues.is_empty() {
                self.database.update_image_access_issues(
                    persisted.page_id,
                    id,
                    &permission_issues,
                    replace,
                )?;
            }
        }

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
                return Err(crate::error::IncompleteContent {
                    images,
                    attachments,
                }
                .into());
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
        sections_by_id: &HashMap<i64, String>,
        repair: bool,
    ) -> AppResult<Vec<DownloadRequest>> {
        let directory = self.database.torrent_directory(page_id)?;
        let knife_paths = self.database.knife_image_paths(page_id)?;
        let existing: HashMap<_, _> = self
            .database
            .get_page_images_with_paths(page_id)?
            .into_iter()
            .filter(|(_, path)| !path.is_empty())
            .filter_map(|(id, path)| {
                if repair {
                    super::torrent_library::portable_path(&path)
                        .ok()
                        .map(|path| (id, path))
                } else {
                    Some((
                        id,
                        PathBuf::from(path.replace('\\', "/"))
                            .components()
                            .collect(),
                    ))
                }
            })
            .collect();
        let gallery_directory = if repair && directory.is_none() {
            let mut directories = HashMap::<&Path, usize>::new();
            for parent in existing.values().filter_map(|path| path.parent()) {
                *directories.entry(parent).or_default() += 1;
            }
            directories
                .into_iter()
                .max_by(|(a, n), (b, m)| n.cmp(m).then_with(|| b.cmp(a)))
                .map(|(path, _)| path)
        } else {
            None
        };
        let mut used_paths = HashSet::new();
        let mut seen_ids = HashSet::new();
        if repair {
            used_paths.extend(existing.values().cloned());
        }
        Ok(page
            .images
            .iter()
            .filter_map(|image| {
                let image_id = image_ids_by_url.get(&image.source_url)?;
                if repair && !seen_ids.insert(*image_id) {
                    return None;
                }
                let mut request =
                    build_download_request(image, page, page_id, *image_id, &self.strategy);
                let existing_path = existing.get(image_id).filter(|path| {
                    directory.as_ref().is_none_or(|directory| {
                        (path.starts_with(directory)
                            && sections_by_id.get(image_id).is_none_or(|section| {
                                path.starts_with(directory.join("images").join(section))
                            }))
                            || self.base_dir.join(path).is_file()
                    })
                });
                if let Some(path) = existing_path {
                    request.relative_path = path.clone();
                } else if let Some(directory) = &directory {
                    let filename = request.relative_path.file_name()?.to_owned();
                    let mut images = directory.join("images");
                    if let Some(section) = sections_by_id.get(image_id) {
                        images.push(section);
                    }
                    request.relative_path = images.join(filename);
                } else if let Some(path) = knife_paths.get(image_id) {
                    request.relative_path = path.clone();
                } else if let Some(directory) = gallery_directory {
                    request.relative_path = directory.join(request.relative_path.file_name()?);
                }
                if existing_path.is_none() {
                    let has_preview = image
                        .preview_url
                        .as_deref()
                        .is_some_and(|preview| preview != image.source_url);
                    if (repair && used_paths.contains(&request.relative_path))
                        || (has_preview && self.base_dir.join(&request.relative_path).exists())
                    {
                        // A new image can share an older image's filename after reordering.
                        let stem = request.relative_path.file_stem()?;
                        for index in 0.. {
                            let mut name = stem.to_os_string();
                            name.push(format!("_{image_id}"));
                            if index > 0 {
                                name.push(format!("_{index}"));
                            }
                            if let Some(extension) = request.relative_path.extension() {
                                name.push(".");
                                name.push(extension);
                            }
                            let candidate = request.relative_path.with_file_name(name);
                            if !used_paths.contains(&candidate) {
                                request.relative_path = candidate;
                                break;
                            }
                        }
                    }
                }
                if repair {
                    used_paths.insert(request.relative_path.clone());
                }
                Some(request)
            })
            .collect())
    }

    async fn execute_downloads(
        &self,
        requests: Vec<DownloadRequest>,
        max_concurrent_downloads: usize,
        restricted_client: Option<&OriginRestrictedClient>,
        repair: bool,
        permission_issues: &mut Vec<ImageAccessIssue>,
    ) -> AppResult<()> {
        self.ui.begin_image_batch(requests.len());
        let results = stream::iter(requests)
            .map(|request| async move {
                let result = self.download_one(request, restricted_client, repair).await;
                self.ui.finish_image();
                result
            })
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
                    if let Some(permission) = error.downcast_ref::<ImagePermissionRequired>() {
                        permission_issues.push(permission.0.clone());
                        continue;
                    }
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
        repair: bool,
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
        let absolute = self.base_dir.join(&request.relative_path);
        let absolute_str = absolute.to_string_lossy();

        if self.file_store.exists(&absolute_str) {
            use tokio::io::AsyncReadExt;
            let mut file = tokio::fs::File::open(&absolute).await?;
            let mut header = [0; 32];
            let size = file.read(&mut header).await?;
            if image::guess_format(&header[..size]).is_ok() {
                self.ui.print_output_file(&absolute_str);
                self.ui.print_skipped_existing(&absolute_str);
                return Ok(self.prepare_local_artifact(&request, &absolute).await);
            }
            if !repair {
                return Err("existing image file has an invalid image header".into());
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
        let format = image::guess_format(&header)
            .map_err(|_| "image response has an invalid image header")?;
        if format == image::ImageFormat::Gif
            && !request
                .relative_path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
        {
            let mut destination = request.relative_path.with_extension("gif");
            if self.base_dir.join(&destination).try_exists()? {
                let mut stem = destination
                    .file_stem()
                    .ok_or("GIF destination has no filename")?
                    .to_os_string();
                stem.push(format!("_{}.gif", request.image_id));
                destination.set_file_name(stem);
                if self.base_dir.join(&destination).try_exists()? {
                    return Err("GIF destination already exists".into());
                }
            }
            request.relative_path = destination;
        } else if request.relative_path.extension().is_none() {
            request
                .relative_path
                .set_extension(image_extension_from_content_type(&content_type));
        }
        let absolute = self.base_dir.join(&request.relative_path);
        let absolute_str = absolute.to_string_lossy();
        self.ui.print_output_file(&absolute_str);
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

fn take_image_access_issues(page: &mut ScrapedItem) -> Vec<ImageAccessIssue> {
    let mut issues = Vec::new();
    page.attributes.retain(|attribute| {
        if let Some(issue) = ImageAccessIssue::from_metadata(attribute) {
            issues.push(issue);
            return false;
        }
        attribute.kind != ISSUE_KIND
    });
    issues.sort_unstable();
    issues.dedup();
    issues
}

fn take_image_sections(page: &mut ScrapedItem) -> HashMap<String, String> {
    let mut sections = HashMap::new();
    let mut ordinal = 0;
    page.attributes.retain(|attribute| {
        if attribute.kind != "image_section" {
            return true;
        }
        if let Ok(indices) = serde_json::from_str::<Vec<usize>>(&attribute.value) {
            ordinal += 1;
            let name = crate::fs::torrent::safe_component(&attribute.label, "Section", 60);
            let folder = format!("{ordinal:02} - {name}");
            for index in indices {
                if let Some(image) = page.images.get(index) {
                    sections
                        .entry(image.source_url.clone())
                        .or_insert_with(|| folder.clone());
                }
            }
        }
        false
    });
    sections
}

fn image_extension_from_content_type(content_type: &str) -> &'static str {
    match content_type {
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
        assert_eq!(image_extension_from_content_type("image/png"), "png");
        assert_eq!(image_extension_from_content_type("image/webp"), "webp");
        assert_eq!(image_extension_from_content_type(""), "jpg");
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
            .build_download_requests(&page, 11, &image_ids_by_url, &HashMap::new(), false)
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
    async fn knife_profile_downloads_and_repairs_multiple_sources_without_path_collisions() {
        use std::io::Cursor;
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let root = tempfile::tempdir().unwrap();
        let database = Database::new(root.path().join("archive.sqlite3"));
        database.initialize().unwrap();
        let base = root.path().join("downloads");
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn({
            let png = png.clone();
            async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    stream.read(&mut [0; 4096]).await.unwrap();
                    let header = format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", png.len());
                    stream.write_all(header.as_bytes()).await.unwrap();
                    stream.write_all(&png).await.unwrap();
                }
            }
        });
        let product = |path: &str, brand: &str, model: &str| ScrapedItem {
            site_key: format!("fixture-{path}"),
            page_url: format!("https://{path}.example.test/products/1"),
            title: format!("{brand} {model}"),
            raw_html: String::new(),
            images: vec![sample_image(&format!("{origin}/{path}.png"))],
            attributes: [
                ("catalog_profile", "knife"),
                ("manufacturer", brand),
                ("model", model),
                ("steel", "CPM 3V"),
            ]
            .into_iter()
            .map(|(kind, value)| crate::models::MetadataAttribute {
                kind: kind.into(),
                label: kind.into(),
                value: value.into(),
                source_url: None,
                is_female: None,
            })
            .collect(),
        };
        let first = product("first", "Example Forge", "Ridge");
        let second = product("second", "example forge", "ridge");
        let service = |pages: &[ScrapedItem]| {
            let mut registry = ScraperRegistry::new();
            for page in pages {
                registry.register(Box::new(RetryFixtureScraper {
                    page: page.clone(),
                    attachment_calls: Arc::default(),
                    attachment_fails: false,
                }));
            }
            ContentDownloadService::new(
                registry,
                database.clone(),
                FileDownloadStore::new(),
                ConsoleUi::new(),
                StructuredDirectoryStrategy,
                base.clone(),
                root.path().join("thumbs"),
            )
        };
        let downloader = service(&[first.clone(), second.clone()]);
        let config = AppConfig::default();
        for page in [&first, &second] {
            downloader
                .run_for_url(&config, &page.page_url)
                .await
                .unwrap();
        }
        let model_dir = Path::new("knife").join("Example Forge").join("Ridge");
        let first_file = model_dir.join("Example Forge - Ridge - CPM 3V - 001.png");
        let second_file = model_dir.join("Example Forge - Ridge - CPM 3V - 001 (1).png");
        for (page, file) in [(&first, &first_file), (&second, &second_file)] {
            let (page_id, _, _) = database
                .saved_page_location(&page.page_url)
                .unwrap()
                .unwrap();
            assert_eq!(std::fs::read(base.join(file)).unwrap(), png);
            assert_eq!(
                database
                    .knife_image_paths(page_id)
                    .unwrap()
                    .into_values()
                    .collect::<Vec<_>>(),
                [file.clone()]
            );
        }

        let mut refreshed = first.clone();
        refreshed.attributes.clear();
        refreshed
            .images
            .insert(0, sample_image(&format!("{origin}/opaque")));
        std::fs::remove_file(base.join(&first_file)).unwrap();
        service(&[refreshed])
            .repair_page_images(&config, &first.page_url)
            .await
            .unwrap();
        assert_eq!(std::fs::read(base.join(&first_file)).unwrap(), png);
        let restored = model_dir.join("Example Forge - Ridge - CPM 3V - 001 (2).png");
        assert_eq!(std::fs::read(base.join(&restored)).unwrap(), png);

        let connection = rusqlite::Connection::open(root.path().join("archive.sqlite3")).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM knife_models", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM knife_sources", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM page_images WHERE local_file_path IS NOT NULL",
                    [],
                    |row| row.get::<_, usize>(0)
                )
                .unwrap(),
            3
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_release_without_a_torrent_link_uses_the_studio_directory_for_new_files() {
        let root = tempfile::tempdir().unwrap();
        let database = Database::new(root.path().join("archive.sqlite3"));
        database.initialize().unwrap();
        let base = root.path().join("downloads");
        let service = ContentDownloadService::new(
            ScraperRegistry::new(),
            database.clone(),
            FileDownloadStore::new(),
            ConsoleUi::new(),
            StructuredDirectoryStrategy,
            base.clone(),
            root.path().join("thumbs"),
        );
        let mut page = sample_page();
        page.images = vec![
            sample_image("https://example.test/first.png"),
            sample_image("https://example.test/second.png"),
        ];
        for (kind, label, value) in [
            ("actor", "Actor", "Example Actor"),
            ("studio", "Studio", "First Studio"),
            ("studio", "Studio", "Second Studio"),
            (
                "notice",
                crate::models::TORRENT_UNAVAILABLE_NOTICE,
                "Sign in to access the torrent link.",
            ),
        ] {
            page.attributes.push(crate::models::MetadataAttribute {
                kind: kind.into(),
                label: label.into(),
                value: value.into(),
                source_url: None,
                is_female: None,
            });
        }
        let persisted = database.upsert_scraped_item(&page).unwrap();
        let card = database.torrent_card(persisted.page_id).unwrap();
        let studio_dir = PathBuf::from(card.directory.unwrap());
        assert_eq!(card.primary_studio, "First Studio");
        assert!(studio_dir
            .components()
            .any(|part| part.as_os_str() == "First Studio"));
        super::super::attachments::save_description(persisted.page_id, &database, &base)
            .await
            .unwrap();
        assert!(base.join(&studio_dir).join("description.txt").is_file());
        let make_requests = || {
            service
                .build_download_requests(
                    &page,
                    persisted.page_id,
                    &persisted.image_ids_by_url,
                    &HashMap::new(),
                    false,
                )
                .unwrap()
        };
        assert!(make_requests()
            .iter()
            .all(|r| r.relative_path.starts_with(studio_dir.join("images"))));

        let existing = PathBuf::from("example-source/example_actor/release/first.png");
        let missing = PathBuf::from("example-source/example_actor/release/second.png");
        std::fs::create_dir_all(base.join(existing.parent().unwrap())).unwrap();
        std::fs::write(base.join(&existing), b"saved image").unwrap();
        for (image, path) in page.images.iter().zip([&existing, &missing]) {
            database
                .update_image_file_location(
                    persisted.page_id,
                    persisted.image_ids_by_url[&image.source_url],
                    &path.to_string_lossy(),
                )
                .unwrap();
        }
        let requests = make_requests();
        assert_eq!(requests[0].relative_path, existing);
        assert!(requests[1]
            .relative_path
            .starts_with(studio_dir.join("images")));
        assert!(!base.join(missing).exists());

        page.attributes.push(crate::models::MetadataAttribute {
            kind: "image_section".into(),
            label: "../Scene: two".into(),
            value: "[1,99]".into(),
            source_url: None,
            is_female: None,
        });
        let sections = take_image_sections(&mut page);
        assert_eq!(sections.len(), 1);
        assert!(!page.attributes.iter().any(|a| a.kind == "image_section"));
        let second = &page.images[1].source_url;
        let requests = service
            .build_download_requests(
                &page,
                persisted.page_id,
                &persisted.image_ids_by_url,
                &HashMap::from([(persisted.image_ids_by_url[second], sections[second].clone())]),
                false,
            )
            .unwrap();
        assert_eq!(requests[0].relative_path, existing);
        assert!(requests[1]
            .relative_path
            .starts_with(studio_dir.join("images").join("01 - _Scene_ two")));
    }

    #[tokio::test]
    async fn downloads_animated_gifs_with_their_original_bytes_and_extension() {
        use image::{AnimationDecoder, Frame, Rgba, RgbaImage};
        use std::io::{BufReader, Cursor};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            for color in [Rgba([255, 0, 0, 255]), Rgba([0, 255, 0, 255])] {
                encoder
                    .encode_frame(Frame::new(RgbaImage::from_pixel(4, 4, color)))
                    .unwrap();
            }
        }
        for (name, mime) in [
            ("direct.gif", "image/gif"),
            ("opaque", "application/octet-stream"),
            ("preview.jpg", "image/gif"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let bytes = gif.clone();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                socket.read(&mut [0; 4096]).await.unwrap();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(&bytes).await.unwrap();
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
                image_url: format!("http://{address}/{name}"),
                image_id: 1,
                page_id: 1,
                relative_path: Path::new("scene").join(name),
            };
            let result = service.download_one(request, None, false).await.unwrap();
            let path = PathBuf::from(result.file_path);
            assert_eq!(path.extension().unwrap(), "gif");
            let saved = std::fs::read(service.base_dir.join(path)).unwrap();
            assert_eq!(saved, gif);
            let decoder =
                image::codecs::gif::GifDecoder::new(BufReader::new(Cursor::new(saved))).unwrap();
            assert_eq!(decoder.into_frames().collect_frames().unwrap().len(), 2);
            assert!(Path::new(&result.thumbnail_path.unwrap()).is_file());
            server.await.unwrap();
        }
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
                || crate::db::collection_url_alternate(&self.page.page_url).as_deref() == Some(url)
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
            url: &'a str,
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
                if url.ends_with(".torrent") {
                    if self.attachment_fails {
                        return Err("HTTP 429 Too Many Requests".into());
                    }
                    return Ok(crate::models::AttachmentData {
                        bytes: b"d4:infod6:lengthi1e4:name4:demoee".to_vec(),
                        content_type: "application/x-bittorrent".into(),
                    });
                }
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
    async fn repairs_a_legacy_gallery_in_its_saved_folder() {
        use std::io::Cursor;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let root = tempfile::tempdir().unwrap();
        let database = Database::new(root.path().join("archive.db"));
        database.initialize().unwrap();
        let base = root.path().join("downloads");
        let folder = Path::new("my library/edited collection");
        std::fs::create_dir_all(base.join(folder)).unwrap();
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let downloads = Arc::new(Mutex::new(Vec::new()));
        let server = tokio::spawn({
            let downloads = downloads.clone();
            let png = png.clone();
            async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let mut request = [0; 4096];
                    let len = stream.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..len]);
                    if let Some(path) = request.split_whitespace().nth(1) {
                        downloads.lock().unwrap().push(path.to_owned());
                    }
                    let header = format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", png.len());
                    stream.write_all(header.as_bytes()).await.unwrap();
                    stream.write_all(&png).await.unwrap();
                }
            }
        });

        let mut saved = sample_page();
        saved.page_url.pop();
        let image_at = |path: &str, order, name: Option<&str>| {
            let mut image = sample_image(&format!("{origin}/{path}"));
            image.sort_order = order;
            image.local_file_path =
                name.map(|name| folder.join(name).to_string_lossy().into_owned());
            image
        };
        saved.images = vec![
            image_at("old/picture.png", 0, Some("001_picture.png")),
            image_at("broken.png", 1, Some("002_broken.png")),
            image_at("omitted.png", 2, None),
        ];
        let old_file = base.join(folder).join("001_picture.png");
        std::fs::write(&old_file, &png).unwrap();
        std::fs::write(base.join(folder).join("002_broken.png"), b"broken").unwrap();
        let persisted = database.upsert_scraped_item(&saved).unwrap();
        assert!(database
            .torrent_directory(persisted.page_id)
            .unwrap()
            .is_none());

        let mut fresh = saved.clone();
        fresh.page_url.push('/');
        fresh.title = "Renamed on the website".into();
        fresh.images.pop();
        let added_image = image_at("new/picture.png", 0, None);
        fresh.images.insert(0, added_image.clone());
        fresh.images.push(added_image);
        let mut registry = ScraperRegistry::new();
        registry.register(Box::new(RetryFixtureScraper {
            page: fresh,
            attachment_calls: Default::default(),
            attachment_fails: false,
        }));
        let service = ContentDownloadService::new(
            registry,
            database.clone(),
            FileDownloadStore::new(),
            ConsoleUi::new(),
            StructuredDirectoryStrategy,
            base.clone(),
            root.path().join("thumbs"),
        );
        service
            .repair_page_images(&AppConfig::default(), &saved.page_url)
            .await
            .unwrap();
        service
            .repair_page_images(&AppConfig::default(), &saved.page_url)
            .await
            .unwrap();
        let mut fetched = downloads.lock().unwrap().clone();
        fetched.sort();
        assert_eq!(fetched, ["/broken.png", "/new/picture.png", "/omitted.png"]);
        assert_eq!(std::fs::read(old_file).unwrap(), png);
        let images = database.page_repair_images(persisted.page_id).unwrap();
        assert_eq!(images.len(), 4);
        let paths = images
            .iter()
            .map(|(_, image)| {
                let path = PathBuf::from(image.local_file_path.as_ref().unwrap());
                assert_eq!(path.parent(), Some(folder));
                assert_eq!(std::fs::read(base.join(&path)).unwrap(), png);
                path
            })
            .collect::<HashSet<_>>();
        assert_eq!(paths.len(), 4);
        let connection = rusqlite::Connection::open(root.path().join("archive.db")).unwrap();
        let title: String = connection
            .query_row(
                "SELECT title FROM source_pages WHERE id=?1",
                [persisted.page_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, saved.title);
        assert_eq!(
            database.page_image_paths(persisted.page_id).unwrap().len(),
            4
        );
        assert_eq!(
            database
                .saved_page_location(&format!("{}/", saved.page_url))
                .unwrap(),
            Some((persisted.page_id, false, saved.page_url.clone()))
        );
        server.abort();
    }

    #[tokio::test]
    async fn torrent_file_only_repairs_the_attachment_and_leaves_other_assets_alone() {
        use crate::models::{MetadataAttribute, TorrentQuery, TORRENT_UNAVAILABLE_NOTICE};
        use crate::services::url_queue::download_failure_kind;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("downloads");
        std::fs::create_dir_all(&base).unwrap();
        let database = Database::new(root.path().join("library.db"));
        database.initialize().unwrap();
        let attribute =
            |kind: &str, label: &str, value: &str, source: Option<&str>| MetadataAttribute {
                kind: kind.into(),
                label: label.into(),
                value: value.into(),
                source_url: source.map(str::to_owned),
                is_female: None,
            };
        let mut saved = sample_page();
        saved.title = "Keep the saved title".into();
        saved.attributes = vec![attribute(
            "notice",
            TORRENT_UNAVAILABLE_NOTICE,
            "Sign in",
            None,
        )];
        let page_id = database.upsert_scraped_item(&saved).unwrap().page_id;
        let card = database.torrent_card(page_id).unwrap();
        let directory = Path::new(card.directory.as_ref().unwrap());
        let relative = directory.join("release.TORRENT");
        let magnet = directory.join("release.magnet.txt");
        std::fs::create_dir_all(base.join(directory)).unwrap();
        std::fs::write(base.join(&magnet), b"existing magnet").unwrap();
        database
            .save_page_attachment(
                page_id,
                "magnet:fixture",
                "Magnet",
                &magnet.to_string_lossy(),
            )
            .unwrap();

        let query = TorrentQuery {
            missing_torrent: true,
            ..Default::default()
        };
        let count = || {
            database
                .search_torrent_summaries_with_storage(&query, Some(&base), None)
                .unwrap()
                .total
        };
        assert_eq!(count(), 1);
        let torrent = "https://example.test/release.torrent";
        database
            .save_page_attachment(
                page_id,
                torrent,
                "release.TORRENT",
                &relative.to_string_lossy(),
            )
            .unwrap();
        assert_eq!(count(), 1);
        std::fs::write(base.join(&relative), []).unwrap();
        assert_eq!(count(), 1);

        let mut remote = saved.clone();
        remote.title = "Changed remote title".into();
        remote
            .images
            .push(sample_image("https://example.invalid/image.png"));
        remote.attributes = vec![
            attribute(
                "attachment",
                "sidecar.txt",
                "Sidecar",
                Some("https://example.test/sidecar.txt"),
            ),
            attribute("attachment", "release.TORRENT", "Download", Some(torrent)),
        ];
        let calls = Arc::new(AtomicUsize::new(0));
        let make_service = |attachment_fails| {
            let mut registry = ScraperRegistry::new();
            registry.register(Box::new(RetryFixtureScraper {
                page: remote.clone(),
                attachment_calls: calls.clone(),
                attachment_fails,
            }));
            ContentDownloadService::new(
                registry,
                database.clone(),
                FileDownloadStore::new(),
                ConsoleUi::new(),
                StructuredDirectoryStrategy,
                base.clone(),
                root.path().join("thumbs"),
            )
        };
        let config = AppConfig::default();
        let error = make_service(true)
            .download_torrent_file(&config, &saved.page_url)
            .await
            .unwrap_err();
        assert_eq!(
            download_failure_kind(error.as_ref()),
            crate::models::QueueFailureKind::RateLimit
        );
        assert_eq!(count(), 1);
        let service = make_service(false);
        service
            .download_torrent_file(&config, &saved.page_url)
            .await
            .unwrap();
        service
            .download_torrent_file(&config, &saved.page_url)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(count(), 0);
        assert_eq!(
            std::fs::read(base.join(&relative)).unwrap(),
            b"d4:infod6:lengthi1e4:name4:demoee"
        );
        assert_eq!(
            std::fs::read(base.join(&magnet)).unwrap(),
            b"existing magnet"
        );
        let after = database.torrent_card(page_id).unwrap();
        assert_eq!(after.title, saved.title);
        assert!(after.images.is_empty());
        assert_eq!(after.attachments.len(), 2);
        assert!(!after
            .attributes
            .iter()
            .any(|a| a.label == TORRENT_UNAVAILABLE_NOTICE));
        assert!(after.revision > card.revision);
        assert_eq!(std::fs::read_dir(base.join(directory)).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn quota_failure_saves_other_files_and_can_retry_after_a_restart() {
        use crate::models::{MetadataAttribute, QueueFailureKind, TORRENT_UNAVAILABLE_NOTICE};
        use crate::services::url_queue::{download_failure_kind, QueueJob, UrlQueueService};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let root = tempfile::tempdir().unwrap();
        let database = Database::new(root.path().join("library.db"));
        let base = root.path().join("downloads");
        let mut page = sample_page();
        let attribute =
            |kind: &str, label: &str, value: &str, source: Option<&str>| MetadataAttribute {
                kind: kind.into(),
                label: label.into(),
                value: value.into(),
                source_url: source.map(str::to_owned),
                is_female: None,
            };
        let torrent_url = "https://example.test/release.torrent";
        let torrent = attribute(
            "attachment",
            "release.torrent",
            torrent_url,
            Some(torrent_url),
        );
        let magnet = "magnet:?xt=urn:btih:0123456789012345678901234567890123456789";
        page.attributes = vec![torrent.clone(), attribute("magnet", "Magnet", magnet, None)];
        let url = page.page_url.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let service = |page, attachment_fails| {
            let mut registry = ScraperRegistry::new();
            registry.register(Box::new(RetryFixtureScraper {
                page,
                attachment_calls: calls.clone(),
                attachment_fails,
            }));
            ContentDownloadService::new(
                registry,
                database.clone(),
                FileDownloadStore::new(),
                ConsoleUi::new(),
                StructuredDirectoryStrategy,
                base.clone(),
                root.path().join("thumbs"),
            )
        };
        let open_queue = || {
            UrlQueueService::with_registry_and_queue_path(
                ScraperRegistry::with_public_scrapers(),
                root.path().join("queue.sqlite3"),
            )
        };
        let config = AppConfig::default();
        let error = service(page.clone(), true)
            .run_for_url(&config, &url)
            .await
            .unwrap_err();
        assert_eq!(
            download_failure_kind(error.as_ref()),
            QueueFailureKind::RateLimit
        );
        let queue = open_queue();
        queue.mark_failed_with_kind(&url, &error.to_string(), QueueFailureKind::RateLimit);
        drop(queue);
        let mut card = database
            .search_torrents(&Default::default())
            .unwrap()
            .cards
            .remove(0);
        assert_eq!(card.attachments.len(), 2);
        assert!(card
            .attachments
            .iter()
            .all(|file| base.join(&file.local_file_path).is_file()));
        assert!(card
            .attachments
            .iter()
            .any(|file| file.filename == "Magnet"));

        let notice = attribute(
            "notice",
            TORRENT_UNAVAILABLE_NOTICE,
            "Link temporarily hidden",
            None,
        );
        let mut hidden = page.clone();
        hidden.attributes[0] = notice;
        let error = service(hidden.clone(), true)
            .run_for_url(&config, &url)
            .await
            .unwrap_err();
        assert_eq!(
            download_failure_kind(error.as_ref()),
            QueueFailureKind::Attachment
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let queue = open_queue();
        assert!(queue.retry_failure(&url).unwrap());
        let (job, mode) = queue.take_next_job().unwrap();
        assert_eq!(mode, QueueJob::Full);
        service(page, false)
            .run_for_url(&config, &job)
            .await
            .unwrap();
        queue.mark_finished(&job);
        assert_eq!(queue.snapshot_light().failed_total, 0);
        card = database.torrent_card(card.page_id).unwrap();
        let saved = card
            .attachments
            .iter()
            .find(|file| file.filename == "release.torrent")
            .unwrap();
        assert!(base.join(&saved.local_file_path).is_file());
        service(hidden, true)
            .run_for_url(&config, &url)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
                scraper.page.attributes = vec![crate::models::MetadataAttribute {
                    kind: "image_section".into(),
                    label: "Scene 1".into(),
                    value: "[0]".into(),
                    source_url: None,
                    is_female: None,
                }];
                scraper.page.images.pop();
                let ready_path = folder.join("images").join("001_ready.png");
                let ready_file = service.base_dir.join(&ready_path);
                let ready_bytes = std::fs::read(&ready_file).unwrap();
                service.scraper_registry.register(Box::new(scraper));
                let result = service.repair_page_images(&config, &page.page_url).await;
                let card = service.database.torrent_card(page_id).unwrap();
                assert_eq!(card.title, page.title);
                assert_eq!(saved_attachments.len(), 2);
                assert_eq!(read_attachments(), saved_attachments);
                assert_eq!(attachment_calls.load(Ordering::SeqCst), 1);
                let grouped = folder
                    .join("images")
                    .join("01 - Scene 1")
                    .join("001_ready.png");
                assert!(!ready_file.exists());
                assert_eq!(
                    std::fs::read(service.base_dir.join(&grouped)).unwrap(),
                    ready_bytes
                );
                assert!(card
                    .images
                    .iter()
                    .any(|image| { image.local_file_path.as_deref() == grouped.to_str() }));
                assert!(service.database.torrent_journals().unwrap().is_empty());
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

        let progress = crate::ui::QueueProgress::default();
        let service = ContentDownloadService::new(
            ScraperRegistry::with_public_scrapers(),
            database,
            FileDownloadStore::new(),
            ConsoleUi::with_queue_progress(crate::ui::UiLog::default(), progress.clone()),
            StructuredDirectoryStrategy,
            base_dir,
            thumb_dir,
        );
        service
            .execute_downloads(requests, 2, None, false, &mut Vec::new())
            .await
            .expect("finalize existing files");
        let progress = progress.snapshot();
        assert_eq!(progress.images_total, 2);
        assert_eq!(progress.images_processed, 2);

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

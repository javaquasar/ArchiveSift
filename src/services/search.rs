use std::path::Path;

use crate::db::{Database, QueryCancellation};
use crate::error::SearchServiceError;
use crate::models::{DeleteMode, SearchCursor, SearchImageResult, SearchQuery, SearchResultPage};
use crate::AppResult;

pub struct SearchService {
    database: Database,
}

impl SearchService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn search_images(&self, query: &SearchQuery) -> AppResult<Vec<SearchImageResult>> {
        self.ensure_database_initialized()?;
        self.database
            .search_images(query)
            .map_err(|source| self.query_failed("search_images", source))
    }

    pub fn search_images_page(
        &self,
        query: &SearchQuery,
        cursor: Option<&SearchCursor>,
        page_size: usize,
    ) -> AppResult<SearchResultPage> {
        self.ensure_database_initialized()?;
        self.database
            .search_images_page(query, cursor, page_size)
            .map_err(|source| self.query_failed("search_images_page", source))
    }

    pub fn search_images_page_lightweight(
        &self,
        query: &SearchQuery,
        cursor: Option<&SearchCursor>,
        page_size: usize,
        cancellation: &QueryCancellation,
    ) -> AppResult<SearchResultPage> {
        self.ensure_database_initialized()?;
        self.database
            .search_images_page_lightweight(query, cursor, page_size, cancellation)
            .map_err(|source| self.query_failed("search_images_page_lightweight", source))
    }

    pub fn load_images_by_ids(&self, image_ids: &[i64]) -> AppResult<Vec<SearchImageResult>> {
        self.ensure_database_initialized()?;
        self.database
            .search_images_by_ids(image_ids)
            .map_err(|source| self.query_failed("load_images_by_ids", source))
    }

    pub fn search_recent_pages(&self, limit: usize) -> AppResult<Vec<SearchImageResult>> {
        self.ensure_database_initialized()?;
        self.database
            .search_images_recent_pages(limit)
            .map_err(|source| self.query_failed("search_recent_pages", source))
    }

    pub fn search_recent_pages_cancellable(
        &self,
        limit: usize,
        cancellation: &QueryCancellation,
    ) -> AppResult<Vec<SearchImageResult>> {
        self.ensure_database_initialized()?;
        self.database
            .search_images_recent_pages_cancellable(limit, cancellation)
            .map_err(|source| self.query_failed("search_recent_pages_cancellable", source))
    }

    pub fn search_recent_pages_page_lightweight(
        &self,
        page_limit: usize,
        cursor: Option<&SearchCursor>,
        page_size: usize,
        cancellation: &QueryCancellation,
    ) -> AppResult<SearchResultPage> {
        self.ensure_database_initialized()?;
        self.database
            .search_images_recent_pages_page_lightweight(
                page_limit,
                cursor,
                page_size,
                cancellation,
            )
            .map_err(|source| self.query_failed("search_recent_pages_page_lightweight", source))
    }

    pub fn suggest_tags(&self, query: &str) -> AppResult<Vec<String>> {
        self.ensure_database_initialized()?;
        self.database
            .suggest_attribute_values("tag", query, 8)
            .map_err(|source| self.query_failed("suggest_tags", source))
    }

    pub fn suggest_actors(&self, query: &str) -> AppResult<Vec<String>> {
        self.ensure_database_initialized()?;
        self.database
            .suggest_attribute_values("actor", query, 8)
            .map_err(|source| self.query_failed("suggest_actors", source))
    }

    pub fn suggest_categories(&self, query: &str) -> AppResult<Vec<String>> {
        self.ensure_database_initialized()?;
        self.database
            .suggest_attribute_values("category", query, 8)
            .map_err(|source| self.query_failed("suggest_categories", source))
    }

    pub fn suggest_studios(&self, query: &str) -> AppResult<Vec<String>> {
        self.ensure_database_initialized()?;
        self.database
            .suggest_attribute_values("studio", query, 8)
            .map_err(|source| self.query_failed("suggest_studios", source))
    }

    pub fn add_tags_to_images(&self, image_ids: &[i64], values: &[String]) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .add_image_attributes(image_ids, "tag", "Tag", values)
            .map_err(|source| self.query_failed("add_tags_to_images", source))
    }

    pub fn add_categories_to_images(&self, image_ids: &[i64], values: &[String]) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .add_image_attributes(image_ids, "category", "Category", values)
            .map_err(|source| self.query_failed("add_categories_to_images", source))
    }

    pub fn add_actors_to_images(&self, image_ids: &[i64], values: &[String]) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .add_image_attributes(image_ids, "actor", "Actor", values)
            .map_err(|source| self.query_failed("add_actors_to_images", source))
    }

    pub fn add_studios_to_images(&self, image_ids: &[i64], values: &[String]) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .add_image_attributes(image_ids, "studio", "Studio", values)
            .map_err(|source| self.query_failed("add_studios_to_images", source))
    }

    pub fn remove_tags_from_images(&self, image_ids: &[i64], values: &[String]) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .remove_image_attributes(image_ids, "tag", values)
            .map_err(|source| self.query_failed("remove_tags_from_images", source))
    }

    pub fn remove_categories_from_images(
        &self,
        image_ids: &[i64],
        values: &[String],
    ) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .remove_image_attributes(image_ids, "category", values)
            .map_err(|source| self.query_failed("remove_categories_from_images", source))
    }

    pub fn remove_actors_from_images(&self, image_ids: &[i64], values: &[String]) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .remove_image_attributes(image_ids, "actor", values)
            .map_err(|source| self.query_failed("remove_actors_from_images", source))
    }

    pub fn remove_studios_from_images(
        &self,
        image_ids: &[i64],
        values: &[String],
    ) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .remove_image_attributes(image_ids, "studio", values)
            .map_err(|source| self.query_failed("remove_studios_from_images", source))
    }

    pub fn toggle_favorite(&self, image_id: i64, is_favorite: bool) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .set_favorite(image_id, is_favorite)
            .map_err(|source| self.query_failed("toggle_favorite", source))
    }

    pub fn set_actor_gender_only(
        &self,
        actor_name: &str,
        is_female: Option<bool>,
    ) -> AppResult<()> {
        self.ensure_database_initialized()?;
        self.database
            .set_actor_gender(actor_name, is_female)
            .map_err(|source| self.query_failed("set_actor_gender_only", source))
    }

    pub fn delete_images_only(
        &self,
        image_ids: &[i64],
        mode: DeleteMode,
        base_dir: &str,
    ) -> AppResult<()> {
        self.ensure_database_initialized()?;

        if mode == DeleteMode::DbAndDisk {
            let paths = self
                .database
                .get_image_local_paths(image_ids)
                .map_err(|source| self.query_failed("load_image_local_paths", source))?;
            for (_image_id, stored_path) in paths {
                let full_path = resolve_path(base_dir, &stored_path);
                if full_path.exists() {
                    std::fs::remove_file(&full_path).map_err(
                        |source| -> Box<dyn std::error::Error + Send + Sync> {
                            Box::new(SearchServiceError::DeleteLocalFile {
                                path: full_path.clone(),
                                source,
                            })
                        },
                    )?;
                }
            }
        }

        self.database
            .delete_images(image_ids)
            .map_err(|source| self.query_failed("delete_images_only", source))
    }

    fn ensure_database_initialized(&self) -> AppResult<()> {
        self.database
            .initialize()
            .map_err(|source| Box::new(SearchServiceError::InitializeDatabase { source }) as _)
    }

    fn query_failed(
        &self,
        operation: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        Box::new(SearchServiceError::QueryFailed { operation, source })
    }
}

fn resolve_path(base_dir: &str, stored: &str) -> std::path::PathBuf {
    let p = Path::new(stored);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(base_dir).join(p)
    }
}

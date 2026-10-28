use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use futures::{Stream, StreamExt};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
use tokio::fs as tokio_fs;
use tokio::io::AsyncWriteExt;

use crate::error::{DatabaseError, FileStoreError};
use crate::models::{
    DateBrowseDirection, DateGroup, DateGroupCursor, DateGroupPage, MetadataAttribute,
    ScrapedImage, ScrapedItem, SearchCursor, SearchImageResult, SearchPageSummary, SearchQuery,
    SearchResultPage,
};
use crate::AppResult;

pub struct FileDownloadStore;

impl FileDownloadStore {
    pub fn new() -> Self {
        Self
    }

    pub fn exists(&self, file_name: &str) -> bool {
        Path::new(file_name).exists()
    }

    pub async fn write_bytes(&self, file_name: &str, bytes: &[u8]) -> AppResult<()> {
        let chunks =
            futures::stream::once(async move { Ok::<&[u8], std::convert::Infallible>(bytes) });
        self.write_stream_atomic(file_name, chunks).await
    }

    /// Streams a download into a temporary sibling file and renames it only
    /// after the complete body has been flushed successfully.
    pub async fn write_stream_atomic<S, B, E>(&self, file_name: &str, stream: S) -> AppResult<()>
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let path = Path::new(file_name);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio_fs::create_dir_all(parent).await.map_err(|source| {
                    FileStoreError::CreateParentDir {
                        path: parent.to_path_buf(),
                        source,
                    }
                })?;
            }
        }

        let temp_path = partial_download_path(path);
        let mut file = tokio_fs::File::create(&temp_path).await.map_err(|source| {
            FileStoreError::WriteFile {
                path: temp_path.clone(),
                source,
            }
        })?;
        let mut stream = Box::pin(stream);

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(source) => {
                    drop(file);
                    remove_partial_file(&temp_path).await;
                    return Err(FileStoreError::StreamFile {
                        path: temp_path,
                        source: Box::new(source),
                    }
                    .into());
                }
            };

            if let Err(source) = file.write_all(chunk.as_ref()).await {
                drop(file);
                remove_partial_file(&temp_path).await;
                return Err(FileStoreError::WriteFile {
                    path: temp_path,
                    source,
                }
                .into());
            }
        }

        if let Err(source) = file.flush().await {
            drop(file);
            remove_partial_file(&temp_path).await;
            return Err(FileStoreError::WriteFile {
                path: temp_path,
                source,
            }
            .into());
        }
        drop(file);

        if let Err(source) = tokio_fs::rename(&temp_path, path).await {
            remove_partial_file(&temp_path).await;
            return Err(FileStoreError::ReplaceFile {
                from: temp_path,
                to: path.to_path_buf(),
                source,
            }
            .into());
        }

        Ok(())
    }
}

fn partial_download_path(path: &Path) -> PathBuf {
    static NEXT_PARTIAL_ID: AtomicU64 = AtomicU64::new(0);

    let id = NEXT_PARTIAL_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    path.with_file_name(format!(".{file_name}.part-{}-{id}", std::process::id()))
}

async fn remove_partial_file(path: &Path) {
    let _ = tokio_fs::remove_file(path).await;
}

#[cfg(test)]
mod file_store_tests {
    use super::*;

    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!("archivesift_file_store_{label}_{nanos}"))
    }

    #[tokio::test]
    async fn streamed_download_is_finalized_only_after_all_chunks_arrive() {
        let root = unique_test_root("success");
        let target = root.join("nested").join("image.bin");
        let chunks = futures::stream::iter(vec![
            Ok::<_, io::Error>(b"hello ".to_vec()),
            Ok::<_, io::Error>(b"world".to_vec()),
        ]);

        FileDownloadStore::new()
            .write_stream_atomic(&target.to_string_lossy(), chunks)
            .await
            .expect("streamed write should succeed");

        assert_eq!(
            std::fs::read(&target).expect("target should exist"),
            b"hello world"
        );
        let sibling_names = std::fs::read_dir(target.parent().expect("target parent"))
            .expect("read target parent")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(sibling_names, vec![std::ffi::OsString::from("image.bin")]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn byte_writes_use_the_same_atomic_finalization_path() {
        let root = unique_test_root("bytes");
        let target = root.join("image.bin");

        FileDownloadStore::new()
            .write_bytes(&target.to_string_lossy(), b"complete body")
            .await
            .expect("byte write should succeed");

        assert_eq!(
            std::fs::read(&target).expect("finalized target should exist"),
            b"complete body"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_stream_removes_partial_file_and_preserves_existing_target() {
        let root = unique_test_root("failure");
        std::fs::create_dir_all(&root).expect("create test root");
        let target = root.join("image.bin");
        std::fs::write(&target, b"existing").expect("seed target");
        let chunks = futures::stream::iter(vec![
            Ok::<_, io::Error>(b"replacement".to_vec()),
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "stream failed",
            )),
        ]);

        let error = FileDownloadStore::new()
            .write_stream_atomic(&target.to_string_lossy(), chunks)
            .await
            .expect_err("stream failure should be reported");

        assert!(error.to_string().contains("stream failed"));
        assert_eq!(
            std::fs::read(&target).expect("existing target should remain"),
            b"existing"
        );
        let sibling_names = std::fs::read_dir(&root)
            .expect("read test root")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(sibling_names, vec![std::ffi::OsString::from("image.bin")]);

        let _ = std::fs::remove_dir_all(root);
    }
}

const READ_POOL_SIZE: usize = 3;
const MAX_CACHED_DATABASE_RUNTIMES: usize = 4;
const READER_CACHE_KIB: i64 = 8 * 1024;
const WRITER_CACHE_KIB: i64 = 16 * 1024;
const SQLITE_ID_BATCH_SIZE: usize = 400;
const SQLITE_ATTRIBUTE_VALUE_BATCH_SIZE: usize = 100;

#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    runtime: Arc<DatabaseRuntime>,
}

struct DatabaseRuntime {
    path: PathBuf,
    readers: Arc<ReadConnectionPool>,
    writer: Mutex<Option<Connection>>,
}

struct ReadConnectionPool {
    path: PathBuf,
    max_connections: usize,
    state: Mutex<ReadPoolState>,
    available: Condvar,
}

#[derive(Default)]
struct ReadPoolState {
    idle: Vec<Connection>,
    total: usize,
}

struct PooledReadConnection {
    connection: Option<Connection>,
    pool: Arc<ReadConnectionPool>,
}

struct WriterConnection<'a> {
    guard: MutexGuard<'a, Option<Connection>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageLocalPathUpdate {
    pub image_id: i64,
    pub new_path: String,
}

pub struct PersistedItem {
    pub page_id: i64,
    pub image_ids_by_url: HashMap<String, i64>,
}

#[derive(Clone, Default)]
pub struct QueryCancellation {
    state: Arc<Mutex<QueryCancellationState>>,
}

#[derive(Default)]
struct QueryCancellationState {
    cancelled: bool,
    interrupt: Option<rusqlite::InterruptHandle>,
}

impl QueryCancellation {
    pub fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.cancelled = true;
        if let Some(interrupt) = &state.interrupt {
            interrupt.interrupt();
        }
    }

    fn register(&self, connection: &Connection) -> AppResult<QueryCancellationGuard> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.cancelled {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "database query was superseded",
            )
            .into());
        }
        state.interrupt = Some(connection.get_interrupt_handle());
        Ok(QueryCancellationGuard {
            cancellation: self.clone(),
        })
    }
}

struct QueryCancellationGuard {
    cancellation: QueryCancellation,
}

impl Drop for QueryCancellationGuard {
    fn drop(&mut self) {
        let mut state = self
            .cancellation
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.interrupt = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageArtifactUpdate {
    pub page_id: i64,
    pub image_id: i64,
    pub file_path: String,
    pub thumbnail_path: Option<String>,
}

fn initialized_databases() -> &'static Mutex<HashSet<PathBuf>> {
    static INITIALIZED_DATABASES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    INITIALIZED_DATABASES.get_or_init(|| Mutex::new(HashSet::new()))
}

fn database_runtimes() -> &'static Mutex<HashMap<PathBuf, Arc<DatabaseRuntime>>> {
    static DATABASE_RUNTIMES: OnceLock<Mutex<HashMap<PathBuf, Arc<DatabaseRuntime>>>> =
        OnceLock::new();
    DATABASE_RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn database_identity(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[derive(Clone, Copy)]
enum ConnectionRole {
    Reader,
    Writer,
}

fn open_configured_connection(path: &Path, role: ConnectionRole) -> AppResult<Connection> {
    let connection = Connection::open(path).map_err(|source| DatabaseError::OpenConnection {
        path: path.to_path_buf(),
        source,
    })?;
    let cache_kib = match role {
        ConnectionRole::Reader => READER_CACHE_KIB,
        ConnectionRole::Writer => WRITER_CACHE_KIB,
    };
    connection
        .execute_batch(&format!(
            r#"
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -{cache_kib};
            PRAGMA temp_store = FILE;
            "#,
        ))
        .map_err(|source| DatabaseError::ConfigureConnection {
            path: path.to_path_buf(),
            source,
        })?;
    connection.set_prepared_statement_cache_capacity(64);
    Ok(connection)
}

impl ReadConnectionPool {
    fn new(path: PathBuf, max_connections: usize) -> Self {
        Self {
            path,
            max_connections: max_connections.max(1),
            state: Mutex::new(ReadPoolState::default()),
            available: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> AppResult<PooledReadConnection> {
        loop {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(connection) = state.idle.pop() {
                return Ok(PooledReadConnection {
                    connection: Some(connection),
                    pool: Arc::clone(self),
                });
            }
            if state.total < self.max_connections {
                state.total += 1;
                drop(state);
                match open_configured_connection(&self.path, ConnectionRole::Reader) {
                    Ok(connection) => {
                        return Ok(PooledReadConnection {
                            connection: Some(connection),
                            pool: Arc::clone(self),
                        });
                    }
                    Err(error) => {
                        let mut state = self
                            .state
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        state.total = state.total.saturating_sub(1);
                        self.available.notify_one();
                        return Err(error);
                    }
                }
            }
            let _state = self
                .available
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }
}

impl Deref for PooledReadConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection.as_ref().expect("pooled connection missing")
    }
}

impl DerefMut for PooledReadConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_mut().expect("pooled connection missing")
    }
}

impl Drop for PooledReadConnection {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let mut state = self
                .pool
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.idle.push(connection);
            self.pool.available.notify_one();
        }
    }
}

impl Deref for WriterConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().expect("writer connection missing")
    }
}

impl DerefMut for WriterConnection<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().expect("writer connection missing")
    }
}

impl Database {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = database_identity(&path.into());
        let runtime = {
            let mut runtimes = database_runtimes()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(runtime) = runtimes.get(&path) {
                Arc::clone(runtime)
            } else {
                while runtimes.len() >= MAX_CACHED_DATABASE_RUNTIMES {
                    let Some(inactive_path) = runtimes.iter().find_map(|(path, runtime)| {
                        (Arc::strong_count(runtime) == 1).then(|| path.clone())
                    }) else {
                        break;
                    };
                    runtimes.remove(&inactive_path);
                }
                let runtime = Arc::new(DatabaseRuntime {
                    path: path.clone(),
                    readers: Arc::new(ReadConnectionPool::new(path.clone(), READ_POOL_SIZE)),
                    writer: Mutex::new(None),
                });
                runtimes.insert(path.clone(), Arc::clone(&runtime));
                runtime
            }
        };
        Self { path, runtime }
    }

    fn connect(&self) -> AppResult<PooledReadConnection> {
        self.runtime.readers.acquire()
    }

    fn writer_connection(&self) -> AppResult<WriterConnection<'_>> {
        let mut guard = self
            .runtime
            .writer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if guard.is_none() {
            *guard = Some(open_configured_connection(
                &self.runtime.path,
                ConnectionRole::Writer,
            )?);
        }
        Ok(WriterConnection { guard })
    }

    pub fn initialize(&self) -> AppResult<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|source| DatabaseError::CreateDatabaseDir {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }

        let identity = database_identity(&self.path);
        let mut initialized = initialized_databases().lock().unwrap();
        if self.path.exists() && initialized.contains(&identity) {
            return Ok(());
        }

        let mut connection = self.writer_connection()?;
        connection.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS source_pages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                site_key TEXT NOT NULL,
                page_url TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                raw_html TEXT NOT NULL,
                image_count INTEGER NOT NULL DEFAULT 0,
                favorite_count INTEGER NOT NULL DEFAULT 0,
                pending_image_count INTEGER NOT NULL DEFAULT 0,
                download_state TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                scraped_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS images (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_url TEXT NOT NULL UNIQUE,
                preview_url TEXT,
                alt_text TEXT,
                width INTEGER,
                height INTEGER,
                is_favorite INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS page_images (
                page_id INTEGER NOT NULL,
                image_id INTEGER NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                local_file_path TEXT,
                downloaded_at TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (page_id, image_id),
                FOREIGN KEY (page_id) REFERENCES source_pages(id) ON DELETE CASCADE,
                FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS attributes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                label TEXT NOT NULL,
                value TEXT NOT NULL,
                normalized_value TEXT NOT NULL,
                source_url TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(kind, normalized_value)
            );

            CREATE TABLE IF NOT EXISTS page_attributes (
                page_id INTEGER NOT NULL,
                attribute_id INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (page_id, attribute_id),
                FOREIGN KEY (page_id) REFERENCES source_pages(id) ON DELETE CASCADE,
                FOREIGN KEY (attribute_id) REFERENCES attributes(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS image_attributes (
                image_id INTEGER NOT NULL,
                attribute_id INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (image_id, attribute_id),
                FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE,
                FOREIGN KEY (attribute_id) REFERENCES attributes(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS page_html_archive (
                page_id INTEGER PRIMARY KEY,
                raw_html TEXT NOT NULL,
                captured_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (page_id) REFERENCES source_pages(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS app_schema_migrations (
                migration_key TEXT PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS page_search_fts USING fts5(
                title,
                metadata,
                tokenize = 'trigram'
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS image_search_fts USING fts5(
                metadata,
                tokenize = 'trigram'
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS attribute_search_fts USING fts5(
                value,
                tokenize = 'trigram'
            );

            CREATE TRIGGER IF NOT EXISTS trg_source_pages_delete_search
            AFTER DELETE ON source_pages
            BEGIN
                DELETE FROM page_search_fts WHERE rowid = OLD.id;
            END;
            CREATE TRIGGER IF NOT EXISTS trg_images_delete_search
            AFTER DELETE ON images
            BEGIN
                DELETE FROM image_search_fts WHERE rowid = OLD.id;
            END;
            CREATE TRIGGER IF NOT EXISTS trg_attributes_delete_search
            AFTER DELETE ON attributes
            BEGIN
                DELETE FROM attribute_search_fts WHERE rowid = OLD.id;
            END;

            CREATE INDEX IF NOT EXISTS idx_source_pages_site_key ON source_pages(site_key);
            CREATE INDEX IF NOT EXISTS idx_source_pages_search_order
                ON source_pages(scraped_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_images_is_favorite ON images(is_favorite);
            CREATE INDEX IF NOT EXISTS idx_page_images_page_id ON page_images(page_id, sort_order);
            CREATE INDEX IF NOT EXISTS idx_page_images_image_id ON page_images(image_id, page_id);
            CREATE INDEX IF NOT EXISTS idx_page_images_local_file_path ON page_images(local_file_path);
            CREATE INDEX IF NOT EXISTS idx_page_attributes_attribute_id ON page_attributes(attribute_id, page_id);
            CREATE INDEX IF NOT EXISTS idx_image_attributes_attribute_id ON image_attributes(attribute_id, image_id);
            "#,
        )
        .map_err(|source| DatabaseError::InitializeSchema {
            path: self.path.clone(),
            source,
        })?;

        // Safe migrations: ignore errors on columns that already exist.
        let _ = connection.execute(
            "ALTER TABLE images ADD COLUMN is_favorite INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = connection.execute("ALTER TABLE page_images ADD COLUMN thumbnail_path TEXT", []);
        let _ = connection.execute("ALTER TABLE attributes ADD COLUMN is_female INTEGER", []);
        let _ = connection.execute(
            "ALTER TABLE source_pages ADD COLUMN image_count INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE source_pages ADD COLUMN favorite_count INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE source_pages ADD COLUMN pending_image_count INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE source_pages ADD COLUMN download_state TEXT NOT NULL DEFAULT 'pending'",
            [],
        );
        connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_source_pages_date_browse ON source_pages(favorite_count, scraped_at, id)",
                [],
            )
            .map_err(|source| DatabaseError::InitializeSchema {
                path: self.path.clone(),
                source,
            })?;

        let derived_data_migrated = connection
            .query_row(
                "SELECT 1 FROM app_schema_migrations WHERE migration_key = 'derived-data-v1'",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(|source| DatabaseError::QueryRows {
                operation: "check_derived_data_migration",
                source,
            })?
            .is_some();

        if !derived_data_migrated {
            let transaction =
                connection
                    .transaction()
                    .map_err(|source| DatabaseError::BeginTransaction {
                        operation: "migrate_derived_data",
                        source,
                    })?;
            transaction
                .execute_batch(
                    r#"
                    INSERT INTO page_html_archive (page_id, raw_html, captured_at)
                    SELECT id, raw_html, updated_at
                    FROM source_pages
                    WHERE raw_html <> ''
                    ON CONFLICT(page_id) DO NOTHING;

                    UPDATE source_pages SET raw_html = '' WHERE raw_html <> '';

                    UPDATE source_pages
                    SET image_count = (
                            SELECT COUNT(*) FROM page_images pi WHERE pi.page_id = source_pages.id
                        ),
                        favorite_count = (
                            SELECT COUNT(*)
                            FROM page_images pi
                            INNER JOIN images i ON i.id = pi.image_id
                            WHERE pi.page_id = source_pages.id AND i.is_favorite = 1
                        ),
                        pending_image_count = (
                            SELECT COUNT(*)
                            FROM page_images pi
                            WHERE pi.page_id = source_pages.id
                              AND (
                                  pi.local_file_path IS NULL
                                  OR TRIM(pi.local_file_path) = ''
                                  OR pi.downloaded_at IS NULL
                              )
                        );

                    UPDATE source_pages
                    SET download_state = CASE
                        WHEN image_count > 0 AND pending_image_count = 0 THEN 'complete'
                        WHEN image_count > pending_image_count THEN 'partial'
                        ELSE 'pending'
                    END;

                    DELETE FROM page_search_fts;
                    INSERT INTO page_search_fts (rowid, title, metadata)
                    SELECT sp.id, sp.title, COALESCE(GROUP_CONCAT(a.normalized_value, ' '), '')
                    FROM source_pages sp
                    LEFT JOIN page_attributes pa ON pa.page_id = sp.id
                    LEFT JOIN attributes a ON a.id = pa.attribute_id
                    GROUP BY sp.id;

                    DELETE FROM image_search_fts;
                    INSERT INTO image_search_fts (rowid, metadata)
                    SELECT i.id, COALESCE(GROUP_CONCAT(a.normalized_value, ' '), '')
                    FROM images i
                    LEFT JOIN image_attributes ia ON ia.image_id = i.id
                    LEFT JOIN attributes a ON a.id = ia.attribute_id
                    GROUP BY i.id;

                    DELETE FROM attribute_search_fts;
                    INSERT INTO attribute_search_fts (rowid, value)
                    SELECT id, normalized_value FROM attributes;

                    DROP INDEX IF EXISTS idx_source_pages_scraped_at;
                    DROP INDEX IF EXISTS idx_images_source_url;
                    DROP INDEX IF EXISTS idx_attributes_kind_value;
                    DROP INDEX IF EXISTS idx_page_attributes_page_id;
                    DROP INDEX IF EXISTS idx_image_attributes_image_id;

                    INSERT INTO app_schema_migrations (migration_key)
                    VALUES ('derived-data-v1');
                    "#,
                )
                .map_err(|source| DatabaseError::InitializeSchema {
                    path: self.path.clone(),
                    source,
                })?;
            transaction
                .commit()
                .map_err(|source| DatabaseError::CommitTransaction {
                    operation: "migrate_derived_data",
                    source,
                })?;
        }

        let _ = connection.execute_batch("PRAGMA optimize;");

        initialized.insert(identity);
        Ok(())
    }

    pub fn upsert_scraped_item(&self, page: &ScrapedItem) -> AppResult<PersistedItem> {
        let mut connection = self.writer_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| DatabaseError::BeginTransaction {
                    operation: "upsert_scraped_item",
                    source,
                })?;
        let page_id = upsert_source_page(&transaction, page)?;

        transaction
            .execute(
                "DELETE FROM page_attributes WHERE page_id = ?1",
                params![page_id],
            )
            .map_err(|source| DatabaseError::ExecuteStatement {
                operation: "delete_page_attributes_for_page",
                source,
            })?;

        {
            let mut link_attribute = transaction
                .prepare_cached(
                    "INSERT OR IGNORE INTO page_attributes (page_id, attribute_id) VALUES (?1, ?2)",
                )
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "insert_page_attribute_link",
                    source,
                })?;
            for attribute in &page.attributes {
                let attribute_id = upsert_attribute(&transaction, attribute)?;
                link_attribute
                    .execute(params![page_id, attribute_id])
                    .map_err(|source| DatabaseError::ExecuteStatement {
                        operation: "insert_page_attribute_link",
                        source,
                    })?;
            }
        }

        let mut image_ids_by_url = HashMap::new();
        {
            let mut link_image = transaction
                .prepare_cached(
                    r#"
                    INSERT INTO page_images (page_id, image_id, sort_order, local_file_path, downloaded_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, NULL, CURRENT_TIMESTAMP)
                    ON CONFLICT(page_id, image_id) DO UPDATE SET
                        sort_order = excluded.sort_order,
                        updated_at = CURRENT_TIMESTAMP
                    "#,
                )
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "upsert_page_image_link",
                    source,
                })?;
            for image in &page.images {
                let image_id = upsert_image(&transaction, image)?;
                link_image
                    .execute(params![
                        page_id,
                        image_id,
                        image.sort_order,
                        image.local_file_path
                    ])
                    .map_err(|source| DatabaseError::ExecuteStatement {
                        operation: "upsert_page_image_link",
                        source,
                    })?;
                image_ids_by_url.insert(image.source_url.clone(), image_id);
            }
        }

        refresh_page_search_index(&transaction, page_id)?;
        refresh_page_counters(&transaction, page_id)?;

        transaction
            .commit()
            .map_err(|source| DatabaseError::CommitTransaction {
                operation: "upsert_scraped_item",
                source,
            })?;

        Ok(PersistedItem {
            page_id,
            image_ids_by_url,
        })
    }

    pub fn update_image_file_location(
        &self,
        page_id: i64,
        image_id: i64,
        file_path: &str,
    ) -> AppResult<()> {
        self.update_image_artifacts(page_id, image_id, file_path, None)
    }

    /// Finalizes the local file and optional thumbnail metadata in one SQLite write.
    pub fn update_image_artifacts(
        &self,
        page_id: i64,
        image_id: i64,
        file_path: &str,
        thumbnail_path: Option<&str>,
    ) -> AppResult<()> {
        self.update_image_artifacts_batch(&[ImageArtifactUpdate {
            page_id,
            image_id,
            file_path: file_path.to_string(),
            thumbnail_path: thumbnail_path.map(ToString::to_string),
        }])
    }

    /// Finalizes all successfully prepared files for a content item in one transaction.
    pub fn update_image_artifacts_batch(&self, artifacts: &[ImageArtifactUpdate]) -> AppResult<()> {
        if artifacts.is_empty() {
            return Ok(());
        }

        let mut connection = self.writer_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| DatabaseError::BeginTransaction {
                    operation: "update_image_artifacts_batch",
                    source,
                })?;
        {
            let mut statement = transaction
                .prepare(
                    r#"
                UPDATE page_images
                SET local_file_path = ?3,
                    thumbnail_path = COALESCE(?4, thumbnail_path),
                    downloaded_at = CURRENT_TIMESTAMP,
                    updated_at = CURRENT_TIMESTAMP
                WHERE page_id = ?1 AND image_id = ?2
                "#,
                )
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "update_image_artifacts_batch",
                    source,
                })?;

            for artifact in artifacts {
                statement
                    .execute(params![
                        artifact.page_id,
                        artifact.image_id,
                        artifact.file_path.as_str(),
                        artifact.thumbnail_path.as_deref(),
                    ])
                    .map_err(|source| DatabaseError::ExecuteStatement {
                        operation: "update_image_artifacts_batch",
                        source,
                    })?;
            }
        }
        for page_id in unique_ids(artifacts.iter().map(|artifact| artifact.page_id)) {
            refresh_page_counters(&transaction, page_id)?;
        }
        transaction
            .commit()
            .map_err(|source| DatabaseError::CommitTransaction {
                operation: "update_image_artifacts_batch",
                source,
            })?;
        Ok(())
    }

    /// Returns `true` only when the page was fully downloaded already.
    ///
    /// A mere `source_pages` row is not enough, because content metadata is
    /// written before images finish downloading.  Treating that as "processed"
    /// causes freshly discovered content items to be skipped prematurely while
    /// their files are still missing or only partially downloaded.
    pub fn has_processed_url(&self, page_url: &str) -> AppResult<bool> {
        let connection = self.connect()?;
        let existing = connection
            .query_row(
                r#"
                SELECT 1
                FROM source_pages
                WHERE page_url = ?1
                  AND image_count > 0
                  AND download_state = 'complete'
                LIMIT 1
                "#,
                params![page_url],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|source| DatabaseError::QueryRows {
                operation: "has_processed_url",
                source,
            })?;

        Ok(existing.is_some())
    }

    /// Returns the subset of URLs whose page images have all been downloaded.
    ///
    /// Inputs are chunked below SQLite's common bind-parameter limit, while a
    /// single configured connection is reused for the whole batch.
    pub fn processed_urls(&self, page_urls: &[String]) -> AppResult<HashSet<String>> {
        const URLS_PER_QUERY: usize = 500;

        if page_urls.is_empty() {
            return Ok(HashSet::new());
        }

        let connection = self.connect()?;
        let mut processed = HashSet::new();

        for chunk in page_urls.chunks(URLS_PER_QUERY) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                r#"
                SELECT page_url
                FROM source_pages
                WHERE page_url IN ({placeholders})
                  AND image_count > 0
                  AND download_state = 'complete'
                "#,
            );
            let mut statement =
                connection
                    .prepare(&sql)
                    .map_err(|source| DatabaseError::PrepareStatement {
                        operation: "processed_urls",
                        source,
                    })?;
            let rows = statement
                .query_map(params_from_iter(chunk.iter()), |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|source| DatabaseError::QueryRows {
                    operation: "processed_urls",
                    source,
                })?;

            for row in rows {
                processed.insert(row.map_err(|source| DatabaseError::DecodeRow {
                    operation: "processed_urls",
                    source,
                })?);
            }
        }

        Ok(processed)
    }

    /// Stores the absolute path to the pre-generated thumbnail for a page+image pair.
    pub fn update_thumbnail_path(
        &self,
        page_id: i64,
        image_id: i64,
        thumb_path: &str,
    ) -> AppResult<()> {
        let connection = self.writer_connection()?;
        connection
            .execute(
                "UPDATE page_images SET thumbnail_path = ?1 WHERE page_id = ?2 AND image_id = ?3",
                params![thumb_path, page_id, image_id],
            )
            .map_err(|source| DatabaseError::ExecuteStatement {
                operation: "update_thumbnail_path",
                source,
            })?;
        Ok(())
    }

    /// Returns `(image_id, local_file_path)` for all images on a page that have a local file.
    pub fn get_page_images_with_paths(&self, page_id: i64) -> AppResult<Vec<(i64, String)>> {
        let connection = self.connect()?;
        let mut stmt = connection
            .prepare(
                "SELECT image_id, local_file_path \
             FROM page_images \
             WHERE page_id = ?1 AND local_file_path IS NOT NULL",
            )
            .map_err(|source| DatabaseError::PrepareStatement {
                operation: "get_page_images_with_paths",
                source,
            })?;
        let rows = stmt
            .query_map(params![page_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| DatabaseError::QueryRows {
                operation: "get_page_images_with_paths",
                source,
            })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|source| DatabaseError::DecodeRow {
                operation: "get_page_images_with_paths",
                source,
            })?);
        }
        Ok(result)
    }

    pub fn archived_page_html(&self, page_id: i64) -> AppResult<Option<String>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT raw_html FROM page_html_archive WHERE page_id = ?1",
                params![page_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| DatabaseError::QueryRows {
                operation: "archived_page_html",
                source,
            })
            .map_err(Into::into)
    }

    /// Updates the stored relative path for a single image record.
    pub fn update_image_local_path(&self, image_id: i64, new_path: &str) -> AppResult<()> {
        self.update_image_local_paths(&[ImageLocalPathUpdate {
            image_id,
            new_path: new_path.to_string(),
        }])
    }

    /// Applies path moves in one serialized transaction after filesystem moves
    /// have completed successfully.
    pub fn update_image_local_paths(&self, updates: &[ImageLocalPathUpdate]) -> AppResult<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut connection = self.writer_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| DatabaseError::BeginTransaction {
                    operation: "update_image_local_paths",
                    source,
                })?;
        {
            let mut statement = transaction
                .prepare_cached(
                    "UPDATE page_images SET local_file_path = ?1, updated_at = CURRENT_TIMESTAMP WHERE image_id = ?2",
                )
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "update_image_local_paths",
                    source,
                })?;
            for update in updates {
                statement
                    .execute(params![update.new_path, update.image_id])
                    .map_err(|source| DatabaseError::ExecuteStatement {
                        operation: "update_image_local_paths",
                        source,
                    })?;
            }
        }
        transaction
            .commit()
            .map_err(|source| DatabaseError::CommitTransaction {
                operation: "update_image_local_paths",
                source,
            })?;
        Ok(())
    }

    pub fn set_favorite(&self, image_id: i64, is_favorite: bool) -> AppResult<()> {
        let mut connection = self.writer_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| DatabaseError::BeginTransaction {
                    operation: "set_favorite",
                    source,
                })?;
        let page_ids = {
            let mut statement = transaction
                .prepare_cached("SELECT page_id FROM page_images WHERE image_id = ?1")
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "load_favorite_page_ids",
                    source,
                })?;
            let rows = statement
                .query_map(params![image_id], |row| row.get::<_, i64>(0))
                .map_err(|source| DatabaseError::QueryRows {
                    operation: "load_favorite_page_ids",
                    source,
                })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|source| DatabaseError::DecodeRow {
                    operation: "load_favorite_page_ids",
                    source,
                })?
        };
        transaction
            .execute(
                "UPDATE images SET is_favorite = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
                params![is_favorite as i64, image_id],
            )
            .map_err(|source| DatabaseError::ExecuteStatement {
                operation: "set_favorite",
                source,
            })?;
        for page_id in page_ids {
            refresh_page_counters(&transaction, page_id)?;
        }
        transaction
            .commit()
            .map_err(|source| DatabaseError::CommitTransaction {
                operation: "set_favorite",
                source,
            })?;
        Ok(())
    }

    pub fn set_actor_gender(&self, actor_name: &str, is_female: Option<bool>) -> AppResult<()> {
        let connection = self.writer_connection()?;
        let normalized_value = actor_name.trim().to_ascii_lowercase();
        let gender_value: Option<i64> = is_female.map(|value| if value { 1 } else { 0 });
        connection
            .execute(
                r#"
            UPDATE attributes
            SET is_female = ?1
            WHERE kind = 'actor' AND normalized_value = ?2
            "#,
                params![gender_value, normalized_value],
            )
            .map_err(|source| DatabaseError::ExecuteStatement {
                operation: "set_actor_gender",
                source,
            })?;
        Ok(())
    }

    /// Returns `(image_id, local_file_path)` pairs for images that have a local file recorded.
    pub fn get_image_local_paths(&self, image_ids: &[i64]) -> AppResult<Vec<(i64, String)>> {
        if image_ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.connect()?;
        let mut result = Vec::new();
        let image_ids = unique_ids(image_ids.iter().copied());
        for image_batch in image_ids.chunks(SQLITE_ID_BATCH_SIZE) {
            let placeholders = sql_placeholders(image_batch.len());
            let sql = format!(
                "SELECT image_id, local_file_path FROM page_images WHERE image_id IN ({}) AND local_file_path IS NOT NULL",
                placeholders
            );
            let mut statement =
                connection
                    .prepare(&sql)
                    .map_err(|source| DatabaseError::PrepareStatement {
                        operation: "get_image_local_paths",
                        source,
                    })?;
            let rows = statement
                .query_map(params_from_iter(image_batch.iter()), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|source| DatabaseError::QueryRows {
                    operation: "get_image_local_paths",
                    source,
                })?;
            for row in rows {
                result.push(row.map_err(|source| DatabaseError::DecodeRow {
                    operation: "get_image_local_paths",
                    source,
                })?);
            }
        }
        Ok(result)
    }

    pub fn delete_images(&self, image_ids: &[i64]) -> AppResult<()> {
        if image_ids.is_empty() {
            return Ok(());
        }
        let mut connection = self.writer_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| DatabaseError::BeginTransaction {
                    operation: "delete_images",
                    source,
                })?;
        let image_ids = unique_ids(image_ids.iter().copied());
        let mut page_ids = HashSet::new();
        for image_batch in image_ids.chunks(SQLITE_ID_BATCH_SIZE) {
            let placeholders = sql_placeholders(image_batch.len());
            let sql = format!(
                "SELECT DISTINCT page_id FROM page_images WHERE image_id IN ({})",
                placeholders
            );
            let mut statement =
                transaction
                    .prepare(&sql)
                    .map_err(|source| DatabaseError::PrepareStatement {
                        operation: "load_deleted_image_page_ids",
                        source,
                    })?;
            let rows = statement
                .query_map(params_from_iter(image_batch.iter()), |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|source| DatabaseError::QueryRows {
                    operation: "load_deleted_image_page_ids",
                    source,
                })?;
            for row in rows {
                page_ids.insert(row.map_err(|source| DatabaseError::DecodeRow {
                    operation: "load_deleted_image_page_ids",
                    source,
                })?);
            }
            drop(statement);

            let sql = format!("DELETE FROM images WHERE id IN ({})", placeholders);
            transaction
                .execute(&sql, params_from_iter(image_batch.iter()))
                .map_err(|source| DatabaseError::ExecuteStatement {
                    operation: "delete_images",
                    source,
                })?;
        }
        for page_id in page_ids {
            refresh_page_counters(&transaction, page_id)?;
        }
        transaction
            .commit()
            .map_err(|source| DatabaseError::CommitTransaction {
                operation: "delete_images",
                source,
            })?;
        Ok(())
    }

    pub fn remove_image_attributes(
        &self,
        image_ids: &[i64],
        kind: &str,
        values: &[String],
    ) -> AppResult<()> {
        if image_ids.is_empty() || values.is_empty() {
            return Ok(());
        }
        let mut connection = self.writer_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| DatabaseError::BeginTransaction {
                    operation: "remove_image_attributes",
                    source,
                })?;

        let image_ids = unique_ids(image_ids.iter().copied());
        let normalized_values = values
            .iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for image_batch in image_ids.chunks(SQLITE_ID_BATCH_SIZE) {
            for value_batch in normalized_values.chunks(SQLITE_ATTRIBUTE_VALUE_BATCH_SIZE) {
                let sql = format!(
                    r#"
                    DELETE FROM image_attributes
                    WHERE image_id IN ({img})
                      AND attribute_id IN (
                        SELECT id FROM attributes
                        WHERE kind = ?
                          AND normalized_value IN ({val})
                      )
                    "#,
                    img = sql_placeholders(image_batch.len()),
                    val = sql_placeholders(value_batch.len()),
                );
                let mut all_params = image_batch
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                all_params.push(kind.to_string());
                all_params.extend(value_batch.iter().cloned());
                transaction
                    .execute(&sql, params_from_iter(all_params.iter()))
                    .map_err(|source| DatabaseError::ExecuteStatement {
                        operation: "remove_image_attributes",
                        source,
                    })?;
            }
        }
        for image_id in image_ids {
            refresh_image_search_index(&transaction, image_id)?;
        }
        transaction
            .commit()
            .map_err(|source| DatabaseError::CommitTransaction {
                operation: "remove_image_attributes",
                source,
            })?;
        Ok(())
    }

    pub fn add_image_attributes(
        &self,
        image_ids: &[i64],
        kind: &str,
        label: &str,
        values: &[String],
    ) -> AppResult<()> {
        self.add_image_attribute_groups(image_ids, &[(kind, label, values)])
    }

    /// Adds several preset metadata groups using one writer transaction and a
    /// reused link statement.
    pub fn add_image_attribute_groups(
        &self,
        image_ids: &[i64],
        groups: &[(&str, &str, &[String])],
    ) -> AppResult<()> {
        if image_ids.is_empty() || groups.iter().all(|(_, _, values)| values.is_empty()) {
            return Ok(());
        }
        let mut connection = self.writer_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| DatabaseError::BeginTransaction {
                    operation: "add_image_attributes",
                    source,
                })?;

        {
            let mut link_attribute = transaction
                .prepare_cached(
                    "INSERT OR IGNORE INTO image_attributes (image_id, attribute_id) VALUES (?1, ?2)",
                )
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "insert_image_attribute_link",
                    source,
                })?;
            for (kind, label, values) in groups {
                for value in *values {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    let attribute = MetadataAttribute {
                        kind: (*kind).to_string(),
                        label: (*label).to_string(),
                        value: trimmed.to_string(),
                        source_url: None,
                        is_female: None,
                    };
                    let attribute_id = upsert_attribute(&transaction, &attribute)?;

                    for image_id in image_ids {
                        link_attribute
                            .execute(params![image_id, attribute_id])
                            .map_err(|source| DatabaseError::ExecuteStatement {
                                operation: "insert_image_attribute_link",
                                source,
                            })?;
                    }
                }
            }
        }
        for image_id in unique_ids(image_ids.iter().copied()) {
            refresh_image_search_index(&transaction, image_id)?;
        }

        transaction
            .commit()
            .map_err(|source| DatabaseError::CommitTransaction {
                operation: "add_image_attributes",
                source,
            })?;
        Ok(())
    }

    pub fn search_images(&self, query: &SearchQuery) -> AppResult<Vec<SearchImageResult>> {
        Ok(self.search_images_page(query, None, 500)?.items)
    }

    /// Returns a stable keyset page. Unlike OFFSET pagination, advancing to the
    /// next page does not rescan or retain every preceding row.
    pub fn search_images_page(
        &self,
        query: &SearchQuery,
        cursor: Option<&SearchCursor>,
        page_size: usize,
    ) -> AppResult<SearchResultPage> {
        self.search_images_page_internal(query, cursor, page_size, true, None)
    }

    pub fn search_images_page_lightweight(
        &self,
        query: &SearchQuery,
        cursor: Option<&SearchCursor>,
        page_size: usize,
        cancellation: &QueryCancellation,
    ) -> AppResult<SearchResultPage> {
        self.search_images_page_internal(query, cursor, page_size, false, Some(cancellation))
    }

    fn search_images_page_internal(
        &self,
        query: &SearchQuery,
        cursor: Option<&SearchCursor>,
        page_size: usize,
        load_metadata: bool,
        cancellation: Option<&QueryCancellation>,
    ) -> AppResult<SearchResultPage> {
        let connection = self.connect()?;
        let _cancellation_guard = cancellation
            .map(|cancellation| cancellation.register(&connection))
            .transpose()?;
        let page_size = page_size.clamp(1, 500);

        let mut sql = String::from(
            r#"
            SELECT
                images.id,
                source_pages.id,
                source_pages.title,
                source_pages.page_url,
                images.source_url,
                images.preview_url,
                page_images.local_file_path,
                images.is_favorite,
                page_images.thumbnail_path,
                source_pages.scraped_at,
                page_images.sort_order
            FROM images
            INNER JOIN page_images ON page_images.image_id = images.id
            INNER JOIN source_pages ON source_pages.id = page_images.page_id
            WHERE 1 = 1
            "#,
        );

        let mut params = Vec::<String>::new();

        if query.favorites_only {
            sql.push_str(" AND images.is_favorite = 1");
        }

        if let Some(site_key) = query
            .site_key
            .as_deref()
            .filter(|site| !site.trim().is_empty())
        {
            sql.push_str(" AND source_pages.site_key = ?");
            params.push(site_key.trim().to_string());
        }

        if !query.text.trim().is_empty() {
            let normalized = query.text.trim().to_ascii_lowercase();
            if normalized.chars().count() >= 3 {
                sql.push_str(
                    r#"
                    AND (
                        source_pages.id IN (
                            SELECT rowid FROM page_search_fts
                            WHERE page_search_fts MATCH ?
                        )
                        OR images.id IN (
                            SELECT rowid FROM image_search_fts
                            WHERE image_search_fts MATCH ?
                        )
                    )
                    "#,
                );
                let fts_query = fts_literal(&normalized);
                params.push(fts_query.clone());
                params.push(fts_query);
            } else {
                // FTS5's trigram tokenizer cannot index one- or two-character
                // queries. Keep the compatibility fallback only for that case.
                sql.push_str(
                    r#"
                    AND (
                        LOWER(source_pages.title) LIKE ?
                        OR EXISTS (
                            SELECT 1
                            FROM page_attributes
                            INNER JOIN attributes ON attributes.id = page_attributes.attribute_id
                            WHERE page_attributes.page_id = source_pages.id
                              AND attributes.normalized_value LIKE ?
                        )
                        OR EXISTS (
                            SELECT 1
                            FROM image_attributes
                            INNER JOIN attributes ON attributes.id = image_attributes.attribute_id
                            WHERE image_attributes.image_id = images.id
                              AND attributes.normalized_value LIKE ?
                        )
                    )
                    "#,
                );
                let pattern = format!("%{normalized}%");
                params.push(pattern.clone());
                params.push(pattern.clone());
                params.push(pattern);
            }
        }

        append_attribute_filter(&mut sql, &mut params, "tag", &query.tags);
        append_attribute_filter(&mut sql, &mut params, "actor", &query.actors);
        append_attribute_filter(&mut sql, &mut params, "category", &query.categories);
        append_attribute_filter(&mut sql, &mut params, "studio", &query.studios);
        append_attribute_filter(&mut sql, &mut params, "channel", &query.channels);

        if let Some(page_id) = query.page_id {
            sql.push_str(" AND source_pages.id = ?");
            params.push(page_id.to_string());
        }

        if let Some(cursor) = cursor {
            sql.push_str(
                r#"
                AND (
                    source_pages.scraped_at < ?
                    OR (source_pages.scraped_at = ? AND source_pages.id < ?)
                    OR (
                        source_pages.scraped_at = ? AND source_pages.id = ?
                        AND page_images.sort_order > ?
                    )
                    OR (
                        source_pages.scraped_at = ? AND source_pages.id = ?
                        AND page_images.sort_order = ? AND images.id > ?
                    )
                )
                "#,
            );
            params.push(cursor.scraped_at.clone());
            params.push(cursor.scraped_at.clone());
            params.push(cursor.page_id.to_string());
            params.push(cursor.scraped_at.clone());
            params.push(cursor.page_id.to_string());
            params.push(cursor.sort_order.to_string());
            params.push(cursor.scraped_at.clone());
            params.push(cursor.page_id.to_string());
            params.push(cursor.sort_order.to_string());
            params.push(cursor.image_id.to_string());
        }

        sql.push_str(&format!(
            " ORDER BY source_pages.scraped_at DESC, source_pages.id DESC, page_images.sort_order ASC, images.id ASC LIMIT {}",
            page_size + 1
        ));

        let mut statement =
            connection
                .prepare(&sql)
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "search_images",
                    source,
                })?;
        let rows = statement
            .query_map(params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            })
            .map_err(|source| DatabaseError::QueryRows {
                operation: "search_images",
                source,
            })?;

        let mut raw_rows = collect_raw_rows(rows)?;
        let has_more = raw_rows.len() > page_size;
        if has_more {
            raw_rows.truncate(page_size);
        }
        let next_cursor = has_more.then(|| {
            raw_rows
                .last()
                .expect("non-empty limited result page")
                .cursor()
        });
        let items = if load_metadata {
            build_results_from_rows(&connection, raw_rows)?
        } else {
            build_lightweight_results_from_rows(raw_rows)
        };
        Ok(SearchResultPage { items, next_cursor })
    }

    /// Returns images from the N most recently scraped pages, ordered by scrape time desc.
    pub fn search_images_recent_pages(
        &self,
        page_limit: usize,
    ) -> AppResult<Vec<SearchImageResult>> {
        Ok(self
            .search_images_recent_pages_page_internal(page_limit, None, 2_000, true, None)?
            .items)
    }

    pub fn search_images_recent_pages_cancellable(
        &self,
        page_limit: usize,
        cancellation: &QueryCancellation,
    ) -> AppResult<Vec<SearchImageResult>> {
        Ok(self
            .search_images_recent_pages_page_internal(
                page_limit,
                None,
                2_000,
                true,
                Some(cancellation),
            )?
            .items)
    }

    pub fn search_images_recent_pages_page_lightweight(
        &self,
        page_limit: usize,
        cursor: Option<&SearchCursor>,
        page_size: usize,
        cancellation: &QueryCancellation,
    ) -> AppResult<SearchResultPage> {
        self.search_images_recent_pages_page_internal(
            page_limit,
            cursor,
            page_size,
            false,
            Some(cancellation),
        )
    }

    fn search_images_recent_pages_page_internal(
        &self,
        page_limit: usize,
        cursor: Option<&SearchCursor>,
        page_size: usize,
        load_metadata: bool,
        cancellation: Option<&QueryCancellation>,
    ) -> AppResult<SearchResultPage> {
        let connection = self.connect()?;
        let _cancellation_guard = cancellation
            .map(|cancellation| cancellation.register(&connection))
            .transpose()?;
        let page_size = page_size.clamp(1, 2_000);
        let mut sql = format!(
            r#"
            SELECT
                images.id,
                source_pages.id,
                source_pages.title,
                source_pages.page_url,
                images.source_url,
                images.preview_url,
                page_images.local_file_path,
                images.is_favorite,
                page_images.thumbnail_path,
                source_pages.scraped_at,
                page_images.sort_order
            FROM images
            INNER JOIN page_images ON page_images.image_id = images.id
            INNER JOIN source_pages ON source_pages.id = page_images.page_id
            WHERE source_pages.id IN (
                SELECT id FROM source_pages
                ORDER BY scraped_at DESC, id DESC
                LIMIT {}
            )
            "#,
            page_limit.max(1)
        );
        let mut params = Vec::<String>::new();
        if let Some(cursor) = cursor {
            sql.push_str(
                r#"
                AND (
                    source_pages.scraped_at < ?
                    OR (source_pages.scraped_at = ? AND source_pages.id < ?)
                    OR (
                        source_pages.scraped_at = ? AND source_pages.id = ?
                        AND page_images.sort_order > ?
                    )
                    OR (
                        source_pages.scraped_at = ? AND source_pages.id = ?
                        AND page_images.sort_order = ? AND images.id > ?
                    )
                )
                "#,
            );
            params.push(cursor.scraped_at.clone());
            params.push(cursor.scraped_at.clone());
            params.push(cursor.page_id.to_string());
            params.push(cursor.scraped_at.clone());
            params.push(cursor.page_id.to_string());
            params.push(cursor.sort_order.to_string());
            params.push(cursor.scraped_at.clone());
            params.push(cursor.page_id.to_string());
            params.push(cursor.sort_order.to_string());
            params.push(cursor.image_id.to_string());
        }
        sql.push_str(&format!(
            " ORDER BY source_pages.scraped_at DESC, source_pages.id DESC, page_images.sort_order ASC, images.id ASC LIMIT {}",
            page_size + 1
        ));

        let mut statement =
            connection
                .prepare(&sql)
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "search_images_recent_pages",
                    source,
                })?;
        let rows = statement
            .query_map(params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            })
            .map_err(|source| DatabaseError::QueryRows {
                operation: "search_images_recent_pages",
                source,
            })?;

        let mut raw_rows = collect_raw_rows(rows)?;
        let has_more = raw_rows.len() > page_size;
        if has_more {
            raw_rows.truncate(page_size);
        }
        let next_cursor = has_more.then(|| {
            raw_rows
                .last()
                .expect("non-empty limited recent-page result")
                .cursor()
        });
        let items = if load_metadata {
            build_results_from_rows(&connection, raw_rows)?
        } else {
            build_lightweight_results_from_rows(raw_rows)
        };
        Ok(SearchResultPage { items, next_cursor })
    }

    /// Loads complete metadata only for the image cards that are visible or
    /// were just mutated. This keeps the initial paged search lightweight.
    pub fn search_images_by_ids(&self, image_ids: &[i64]) -> AppResult<Vec<SearchImageResult>> {
        if image_ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.connect()?;
        let image_ids = unique_ids(image_ids.iter().copied());
        let mut raw_rows = Vec::with_capacity(image_ids.len());
        for image_batch in image_ids.chunks(SQLITE_ID_BATCH_SIZE) {
            let placeholders = sql_placeholders(image_batch.len());
            let sql = format!(
                r#"
                SELECT
                    images.id, source_pages.id, source_pages.title, source_pages.page_url,
                    images.source_url, images.preview_url, page_images.local_file_path,
                    images.is_favorite, page_images.thumbnail_path, source_pages.scraped_at,
                    page_images.sort_order
                FROM images
                INNER JOIN page_images ON page_images.image_id = images.id
                INNER JOIN source_pages ON source_pages.id = page_images.page_id
                WHERE images.id IN ({placeholders})
                "#
            );
            let mut statement =
                connection
                    .prepare(&sql)
                    .map_err(|source| DatabaseError::PrepareStatement {
                        operation: "search_images_by_ids",
                        source,
                    })?;
            let rows = statement
                .query_map(params_from_iter(image_batch.iter()), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, i64>(10)?,
                    ))
                })
                .map_err(|source| DatabaseError::QueryRows {
                    operation: "search_images_by_ids",
                    source,
                })?;
            raw_rows.extend(collect_raw_rows(rows)?);
        }
        raw_rows.sort_by(|left, right| {
            right
                .scraped_at
                .cmp(&left.scraped_at)
                .then(right.page_id.cmp(&left.page_id))
                .then(left.sort_order.cmp(&right.sort_order))
                .then(left.image_id.cmp(&right.image_id))
        });
        build_results_from_rows(&connection, raw_rows)
    }

    /// Returns all source pages with transactionally maintained statistics,
    /// sorted by `scraped_at` ascending. Caller can reverse/filter client-side.
    pub fn get_date_groups(&self) -> AppResult<Vec<DateGroup>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                r#"
            SELECT
                sp.id,
                sp.scraped_at,
                SUBSTR(sp.scraped_at, 1, 10) AS date_str,
                sp.title,
                sp.image_count,
                sp.favorite_count
            FROM source_pages sp
            ORDER BY sp.scraped_at ASC
            "#,
            )
            .map_err(|source| DatabaseError::PrepareStatement {
                operation: "get_date_groups",
                source,
            })?;

        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|source| DatabaseError::QueryRows {
                operation: "get_date_groups",
                source,
            })?;

        let mut groups = Vec::new();
        for row in rows {
            let (page_id, scraped_at, date_str, title, image_count, fav_count) =
                row.map_err(|source| DatabaseError::DecodeRow {
                    operation: "get_date_groups",
                    source,
                })?;
            groups.push(DateGroup {
                page_id,
                scraped_at,
                date_str,
                title,
                image_count: image_count as usize,
                fav_count: fav_count as usize,
            });
        }
        Ok(groups)
    }

    pub fn get_date_group_page(
        &self,
        cursor: Option<&DateGroupCursor>,
        direction: DateBrowseDirection,
        ascending: bool,
        max_favorites_exclusive: Option<usize>,
    ) -> AppResult<DateGroupPage> {
        let connection = self.connect()?;
        let favorite_clause = max_favorites_exclusive
            .map(|_| " WHERE favorite_count < ?")
            .unwrap_or("");
        let count_sql = format!("SELECT COUNT(*) FROM source_pages{favorite_clause}");
        let total = if let Some(limit) = max_favorites_exclusive {
            connection.query_row(&count_sql, params![limit as i64], |row| {
                row.get::<_, i64>(0)
            })?
        } else {
            connection.query_row(&count_sql, [], |row| row.get::<_, i64>(0))?
        } as usize;
        if total == 0 {
            return Ok(DateGroupPage { group: None, total });
        }

        let desired_ascending = ascending;
        let query_ascending = match direction {
            DateBrowseDirection::Previous => !desired_ascending,
            DateBrowseDirection::Initial | DateBrowseDirection::Next => desired_ascending,
        };
        let comparator = match direction {
            DateBrowseDirection::Initial => None,
            DateBrowseDirection::Next if desired_ascending => Some(">"),
            DateBrowseDirection::Next => Some("<"),
            DateBrowseDirection::Previous if desired_ascending => Some("<"),
            DateBrowseDirection::Previous => Some(">"),
        };

        let load = |cursor: Option<&DateGroupCursor>,
                    query_ascending: bool|
         -> AppResult<Option<DateGroup>> {
            let mut sql = String::from(
                "SELECT id, scraped_at, SUBSTR(scraped_at, 1, 10), title, image_count, favorite_count FROM source_pages WHERE 1 = 1",
            );
            let mut values = Vec::<rusqlite::types::Value>::new();
            if let Some(limit) = max_favorites_exclusive {
                sql.push_str(" AND favorite_count < ?");
                values.push((limit as i64).into());
            }
            if let (Some(cursor), Some(comparator)) = (cursor, comparator) {
                sql.push_str(&format!(
                    " AND (scraped_at {comparator} ? OR (scraped_at = ? AND id {comparator} ?))"
                ));
                values.push(cursor.scraped_at.clone().into());
                values.push(cursor.scraped_at.clone().into());
                values.push(cursor.page_id.into());
            }
            sql.push_str(if query_ascending {
                " ORDER BY scraped_at ASC, id ASC LIMIT 1"
            } else {
                " ORDER BY scraped_at DESC, id DESC LIMIT 1"
            });
            let mut statement = connection.prepare(&sql)?;
            statement
                .query_row(params_from_iter(values.iter()), |row| {
                    Ok(DateGroup {
                        page_id: row.get(0)?,
                        scraped_at: row.get(1)?,
                        date_str: row.get(2)?,
                        title: row.get(3)?,
                        image_count: row.get::<_, i64>(4)? as usize,
                        fav_count: row.get::<_, i64>(5)? as usize,
                    })
                })
                .optional()
                .map_err(Into::into)
        };

        let mut group = load(
            cursor.filter(|_| direction != DateBrowseDirection::Initial),
            query_ascending,
        )?;
        if group.is_none() && cursor.is_some() {
            // Wrap around in the effective sort direction.
            let wrap_ascending = match direction {
                DateBrowseDirection::Previous => !desired_ascending,
                DateBrowseDirection::Initial | DateBrowseDirection::Next => desired_ascending,
            };
            group = load(None, wrap_ascending)?;
        }
        Ok(DateGroupPage { group, total })
    }

    pub fn suggest_attribute_values(
        &self,
        kind: &str,
        query: &str,
        limit: usize,
    ) -> AppResult<Vec<String>> {
        let connection = self.connect()?;
        let normalized = query.trim().to_ascii_lowercase();
        let limit = limit.max(1);
        let mut values = Vec::new();

        if normalized.is_empty() {
            let mut statement = connection
                .prepare_cached(
                    "SELECT value FROM attributes WHERE kind = ?1 ORDER BY normalized_value LIMIT ?2",
                )
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "suggest_attribute_values",
                    source,
                })?;
            let rows = statement
                .query_map(params![kind, limit as i64], |row| row.get::<_, String>(0))
                .map_err(|source| DatabaseError::QueryRows {
                    operation: "suggest_attribute_values",
                    source,
                })?;
            for row in rows {
                values.push(row.map_err(|source| DatabaseError::DecodeRow {
                    operation: "suggest_attribute_values",
                    source,
                })?);
            }
            return Ok(values);
        }

        let prefix_end = format!("{normalized}{}", char::MAX);
        {
            let mut statement = connection
                .prepare_cached(
                    r#"
                    SELECT value
                    FROM attributes
                    WHERE kind = ?1
                      AND normalized_value >= ?2
                      AND normalized_value < ?3
                    ORDER BY normalized_value
                    LIMIT ?4
                    "#,
                )
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "suggest_attribute_prefix_values",
                    source,
                })?;
            let rows = statement
                .query_map(params![kind, normalized, prefix_end, limit as i64], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|source| DatabaseError::QueryRows {
                    operation: "suggest_attribute_prefix_values",
                    source,
                })?;
            for row in rows {
                values.push(row.map_err(|source| DatabaseError::DecodeRow {
                    operation: "suggest_attribute_prefix_values",
                    source,
                })?);
            }
        }

        let remaining = limit.saturating_sub(values.len());
        if remaining == 0 {
            return Ok(values);
        }

        let fallback_sql = if normalized.chars().count() >= 3 {
            r#"
            SELECT a.value
            FROM attribute_search_fts
            INNER JOIN attributes a ON a.id = attribute_search_fts.rowid
            WHERE attribute_search_fts MATCH ?1
              AND a.kind = ?2
              AND NOT (a.normalized_value >= ?3 AND a.normalized_value < ?4)
            ORDER BY a.normalized_value
            LIMIT ?5
            "#
        } else {
            r#"
            SELECT value
            FROM attributes
            WHERE normalized_value LIKE ?1
              AND kind = ?2
              AND NOT (normalized_value >= ?3 AND normalized_value < ?4)
            ORDER BY normalized_value
            LIMIT ?5
            "#
        };
        let fallback_query = if normalized.chars().count() >= 3 {
            fts_literal(&normalized)
        } else {
            format!("%{normalized}%")
        };
        let mut statement =
            connection
                .prepare(fallback_sql)
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "suggest_attribute_fallback_values",
                    source,
                })?;
        let rows = statement
            .query_map(
                params![
                    fallback_query,
                    kind,
                    normalized,
                    prefix_end,
                    remaining as i64
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|source| DatabaseError::QueryRows {
                operation: "suggest_attribute_fallback_values",
                source,
            })?;
        for row in rows {
            values.push(row.map_err(|source| DatabaseError::DecodeRow {
                operation: "suggest_attribute_fallback_values",
                source,
            })?);
        }

        Ok(values)
    }
}

fn append_attribute_filter(
    sql: &mut String,
    params: &mut Vec<String>,
    kind: &str,
    values: &[String],
) {
    if values.is_empty() {
        return;
    }

    // The UNIQUE(kind, normalized_value) auto-index satisfies this exact
    // relational filter without applying LOWER() to stored values.
    sql.push_str(
        r#"
        AND (
            EXISTS (
                SELECT 1
                FROM page_attributes
                INNER JOIN attributes ON attributes.id = page_attributes.attribute_id
                WHERE page_attributes.page_id = source_pages.id
                  AND attributes.kind = ?
                  AND attributes.normalized_value IN (
        "#,
    );
    params.push(kind.to_string());
    push_value_placeholders(sql, params, values);
    sql.push_str(
        r#"
                  )
            )
            OR EXISTS (
                SELECT 1
                FROM image_attributes
                INNER JOIN attributes ON attributes.id = image_attributes.attribute_id
                WHERE image_attributes.image_id = images.id
                  AND attributes.kind = ?
                  AND attributes.normalized_value IN (
        "#,
    );
    params.push(kind.to_string());
    push_value_placeholders(sql, params, values);
    sql.push_str(
        r#"
                  )
            )
        )
        "#,
    );
}

fn push_value_placeholders(sql: &mut String, params: &mut Vec<String>, values: &[String]) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
        params.push(value.trim().to_ascii_lowercase());
    }
}

fn fts_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

// ── Shared result-building helpers ───────────────────────────────────────────

struct RawRow {
    image_id: i64,
    page_id: i64,
    page_title: String,
    page_url: String,
    source_url: String,
    preview_url: Option<String>,
    local_file_path: Option<String>,
    thumbnail_path: Option<String>,
    is_favorite: bool,
    scraped_at: String,
    sort_order: i64,
}

impl RawRow {
    fn cursor(&self) -> SearchCursor {
        SearchCursor {
            scraped_at: self.scraped_at.clone(),
            page_id: self.page_id,
            sort_order: self.sort_order,
            image_id: self.image_id,
        }
    }
}

/// Drains a rusqlite MappedRows iterator into a `Vec<RawRow>`.
fn collect_raw_rows(
    rows: impl Iterator<
        Item = rusqlite::Result<(
            i64,
            i64,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
            String,
            i64,
        )>,
    >,
) -> AppResult<Vec<RawRow>> {
    let mut raw_rows = Vec::new();
    for row in rows {
        let (
            image_id,
            page_id,
            page_title,
            page_url,
            source_url,
            preview_url,
            local_file_path,
            is_favorite_int,
            thumbnail_path,
            scraped_at,
            sort_order,
        ) = row.map_err(|source| DatabaseError::DecodeRow {
            operation: "collect_search_rows",
            source,
        })?;
        raw_rows.push(RawRow {
            image_id,
            page_id,
            page_title,
            page_url,
            source_url,
            preview_url,
            local_file_path,
            thumbnail_path,
            is_favorite: is_favorite_int != 0,
            scraped_at,
            sort_order,
        });
    }
    Ok(raw_rows)
}

/// Batch-loads attributes for all rows and assembles `SearchImageResult` values.
fn build_results_from_rows(
    connection: &Connection,
    raw_rows: Vec<RawRow>,
) -> AppResult<Vec<SearchImageResult>> {
    let page_ids = unique_ids(raw_rows.iter().map(|row| row.page_id));
    let image_ids = unique_ids(raw_rows.iter().map(|row| row.image_id));

    let page_attrs = batch_load_page_attributes(connection, &page_ids)?
        .into_iter()
        .map(|(page_id, mut attributes)| {
            sort_metadata_attributes(&mut attributes);
            (page_id, Arc::new(attributes))
        })
        .collect::<HashMap<_, _>>();
    let image_attrs = batch_load_image_attributes(connection, &image_ids)?;
    let empty_attributes = Arc::new(Vec::new());
    let mut pages = HashMap::<i64, Arc<SearchPageSummary>>::new();
    for row in &raw_rows {
        let attributes = page_attrs
            .get(&row.page_id)
            .cloned()
            .unwrap_or_else(|| Arc::clone(&empty_attributes));
        pages.entry(row.page_id).or_insert_with(|| {
            Arc::new(SearchPageSummary {
                page_id: row.page_id,
                title: row.page_title.clone(),
                url: row.page_url.clone(),
                attributes,
            })
        });
    }

    let mut results = Vec::with_capacity(raw_rows.len());
    for row in raw_rows {
        let page = pages
            .get(&row.page_id)
            .cloned()
            .expect("page summary built for every raw row");
        let attributes =
            merge_result_attributes(Arc::clone(&page.attributes), image_attrs.get(&row.image_id));
        results.push(SearchImageResult {
            image_id: row.image_id,
            page,
            source_url: row.source_url,
            preview_url: row.preview_url,
            local_file_path: row.local_file_path,
            thumbnail_path: row.thumbnail_path,
            is_favorite: row.is_favorite,
            attributes,
        });
    }
    Ok(results)
}

fn build_lightweight_results_from_rows(raw_rows: Vec<RawRow>) -> Vec<SearchImageResult> {
    let empty_attributes = Arc::new(Vec::new());
    let mut pages = HashMap::<i64, Arc<SearchPageSummary>>::new();
    let mut results = Vec::with_capacity(raw_rows.len());
    for row in raw_rows {
        let page = pages
            .entry(row.page_id)
            .or_insert_with(|| {
                Arc::new(SearchPageSummary {
                    page_id: row.page_id,
                    title: row.page_title.clone(),
                    url: row.page_url.clone(),
                    attributes: Arc::clone(&empty_attributes),
                })
            })
            .clone();
        results.push(SearchImageResult {
            image_id: row.image_id,
            page,
            source_url: row.source_url,
            preview_url: row.preview_url,
            local_file_path: row.local_file_path,
            thumbnail_path: row.thumbnail_path,
            is_favorite: row.is_favorite,
            attributes: Arc::clone(&empty_attributes),
        });
    }
    results
}

fn unique_ids(ids: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut seen = HashSet::new();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

fn sql_placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

fn merge_result_attributes(
    page_attributes: Arc<Vec<MetadataAttribute>>,
    image_attributes: Option<&Vec<MetadataAttribute>>,
) -> Arc<Vec<MetadataAttribute>> {
    let Some(image_attributes) = image_attributes else {
        return page_attributes;
    };

    let mut merged: Option<Vec<MetadataAttribute>> = None;
    for attribute in image_attributes {
        let current = merged.as_deref().unwrap_or(page_attributes.as_slice());
        let duplicate = current.iter().any(|existing| {
            existing.kind == attribute.kind && existing.value.eq_ignore_ascii_case(&attribute.value)
        });
        if !duplicate {
            merged
                .get_or_insert_with(|| page_attributes.as_ref().clone())
                .push(attribute.clone());
        }
    }

    match merged {
        Some(mut attributes) => {
            sort_metadata_attributes(&mut attributes);
            Arc::new(attributes)
        }
        None => page_attributes,
    }
}

fn sort_metadata_attributes(attributes: &mut [MetadataAttribute]) {
    attributes.sort_by(|a, b| {
        kind_order(&a.kind)
            .cmp(&kind_order(&b.kind))
            .then(a.value.cmp(&b.value))
    });
}

/// Display priority for attribute kinds: lower number = shown first.
/// Actors always appear at the top regardless of which table they came from.
/// actor → channel → studio → category → tag → others
fn kind_order(kind: &str) -> u8 {
    match kind {
        "actor" => 0,
        "channel" => 1,
        "studio" => 2,
        "category" => 3,
        "tag" => 4,
        _ => 5,
    }
}

/// Batch-load page-level attributes for a set of page IDs in a single query.
/// Returns a map of page_id → Vec<MetadataAttribute>.
fn batch_load_page_attributes(
    connection: &Connection,
    page_ids: &[i64],
) -> AppResult<HashMap<i64, Vec<MetadataAttribute>>> {
    if page_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut map: HashMap<i64, Vec<MetadataAttribute>> = HashMap::new();
    for page_batch in page_ids.chunks(SQLITE_ID_BATCH_SIZE) {
        let sql = format!(
            r#"
            SELECT pa.page_id, a.kind, a.label, a.value, a.source_url, a.is_female
            FROM page_attributes pa
            INNER JOIN attributes a ON a.id = pa.attribute_id
            WHERE pa.page_id IN ({})
            ORDER BY pa.page_id, a.kind, a.value
            "#,
            sql_placeholders(page_batch.len())
        );
        let mut statement =
            connection
                .prepare(&sql)
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "batch_load_page_attributes",
                    source,
                })?;
        let rows = statement
            .query_map(params_from_iter(page_batch.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    MetadataAttribute {
                        kind: row.get(1)?,
                        label: row.get(2)?,
                        value: row.get(3)?,
                        source_url: row.get(4)?,
                        is_female: row.get::<_, Option<i64>>(5)?.map(|v| v != 0),
                    },
                ))
            })
            .map_err(|source| DatabaseError::QueryRows {
                operation: "batch_load_page_attributes",
                source,
            })?;
        for row in rows {
            let (page_id, attr) = row.map_err(|source| DatabaseError::DecodeRow {
                operation: "batch_load_page_attributes",
                source,
            })?;
            map.entry(page_id).or_default().push(attr);
        }
    }

    Ok(map)
}

/// Batch-load image-level attributes for a set of image IDs in a single query.
/// Returns a map of image_id → Vec<MetadataAttribute>.
fn batch_load_image_attributes(
    connection: &Connection,
    image_ids: &[i64],
) -> AppResult<HashMap<i64, Vec<MetadataAttribute>>> {
    if image_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut map: HashMap<i64, Vec<MetadataAttribute>> = HashMap::new();
    for image_batch in image_ids.chunks(SQLITE_ID_BATCH_SIZE) {
        let sql = format!(
            r#"
            SELECT ia.image_id, a.kind, a.label, a.value, a.source_url, a.is_female
            FROM image_attributes ia
            INNER JOIN attributes a ON a.id = ia.attribute_id
            WHERE ia.image_id IN ({})
            ORDER BY ia.image_id, a.kind, a.value
            "#,
            sql_placeholders(image_batch.len())
        );
        let mut statement =
            connection
                .prepare(&sql)
                .map_err(|source| DatabaseError::PrepareStatement {
                    operation: "batch_load_image_attributes",
                    source,
                })?;
        let rows = statement
            .query_map(params_from_iter(image_batch.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    MetadataAttribute {
                        kind: row.get(1)?,
                        label: row.get(2)?,
                        value: row.get(3)?,
                        source_url: row.get(4)?,
                        is_female: row.get::<_, Option<i64>>(5)?.map(|v| v != 0),
                    },
                ))
            })
            .map_err(|source| DatabaseError::QueryRows {
                operation: "batch_load_image_attributes",
                source,
            })?;
        for row in rows {
            let (image_id, attr) = row.map_err(|source| DatabaseError::DecodeRow {
                operation: "batch_load_image_attributes",
                source,
            })?;
            map.entry(image_id).or_default().push(attr);
        }
    }

    Ok(map)
}

fn refresh_page_counters(transaction: &Transaction<'_>, page_id: i64) -> AppResult<()> {
    transaction
        .execute(
            r#"
            UPDATE source_pages
            SET image_count = (
                    SELECT COUNT(*) FROM page_images WHERE page_id = ?1
                ),
                favorite_count = (
                    SELECT COUNT(*)
                    FROM page_images pi
                    INNER JOIN images i ON i.id = pi.image_id
                    WHERE pi.page_id = ?1 AND i.is_favorite = 1
                ),
                pending_image_count = (
                    SELECT COUNT(*)
                    FROM page_images
                    WHERE page_id = ?1
                      AND (
                          local_file_path IS NULL
                          OR TRIM(local_file_path) = ''
                          OR downloaded_at IS NULL
                      )
                ),
                download_state = CASE
                    WHEN (SELECT COUNT(*) FROM page_images WHERE page_id = ?1) > 0
                     AND NOT EXISTS (
                         SELECT 1 FROM page_images
                         WHERE page_id = ?1
                           AND (
                               local_file_path IS NULL
                               OR TRIM(local_file_path) = ''
                               OR downloaded_at IS NULL
                           )
                     ) THEN 'complete'
                    WHEN EXISTS (
                        SELECT 1 FROM page_images
                        WHERE page_id = ?1
                          AND local_file_path IS NOT NULL
                          AND TRIM(local_file_path) <> ''
                          AND downloaded_at IS NOT NULL
                    ) THEN 'partial'
                    ELSE 'pending'
                END
            WHERE id = ?1
            "#,
            params![page_id],
        )
        .map_err(|source| DatabaseError::ExecuteStatement {
            operation: "refresh_page_counters",
            source,
        })?;
    Ok(())
}

fn refresh_page_search_index(transaction: &Transaction<'_>, page_id: i64) -> AppResult<()> {
    transaction
        .execute(
            "DELETE FROM page_search_fts WHERE rowid = ?1",
            params![page_id],
        )
        .map_err(|source| DatabaseError::ExecuteStatement {
            operation: "delete_page_search_index",
            source,
        })?;
    transaction
        .execute(
            r#"
            INSERT INTO page_search_fts (rowid, title, metadata)
            SELECT sp.id, sp.title, COALESCE(GROUP_CONCAT(a.normalized_value, ' '), '')
            FROM source_pages sp
            LEFT JOIN page_attributes pa ON pa.page_id = sp.id
            LEFT JOIN attributes a ON a.id = pa.attribute_id
            WHERE sp.id = ?1
            GROUP BY sp.id
            "#,
            params![page_id],
        )
        .map_err(|source| DatabaseError::ExecuteStatement {
            operation: "refresh_page_search_index",
            source,
        })?;
    Ok(())
}

fn refresh_image_search_index(transaction: &Transaction<'_>, image_id: i64) -> AppResult<()> {
    transaction
        .execute(
            "DELETE FROM image_search_fts WHERE rowid = ?1",
            params![image_id],
        )
        .map_err(|source| DatabaseError::ExecuteStatement {
            operation: "delete_image_search_index",
            source,
        })?;
    transaction
        .execute(
            r#"
            INSERT INTO image_search_fts (rowid, metadata)
            SELECT i.id, COALESCE(GROUP_CONCAT(a.normalized_value, ' '), '')
            FROM images i
            LEFT JOIN image_attributes ia ON ia.image_id = i.id
            LEFT JOIN attributes a ON a.id = ia.attribute_id
            WHERE i.id = ?1
            GROUP BY i.id
            "#,
            params![image_id],
        )
        .map_err(|source| DatabaseError::ExecuteStatement {
            operation: "refresh_image_search_index",
            source,
        })?;
    Ok(())
}

fn refresh_attribute_search_index(
    transaction: &Transaction<'_>,
    attribute_id: i64,
) -> AppResult<()> {
    transaction
        .execute(
            "DELETE FROM attribute_search_fts WHERE rowid = ?1",
            params![attribute_id],
        )
        .map_err(|source| DatabaseError::ExecuteStatement {
            operation: "delete_attribute_search_index",
            source,
        })?;
    transaction
        .execute(
            r#"
            INSERT INTO attribute_search_fts (rowid, value)
            SELECT id, normalized_value FROM attributes WHERE id = ?1
            "#,
            params![attribute_id],
        )
        .map_err(|source| DatabaseError::ExecuteStatement {
            operation: "refresh_attribute_search_index",
            source,
        })?;
    Ok(())
}

fn upsert_source_page(transaction: &Transaction<'_>, page: &ScrapedItem) -> AppResult<i64> {
    let page_id = transaction
        .query_row(
            r#"
            INSERT INTO source_pages (site_key, page_url, title, raw_html, scraped_at, updated_at)
            VALUES (?1, ?2, ?3, '', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT(page_url) DO UPDATE SET
                site_key = excluded.site_key,
                title = excluded.title,
                raw_html = '',
                scraped_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP
            RETURNING id
            "#,
            params![page.site_key, page.page_url, page.title],
            |row| row.get(0),
        )
        .map_err(|source| DatabaseError::QueryRows {
            operation: "upsert_source_page",
            source,
        })?;

    if !page.raw_html.is_empty() {
        transaction
            .execute(
                r#"
                INSERT INTO page_html_archive (page_id, raw_html, captured_at)
                VALUES (?1, ?2, CURRENT_TIMESTAMP)
                ON CONFLICT(page_id) DO UPDATE SET
                    raw_html = excluded.raw_html,
                    captured_at = CURRENT_TIMESTAMP
                "#,
                params![page_id, page.raw_html],
            )
            .map_err(|source| DatabaseError::ExecuteStatement {
                operation: "archive_source_page_html",
                source,
            })?;
    }

    Ok(page_id)
}

fn upsert_image(transaction: &Transaction<'_>, image: &ScrapedImage) -> AppResult<i64> {
    Ok(transaction
        .query_row(
            r#"
            INSERT INTO images (source_url, preview_url, alt_text, width, height, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP)
            ON CONFLICT(source_url) DO UPDATE SET
                preview_url = excluded.preview_url,
                alt_text = excluded.alt_text,
                width = excluded.width,
                height = excluded.height,
                updated_at = CURRENT_TIMESTAMP
            RETURNING id
            "#,
            params![
                image.source_url,
                image.preview_url,
                image.alt_text,
                image.width,
                image.height
            ],
            |row| row.get(0),
        )
        .map_err(|source| DatabaseError::QueryRows {
            operation: "upsert_image",
            source,
        })?)
}

fn upsert_attribute(
    transaction: &Transaction<'_>,
    attribute: &MetadataAttribute,
) -> AppResult<i64> {
    let normalized_value = attribute.value.trim().to_ascii_lowercase();
    let is_female: Option<i64> = attribute.is_female.map(|f| f as i64);

    let attribute_id = transaction
        .query_row(
            r#"
            INSERT INTO attributes (kind, label, value, normalized_value, source_url, is_female)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(kind, normalized_value) DO UPDATE SET
                label = excluded.label,
                value = excluded.value,
                source_url = COALESCE(excluded.source_url, attributes.source_url),
                is_female = COALESCE(excluded.is_female, attributes.is_female)
            RETURNING id
            "#,
            params![
                attribute.kind,
                attribute.label,
                attribute.value,
                normalized_value,
                attribute.source_url,
                is_female
            ],
            |row| row.get(0),
        )
        .map_err(|source| DatabaseError::QueryRows {
            operation: "upsert_attribute",
            source,
        })?;

    refresh_attribute_search_index(transaction, attribute_id)?;

    Ok(attribute_id)
}

#[cfg(test)]
mod database_performance_tests {
    use super::*;

    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!("archivesift_database_{label}_{nanos}"))
    }

    fn seed_page_image(database: &Database) -> (i64, i64) {
        let connection = database
            .writer_connection()
            .expect("open configured database");
        connection
            .execute(
                "INSERT INTO source_pages (site_key, page_url, title, raw_html) VALUES ('demo', 'https://demo.test/gallery', 'Demo', '')",
                [],
            )
            .expect("insert page");
        let page_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO images (source_url) VALUES ('https://demo.test/image.jpg')",
                [],
            )
            .expect("insert image");
        let image_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO page_images (page_id, image_id, sort_order) VALUES (?1, ?2, 0)",
                params![page_id, image_id],
            )
            .expect("link page image");
        (page_id, image_id)
    }

    fn add_page_image(database: &Database, page_id: i64, suffix: &str) -> i64 {
        let connection = database
            .writer_connection()
            .expect("open configured database");
        connection
            .execute(
                "INSERT INTO images (source_url) VALUES (?1)",
                params![format!("https://demo.test/{suffix}.jpg")],
            )
            .expect("insert additional image");
        let image_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO page_images (page_id, image_id, sort_order) VALUES (?1, ?2, 1)",
                params![page_id, image_id],
            )
            .expect("link additional page image");
        image_id
    }

    fn add_page_attribute(database: &Database, page_id: i64, kind: &str, label: &str, value: &str) {
        let connection = database
            .writer_connection()
            .expect("open configured database");
        connection
            .execute(
                "INSERT INTO attributes (kind, label, value, normalized_value) VALUES (?1, ?2, ?3, ?4)",
                params![kind, label, value, value.to_ascii_lowercase()],
            )
            .expect("insert page attribute");
        let attribute_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO page_attributes (page_id, attribute_id) VALUES (?1, ?2)",
                params![page_id, attribute_id],
            )
            .expect("link page attribute");
    }

    #[test]
    fn bulk_id_operations_cross_sqlite_batch_boundaries() {
        let root = unique_test_root("bulk-id-batches");
        let database = Database::new(root.join("library.db"));
        database.initialize().unwrap();
        let image_count = SQLITE_ID_BATCH_SIZE + 37;
        let mut image_ids = Vec::with_capacity(image_count);

        {
            let mut connection = database.writer_connection().expect("open database");
            let transaction = connection.transaction().expect("begin seed transaction");
            transaction
                .execute(
                    "INSERT INTO source_pages (site_key, page_url, title, raw_html) VALUES ('demo', 'https://demo.test/bulk', 'Bulk', '')",
                    [],
                )
                .expect("insert page");
            let page_id = transaction.last_insert_rowid();
            transaction
                .execute(
                    "INSERT INTO attributes (kind, label, value, normalized_value) VALUES ('tag', 'Tag', 'Bulk tag', 'bulk tag')",
                    [],
                )
                .expect("insert attribute");
            let attribute_id = transaction.last_insert_rowid();
            for index in 0..image_count {
                transaction
                    .execute(
                        "INSERT INTO images (source_url) VALUES (?1)",
                        params![format!("https://demo.test/bulk-{index}.jpg")],
                    )
                    .expect("insert image");
                let image_id = transaction.last_insert_rowid();
                image_ids.push(image_id);
                transaction
                    .execute(
                        "INSERT INTO page_images (page_id, image_id, sort_order, local_file_path) VALUES (?1, ?2, ?3, ?4)",
                        params![page_id, image_id, index as i64, format!("bulk/{index}.jpg")],
                    )
                    .expect("link image");
                transaction
                    .execute(
                        "INSERT INTO image_attributes (image_id, attribute_id) VALUES (?1, ?2)",
                        params![image_id, attribute_id],
                    )
                    .expect("link attribute");
            }
            transaction.commit().expect("commit seed transaction");
        }

        let paths = database
            .get_image_local_paths(&image_ids)
            .expect("load batched paths");
        assert_eq!(paths.len(), image_count);

        let hydrated = database
            .search_images_by_ids(&image_ids)
            .expect("hydrate batched IDs");
        assert_eq!(hydrated.len(), image_count);
        assert!(hydrated.iter().all(|image| image
            .attributes
            .iter()
            .any(|attribute| attribute.value == "Bulk tag")));

        database
            .remove_image_attributes(&image_ids, "tag", &["Bulk tag".to_string()])
            .expect("remove attributes in batches");
        let hydrated = database
            .search_images_by_ids(&image_ids)
            .expect("reload batched IDs");
        assert!(hydrated.iter().all(|image| image.attributes.is_empty()));

        database
            .delete_images(&image_ids)
            .expect("delete in batches");
        assert!(database
            .search_images_by_ids(&image_ids)
            .expect("reload deleted IDs")
            .is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lightweight_search_defers_metadata_until_ids_are_hydrated() {
        let root = unique_test_root("lightweight-search");
        let database = Database::new(root.join("library.db"));
        database.initialize().unwrap();
        let (page_id, image_id) = seed_page_image(&database);
        add_page_attribute(&database, page_id, "actor", "Actor", "Alice");
        let cancellation = QueryCancellation::default();

        let page = database
            .search_images_page_lightweight(&SearchQuery::default(), None, 120, &cancellation)
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(page.items[0].attributes.is_empty());

        let hydrated = database.search_images_by_ids(&[image_id]).unwrap();
        assert_eq!(hydrated[0].actors(), vec!["Alice"]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_search_is_rejected_before_it_acquires_sqlite_work() {
        let root = unique_test_root("cancelled-search");
        let database = Database::new(root.join("library.db"));
        database.initialize().unwrap();
        seed_page_image(&database);
        let cancellation = QueryCancellation::default();
        cancellation.cancel();

        assert!(database
            .search_images_page_lightweight(&SearchQuery::default(), None, 120, &cancellation,)
            .is_err());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_recent_page_search_is_rejected_before_sqlite_work() {
        let root = unique_test_root("cancelled-recent-search");
        let database = Database::new(root.join("library.db"));
        database.initialize().unwrap();
        seed_page_image(&database);
        let cancellation = QueryCancellation::default();
        cancellation.cancel();

        assert!(database
            .search_images_recent_pages_cancellable(10, &cancellation)
            .is_err());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recent_page_search_uses_keyset_pages_and_lazy_metadata() {
        let root = unique_test_root("recent-keyset-search");
        let database = Database::new(root.join("library.db"));
        database.initialize().unwrap();
        let (page_id, _) = seed_page_image(&database);
        add_page_image(&database, page_id, "second-image");
        add_page_attribute(&database, page_id, "actor", "Actor", "Alice");

        let first = database
            .search_images_recent_pages_page_lightweight(1, None, 1, &QueryCancellation::default())
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert!(first.items[0].attributes.is_empty());
        let cursor = first
            .next_cursor
            .expect("second recent image should remain");

        let second = database
            .search_images_recent_pages_page_lightweight(
                1,
                Some(&cursor),
                1,
                &QueryCancellation::default(),
            )
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert_ne!(first.items[0].image_id, second.items[0].image_id);
        assert!(second.next_cursor.is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn date_browser_uses_cursor_order_filter_and_wraparound() {
        let root = unique_test_root("date-cursor");
        let database = Database::new(root.join("library.db"));
        database.initialize().unwrap();
        {
            let connection = database.writer_connection().unwrap();
            for (id, scraped_at, favorites) in [
                (1, "2026-01-01 00:00:00", 0),
                (2, "2026-01-02 00:00:00", 5),
                (3, "2026-01-03 00:00:00", 1),
            ] {
                connection.execute(
                    "INSERT INTO source_pages(id, site_key, page_url, title, raw_html, scraped_at, favorite_count) VALUES (?1, 'demo', ?2, ?3, '', ?4, ?5)",
                    params![id, format!("https://demo.test/{id}"), format!("Page {id}"), scraped_at, favorites],
                ).unwrap();
            }
        }

        let newest = database
            .get_date_group_page(None, DateBrowseDirection::Initial, false, Some(3))
            .unwrap();
        assert_eq!(newest.total, 2);
        assert_eq!(newest.group.as_ref().unwrap().page_id, 3);
        let cursor = DateGroupCursor {
            scraped_at: newest.group.as_ref().unwrap().scraped_at.clone(),
            page_id: 3,
        };
        let older = database
            .get_date_group_page(Some(&cursor), DateBrowseDirection::Next, false, Some(3))
            .unwrap();
        assert_eq!(older.group.as_ref().unwrap().page_id, 1);
        let oldest_cursor = DateGroupCursor {
            scraped_at: older.group.as_ref().unwrap().scraped_at.clone(),
            page_id: 1,
        };
        let wrapped = database
            .get_date_group_page(
                Some(&oldest_cursor),
                DateBrowseDirection::Next,
                false,
                Some(3),
            )
            .unwrap();
        assert_eq!(wrapped.group.as_ref().unwrap().page_id, 3);

        let _ = std::fs::remove_dir_all(root);
    }

    fn scraped_item(image_count: usize) -> ScrapedItem {
        ScrapedItem {
            site_key: "demo".to_string(),
            page_url: "https://demo.test/fts-gallery".to_string(),
            title: "Azure Sunset Gallery".to_string(),
            raw_html: "<html>diagnostic source</html>".to_string(),
            images: (0..image_count)
                .map(|index| ScrapedImage {
                    source_url: format!("https://demo.test/fts-{index}.jpg"),
                    preview_url: None,
                    alt_text: None,
                    width: None,
                    height: None,
                    sort_order: index as i64,
                    local_file_path: None,
                })
                .collect(),
            attributes: vec![MetadataAttribute {
                kind: "actor".to_string(),
                label: "Actor".to_string(),
                value: "Alice Blue".to_string(),
                source_url: None,
                is_female: Some(true),
            }],
        }
    }

    #[test]
    fn search_combines_exact_site_and_tag_filters() {
        let root = unique_test_root("site_tag_filter");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().unwrap();

        let mut example_site = scraped_item(1);
        example_site.site_key = "example_site".to_string();
        example_site.page_url = "https://source.example.test/scenes/example".to_string();
        example_site.images[0].source_url =
            "https://source.example.test/images/example".to_string();
        example_site.attributes.push(MetadataAttribute {
            kind: "tag".to_string(),
            label: "Tag".to_string(),
            value: "Outdoor".to_string(),
            source_url: None,
            is_female: None,
        });
        database.upsert_scraped_item(&example_site).unwrap();

        let mut demo = scraped_item(1);
        demo.attributes.push(MetadataAttribute {
            kind: "tag".to_string(),
            label: "Tag".to_string(),
            value: "Outdoor".to_string(),
            source_url: None,
            is_female: None,
        });
        database.upsert_scraped_item(&demo).unwrap();

        let query = SearchQuery {
            site_key: Some("example_site".to_string()),
            tags: vec!["Outdoor".to_string()],
            ..SearchQuery::default()
        };
        let results = database.search_images(&query).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page_url(), example_site.page_url);

        let no_match = database
            .search_images(&SearchQuery {
                site_key: Some("example_site".to_string()),
                tags: vec!["Missing".to_string()],
                ..SearchQuery::default()
            })
            .unwrap();
        assert!(no_match.is_empty());
        drop(database);
    }

    #[test]
    fn operational_connections_receive_performance_pragmas() {
        let root = unique_test_root("pragmas");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let connection = database.connect().expect("connect database");

        let foreign_keys: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign_keys pragma");
        let busy_timeout: i64 = connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("busy_timeout pragma");
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("synchronous pragma");
        let cache_size: i64 = connection
            .query_row("PRAGMA cache_size", [], |row| row.get(0))
            .expect("cache_size pragma");
        let temp_store: i64 = connection
            .query_row("PRAGMA temp_store", [], |row| row.get(0))
            .expect("temp_store pragma");

        assert_eq!(foreign_keys, 1);
        assert_eq!(busy_timeout, 5_000);
        assert_eq!(synchronous, 1);
        assert_eq!(cache_size, -READER_CACHE_KIB);
        assert_eq!(temp_store, 1);

        drop(connection);
        let writer = database
            .writer_connection()
            .expect("connect serialized writer");
        let writer_cache_size: i64 = writer
            .query_row("PRAGMA cache_size", [], |row| row.get(0))
            .expect("writer cache_size pragma");
        assert_eq!(writer_cache_size, -WRITER_CACHE_KIB);

        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_initialization_of_one_database_is_safe() {
        let root = unique_test_root("concurrent_init");
        let path = root.join("test.sqlite3");
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Database::new(path).initialize()
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle
                .join()
                .expect("initializer thread should not panic")
                .expect("database initialization should succeed");
        }

        let connection = Connection::open(&path).expect("open initialized database");
        let table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'page_images'",
                [],
                |row| row.get(0),
            )
            .expect("query initialized schema");
        assert_eq!(table_count, 1);

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bounded_reader_pool_reuses_connections() {
        let root = unique_test_root("reader_pool");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");

        let readers = (0..READ_POOL_SIZE)
            .map(|_| database.connect().expect("acquire pooled reader"))
            .collect::<Vec<_>>();
        assert_eq!(
            database.runtime.readers.state.lock().unwrap().total,
            READ_POOL_SIZE
        );

        drop(readers);
        let reader = database.connect().expect("reuse pooled reader");
        assert_eq!(
            database.runtime.readers.state.lock().unwrap().total,
            READ_POOL_SIZE,
            "returning and reacquiring a reader must not open another connection"
        );

        drop(reader);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn database_runtime_is_reused_across_short_lived_handles() {
        let root = unique_test_root("shared_runtime");
        let path = root.join("test.sqlite3");
        let first = Database::new(&path);
        first.initialize().expect("initialize database");
        let first_runtime = Arc::clone(&first.runtime);
        {
            let reader = first.connect().expect("open first reader");
            drop(reader);
        }
        drop(first);

        let second = Database::new(&path);
        assert!(Arc::ptr_eq(&first_runtime, &second.runtime));
        assert_eq!(
            second.runtime.readers.state.lock().unwrap().total,
            1,
            "the second handle should inherit the existing reader pool"
        );

        drop(second);
        drop(first_runtime);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn image_file_and_thumbnail_are_finalized_in_one_update() {
        let root = unique_test_root("artifacts");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let (page_id, image_id) = seed_page_image(&database);

        database
            .update_image_artifacts(
                page_id,
                image_id,
                "demo/gallery/image.jpg",
                Some("thumbs/0/1.jpg"),
            )
            .expect("finalize image artifacts");

        let connection = database.connect().expect("connect database");
        let stored: (Option<String>, Option<String>, bool) = connection
            .query_row(
                "SELECT local_file_path, thumbnail_path, downloaded_at IS NOT NULL FROM page_images WHERE page_id = ?1 AND image_id = ?2",
                params![page_id, image_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("load finalized image artifacts");

        assert_eq!(stored.0.as_deref(), Some("demo/gallery/image.jpg"));
        assert_eq!(stored.1.as_deref(), Some("thumbs/0/1.jpg"));
        assert!(stored.2);

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn content_artifacts_are_finalized_in_one_batch_transaction() {
        let root = unique_test_root("artifact_batch");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let (page_id, first_image_id) = seed_page_image(&database);
        let second_image_id = add_page_image(&database, page_id, "second");
        let updates = vec![
            ImageArtifactUpdate {
                page_id,
                image_id: first_image_id,
                file_path: "gallery/first.jpg".to_string(),
                thumbnail_path: Some("thumbs/first.jpg".to_string()),
            },
            ImageArtifactUpdate {
                page_id,
                image_id: second_image_id,
                file_path: "gallery/second.jpg".to_string(),
                thumbnail_path: Some("thumbs/second.jpg".to_string()),
            },
        ];

        database
            .update_image_artifacts_batch(&updates)
            .expect("finalize gallery artifacts");

        let connection = database.connect().expect("connect database");
        let finalized: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM page_images WHERE page_id = ?1 AND local_file_path IS NOT NULL AND thumbnail_path IS NOT NULL AND downloaded_at IS NOT NULL",
                params![page_id],
                |row| row.get(0),
            )
            .expect("count finalized artifacts");
        assert_eq!(finalized, 2);

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn content_artifact_batch_rolls_back_when_one_update_fails() {
        let root = unique_test_root("artifact_rollback");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let (page_id, first_image_id) = seed_page_image(&database);
        let second_image_id = add_page_image(&database, page_id, "rollback");
        let connection = database.connect().expect("connect database");
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER reject_second_artifact BEFORE UPDATE OF local_file_path ON page_images WHEN NEW.image_id = {second_image_id} BEGIN SELECT RAISE(ABORT, 'reject test update'); END;"
            ))
            .expect("create failure trigger");
        drop(connection);

        let error = database
            .update_image_artifacts_batch(&[
                ImageArtifactUpdate {
                    page_id,
                    image_id: first_image_id,
                    file_path: "gallery/first.jpg".to_string(),
                    thumbnail_path: None,
                },
                ImageArtifactUpdate {
                    page_id,
                    image_id: second_image_id,
                    file_path: "gallery/second.jpg".to_string(),
                    thumbnail_path: None,
                },
            ])
            .expect_err("trigger should reject the batch");
        assert!(error.to_string().contains("reject test update"));

        let connection = database.connect().expect("connect database");
        let finalized: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM page_images WHERE page_id = ?1 AND local_file_path IS NOT NULL",
                params![page_id],
                |row| row.get(0),
            )
            .expect("count rolled-back artifacts");
        assert_eq!(finalized, 0, "the first update must be rolled back too");

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn image_path_moves_are_committed_as_one_batch() {
        let root = unique_test_root("path_move_batch");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let (page_id, first_image_id) = seed_page_image(&database);
        let second_image_id = add_page_image(&database, page_id, "path-second");

        database
            .update_image_local_paths(&[
                ImageLocalPathUpdate {
                    image_id: first_image_id,
                    new_path: "site/new/gallery/first.jpg".to_string(),
                },
                ImageLocalPathUpdate {
                    image_id: second_image_id,
                    new_path: "site/new/gallery/second.jpg".to_string(),
                },
            ])
            .expect("batch path update");

        let connection = database.connect().expect("load moved paths");
        let paths = database
            .get_page_images_with_paths(page_id)
            .expect("query moved page paths");
        assert_eq!(paths.len(), 2);
        assert!(paths
            .iter()
            .all(|(_, path)| path.contains("site/new/gallery")));

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn preset_attribute_groups_roll_back_together() {
        let root = unique_test_root("preset_group_rollback");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let (_page_id, image_id) = seed_page_image(&database);
        let connection = database
            .writer_connection()
            .expect("open serialized writer");
        connection
            .execute_batch(
                r#"
                CREATE TRIGGER reject_category_link
                BEFORE INSERT ON image_attributes
                WHEN (SELECT kind FROM attributes WHERE id = NEW.attribute_id) = 'category'
                BEGIN
                    SELECT RAISE(ABORT, 'reject category link');
                END;
                "#,
            )
            .expect("create category rejection trigger");
        drop(connection);

        let error = database
            .add_image_attribute_groups(
                &[image_id],
                &[
                    ("tag", "Tag", &["fast".to_string()]),
                    ("category", "Category", &["blocked".to_string()]),
                ],
            )
            .expect_err("second group should reject transaction");
        assert!(error.to_string().contains("reject category link"));

        let connection = database.connect().expect("inspect rolled back metadata");
        let links: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM image_attributes WHERE image_id = ?1",
                params![image_id],
                |row| row.get(0),
            )
            .expect("count image metadata links");
        assert_eq!(links, 0, "all preset groups must roll back together");

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn processed_urls_batches_large_inputs_and_returns_only_complete_pages() {
        let root = unique_test_root("processed_urls");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let (page_id, image_id) = seed_page_image(&database);
        database
            .update_image_artifacts(page_id, image_id, "demo/image.jpg", None)
            .expect("finalize seeded image");

        let connection = database.connect().expect("connect database");
        connection
            .execute(
                "INSERT INTO source_pages (site_key, page_url, title, raw_html) VALUES ('demo', 'https://demo.test/incomplete', 'Incomplete', '')",
                [],
            )
            .expect("insert incomplete page");
        let incomplete_page_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO images (source_url) VALUES ('https://demo.test/incomplete.jpg')",
                [],
            )
            .expect("insert incomplete image");
        let incomplete_image_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO page_images (page_id, image_id, sort_order) VALUES (?1, ?2, 0)",
                params![incomplete_page_id, incomplete_image_id],
            )
            .expect("link incomplete image without downloaded artifacts");
        drop(connection);

        let mut urls = (0..501)
            .map(|index| format!("https://demo.test/missing/{index}"))
            .collect::<Vec<_>>();
        urls.push("https://demo.test/gallery".to_string());
        urls.push("https://demo.test/incomplete".to_string());

        let processed = database
            .processed_urls(&urls)
            .expect("query processed URLs in batches");

        assert_eq!(
            processed,
            HashSet::from(["https://demo.test/gallery".to_string()])
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn page_counters_track_downloads_favorites_and_deletes() {
        let root = unique_test_root("page_counters");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let persisted = database
            .upsert_scraped_item(&scraped_item(2))
            .expect("persist gallery");
        let mut image_ids = persisted
            .image_ids_by_url
            .values()
            .copied()
            .collect::<Vec<_>>();
        image_ids.sort_unstable();

        let connection = database.connect().expect("read counters");
        let pending: (i64, i64, i64, String) = connection
            .query_row(
                "SELECT image_count, favorite_count, pending_image_count, download_state FROM source_pages WHERE id = ?1",
                params![persisted.page_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("load pending counters");
        assert_eq!(pending, (2, 0, 2, "pending".to_string()));
        drop(connection);

        database
            .update_image_artifacts_batch(
                &image_ids
                    .iter()
                    .enumerate()
                    .map(|(index, image_id)| ImageArtifactUpdate {
                        page_id: persisted.page_id,
                        image_id: *image_id,
                        file_path: format!("gallery/{index}.jpg"),
                        thumbnail_path: None,
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("finalize gallery");
        database
            .set_favorite(image_ids[0], true)
            .expect("favorite first image");

        let connection = database.connect().expect("read complete counters");
        let complete: (i64, i64, i64, String) = connection
            .query_row(
                "SELECT image_count, favorite_count, pending_image_count, download_state FROM source_pages WHERE id = ?1",
                params![persisted.page_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("load complete counters");
        assert_eq!(complete, (2, 1, 0, "complete".to_string()));
        drop(connection);

        database
            .delete_images(&[image_ids[0]])
            .expect("delete favorited image");
        let connection = database.connect().expect("read counters after delete");
        let after_delete: (i64, i64) = connection
            .query_row(
                "SELECT image_count, favorite_count FROM source_pages WHERE id = ?1",
                params![persisted.page_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("load counters after delete");
        assert_eq!(after_delete, (1, 0));

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fts_search_and_two_phase_suggestions_find_substrings() {
        let root = unique_test_root("fts_search");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let persisted = database
            .upsert_scraped_item(&scraped_item(1))
            .expect("persist searchable gallery");
        let image_id = *persisted
            .image_ids_by_url
            .values()
            .next()
            .expect("persisted image id");
        database
            .add_image_attributes(&[image_id], "tag", "Tag", &["Forest Green".to_string()])
            .expect("add searchable image metadata");

        for text in ["zure Sunset", "lice Blu", "rest Gree"] {
            let results = database
                .search_images(&SearchQuery {
                    text: text.to_string(),
                    ..SearchQuery::default()
                })
                .expect("FTS search");
            assert_eq!(results.len(), 1, "expected FTS match for {text}");
        }

        assert_eq!(
            database
                .suggest_attribute_values("actor", "ali", 8)
                .expect("prefix suggestion"),
            vec!["Alice Blue"]
        );
        assert_eq!(
            database
                .suggest_attribute_values("actor", "ice bl", 8)
                .expect("infix FTS suggestion"),
            vec!["Alice Blue"]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn keyset_search_pages_have_no_duplicates_or_gaps() {
        let root = unique_test_root("keyset_search");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        database
            .upsert_scraped_item(&scraped_item(7))
            .expect("persist paginated gallery");

        let mut cursor = None;
        let mut image_ids = Vec::new();
        loop {
            let page = database
                .search_images_page(&SearchQuery::default(), cursor.as_ref(), 2)
                .expect("load keyset page");
            image_ids.extend(page.items.iter().map(|item| item.image_id));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        assert_eq!(image_ids.len(), 7);
        assert_eq!(unique_ids(image_ids.clone()).len(), 7);
        let mut sorted = image_ids;
        sorted.sort_unstable();
        assert!(sorted.windows(2).all(|pair| pair[0] < pair[1]));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_rows_share_page_summary_and_archived_html_leaves_hot_row_empty() {
        let root = unique_test_root("shared_page_archive");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let page = scraped_item(2);
        let expected_html = page.raw_html.clone();
        let persisted = database
            .upsert_scraped_item(&page)
            .expect("persist gallery with HTML");

        let results = database
            .search_images(&SearchQuery::default())
            .expect("load shared page results");
        assert_eq!(results.len(), 2);
        assert!(Arc::ptr_eq(&results[0].page, &results[1].page));
        assert_eq!(
            database
                .archived_page_html(persisted.page_id)
                .expect("load archived HTML"),
            Some(expected_html)
        );
        let connection = database.connect().expect("read hot source row");
        let hot_html: String = connection
            .query_row(
                "SELECT raw_html FROM source_pages WHERE id = ?1",
                params![persisted.page_id],
                |row| row.get(0),
            )
            .expect("load hot raw_html column");
        assert!(hot_html.is_empty());

        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migration_archives_legacy_html_and_removes_redundant_indexes() {
        let root = unique_test_root("derived_migration");
        std::fs::create_dir_all(&root).expect("create migration root");
        let path = root.join("test.sqlite3");
        let connection = Connection::open(&path).expect("create legacy database");
        connection
            .execute_batch(
                r#"
                CREATE TABLE source_pages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    site_key TEXT NOT NULL,
                    page_url TEXT NOT NULL UNIQUE,
                    title TEXT NOT NULL,
                    raw_html TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    scraped_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                INSERT INTO source_pages (site_key, page_url, title, raw_html)
                VALUES ('legacy', 'https://legacy.test/page', 'Legacy', '<html>legacy</html>');
                "#,
            )
            .expect("seed legacy schema");
        drop(connection);

        let database = Database::new(&path);
        database.initialize().expect("migrate legacy database");
        let connection = database.connect().expect("inspect migrated database");
        let archived: String = connection
            .query_row(
                "SELECT raw_html FROM page_html_archive WHERE page_id = 1",
                [],
                |row| row.get(0),
            )
            .expect("legacy HTML archived");
        assert_eq!(archived, "<html>legacy</html>");

        for index in [
            "idx_source_pages_scraped_at",
            "idx_images_source_url",
            "idx_attributes_kind_value",
            "idx_page_attributes_page_id",
            "idx_image_attributes_image_id",
        ] {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    params![index],
                    |row| row.get(0),
                )
                .expect("inspect redundant index");
            assert_eq!(count, 0, "redundant index {index} should be removed");
        }

        let mut plan = connection
            .prepare(
                "EXPLAIN QUERY PLAN SELECT rowid FROM page_search_fts WHERE page_search_fts MATCH '\"lega\"'",
            )
            .expect("prepare FTS query plan");
        let details = plan
            .query_map([], |row| row.get::<_, String>(3))
            .expect("query FTS plan")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("decode FTS plan");
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("VIRTUAL TABLE INDEX")),
            "FTS plan should use its virtual-table index: {details:?}"
        );

        drop(plan);
        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_results_share_page_metadata_until_image_metadata_differs() {
        let root = unique_test_root("shared_search_attributes");
        let database = Database::new(root.join("test.sqlite3"));
        database.initialize().expect("initialize database");
        let (page_id, first_image_id) = seed_page_image(&database);
        let second_image_id = add_page_image(&database, page_id, "shared-second");
        add_page_attribute(&database, page_id, "tag", "Tag", "shared");

        database
            .add_image_attributes(&[second_image_id], "tag", "Tag", &["shared".to_string()])
            .expect("add duplicate image attribute");
        let shared = database
            .search_images(&SearchQuery::default())
            .expect("search shared metadata");
        let first = shared
            .iter()
            .find(|result| result.image_id == first_image_id)
            .expect("first result");
        let second = shared
            .iter()
            .find(|result| result.image_id == second_image_id)
            .expect("second result");
        assert!(Arc::ptr_eq(&first.attributes, &second.attributes));
        assert!(Arc::ptr_eq(&first.page, &second.page));

        database
            .add_image_attributes(&[second_image_id], "actor", "Actor", &["Alice".to_string()])
            .expect("add image-specific attribute");
        let merged = database
            .search_images(&SearchQuery::default())
            .expect("search merged metadata");
        let first = merged
            .iter()
            .find(|result| result.image_id == first_image_id)
            .expect("first result");
        let second = merged
            .iter()
            .find(|result| result.image_id == second_image_id)
            .expect("second result");

        assert!(!Arc::ptr_eq(&first.attributes, &second.attributes));
        assert_eq!(first.attributes.len(), 1);
        assert_eq!(second.attributes.len(), 2);
        assert_eq!(second.attributes[0].kind, "actor");
        assert_eq!(second.attributes[1].kind, "tag");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn search_attribute_queries_deduplicate_ids_in_first_seen_order() {
        assert_eq!(unique_ids([7, 7, 3, 7, 3, 9]), vec![7, 3, 9]);
    }
}

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

#[derive(Debug)]
pub enum HttpClientError {
    RequestFailed { url: String, source: reqwest::Error },
    ResponseTooLarge { url: String, max_bytes: usize },
}

impl Display for HttpClientError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestFailed { url, source } => {
                write!(f, "HTTP request failed for {}: {}", url, source)
            }
            Self::ResponseTooLarge { url, max_bytes } => write!(
                f,
                "HTTP response for {} exceeds the {} byte HTML limit",
                url, max_bytes
            ),
        }
    }
}

impl Error for HttpClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RequestFailed { source, .. } => Some(source),
            Self::ResponseTooLarge { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum ScraperError {
    NoScraperRegistered { url: String },
    ListingNotImplemented { url: String },
}

impl Display for ScraperError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoScraperRegistered { url } => {
                write!(f, "no scraper registered for url {}", url.trim())
            }
            Self::ListingNotImplemented { url } => {
                write!(f, "`scrape_listing` not implemented for url: {}", url)
            }
        }
    }
}

impl Error for ScraperError {}

#[derive(Debug)]
pub enum PersistenceError {
    CreateParentDir {
        path: PathBuf,
        source: std::io::Error,
    },
    WriteTempFile {
        path: PathBuf,
        source: std::io::Error,
    },
    ReplaceFile {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
}

impl Display for PersistenceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateParentDir { path, source } => {
                write!(
                    f,
                    "failed to create parent directory for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::WriteTempFile { path, source } => {
                write!(
                    f,
                    "failed to write temporary file {}: {}",
                    path.display(),
                    source
                )
            }
            Self::ReplaceFile { from, to, source } => {
                write!(
                    f,
                    "failed to replace {} with {}: {}",
                    to.display(),
                    from.display(),
                    source
                )
            }
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDir { source, .. } => Some(source),
            Self::WriteTempFile { source, .. } => Some(source),
            Self::ReplaceFile { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    ParseJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    DeserializeConfig {
        path: PathBuf,
        source: serde_json::Error,
    },
    SerializeConfig {
        path: PathBuf,
        source: serde_json::Error,
    },
    PersistConfig {
        path: PathBuf,
        source: PersistenceError,
    },
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFile { path, source } => {
                write!(
                    f,
                    "failed to read config file {}: {}",
                    path.display(),
                    source
                )
            }
            Self::ParseJson { path, source } => {
                write!(
                    f,
                    "failed to parse config JSON {}: {}",
                    path.display(),
                    source
                )
            }
            Self::DeserializeConfig { path, source } => {
                write!(
                    f,
                    "failed to deserialize config data from {}: {}",
                    path.display(),
                    source
                )
            }
            Self::SerializeConfig { path, source } => {
                write!(
                    f,
                    "failed to serialize config for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::PersistConfig { path, source } => {
                write!(
                    f,
                    "failed to persist config file {}: {}",
                    path.display(),
                    source
                )
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadFile { source, .. } => Some(source),
            Self::ParseJson { source, .. } => Some(source),
            Self::DeserializeConfig { source, .. } => Some(source),
            Self::SerializeConfig { source, .. } => Some(source),
            Self::PersistConfig { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum QueuePersistenceError {
    SerializeQueue {
        path: PathBuf,
        source: serde_json::Error,
    },
    ReadQueue {
        path: PathBuf,
        source: std::io::Error,
    },
    ParseQueue {
        path: PathBuf,
        source: serde_json::Error,
    },
    PersistQueue {
        path: PathBuf,
        source: PersistenceError,
    },
}

impl Display for QueuePersistenceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SerializeQueue { path, source } => {
                write!(
                    f,
                    "failed to serialize pending queue for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::ReadQueue { path, source } => {
                write!(
                    f,
                    "failed to read persisted queue file {}: {}",
                    path.display(),
                    source
                )
            }
            Self::ParseQueue { path, source } => {
                write!(
                    f,
                    "failed to parse persisted queue JSON {}: {}",
                    path.display(),
                    source
                )
            }
            Self::PersistQueue { path, source } => {
                write!(
                    f,
                    "failed to persist pending queue file {}: {}",
                    path.display(),
                    source
                )
            }
        }
    }
}

impl Error for QueuePersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SerializeQueue { source, .. } => Some(source),
            Self::ReadQueue { source, .. } => Some(source),
            Self::ParseQueue { source, .. } => Some(source),
            Self::PersistQueue { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum DatabaseError {
    CreateDatabaseDir {
        path: PathBuf,
        source: std::io::Error,
    },
    OpenConnection {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ConfigureConnection {
        path: PathBuf,
        source: rusqlite::Error,
    },
    InitializeSchema {
        path: PathBuf,
        source: rusqlite::Error,
    },
    BeginTransaction {
        operation: &'static str,
        source: rusqlite::Error,
    },
    CommitTransaction {
        operation: &'static str,
        source: rusqlite::Error,
    },
    ExecuteStatement {
        operation: &'static str,
        source: rusqlite::Error,
    },
    PrepareStatement {
        operation: &'static str,
        source: rusqlite::Error,
    },
    QueryRows {
        operation: &'static str,
        source: rusqlite::Error,
    },
    DecodeRow {
        operation: &'static str,
        source: rusqlite::Error,
    },
    MissingRowAfterUpsert {
        entity: &'static str,
        lookup: String,
    },
}

impl Display for DatabaseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateDatabaseDir { path, source } => {
                write!(
                    f,
                    "failed to create database directory for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::OpenConnection { path, source } => {
                write!(f, "failed to open database {}: {}", path.display(), source)
            }
            Self::ConfigureConnection { path, source } => {
                write!(
                    f,
                    "failed to configure database connection for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::InitializeSchema { path, source } => {
                write!(
                    f,
                    "failed to initialize database schema for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::BeginTransaction { operation, source } => {
                write!(
                    f,
                    "failed to begin database transaction for '{}': {}",
                    operation, source
                )
            }
            Self::CommitTransaction { operation, source } => {
                write!(
                    f,
                    "failed to commit database transaction for '{}': {}",
                    operation, source
                )
            }
            Self::ExecuteStatement { operation, source } => {
                write!(f, "database statement '{}' failed: {}", operation, source)
            }
            Self::PrepareStatement { operation, source } => {
                write!(
                    f,
                    "failed to prepare database statement '{}': {}",
                    operation, source
                )
            }
            Self::QueryRows { operation, source } => {
                write!(f, "database query '{}' failed: {}", operation, source)
            }
            Self::DecodeRow { operation, source } => {
                write!(
                    f,
                    "failed to decode database row for '{}': {}",
                    operation, source
                )
            }
            Self::MissingRowAfterUpsert { entity, lookup } => {
                write!(f, "missing {} row after upsert lookup {}", entity, lookup)
            }
        }
    }
}

impl Error for DatabaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateDatabaseDir { source, .. } => Some(source),
            Self::OpenConnection { source, .. } => Some(source),
            Self::ConfigureConnection { source, .. } => Some(source),
            Self::InitializeSchema { source, .. } => Some(source),
            Self::BeginTransaction { source, .. } => Some(source),
            Self::CommitTransaction { source, .. } => Some(source),
            Self::ExecuteStatement { source, .. } => Some(source),
            Self::PrepareStatement { source, .. } => Some(source),
            Self::QueryRows { source, .. } => Some(source),
            Self::DecodeRow { source, .. } => Some(source),
            Self::MissingRowAfterUpsert { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum SearchServiceError {
    InitializeDatabase {
        source: Box<dyn Error + Send + Sync>,
    },
    QueryFailed {
        operation: &'static str,
        source: Box<dyn Error + Send + Sync>,
    },
    DeleteLocalFile {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl Display for SearchServiceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InitializeDatabase { source } => {
                write!(f, "failed to initialize search database: {}", source)
            }
            Self::QueryFailed { operation, source } => {
                write!(f, "search operation '{}' failed: {}", operation, source)
            }
            Self::DeleteLocalFile { path, source } => {
                write!(
                    f,
                    "failed to delete local image file {}: {}",
                    path.display(),
                    source
                )
            }
        }
    }
}

impl Error for SearchServiceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InitializeDatabase { source } => Some(source.as_ref()),
            Self::QueryFailed { source, .. } => Some(source.as_ref()),
            Self::DeleteLocalFile { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum FileStoreError {
    CreateParentDir {
        path: PathBuf,
        source: std::io::Error,
    },
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },
    StreamFile {
        path: PathBuf,
        source: Box<dyn Error + Send + Sync>,
    },
    ReplaceFile {
        from: PathBuf,
        to: PathBuf,
        source: std::io::Error,
    },
}

impl Display for FileStoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateParentDir { path, source } => {
                write!(
                    f,
                    "failed to create download directory for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::WriteFile { path, source } => {
                write!(
                    f,
                    "failed to write downloaded file {}: {}",
                    path.display(),
                    source
                )
            }
            Self::StreamFile { path, source } => {
                write!(
                    f,
                    "failed while streaming download to {}: {}",
                    path.display(),
                    source
                )
            }
            Self::ReplaceFile { from, to, source } => {
                write!(
                    f,
                    "failed to finalize downloaded file {} as {}: {}",
                    from.display(),
                    to.display(),
                    source
                )
            }
        }
    }
}

impl Error for FileStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDir { source, .. } => Some(source),
            Self::WriteFile { source, .. } => Some(source),
            Self::StreamFile { source, .. } => Some(source.as_ref()),
            Self::ReplaceFile { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum ThumbnailError {
    CreateParentDir {
        path: PathBuf,
        source: std::io::Error,
    },
    LoadSourceImage {
        path: PathBuf,
        source: image::ImageError,
    },
    SaveThumbnail {
        path: PathBuf,
        source: image::ImageError,
    },
    GenerateTaskFailed {
        path: PathBuf,
        source: tokio::task::JoinError,
    },
}

impl Display for ThumbnailError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateParentDir { path, source } => {
                write!(
                    f,
                    "failed to create thumbnail directory for {}: {}",
                    path.display(),
                    source
                )
            }
            Self::LoadSourceImage { path, source } => {
                write!(
                    f,
                    "failed to load source image for thumbnail {}: {}",
                    path.display(),
                    source
                )
            }
            Self::SaveThumbnail { path, source } => {
                write!(f, "failed to save thumbnail {}: {}", path.display(), source)
            }
            Self::GenerateTaskFailed { path, source } => {
                write!(
                    f,
                    "thumbnail generation task failed for {}: {}",
                    path.display(),
                    source
                )
            }
        }
    }
}

impl Error for ThumbnailError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDir { source, .. } => Some(source),
            Self::LoadSourceImage { source, .. } => Some(source),
            Self::SaveThumbnail { source, .. } => Some(source),
            Self::GenerateTaskFailed { source, .. } => Some(source),
        }
    }
}

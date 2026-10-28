use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use reqwest::Url;
use rusqlite::{params, Connection, OptionalExtension};

use crate::db::Database;
use crate::error::QueuePersistenceError;
use crate::logging::{log_non_fatal_error, DiagnosticCategory};
use crate::models::{UrlQueueDecision, UrlQueueOutcome, UrlQueueReport};
#[cfg(test)]
use crate::persistence::atomic_write;
use crate::scraper::ScraperRegistry;
use crate::AppResult;

const QUEUE_PREVIEW_LIMIT: usize = 6;
const URL_ENQUEUE_BATCH_SIZE: usize = 512;

struct QueueStore {
    connection: Mutex<Connection>,
}

pub struct UrlQueueService {
    registry: ScraperRegistry,
    store: Arc<QueueStore>,
    paused: Arc<AtomicBool>,
}

impl UrlQueueService {
    pub fn new() -> Self {
        #[cfg(test)]
        {
            let path = std::env::temp_dir().join(format!(
                "archivesift-default-queue-{}.sqlite3",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            return Self::with_registry_and_queue_path(ScraperRegistry::with_all_scrapers(), path);
        }
        #[cfg(not(test))]
        Self::with_registry(ScraperRegistry::with_all_scrapers())
    }

    pub fn with_registry(registry: ScraperRegistry) -> Self {
        let service = Self::with_registry_and_queue_path(registry, pending_queue_db_path());
        service.migrate_legacy_json(pending_queue_path());
        service
    }

    fn with_registry_and_queue_path(
        registry: ScraperRegistry,
        queue_path: impl Into<PathBuf>,
    ) -> Self {
        let queue_path = queue_path.into();
        if let Some(parent) = queue_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let connection = Connection::open(&queue_path).unwrap_or_else(|error| {
            panic!(
                "failed to open queue database {}: {error}",
                queue_path.display()
            )
        });
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA busy_timeout = 5000;
                CREATE TABLE IF NOT EXISTS queue_items (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    url TEXT NOT NULL UNIQUE,
                    connector TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('pending', 'in_progress', 'failed')),
                    attempt_count INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                CREATE INDEX IF NOT EXISTS idx_queue_items_state_id
                    ON queue_items(state, id);
                UPDATE queue_items
                SET state = 'pending', updated_at = CURRENT_TIMESTAMP
                WHERE state = 'in_progress';
                "#,
            )
            .unwrap_or_else(|error| panic!("failed to initialize queue database: {error}"));
        Self {
            registry,
            store: Arc::new(QueueStore {
                connection: Mutex::new(connection),
            }),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn clear_pending(&self) {
        let connection = self.store.connection.lock().unwrap();
        let _ = connection.execute("DELETE FROM queue_items WHERE state = 'pending'", []);
    }

    /// Full snapshot for non-UI callers. The queue window uses `snapshot_range`
    /// so its per-frame work does not scale with the complete queue length.
    pub fn snapshot(&self) -> crate::models::QueueSnapshot {
        self.snapshot_range(0..usize::MAX)
    }

    /// Returns only the requested pending-URL range plus bounded summary data.
    /// Range bounds are clamped to the current queue length.
    pub fn snapshot_range(&self, range: Range<usize>) -> crate::models::QueueSnapshot {
        self.snapshot_from_store(range).unwrap_or_default()
    }

    /// Lightweight snapshot with counts, active items, and a bounded preview.
    /// The scrollable `pending` range remains empty.
    pub fn snapshot_light(&self) -> crate::models::QueueSnapshot {
        self.snapshot_range(0..0)
    }

    fn snapshot_from_store(
        &self,
        requested: Range<usize>,
    ) -> rusqlite::Result<crate::models::QueueSnapshot> {
        let connection = self.store.connection.lock().unwrap();
        // The UI requests this snapshot frequently. Compute both counters in a
        // single table pass instead of preparing and executing one query per
        // state on every refresh.
        let (pending_total, failed_total) = connection.query_row(
            "SELECT \
                COALESCE(SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END), 0) \
             FROM queue_items",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get::<_, i64>(1)? as usize,
                ))
            },
        )?;
        let pending_start = requested.start.min(pending_total);
        let pending_end = requested.end.max(pending_start).min(pending_total);
        let load_urls = |offset: usize, limit: usize| -> rusqlite::Result<Vec<String>> {
            let mut statement = connection.prepare_cached(
                "SELECT url FROM queue_items WHERE state = 'pending' ORDER BY id LIMIT ?1 OFFSET ?2",
            )?;
            let urls = statement
                .query_map(params![limit as i64, offset as i64], |row| row.get(0))?
                .collect();
            urls
        };
        let mut active_statement = connection.prepare_cached(
            "SELECT url, connector FROM queue_items WHERE state = 'in_progress' ORDER BY id",
        )?;
        let in_progress = active_statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(crate::models::QueueSnapshot {
            paused: self.is_paused(),
            in_progress,
            pending_total,
            failed_total,
            pending_start,
            pending: load_urls(pending_start, pending_end - pending_start)?,
            pending_preview: load_urls(0, QUEUE_PREVIEW_LIMIT)?,
        })
    }

    fn migrate_legacy_json(&self, legacy_path: &Path) {
        let urls = match read_persisted_queue_urls_at(legacy_path) {
            Ok(urls) => urls,
            Err(QueuePersistenceError::ReadQueue { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return
            }
            Err(error) => {
                log_non_fatal_error(DiagnosticCategory::Queue, "migrate legacy queue", &error);
                return;
            }
        };
        let mut connection = self.store.connection.lock().unwrap();
        let Ok(transaction) = connection.transaction() else {
            return;
        };
        for url in urls {
            let connector = url_hostname(&url);
            let _ = transaction.execute(
                "INSERT OR IGNORE INTO queue_items(url, connector, state) VALUES (?1, ?2, 'pending')",
                params![url, connector],
            );
        }
        if transaction.commit().is_ok() {
            let migrated_path = legacy_path.with_extension("json.migrated");
            if !migrated_path.exists() {
                let _ = fs::rename(legacy_path, migrated_path);
            }
        }
    }

    pub fn prune_processed(&self, database: &Database) -> AppResult<()> {
        const PRUNE_BATCH_SIZE: usize = 512;

        let mut offset = 0;
        loop {
            let snapshot = self.snapshot_from_store(offset..offset + PRUNE_BATCH_SIZE)?;
            if snapshot.pending.is_empty() {
                break;
            }
            let inspected = snapshot.pending.len();
            let processed = database.processed_urls(&snapshot.pending)?;
            let processed_count = processed.len();
            if processed_count > 0 {
                let mut connection = self.store.connection.lock().unwrap();
                let transaction = connection.transaction()?;
                for url in processed {
                    transaction.execute("DELETE FROM queue_items WHERE url = ?1", params![url])?;
                }
                transaction.commit()?;
            }

            // Deleted rows shift the next uninspected row towards the front.
            offset += inspected - processed_count;
        }
        Ok(())
    }

    pub fn enqueue_urls<I>(&self, database: &Database, urls: I) -> AppResult<UrlQueueReport>
    where
        I: IntoIterator<Item = String>,
    {
        self.enqueue_urls_internal(database, urls, true, false)
    }

    pub(crate) fn enqueue_urls_summary<I>(
        &self,
        database: &Database,
        urls: I,
        retain_processed_decisions: bool,
    ) -> AppResult<UrlQueueReport>
    where
        I: IntoIterator<Item = String>,
    {
        self.enqueue_urls_internal(database, urls, false, retain_processed_decisions)
    }

    fn enqueue_urls_internal<I>(
        &self,
        database: &Database,
        urls: I,
        include_decisions: bool,
        retain_processed_decisions: bool,
    ) -> AppResult<UrlQueueReport>
    where
        I: IntoIterator<Item = String>,
    {
        database.initialize()?;

        let mut report = UrlQueueReport::default();
        let mut input = urls.into_iter();
        loop {
            let prepared = input
                .by_ref()
                .take(URL_ENQUEUE_BATCH_SIZE)
                .map(|raw_input| {
                    let normalized_url = normalize_supported_url(&self.registry, &raw_input);
                    (raw_input, normalized_url)
                })
                .collect::<Vec<_>>();
            if prepared.is_empty() {
                break;
            }

            report.loaded += prepared.len();
            let normalized_urls = prepared
                .iter()
                .filter_map(|(_, normalized)| normalized.as_ref())
                .cloned()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let processed_urls = database.processed_urls(&normalized_urls)?;

            let mut connection = self.store.connection.lock().unwrap();
            let transaction = connection.transaction()?;
            for (raw_input, normalized_url) in prepared {
                let Some(normalized_url) = normalized_url else {
                    let outcome = if raw_input.trim().is_empty() {
                        UrlQueueOutcome::Empty
                    } else {
                        UrlQueueOutcome::Invalid
                    };
                    report.empty_or_invalid += 1;
                    if include_decisions {
                        report.decisions.push(UrlQueueDecision {
                            raw_input,
                            normalized_url: None,
                            outcome,
                        });
                    }
                    continue;
                };

                if processed_urls.contains(&normalized_url) {
                    report.duplicate_or_processed += 1;
                    if include_decisions || retain_processed_decisions {
                        report.decisions.push(UrlQueueDecision {
                            raw_input,
                            normalized_url: Some(normalized_url),
                            outcome: UrlQueueOutcome::AlreadyProcessed,
                        });
                    }
                    continue;
                }

                let connector = url_hostname(&normalized_url);
                let inserted = transaction.execute(
                    "INSERT OR IGNORE INTO queue_items(url, connector, state) VALUES (?1, ?2, 'pending')",
                    params![normalized_url, connector],
                )?;
                if inserted == 0 {
                    report.duplicate_or_processed += 1;
                    if include_decisions {
                        report.decisions.push(UrlQueueDecision {
                            raw_input,
                            normalized_url: Some(normalized_url),
                            outcome: UrlQueueOutcome::DuplicateQueued,
                        });
                    }
                    continue;
                }

                report.queued_successfully += 1;
                if include_decisions {
                    report.decisions.push(UrlQueueDecision {
                        raw_input,
                        normalized_url: Some(normalized_url),
                        outcome: UrlQueueOutcome::Queued,
                    });
                }
            }
            transaction.commit()?;
        }

        Ok(report)
    }

    pub fn enqueue_text(&self, database: &Database, contents: &str) -> AppResult<UrlQueueReport> {
        let urls = extract_candidate_urls(contents);
        // If the text contains no http(s) candidates at all, return a no-op report
        // so callers can skip logging entirely instead of reporting "N invalid".
        if urls.is_empty() {
            return Ok(UrlQueueReport::default());
        }
        self.enqueue_urls(database, urls)
    }

    pub fn import_urls_from_file(
        &self,
        database: &Database,
        path: &Path,
    ) -> AppResult<UrlQueueReport> {
        self.import_urls_from_file_internal(database, path, true, false)
    }

    /// Bulk-import variant used by the desktop UI. It preserves aggregate
    /// counts but omits one decision allocation per input line.
    pub fn import_urls_from_file_summary(
        &self,
        database: &Database,
        path: &Path,
        retain_processed_decisions: bool,
    ) -> AppResult<UrlQueueReport> {
        self.import_urls_from_file_internal(database, path, false, retain_processed_decisions)
    }

    fn import_urls_from_file_internal(
        &self,
        database: &Database,
        path: &Path,
        include_decisions: bool,
        retain_processed_decisions: bool,
    ) -> AppResult<UrlQueueReport> {
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut report = UrlQueueReport::default();
        let mut batch = Vec::with_capacity(URL_ENQUEUE_BATCH_SIZE);

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            batch.push(trimmed.to_string());
            if batch.len() == URL_ENQUEUE_BATCH_SIZE {
                let partial = self.enqueue_urls_internal(
                    database,
                    std::mem::take(&mut batch),
                    include_decisions,
                    retain_processed_decisions,
                )?;
                merge_queue_report(&mut report, partial);
                batch = Vec::with_capacity(URL_ENQUEUE_BATCH_SIZE);
            }
        }
        if !batch.is_empty() {
            let partial = self.enqueue_urls_internal(
                database,
                batch,
                include_decisions,
                retain_processed_decisions,
            )?;
            merge_queue_report(&mut report, partial);
        }
        Ok(report)
    }

    pub fn take_next_url(&self) -> Option<String> {
        // Check paused BEFORE taking the lock
        if self.is_paused() {
            return None;
        }
        self.claim_next_url().ok().flatten()
    }

    fn claim_next_url(&self) -> rusqlite::Result<Option<String>> {
        let mut connection = self.store.connection.lock().unwrap();
        let transaction = connection.transaction()?;
        let next = transaction
            .query_row(
                "SELECT id, url FROM queue_items WHERE state = 'pending' ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((id, url)) = next else {
            transaction.commit()?;
            return Ok(None);
        };
        transaction.execute(
            "UPDATE queue_items SET state = 'in_progress', attempt_count = attempt_count + 1, last_error = NULL, updated_at = CURRENT_TIMESTAMP WHERE id = ?1 AND state = 'pending'",
            params![id],
        )?;
        transaction.commit()?;
        Ok(Some(url))
    }

    pub fn mark_finished(&self, url: &str) {
        let connection = self.store.connection.lock().unwrap();
        let _ = connection.execute("DELETE FROM queue_items WHERE url = ?1", params![url]);
    }

    pub fn mark_failed(&self, url: &str, error: &str) {
        let connection = self.store.connection.lock().unwrap();
        let _ = connection.execute(
            r#"
            INSERT INTO queue_items(url, connector, state, attempt_count, last_error)
            VALUES (?1, ?2, 'failed', 1, ?3)
            ON CONFLICT(url) DO UPDATE SET
                state = 'failed',
                last_error = excluded.last_error,
                updated_at = CURRENT_TIMESTAMP
            "#,
            params![url, url_hostname(url), error],
        );
    }

    pub fn retry_failed(&self) -> usize {
        let connection = self.store.connection.lock().unwrap();
        connection.execute(
            "UPDATE queue_items SET state = 'pending', updated_at = CURRENT_TIMESTAMP WHERE state = 'failed'",
            [],
        ).unwrap_or(0)
    }

    pub fn has_pending_work(&self) -> bool {
        let connection = self.store.connection.lock().unwrap();
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM queue_items WHERE state IN ('pending', 'in_progress'))",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false)
    }
}

fn merge_queue_report(target: &mut UrlQueueReport, mut partial: UrlQueueReport) {
    target.loaded += partial.loaded;
    target.empty_or_invalid += partial.empty_or_invalid;
    target.duplicate_or_processed += partial.duplicate_or_processed;
    target.queued_successfully += partial.queued_successfully;
    target.decisions.append(&mut partial.decisions);
}

fn pending_queue_path() -> &'static Path {
    Path::new("data/pending-queue.json")
}

fn pending_queue_db_path() -> &'static Path {
    Path::new("data/pending-queue.sqlite3")
}

#[cfg(test)]
fn persist_queue_urls_at(path: &Path, urls: &[String]) -> Result<(), QueuePersistenceError> {
    let json =
        serde_json::to_vec(urls).map_err(|source| QueuePersistenceError::SerializeQueue {
            path: path.to_path_buf(),
            source,
        })?;
    atomic_write(path, &json).map_err(|source| QueuePersistenceError::PersistQueue {
        path: path.to_path_buf(),
        source,
    })
}

fn read_persisted_queue_urls_at(path: &Path) -> Result<Vec<String>, QueuePersistenceError> {
    let content = fs::read_to_string(path).map_err(|source| QueuePersistenceError::ReadQueue {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str::<Vec<String>>(&content).map_err(|source| {
        QueuePersistenceError::ParseQueue {
            path: path.to_path_buf(),
            source,
        }
    })
}

fn url_hostname(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(ToString::to_string))
        .unwrap_or_default()
}

fn normalize_supported_url(registry: &ScraperRegistry, raw_input: &str) -> Option<String> {
    let candidate = raw_input.trim();
    if candidate.is_empty() {
        return None;
    }

    let parsed = Url::parse(candidate).ok()?;
    let mut parsed = parsed;
    parsed.set_fragment(None);

    let mut normalized = parsed.to_string();
    if !normalized.ends_with('/') && !parsed.path().ends_with('/') {
        normalized.push('/');
    }

    if registry.supports_url(&normalized) {
        Some(normalized)
    } else {
        None
    }
}

/// Extracts all http(s) URL candidates from arbitrary text.
/// Only tokens that start with `http://` or `https://` are returned so that
/// plain text words do not pollute the queue as "invalid" entries.
pub fn extract_candidate_urls(contents: &str) -> Vec<String> {
    contents
        .split_whitespace()
        .map(str::trim)
        .filter(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── extract_candidate_urls ─────────────────────────────────────────────────

    #[test]
    fn extract_single_https_url() {
        let result = extract_candidate_urls("https://example.com/gallery/123/");
        assert_eq!(result, vec!["https://example.com/gallery/123/"]);
    }

    #[test]
    fn extract_single_http_url() {
        let result = extract_candidate_urls("http://example.com/page/");
        assert_eq!(result, vec!["http://example.com/page/"]);
    }

    #[test]
    fn extract_multiple_urls_from_plain_text() {
        let text = "Check out https://site.com/a and also https://site.com/b for more";
        let result = extract_candidate_urls(text);
        assert_eq!(result, vec!["https://site.com/a", "https://site.com/b"]);
    }

    #[test]
    fn extract_multiple_urls_newline_separated() {
        let text = "https://site.com/a\nhttps://site.com/b\nhttps://site.com/c";
        let result = extract_candidate_urls(text);
        assert_eq!(
            result,
            vec![
                "https://site.com/a",
                "https://site.com/b",
                "https://site.com/c",
            ]
        );
    }

    #[test]
    fn extract_returns_empty_for_plain_text_no_urls() {
        let result = extract_candidate_urls("Hello world, this is just text");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_returns_empty_for_empty_string() {
        let result = extract_candidate_urls("");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_returns_empty_for_whitespace_only() {
        let result = extract_candidate_urls("   \n\t  ");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_ignores_ftp_and_other_schemes() {
        let text = "ftp://files.example.com/file.zip mailto:user@example.com https://ok.com/";
        let result = extract_candidate_urls(text);
        assert_eq!(result, vec!["https://ok.com/"]);
    }

    #[test]
    fn extract_handles_mixed_whitespace_separators() {
        let text = "  https://a.com/1   \t  https://b.com/2  \n  https://c.com/3  ";
        let result = extract_candidate_urls(text);
        assert_eq!(
            result,
            vec!["https://a.com/1", "https://b.com/2", "https://c.com/3",]
        );
    }

    #[test]
    fn extract_deduplicates_nothing_returns_all_occurrences() {
        // Deduplication is the queue's responsibility, not extract's.
        let text = "https://site.com/a https://site.com/a";
        let result = extract_candidate_urls(text);
        assert_eq!(result.len(), 2);
    }

    // ── enqueue_urls / enqueue_text ───────────────────────────────────────────

    /// Public demo content URL that the default registry supports.
    const CONTENT_URL: &str = "https://demo.example.test/demo-site/galleries/summer-escape-001/";

    /// Create a throwaway database in the system temp dir.
    fn temp_db() -> (crate::db::Database, std::path::PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let path = std::env::temp_dir().join(format!("archivesift_queue_test_{}.db", nanos));
        let db = crate::db::Database::new(&path);
        db.initialize().expect("db init failed");
        (db, path)
    }

    /// Insert a fully-downloaded page so the queue recognises it as already downloaded.
    fn mark_processed(db_path: &std::path::Path, url: &str) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO source_pages (site_key, page_url, title, raw_html) \
             VALUES ('test', ?1, 'Test', '')",
            rusqlite::params![url],
        )
        .unwrap();
        let page_id: i64 = conn
            .query_row(
                "SELECT id FROM source_pages WHERE page_url = ?1",
                rusqlite::params![url],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO images (source_url, alt_text) VALUES (?1, 'img')",
            rusqlite::params![format!("{url}image.jpg")],
        )
        .unwrap();
        let image_id: i64 = conn
            .query_row(
                "SELECT id FROM images WHERE source_url = ?1",
                rusqlite::params![format!("{url}image.jpg")],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO page_images (page_id, image_id, sort_order, local_file_path, downloaded_at) \
             VALUES (?1, ?2, 0, 'downloads/test.jpg', CURRENT_TIMESTAMP)",
            rusqlite::params![page_id, image_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE source_pages SET image_count = 1, pending_image_count = 0, download_state = 'complete' WHERE id = ?1",
            rusqlite::params![page_id],
        )
        .unwrap();
    }

    #[test]
    fn enqueue_new_url_is_queued() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();

        let report = svc
            .enqueue_urls(&db, vec![CONTENT_URL.to_string()])
            .expect("enqueue failed");

        assert_eq!(report.queued_successfully, 1, "new URL must be queued");
        assert_eq!(report.duplicate_or_processed, 0);
        assert!(matches!(
            report.decisions[0].outcome,
            UrlQueueOutcome::Queued
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enqueue_already_processed_url_returns_already_processed() {
        let (db, path) = temp_db();
        // Normalise URL the same way the service does: append trailing slash.
        let normalized = format!("{}", CONTENT_URL);
        mark_processed(&path, &normalized);

        let svc = UrlQueueService::new();
        let report = svc
            .enqueue_urls(&db, vec![CONTENT_URL.to_string()])
            .expect("enqueue failed");

        assert_eq!(report.duplicate_or_processed, 1);
        assert_eq!(report.queued_successfully, 0);
        assert!(matches!(
            report.decisions[0].outcome,
            UrlQueueOutcome::AlreadyProcessed
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enqueue_duplicate_in_queue_returns_duplicate_queued() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();

        // First enqueue succeeds.
        svc.enqueue_urls(&db, vec![CONTENT_URL.to_string()])
            .unwrap();
        // Second enqueue of same URL while still in queue → DuplicateQueued.
        let report = svc
            .enqueue_urls(&db, vec![CONTENT_URL.to_string()])
            .expect("second enqueue failed");

        assert_eq!(report.duplicate_or_processed, 1);
        assert!(matches!(
            report.decisions[0].outcome,
            UrlQueueOutcome::DuplicateQueued
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enqueue_invalid_url_returns_invalid() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();

        let report = svc
            .enqueue_urls(
                &db,
                vec!["https://not-a-supported-site.com/page/".to_string()],
            )
            .expect("enqueue failed");

        assert_eq!(report.empty_or_invalid, 1);
        assert!(matches!(
            report.decisions[0].outcome,
            UrlQueueOutcome::Invalid
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enqueue_empty_string_returns_empty() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();

        let report = svc
            .enqueue_urls(&db, vec!["   ".to_string()])
            .expect("enqueue failed");

        assert_eq!(report.empty_or_invalid, 1);
        assert!(matches!(
            report.decisions[0].outcome,
            UrlQueueOutcome::Empty
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enqueue_text_with_valid_url_queues_it() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();
        let text = format!("check this out: {}", CONTENT_URL);

        let report = svc.enqueue_text(&db, &text).expect("enqueue_text failed");

        assert_eq!(report.queued_successfully, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enqueue_text_with_no_urls_returns_empty_report() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();

        let report = svc
            .enqueue_text(&db, "just some plain text without urls")
            .expect("enqueue_text failed");

        // The report should be the default (no decisions at all).
        assert_eq!(report.loaded, 0);
        assert_eq!(report.queued_successfully, 0);
        assert!(report.decisions.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enqueue_text_already_processed_returns_already_processed() {
        let (db, path) = temp_db();
        mark_processed(&path, CONTENT_URL);
        let svc = UrlQueueService::new();
        let text = format!("see this content item: {}", CONTENT_URL);

        let report = svc.enqueue_text(&db, &text).expect("enqueue_text failed");

        assert_eq!(report.duplicate_or_processed, 1);
        assert!(matches!(
            report.decisions[0].outcome,
            UrlQueueOutcome::AlreadyProcessed
        ));
        let _ = std::fs::remove_file(path);
    }

    fn temp_queue_file(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("archivesift_queue_{label}_{nanos}.json"))
    }

    #[test]
    fn large_summary_import_preserves_counts_without_per_url_decisions() {
        let (database, database_path) = temp_db();
        let queue_path = temp_queue_file("summary-import-queue");
        let import_path = temp_queue_file("summary-import-input").with_extension("txt");
        let service = UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_public_scrapers(),
            &queue_path,
        );

        let mut lines = (0..2_000)
            .map(|index| {
                format!("https://demo.example.test/demo-site/galleries/import-{index:04}/")
            })
            .collect::<Vec<_>>();
        lines.push(lines[0].clone());
        lines.push("not-a-url".to_string());
        std::fs::write(&import_path, lines.join("\n")).expect("write import fixture");

        let report = service
            .import_urls_from_file_summary(&database, &import_path, false)
            .expect("large summary import should succeed");

        assert_eq!(report.loaded, 2_002);
        assert_eq!(report.queued_successfully, 2_000);
        assert_eq!(report.empty_or_invalid, 1);
        assert_eq!(report.duplicate_or_processed, 1);
        assert!(report.decisions.is_empty());
        assert_eq!(service.snapshot_light().pending_total, 2_000);

        let _ = std::fs::remove_file(import_path);
        let _ = std::fs::remove_file(queue_path);
        let _ = std::fs::remove_file(database_path);
    }

    #[test]
    fn discovered_url_summary_batches_without_retaining_decisions() {
        let (database, database_path) = temp_db();
        let queue_path = temp_queue_file("discovered-summary-queue");
        let service = UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_public_scrapers(),
            &queue_path,
        );
        let urls = (0..(URL_ENQUEUE_BATCH_SIZE + 17)).map(|index| {
            format!("https://demo.example.test/demo-site/galleries/discovered-{index:04}/")
        });

        let report = service
            .enqueue_urls_summary(&database, urls, false)
            .expect("enqueue discovered URL summary");

        assert_eq!(report.loaded, URL_ENQUEUE_BATCH_SIZE + 17);
        assert_eq!(report.queued_successfully, URL_ENQUEUE_BATCH_SIZE + 17);
        assert!(report.decisions.is_empty());

        let _ = std::fs::remove_file(queue_path);
        let _ = std::fs::remove_file(database_path);
    }

    #[test]
    fn summary_import_can_retain_only_processed_decisions_for_rescrape_prompts() {
        let (database, database_path) = temp_db();
        let queue_path = temp_queue_file("summary-import-prompts-queue");
        let import_path = temp_queue_file("summary-import-prompts-input").with_extension("txt");
        let service = UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_public_scrapers(),
            &queue_path,
        );
        let new_url = "https://demo.example.test/demo-site/galleries/new-import-002/";
        mark_processed(&database_path, CONTENT_URL);
        std::fs::write(&import_path, format!("{CONTENT_URL}\n{new_url}\nnot-a-url"))
            .expect("write import fixture");

        let report = service
            .import_urls_from_file_summary(&database, &import_path, true)
            .expect("summary import should succeed");

        assert_eq!(report.loaded, 3);
        assert_eq!(report.queued_successfully, 1);
        assert_eq!(report.empty_or_invalid, 1);
        assert_eq!(report.duplicate_or_processed, 1);
        assert_eq!(report.decisions.len(), 1);
        assert!(matches!(
            report.decisions[0].outcome,
            UrlQueueOutcome::AlreadyProcessed
        ));

        let _ = std::fs::remove_file(import_path);
        let _ = std::fs::remove_file(queue_path);
        let _ = std::fs::remove_file(database_path);
    }

    #[test]
    fn persisted_queue_round_trips_urls_in_order() {
        let path = temp_queue_file("roundtrip");
        let urls = vec![
            "https://demo.example.test/demo-site/galleries/summer-escape-001/".to_string(),
            "https://demo.example.test/demo-site/galleries/city-lights-002/".to_string(),
        ];

        persist_queue_urls_at(&path, &urls).expect("persist queue should succeed");
        let restored =
            read_persisted_queue_urls_at(&path).expect("read persisted queue should succeed");

        assert_eq!(restored, urls);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_json_queue_migrates_once_into_sqlite() {
        let json_path = temp_queue_file("legacy-migration");
        let sqlite_path = json_path.with_extension("sqlite3");
        let urls = vec![
            CONTENT_URL.to_string(),
            "https://demo.example.test/demo-site/galleries/city-lights-002/".to_string(),
        ];
        persist_queue_urls_at(&json_path, &urls).unwrap();
        let service = UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_all_scrapers(),
            &sqlite_path,
        );

        service.migrate_legacy_json(&json_path);

        assert_eq!(service.snapshot().pending, urls);
        assert!(!json_path.exists());
        assert!(json_path.with_extension("json.migrated").exists());

        drop(service);
        let _ = std::fs::remove_file(sqlite_path);
        let _ = std::fs::remove_file(json_path.with_extension("json.migrated"));
    }

    #[test]
    fn sqlite_queue_recovers_claimed_work_after_restart() {
        let (db, db_path) = temp_db();
        let queue_path = temp_queue_file("drain");
        let service = UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_all_scrapers(),
            &queue_path,
        );
        let urls = vec![
            CONTENT_URL.to_string(),
            "https://demo.example.test/demo-site/galleries/city-lights-002/".to_string(),
        ];
        service
            .enqueue_urls(&db, urls.clone())
            .expect("enqueue test URLs");

        assert_eq!(service.take_next_url().as_deref(), Some(urls[0].as_str()));
        drop(service);

        let restored = UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_all_scrapers(),
            &queue_path,
        );
        assert_eq!(restored.snapshot().pending, urls);

        while let Some(url) = restored.take_next_url() {
            restored.mark_finished(&url);
        }
        assert!(!restored.has_pending_work());

        let _ = std::fs::remove_file(queue_path);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn sqlite_queue_snapshot_includes_active_and_new_work() {
        let (db, db_path) = temp_db();
        let queue_path = temp_queue_file("active-recovery");
        let service = UrlQueueService::with_registry_and_queue_path(
            ScraperRegistry::with_all_scrapers(),
            &queue_path,
        );
        let first = CONTENT_URL.to_string();
        let second = "https://demo.example.test/demo-site/galleries/city-lights-002/".to_string();
        service
            .enqueue_urls(&db, vec![first.clone(), second.clone()])
            .expect("enqueue initial URLs");
        assert_eq!(service.take_next_url().as_deref(), Some(first.as_str()));

        let third = "https://demo.example.test/demo-site/galleries/winter-trails-003/".to_string();
        service
            .enqueue_urls(&db, vec![third.clone()])
            .expect("enqueue URL while work is active");
        let snapshot = service.snapshot();
        let persisted = snapshot
            .pending
            .into_iter()
            .chain(snapshot.in_progress.into_iter().map(|(url, _)| url))
            .collect::<HashSet<_>>();

        assert_eq!(persisted, HashSet::from([first, second, third]));

        let _ = std::fs::remove_file(queue_path);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn read_persisted_queue_reports_parse_error_for_invalid_json() {
        let path = temp_queue_file("invalid");
        std::fs::write(&path, b"{not valid json]").expect("seed invalid queue file");

        let error = read_persisted_queue_urls_at(&path).expect_err("invalid json should fail");

        assert!(matches!(error, QueuePersistenceError::ParseQueue { .. }));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn take_next_url_moves_entry_to_in_progress_with_hostname_label() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();
        svc.enqueue_urls(&db, vec![CONTENT_URL.to_string()])
            .expect("enqueue failed");

        let next = svc.take_next_url();
        let snapshot = svc.snapshot();

        assert_eq!(next.as_deref(), Some(CONTENT_URL));
        assert!(snapshot.pending.is_empty());
        assert_eq!(
            snapshot.in_progress,
            vec![(CONTENT_URL.to_string(), "demo.example.test".to_string())]
        );
        assert!(
            svc.has_pending_work(),
            "in-progress work should count as pending work"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mark_finished_clears_in_progress_and_pending_work() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();
        svc.enqueue_urls(&db, vec![CONTENT_URL.to_string()])
            .expect("enqueue failed");
        let url = svc.take_next_url().expect("queued url should be available");

        svc.mark_finished(&url);

        let snapshot = svc.snapshot();
        assert!(snapshot.pending.is_empty());
        assert!(snapshot.in_progress.is_empty());
        assert!(!svc.has_pending_work());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_item_is_persisted_and_can_be_retried() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();
        svc.enqueue_urls(&db, vec![CONTENT_URL.to_string()])
            .expect("enqueue failed");
        let url = svc.take_next_url().expect("queued url should be available");

        svc.mark_failed(&url, "temporary network error");

        let failed = svc.snapshot();
        assert_eq!(failed.failed_total, 1);
        assert_eq!(failed.pending_total, 0);
        assert!(failed.in_progress.is_empty());
        assert!(!svc.has_pending_work());

        assert_eq!(svc.retry_failed(), 1);
        let retried = svc.snapshot();
        assert_eq!(retried.failed_total, 0);
        assert_eq!(retried.pending_total, 1);
        assert!(svc.has_pending_work());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_counts_mixed_queue_states_in_one_consistent_view() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();
        let urls = vec![
            CONTENT_URL.to_string(),
            "https://demo.example.test/demo-site/galleries/city-lights-002/".to_string(),
            "https://demo.example.test/demo-site/galleries/winter-trails-003/".to_string(),
        ];
        svc.enqueue_urls(&db, urls).expect("enqueue URLs");

        let failed_url = svc.take_next_url().expect("first queued URL");
        svc.mark_failed(&failed_url, "test failure");
        let active_url = svc.take_next_url().expect("second queued URL");

        let snapshot = svc.snapshot();
        assert_eq!(snapshot.pending_total, 1);
        assert_eq!(snapshot.failed_total, 1);
        assert_eq!(snapshot.in_progress.len(), 1);
        assert_eq!(snapshot.in_progress[0].0, active_url);
        assert_eq!(snapshot.pending.len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_light_hides_pending_urls_but_preserves_pause_state() {
        let (db, path) = temp_db();
        let svc = UrlQueueService::new();
        svc.enqueue_urls(
            &db,
            vec![
                CONTENT_URL.to_string(),
                "https://demo.example.test/demo-site/galleries/city-lights-002/".to_string(),
            ],
        )
        .expect("enqueue failed");
        svc.pause();

        let full = svc.snapshot();
        let light = svc.snapshot_light();

        assert!(full.paused);
        assert_eq!(full.pending.len(), 2);
        assert_eq!(full.pending_total, 2);
        assert!(light.paused);
        assert!(light.pending.is_empty());
        assert_eq!(light.pending_total, 2);
        assert_eq!(light.pending_preview, full.pending);
        assert_eq!(light.in_progress, full.in_progress);

        svc.resume();
        assert!(!svc.is_paused());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_range_clamps_bounds_and_copies_only_requested_rows() {
        let service = UrlQueueService::new();
        {
            let mut connection = service.store.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for index in 0..10 {
                let url = format!("https://demo.example.test/gallery/{index}/");
                transaction.execute(
                    "INSERT INTO queue_items(url, connector, state) VALUES (?1, 'demo.example.test', 'pending')",
                    params![url],
                ).unwrap();
            }
            transaction.commit().unwrap();
        }

        let middle = service.snapshot_range(3..6);
        assert_eq!(middle.pending_total, 10);
        assert_eq!(middle.pending_start, 3);
        assert_eq!(middle.pending.len(), 3);
        assert_eq!(middle.pending[0], "https://demo.example.test/gallery/3/");
        assert_eq!(middle.pending[2], "https://demo.example.test/gallery/5/");
        assert_eq!(middle.pending_preview.len(), QUEUE_PREVIEW_LIMIT);

        let tail = service.snapshot_range(8..usize::MAX);
        assert_eq!(tail.pending_start, 8);
        assert_eq!(tail.pending.len(), 2);

        let past_end = service.snapshot_range(20..30);
        assert_eq!(past_end.pending_start, 10);
        assert!(past_end.pending.is_empty());
        assert_eq!(past_end.pending_total, 10);
    }

    #[test]
    fn large_queue_snapshot_memory_is_bounded_by_requested_range() {
        let service = UrlQueueService::new();
        {
            let mut connection = service.store.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for index in 0..10_000 {
                transaction.execute(
                    "INSERT INTO queue_items(url, connector, state) VALUES (?1, 'demo.example.test', 'pending')",
                    params![format!("https://demo.example.test/gallery/{index}/")],
                ).unwrap();
            }
            transaction.commit().unwrap();
        }

        let snapshot = service.snapshot_range(5_000..5_064);

        assert_eq!(snapshot.pending_total, 10_000);
        assert_eq!(snapshot.pending_start, 5_000);
        assert_eq!(snapshot.pending.len(), 64);
        assert_eq!(snapshot.pending_preview.len(), QUEUE_PREVIEW_LIMIT);
    }

    #[test]
    fn prune_processed_scans_in_batches_without_skipping_shifted_rows() {
        let (db, db_path) = temp_db();
        let service = UrlQueueService::new();
        let processed_url = CONTENT_URL.to_string();
        let pending_url =
            "https://demo.example.test/demo-site/galleries/city-lights-002/".to_string();
        mark_processed(&db_path, &processed_url);
        {
            let mut connection = service.store.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for url in [&processed_url, &pending_url] {
                transaction
                    .execute(
                        "INSERT INTO queue_items(url, connector, state) VALUES (?1, 'demo.example.test', 'pending')",
                        params![url],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }

        service.prune_processed(&db).unwrap();

        assert_eq!(service.snapshot().pending, vec![pending_url]);
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn url_hostname_extracts_host_or_returns_empty_string() {
        assert_eq!(url_hostname(CONTENT_URL), "demo.example.test");
        assert_eq!(url_hostname("not a url"), "");
    }

    #[test]
    fn normalize_supported_url_removes_fragment_and_adds_trailing_slash() {
        let registry = ScraperRegistry::with_public_scrapers();

        let normalized = normalize_supported_url(
            &registry,
            "https://demo.example.test/demo-site/galleries/summer-escape-001#top",
        );

        assert_eq!(
            normalized.as_deref(),
            Some("https://demo.example.test/demo-site/galleries/summer-escape-001/")
        );
    }

    #[test]
    fn normalize_supported_url_rejects_unsupported_or_blank_values() {
        let registry = ScraperRegistry::with_public_scrapers();

        assert_eq!(normalize_supported_url(&registry, "   "), None);
        assert_eq!(
            normalize_supported_url(&registry, "https://example.com/not-supported"),
            None
        );
    }
}

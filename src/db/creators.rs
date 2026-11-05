//! Shared attribution filters and completion for page and image metadata.
use super::Database;
use crate::AppResult;
use rusqlite::{params_from_iter, Connection};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub(crate) const ROLES: [&str; 4] = ["director", "author", "photographer", "creator"];

const NAMES_SQL: &str = "SELECT a.id,a.value FROM attributes a INDEXED BY sqlite_autoindex_attributes_1 WHERE a.kind IN (?1,?2,?3,?4)
         AND (EXISTS(SELECT 1 FROM page_attributes pa WHERE pa.attribute_id=a.id)
           OR EXISTS(SELECT 1 FROM image_attributes ia WHERE ia.attribute_id=a.id))
         ORDER BY a.id";

#[derive(Default)]
pub(super) struct CatalogCache {
    observer: Option<Connection>,
    version: Option<i64>,
    catalog: Arc<Catalog>,
}

#[derive(Default)]
struct Catalog {
    names: Vec<(i64, String)>,
    completions: Vec<(String, String)>,
}

fn names(connection: &Connection) -> AppResult<Vec<(i64, String)>> {
    // The kind index restricts this scan to attribution metadata. Folding names in
    // Rust also supports existing Cyrillic values without rewriting the library.
    let mut statement = connection.prepare_cached(NAMES_SQL)?;
    let rows = statement.query_map(params_from_iter(ROLES), |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

impl Database {
    fn creator_catalog(&self) -> AppResult<Arc<Catalog>> {
        let mut cache = self
            .runtime
            .creator_catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.observer.is_none() {
            cache.observer = Some(super::open_configured_connection(
                &self.path,
                super::ConnectionRole::Reader,
            )?);
        }
        // data_version is comparable only on the same connection. This dedicated
        // observer sees commits from the writer pool and other app instances.
        let connection = cache.observer.as_ref().unwrap();
        let version = connection.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))?;
        if cache.version != Some(version) {
            let mut unique = BTreeMap::new();
            let names = names(connection)?
                .into_iter()
                .map(|(id, name)| {
                    let trimmed = name.trim();
                    let folded = trimmed.to_lowercase();
                    if !folded.is_empty() {
                        unique
                            .entry(folded.clone())
                            .or_insert_with(|| trimmed.to_owned());
                    }
                    (id, folded)
                })
                .collect();
            cache.catalog = Arc::new(Catalog {
                names,
                completions: unique.into_iter().collect(),
            });
            cache.version = Some(version);
        }
        Ok(Arc::clone(&cache.catalog))
    }

    pub fn suggest_creators(&self, query: &str, limit: usize) -> AppResult<Vec<String>> {
        let query = query.trim().to_lowercase();
        let catalog = self.creator_catalog()?;
        let entries = &catalog.completions;
        Ok(entries
            .iter()
            .filter(|(folded, _)| folded.starts_with(&query))
            .chain(
                entries
                    .iter()
                    .filter(|(folded, _)| !folded.starts_with(&query) && folded.contains(&query)),
            )
            .take(limit.clamp(1, 500))
            .map(|(_, name)| name.clone())
            .collect())
    }
}

pub(super) fn append_filter(
    database: &Database,
    sql: &mut String,
    params: &mut Vec<String>,
    page_id: &str,
    image_id: Option<&str>,
    people: &[String],
) -> AppResult<()> {
    let requested = people
        .iter()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect::<HashSet<_>>();
    if requested.is_empty() {
        return Ok(());
    }
    let catalog = database.creator_catalog()?;
    let ids = catalog
        .names
        .iter()
        .filter(|(_, name)| requested.contains(name))
        .map(|(id, _)| id.to_string())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        sql.push_str(" AND 0");
        return Ok(());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    // Alias expressions are supplied only by the two host-owned query builders.
    sql.push_str(&format!(" AND (EXISTS(SELECT 1 FROM page_attributes pa_creator WHERE pa_creator.page_id={page_id} AND pa_creator.attribute_id IN ({placeholders})) OR "));
    params.extend(ids.iter().cloned());
    match image_id {
        Some(image_id) => sql.push_str(&format!("EXISTS(SELECT 1 FROM image_attributes ia_creator WHERE ia_creator.image_id={image_id} AND ia_creator.attribute_id IN ({placeholders})))")),
        None => sql.push_str(&format!("EXISTS(SELECT 1 FROM page_images pi_creator JOIN image_attributes ia_creator ON ia_creator.image_id=pi_creator.image_id WHERE pi_creator.page_id={page_id} AND ia_creator.attribute_id IN ({placeholders})))")),
    }
    params.extend(ids);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_reuses_its_snapshot_and_observes_commits_from_other_connections() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("library.sqlite3");
        let db = Database::new(&path);
        db.initialize().unwrap();
        let first = db.creator_catalog().unwrap();
        assert!(Arc::ptr_eq(&first, &db.creator_catalog().unwrap()));
        let external = Connection::open(&path).unwrap();
        external.execute_batch("INSERT INTO source_pages(id,site_key,page_url,title,raw_html) VALUES(1,'example','https://example.test/1','Example','');
            INSERT INTO attributes(id,kind,label,value,normalized_value) VALUES(1,'author','Author','Борис Автор','борис автор');
            INSERT INTO page_attributes(page_id,attribute_id) VALUES(1,1);").unwrap();
        assert_eq!(db.suggest_creators("БОР", 8).unwrap(), ["Борис Автор"]);
        let updated = db.creator_catalog().unwrap();
        assert!(!Arc::ptr_eq(&first, &updated));
        assert!(Arc::ptr_eq(&updated, &db.creator_catalog().unwrap()));
        external
            .execute("DELETE FROM page_attributes WHERE attribute_id=1", [])
            .unwrap();
        assert!(db.suggest_creators("", 8).unwrap().is_empty());
        let other = Database::new(root.path().join("other.sqlite3"));
        other.initialize().unwrap();
        assert!(other.suggest_creators("", 8).unwrap().is_empty());
        let plan = external
            .prepare(&format!("EXPLAIN QUERY PLAN {NAMES_SQL}"))
            .unwrap()
            .query_map(params_from_iter(ROLES), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" ");
        assert!(
            plan.contains("SEARCH a USING INDEX sqlite_autoindex_attributes_1"),
            "{plan}"
        );
        assert!(!plan.contains("SCAN a"), "{plan}");
    }
}

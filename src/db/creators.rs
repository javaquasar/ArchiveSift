//! Shared attribution filters and completion for page and image metadata.
use super::Database;
use crate::AppResult;
use rusqlite::{params_from_iter, Connection};
use std::collections::{BTreeMap, HashSet};

pub(crate) const ROLES: [&str; 4] = ["director", "author", "photographer", "creator"];

fn names(connection: &Connection) -> AppResult<Vec<(i64, String)>> {
    // The kind index restricts this scan to attribution metadata. Folding names in
    // Rust also supports existing Cyrillic values without rewriting the library.
    let mut statement = connection.prepare_cached(
        "SELECT a.id,a.value FROM attributes a WHERE a.kind IN (?1,?2,?3,?4)
         AND (EXISTS(SELECT 1 FROM page_attributes pa WHERE pa.attribute_id=a.id)
           OR EXISTS(SELECT 1 FROM image_attributes ia WHERE ia.attribute_id=a.id))
         ORDER BY a.id",
    )?;
    let rows = statement.query_map(params_from_iter(ROLES), |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

impl Database {
    pub fn suggest_creators(&self, query: &str, limit: usize) -> AppResult<Vec<String>> {
        let query = query.trim().to_lowercase();
        let connection = self.connect()?;
        let mut unique = BTreeMap::new();
        for (_, name) in names(&connection)? {
            let name = name.trim();
            if !name.is_empty() {
                let folded = name.to_lowercase();
                if folded.contains(&query) {
                    unique.entry(folded).or_insert_with(|| name.to_owned());
                }
            }
        }
        let mut entries = unique.into_iter().collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            (!a.0.starts_with(&query), &a.0).cmp(&(!b.0.starts_with(&query), &b.0))
        });
        Ok(entries
            .into_iter()
            .take(limit.clamp(1, 500))
            .map(|(_, name)| name)
            .collect())
    }
}

pub(super) fn append_filter(
    connection: &Connection,
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
    let ids = names(connection)?
        .into_iter()
        .filter(|(_, name)| requested.contains(&name.trim().to_lowercase()))
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

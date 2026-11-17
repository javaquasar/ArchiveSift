use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::Database;
use crate::knife::{identity, image_extension, KnifeProfile};
use crate::models::ScrapedItem;
use crate::AppResult;

mod archive;

pub(super) fn initialize(connection: &Connection) -> AppResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS knife_models (
            id INTEGER PRIMARY KEY,
            manufacturer TEXT NOT NULL,
            model TEXT NOT NULL,
            manufacturer_key TEXT NOT NULL,
            model_key TEXT NOT NULL,
            directory TEXT NOT NULL,
            directory_key TEXT NOT NULL UNIQUE,
            UNIQUE(manufacturer_key, model_key)
        );
        CREATE TABLE IF NOT EXISTS knife_sources (
            page_id INTEGER PRIMARY KEY REFERENCES source_pages(id) ON DELETE CASCADE,
            model_id INTEGER NOT NULL REFERENCES knife_models(id)
        );
        CREATE INDEX IF NOT EXISTS knife_sources_model ON knife_sources(model_id);
        CREATE TABLE IF NOT EXISTS knife_import_keys (
            record_key TEXT PRIMARY KEY,
            page_id INTEGER NOT NULL REFERENCES knife_sources(page_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS knife_import_keys_page ON knife_import_keys(page_id);
        CREATE TABLE IF NOT EXISTS knife_image_paths (
            page_id INTEGER NOT NULL,
            image_id INTEGER NOT NULL,
            path TEXT NOT NULL,
            stem_key TEXT NOT NULL UNIQUE,
            PRIMARY KEY(page_id, image_id),
            FOREIGN KEY(page_id, image_id) REFERENCES page_images(page_id, image_id) ON DELETE CASCADE
        );"
    )?;
    Ok(())
}

pub(super) fn after_scrape(
    transaction: &Transaction<'_>,
    item: &ScrapedItem,
    page_id: i64,
    image_ids: &HashMap<String, i64>,
    images_only: bool,
) -> AppResult<()> {
    let existing: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT model.manufacturer, model.model, model.directory
         FROM knife_sources source JOIN knife_models model ON model.id = source.model_id
         WHERE source.page_id = ?1",
            [page_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let persisted_steel: Option<String> =
        if images_only && existing.is_some() {
            transaction.query_row(
            "SELECT a.value FROM page_attributes pa JOIN attributes a ON a.id = pa.attribute_id
             WHERE pa.page_id = ?1 AND a.kind = 'steel' ORDER BY a.id LIMIT 1",
            [page_id],
            |row| row.get(0),
        ).optional()?
        } else {
            None
        };
    let metadata = if images_only {
        None
    } else {
        KnifeProfile::from_item(item)?
    };
    let (manufacturer, model, directory) = match existing {
        Some(existing) => existing,
        None => match &metadata {
            Some(knife) => link_model(transaction, page_id, knife)?,
            None => return Ok(()),
        },
    };
    let knife = KnifeProfile {
        manufacturer: &manufacturer,
        model: &model,
        steel: persisted_steel
            .as_deref()
            .or_else(|| metadata.and_then(|profile| profile.steel)),
    };
    let mut get_path = transaction
        .prepare_cached("SELECT 1 FROM knife_image_paths WHERE page_id = ?1 AND image_id = ?2")?;
    let mut reserve_path = transaction.prepare_cached(
        "INSERT INTO knife_image_paths (page_id, image_id, path, stem_key) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(stem_key) DO NOTHING",
    )?;
    for (index, image) in item.images.iter().enumerate() {
        let image_id = image_ids[&image.source_url];
        if get_path.exists(params![page_id, image_id])? {
            continue;
        }
        let extension = url::Url::parse(&image.source_url).ok().and_then(|url| {
            Path::new(url.path())
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| image_extension(value))
                .map(str::to_ascii_lowercase)
        });
        let mut stem = knife.image_stem(index + 1);
        if extension.is_none() {
            stem = stem.replace('.', "_");
        }
        for suffix in 0.. {
            let stem = if suffix == 0 {
                stem.clone()
            } else {
                format!("{stem} ({suffix})")
            };
            let path = format!("{directory}/{stem}");
            let stem_key = path.to_lowercase();
            let path = match &extension {
                Some(extension) => format!("{path}.{extension}"),
                None => path,
            };
            if reserve_path.execute(params![page_id, image_id, path, stem_key])? != 0 {
                break;
            }
        }
    }
    Ok(())
}

fn link_model(
    transaction: &Transaction<'_>,
    page_id: i64,
    knife: &KnifeProfile<'_>,
) -> AppResult<(String, String, String)> {
    let manufacturer_key = identity(knife.manufacturer);
    let model_key = identity(knife.model);
    let mut find_model = transaction.prepare_cached(
        "SELECT id, manufacturer, model, directory FROM knife_models
         WHERE manufacturer_key = ?1 AND model_key = ?2",
    )?;
    let keys = params![manufacturer_key, model_key];
    if !find_model.exists(keys)? {
        // Preserve the same manufacturer folder across source spellings.
        let brand: Option<String> = transaction.query_row(
            "SELECT manufacturer FROM knife_models WHERE manufacturer_key = ?1 ORDER BY id LIMIT 1",
            [&manufacturer_key], |row| row.get(0),
        ).optional()?;
        let manufacturer = brand.as_deref().unwrap_or(knife.manufacturer);
        let canonical = KnifeProfile {
            manufacturer,
            model: knife.model,
            steel: knife.steel,
        };
        let base = canonical.directory().to_string_lossy().replace('\\', "/");
        for suffix in 0.. {
            let directory = if suffix == 0 {
                base.clone()
            } else {
                format!("{base} ({suffix})")
            };
            let inserted = transaction.execute(
                "INSERT INTO knife_models (manufacturer, model, manufacturer_key, model_key, directory, directory_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(directory_key) DO NOTHING",
                params![manufacturer, knife.model, manufacturer_key, model_key, directory, directory.to_lowercase()],
            )?;
            if inserted != 0 {
                break;
            }
        }
    }
    let (id, manufacturer, model, directory): (i64, String, String, String) = find_model
        .query_row(keys, |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
    transaction.execute(
        "INSERT INTO knife_sources (page_id, model_id) VALUES (?1, ?2)",
        params![page_id, id],
    )?;
    Ok((manufacturer, model, directory))
}

pub(super) fn append_manufacturer_filter(
    sql: &mut String,
    params: &mut Vec<String>,
    names: &[String],
) {
    let keys = names
        .iter()
        .map(|name| identity(name))
        .filter(|name| !name.is_empty());
    let start = params.len();
    params.extend(keys);
    if params.len() == start {
        return;
    }
    let placeholders = std::iter::repeat_n("?", params.len() - start)
        .collect::<Vec<_>>()
        .join(", ");
    sql.push_str(&format!(
        " AND EXISTS (SELECT 1 FROM knife_sources ks JOIN knife_models km ON km.id = ks.model_id
         WHERE ks.page_id = source_pages.id AND km.manufacturer_key IN ({placeholders}))"
    ));
}

impl Database {
    pub(crate) fn suggest_knife_manufacturers(
        &self,
        prefix: &str,
        limit: usize,
    ) -> AppResult<Vec<String>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare_cached(
            "SELECT min(manufacturer) FROM knife_models
             WHERE instr(manufacturer_key, ?1) > 0
               AND EXISTS (SELECT 1 FROM knife_sources WHERE model_id = knife_models.id)
             GROUP BY manufacturer_key
             ORDER BY (instr(manufacturer_key, ?1) = 1) DESC, manufacturer_key LIMIT ?2",
        )?;
        let rows = statement.query_map(params![identity(prefix), limit], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn knife_image_paths(&self, page_id: i64) -> AppResult<HashMap<i64, PathBuf>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare_cached("SELECT image_id, path FROM knife_image_paths WHERE page_id = ?1")?;
        let rows = statement.query_map([page_id], |row| {
            Ok((
                row.get(0)?,
                Path::new(&row.get::<_, String>(1)?).components().collect(),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

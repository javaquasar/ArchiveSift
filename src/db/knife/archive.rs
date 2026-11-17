use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::knife::archive::{ArchiveKnife, ImportResult};
use crate::knife::identity;
use crate::knife::preview::ImportPreview;
use crate::models::{MetadataAttribute, ScrapedImage, ScrapedItem};
use crate::AppResult;

use super::{link_model, Database};

#[cfg(test)]
mod group_tests;
#[cfg(test)]
mod tests;
use crate::db::{
    collection_url_alternate, refresh_page_counters, refresh_page_search_index, upsert_attribute,
    upsert_image, upsert_source_page,
};

impl Database {
    pub(crate) fn import_knife_archive(
        &self,
        plan: &ImportPreview,
        download_root: &Path,
    ) -> AppResult<ImportResult> {
        self.initialize()?;
        if download_root.as_os_str().is_empty() {
            return Err("Choose a download folder before importing".into());
        }
        fs::create_dir_all(download_root)?;
        let root = download_root.canonicalize()?;
        let mut result = ImportResult {
            skipped_records: plan.records - plan.importable.len(),
            skipped_files: plan.missing_files + plan.rejected_files,
            ..Default::default()
        };
        for record in &plan.importable {
            match self.import_knife_record(record, plan.archive_images.as_deref(), &root) {
                Ok(row) => {
                    result.new_pages += usize::from(row.new_page);
                    result.existing_pages += usize::from(!row.new_page);
                    result.copied_files += row.copied;
                    result.reused_files += row.reused;
                    result.duplicate_files += row.duplicate;
                    result.skipped_files += row.skipped;
                }
                Err(error) => {
                    result.skipped_records += 1;
                    result.skipped_files += record.images.len();
                    if result.errors.len() < 20 {
                        result
                            .errors
                            .push(format!("Record {} ({}): {error}", record.row, record.title));
                    }
                }
            }
        }
        Ok(result)
    }

    fn import_knife_record(
        &self,
        record: &ArchiveKnife,
        archive: Option<&Path>,
        root: &Path,
    ) -> AppResult<RecordResult> {
        let mut connection = self.writer_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = find_page(&transaction, record)?;
        let url = match &existing {
            Some((_, url)) => url.clone(),
            None => record
                .sources
                .first()
                .cloned()
                .unwrap_or_else(|| offline_page_url(record)),
        };
        let _lease = self
            .try_content_lease(&url)?
            .ok_or("This page is being updated. Run the import again to retry it.")?;
        let mut result = RecordResult {
            new_page: existing.is_none(),
            ..Default::default()
        };
        let page_id = if let Some((page_id, _)) = existing {
            page_id
        } else {
            let page = ScrapedItem {
                site_key: "knife-archive".into(),
                page_url: url,
                title: record.title.clone(),
                raw_html: String::new(),
                images: Vec::new(),
                attributes: Vec::new(),
            };
            let page_id = upsert_source_page(&transaction, &page)?;
            link_model(&transaction, page_id, &record.profile())?;
            page_id
        };

        let (model_id, manufacturer, model, directory): (i64, String, String, String) = transaction
            .query_row(
                "SELECT km.id, km.manufacturer, km.model, km.directory FROM knife_models km
             JOIN knife_sources ks ON ks.model_id = km.id WHERE ks.page_id = ?1",
                [page_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        merge_attributes(&transaction, record, page_id, &manufacturer, &model)?;

        // Keep any files created during this record recoverable if the SQL write fails.
        let mut created = NewFiles::default();
        if let Some(archive) = archive.filter(|_| !record.images.is_empty()) {
            let relative = Path::new(&directory);
            let folder = safe_directory(root, relative)?;
            let mut images = ExistingImages::for_model(&transaction, model_id, root)?;
            let mut order: i64 = transaction.query_row(
                "SELECT coalesce(max(sort_order), 0) + 1 FROM page_images WHERE page_id = ?1",
                [page_id],
                |row| row.get(0),
            )?;
            for image in &record.images {
                let Some((input, digest, width, height)) =
                    inspect_archive_image(archive, &image.relative)
                else {
                    result.skipped += 1;
                    continue;
                };
                let source = image.url.clone().unwrap_or_else(|| {
                    format!("urn:archivesift:knife-image:{}", digest_hex(digest))
                });
                let key = image_asset_key(&source);
                if images.assets.contains(&key) || images.digests.contains(&digest) {
                    result.duplicate += 1;
                    continue;
                }
                let name = image.relative.file_name().expect("validated archive file");
                let relative_path = relative.join(name);
                let destination = folder.join(name);
                let path = portable_text(&relative_path);
                let stem_key = portable_text(&relative_path.with_extension("")).to_lowercase();
                let claimed = transaction.query_row(
                    "SELECT kp.page_id, i.source_url FROM knife_image_paths kp JOIN images i ON i.id = kp.image_id WHERE kp.stem_key = ?1",
                    [&stem_key], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                ).optional()?;
                if claimed
                    .as_ref()
                    .is_some_and(|(owner, original)| *owner != page_id || original != &source)
                {
                    result.skipped += 1;
                    continue;
                }
                match fs::symlink_metadata(&destination) {
                    Ok(meta) if meta.is_file() && file_digest(&destination)? == digest => {
                        result.reused += 1
                    }
                    Ok(_) => {
                        result.skipped += 1;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        let mut temp = tempfile::NamedTempFile::new_in(&folder)?;
                        io::copy(&mut File::open(&input)?, &mut temp)?;
                        temp.flush()?;
                        if file_digest(temp.path())? != digest {
                            return Err("An archive photo changed while being copied. Preview the archive and retry.".into());
                        }
                        temp.persist_noclobber(&destination)?;
                        created.paths.push(destination);
                        result.copied += 1;
                    }
                    Err(error) => return Err(error.into()),
                }
                let scraped = ScrapedImage {
                    source_url: source,
                    preview_url: None,
                    alt_text: Some(record.title.clone()),
                    width: Some(i64::from(width)),
                    height: Some(i64::from(height)),
                    sort_order: order,
                    local_file_path: Some(path.clone()),
                };
                let image_id = upsert_image(&transaction, &scraped)?;
                transaction.execute(
                    "INSERT INTO page_images (page_id, image_id, sort_order, local_file_path, downloaded_at)
                     VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP)
                     ON CONFLICT(page_id, image_id) DO UPDATE SET local_file_path = excluded.local_file_path,
                       downloaded_at = excluded.downloaded_at, updated_at = CURRENT_TIMESTAMP",
                    params![page_id, image_id, order, path],
                )?;
                transaction.execute(
                    "INSERT INTO knife_image_paths (page_id, image_id, path, stem_key)
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(page_id, image_id) DO UPDATE SET
                       path = excluded.path, stem_key = excluded.stem_key",
                    params![page_id, image_id, path, stem_key],
                )?;
                order += 1;
                images.assets.insert(key);
                images.digests.insert(digest);
            }
        }
        for key in record_keys(record) {
            transaction.execute(
                "INSERT OR IGNORE INTO knife_import_keys (record_key, page_id) VALUES (?1, ?2)",
                params![key, page_id],
            )?;
        }
        refresh_page_search_index(&transaction, page_id)?;
        refresh_page_counters(&transaction, page_id)?;
        transaction.commit()?;
        created.committed = true;
        Ok(result)
    }
}

#[derive(Default)]
struct RecordResult {
    new_page: bool,
    copied: usize,
    reused: usize,
    duplicate: usize,
    skipped: usize,
}

fn record_keys(record: &ArchiveKnife) -> impl Iterator<Item = String> + '_ {
    record
        .id
        .iter()
        .map(|id| format!("id:{id}"))
        .chain(record.sources.iter().map(|url| format!("url:{url}")))
}

fn offline_page_url(record: &ArchiveKnife) -> String {
    let name = [
        identity(&record.manufacturer),
        identity(&record.model),
        identity(record.steel.as_deref().unwrap_or_default()),
        record.id.clone().unwrap_or_else(|| identity(&record.title)),
    ]
    .join("\0");
    format!(
        "urn:archivesift:knife:{}",
        digest_hex(Sha256::digest(name).into())
    )
}

fn find_page(
    transaction: &Transaction<'_>,
    record: &ArchiveKnife,
) -> AppResult<Option<(i64, String)>> {
    let mut matches = BTreeSet::new();
    let mut url_matches = BTreeSet::new();
    let mut lookup = transaction
        .prepare_cached("SELECT page_id FROM knife_import_keys WHERE record_key = ?1")?;
    for key in record_keys(record) {
        if let Some(page_id) = lookup.query_row([&key], |row| row.get(0)).optional()? {
            matches.insert(page_id);
            if key.starts_with("url:") {
                url_matches.insert(page_id);
            }
        }
    }
    let mut pages =
        transaction.prepare_cached("SELECT id FROM source_pages WHERE page_url = ?1")?;
    for url in &record.sources {
        let alternate = collection_url_alternate(url);
        for url in std::iter::once(url.as_str()).chain(alternate.as_deref()) {
            if let Some(page_id) = pages.query_row([url], |row| row.get(0)).optional()? {
                matches.insert(page_id);
                url_matches.insert(page_id);
            }
        }
    }
    if record.sources.is_empty() {
        if let Some(page_id) = pages
            .query_row([offline_page_url(record)], |row| row.get(0))
            .optional()?
        {
            matches.insert(page_id);
        }
    }
    if matches.len() > 1 {
        return Err("Archive ID and source URLs point to different saved pages".into());
    }
    let Some(page_id) = matches.first().copied() else {
        return Ok(None);
    };
    let matched: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT sp.page_url, km.manufacturer_key, km.model_key FROM source_pages sp
         JOIN knife_sources ks ON ks.page_id = sp.id JOIN knife_models km ON km.id = ks.model_id
         WHERE sp.id = ?1",
            [page_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((url, manufacturer, model)) = matched else {
        return Err("Source URL is attached to a page outside the knife catalog".into());
    };
    // A matching source can describe the same model with the variant in its name.
    if manufacturer != identity(&record.manufacturer)
        || (model != identity(&record.model) && !url_matches.contains(&page_id))
    {
        return Err("Archive identity conflicts with the saved manufacturer or model".into());
    }
    Ok(Some((page_id, url)))
}

fn merge_attributes(
    transaction: &Transaction<'_>,
    record: &ArchiveKnife,
    page_id: i64,
    manufacturer: &str,
    model: &str,
) -> AppResult<()> {
    let mut existing = transaction.prepare_cached(
        "SELECT a.kind FROM page_attributes pa JOIN attributes a ON a.id = pa.attribute_id WHERE pa.page_id = ?1",
    )?;
    let kinds: HashSet<String> = existing
        .query_map([page_id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut insert = transaction.prepare_cached(
        "INSERT OR IGNORE INTO page_attributes (page_id, attribute_id) VALUES (?1, ?2)",
    )?;
    for attribute in &record.attributes {
        if kinds.contains(&attribute.kind)
            && !matches!(
                attribute.kind.as_str(),
                "archive_source" | "archive_id" | "tag" | "notes"
            )
        {
            continue;
        }
        let canonical = match attribute.kind.as_str() {
            "manufacturer" => Some(manufacturer),
            "model" => Some(model),
            _ => None,
        };
        let replacement = canonical.map(|value| MetadataAttribute {
            value: value.into(),
            ..attribute.clone()
        });
        let id = upsert_attribute(transaction, replacement.as_ref().unwrap_or(attribute))?;
        insert.execute(params![page_id, id])?;
    }
    Ok(())
}

#[derive(Default)]
struct ExistingImages {
    assets: HashSet<String>,
    digests: HashSet<[u8; 32]>,
}

impl ExistingImages {
    fn for_model(transaction: &Transaction<'_>, model_id: i64, root: &Path) -> AppResult<Self> {
        let mut images = Self::default();
        let mut statement = transaction.prepare_cached(
            "SELECT i.source_url, pi.local_file_path FROM knife_sources ks
             JOIN page_images pi ON pi.page_id = ks.page_id JOIN images i ON i.id = pi.image_id
             WHERE ks.model_id = ?1 AND pi.local_file_path IS NOT NULL",
        )?;
        let rows = statement.query_map([model_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (url, path) = row?;
            let path = path.replace('\\', "/");
            let Ok(path) = root.join(&path).canonicalize() else {
                continue;
            };
            if path.starts_with(root) && path.is_file() {
                images.assets.insert(image_asset_key(&url));
                images.digests.insert(file_digest(&path)?);
            }
        }
        Ok(images)
    }
}

/// Image hosts can publish several sizes of the same path below a resolution directory.
fn image_asset_key(source: &str) -> String {
    let Ok(mut url) = url::Url::parse(source) else {
        return source.into();
    };
    if let Some((size, tail)) = url
        .path()
        .strip_prefix("/images/")
        .and_then(|path| path.split_once('/'))
    {
        if size.split_once('x').is_some_and(|(width, height)| {
            width.parse::<u32>().is_ok_and(|n| n > 0) && height.parse::<u32>().is_ok_and(|n| n > 0)
        }) {
            url.set_path(&format!("/images/{tail}"));
        }
    }
    url.set_fragment(None);
    url.into()
}

fn inspect_archive_image(root: &Path, relative: &Path) -> Option<(PathBuf, [u8; 32], u32, u32)> {
    let path = root.join(relative).canonicalize().ok()?;
    if !path.starts_with(root) || !path.is_file() {
        return None;
    }
    let (width, height) = image::ImageReader::open(&path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    let digest = file_digest(&path).ok()?;
    Some((path, digest, width, height))
}

fn file_digest(path: &Path) -> io::Result<[u8; 32]> {
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    Ok(digest.finalize().into())
}

fn digest_hex(digest: [u8; 32]) -> String {
    format!("{:x}", sha2::digest::Output::<Sha256>::from(digest))
}

fn portable_text(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn safe_directory(root: &Path, relative: &Path) -> AppResult<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("Invalid knife directory".into());
        };
        path.push(component);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        path = path.canonicalize()?;
        if !path.starts_with(root) || !path.is_dir() {
            return Err("Knife directory escapes the download folder".into());
        }
    }
    Ok(path)
}

#[derive(Default)]
struct NewFiles {
    paths: Vec<PathBuf>,
    committed: bool,
}

impl Drop for NewFiles {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

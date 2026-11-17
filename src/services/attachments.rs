//! Host-owned binary attachments and passive magnet references on content pages.
use std::path::{Path, PathBuf};

use sha1::Digest;

use crate::db::{Database, FileDownloadStore};
use crate::error::IncompleteAttachments;
use crate::fs::torrent::is_torrent_file;
use crate::fs::torrent_metainfo::TorrentMetainfo;
use crate::models::{MetadataAttribute, ScrapedItem, TorrentCard, TORRENT_UNAVAILABLE_NOTICE};
use crate::scraper::ContentScraper;
use crate::AppResult;

pub(super) const DESCRIPTION_SOURCE: &str = "archivesift:description";
pub(super) const DESCRIPTION_FILENAME: &str = "description.txt";

pub(super) fn description_path(card: &TorrentCard, directory: Option<&str>) -> PathBuf {
    if let Some(existing) = card
        .attachments
        .iter()
        .find(|a| a.source_url == DESCRIPTION_SOURCE)
    {
        return PathBuf::from(existing.local_file_path.replace('\\', "/"));
    }
    if let Some(directory) = directory {
        return PathBuf::from(directory.replace('\\', "/")).join(DESCRIPTION_FILENAME);
    }
    card.attachments
        .iter()
        .find(|a| a.filename.to_ascii_lowercase().ends_with(".torrent"))
        .or_else(|| card.attachments.first())
        .map(|attachment| {
            PathBuf::from(attachment.local_file_path.replace('\\', "/"))
                .with_file_name(DESCRIPTION_FILENAME)
        })
        .unwrap_or_else(|| {
            relative_path(
                &card.site_key,
                card.page_id,
                DESCRIPTION_SOURCE,
                DESCRIPTION_FILENAME,
            )
            .with_file_name(DESCRIPTION_FILENAME)
        })
}

pub(super) fn description_text(card: &TorrentCard) -> String {
    let description = card.value("full_description");
    let description = if description.is_empty() {
        card.value("description")
    } else {
        description
    };
    let mut text = card.title.clone();
    if !description.is_empty() {
        text.push_str("\n\n");
        text.push_str(description);
    }
    for (kind, title) in [
        ("release_details", "Release details"),
        ("media_info", "MediaInfo"),
    ] {
        let mut values = card
            .attributes
            .iter()
            .filter(|attribute| attribute.kind == kind && !attribute.value.trim().is_empty())
            .map(|attribute| attribute.value.as_str());
        if let Some(first) = values.next() {
            text.push_str("\n\n");
            text.push_str(title);
            text.push('\n');
            text.push_str(first);
            for value in values {
                text.push_str("\n\n");
                text.push_str(value);
            }
        }
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Export page-level text once, independently of image and torrent downloads.
/// The caller holds the page lock and file lease through the complete operation.
pub(crate) async fn save_description(
    page_id: i64,
    database: &Database,
    base_dir: &Path,
) -> AppResult<()> {
    if !database.is_torrent_page(page_id)? {
        return Ok(());
    }
    let card = database.torrent_card(page_id)?;
    let existing = card
        .attachments
        .iter()
        .any(|a| a.source_url == DESCRIPTION_SOURCE);
    let relative = description_path(&card, card.directory.as_deref());
    let text = description_text(&card);
    tokio::fs::create_dir_all(base_dir).await?;
    let root = tokio::fs::canonicalize(base_dir).await?;
    let path = super::torrent_library::checked_path(&root, &relative.to_string_lossy())?;
    if !existing && path.try_exists()? {
        // An identical unregistered export can be left by a failed DB write.
        // Other pre-existing text files belong to the user and must be retained.
        if tokio::fs::metadata(&path).await?.len() != text.len() as u64
            || tokio::fs::read(&path).await? != text.as_bytes()
        {
            return Err(
                "description.txt already exists; rename it before exporting the card text".into(),
            );
        }
    }
    FileDownloadStore::new()
        .write_bytes(&path.to_string_lossy(), text.as_bytes())
        .await?;
    database.save_page_attachment(
        page_id,
        DESCRIPTION_SOURCE,
        DESCRIPTION_FILENAME,
        &path.strip_prefix(&root)?.to_string_lossy(),
    )
}

pub(crate) fn valid_magnet(value: &str) -> bool {
    if value.len() > 16 * 1024 {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    url.scheme() == "magnet"
        && url.host_str().is_none()
        && url.path().is_empty()
        && url.fragment().is_none()
        && url.query_pairs().any(|(key, value)| {
            key == "xt"
                && (value.strip_prefix("urn:btih:").is_some_and(|hash| {
                    (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
                        || (hash.len() == 32
                            && hash
                                .bytes()
                                .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b)))
                }) || value.strip_prefix("urn:btmh:1220").is_some_and(|hash| {
                    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
                }))
        })
}

/// Hash the original encoded info dictionary, avoiding changes from reserialization.
pub(crate) fn torrent_magnet(bytes: &[u8]) -> AppResult<String> {
    TorrentMetainfo::parse(bytes)?.magnet()
}

fn relative_path(site_key: &str, page_id: i64, source: &str, name: &str) -> PathBuf {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(100)
            .collect::<String>()
            .trim_matches('.')
            .to_string()
    };
    let hash = sha2::Sha256::digest(source.as_bytes());
    PathBuf::from(clean(site_key))
        .join("attachments")
        .join(page_id.to_string())
        .join(format!("{:.12}_{}", format!("{hash:x}"), clean(name)))
}

fn stored_relative_path(
    database: &Database,
    page: &ScrapedItem,
    page_id: i64,
    source: &str,
    name: &str,
) -> AppResult<PathBuf> {
    if let Some(path) = database.attachment_path(page_id, source)? {
        return Ok(path);
    }
    let old = relative_path(&page.site_key, page_id, source, name);
    Ok(match database.torrent_directory(page_id)? {
        Some(directory) => directory.join(old.file_name().ok_or("attachment has no filename")?),
        None => old,
    })
}

pub(crate) async fn download(
    page: &ScrapedItem,
    page_id: i64,
    scraper: &dyn ContentScraper,
    database: &Database,
    base_dir: &Path,
) -> AppResult<()> {
    let store = FileDownloadStore::new();
    if page
        .attributes
        .iter()
        .filter(|a| matches!(a.kind.as_str(), "attachment" | "magnet"))
        .count()
        > 32
    {
        return Err("too many content attachments".into());
    }
    let mut failures = Vec::new();
    for attribute in &page.attributes {
        let result = match attribute.kind.as_str() {
            "attachment" => {
                save_attachment(
                    page, page_id, attribute, scraper, database, base_dir, &store, false,
                )
                .await
            }
            "magnet" => {
                save_magnet(page, page_id, &attribute.value, database, base_dir, &store).await
            }
            _ => continue,
        };
        if let Err(error) = result {
            let name = crate::fs::torrent::safe_component(&attribute.label, "Attachment", 100);
            failures.push((name, error));
        }
    }
    if page.attributes.iter().any(|attribute| {
        attribute.kind == "notice" && attribute.label == TORRENT_UNAVAILABLE_NOTICE
    }) && !page
        .attributes
        .iter()
        .any(|attribute| attribute.kind == "attachment" && is_torrent_file(&attribute.label))
        && !database
            .torrent_card(page_id)?
            .has_downloaded_torrent(base_dir)
    {
        failures.push((
            "Torrent file".into(),
            "The source did not provide a torrent link. Retry when torrent downloads are available."
                .into(),
        ));
    }
    if !failures.is_empty() {
        return Err(IncompleteAttachments { failures }.into());
    }
    Ok(())
}

pub(super) async fn download_torrent_file(
    page: &ScrapedItem,
    page_id: i64,
    scraper: &dyn ContentScraper,
    database: &Database,
    base_dir: &Path,
) -> AppResult<()> {
    let result = match page.attributes.iter().find(|attribute| {
        attribute.kind == "attachment" && is_torrent_file(&attribute.label)
    }) {
        Some(attribute) => save_attachment(
            page, page_id, attribute, scraper, database, base_dir, &FileDownloadStore::new(), true,
        ).await,
        None => Err("The source did not provide a .torrent link. Check your access or retry when the file is available.".into()),
    };
    result.map_err(|error| {
        IncompleteAttachments {
            failures: vec![("Torrent file".into(), error)],
        }
        .into()
    })
}

async fn save_attachment(
    page: &ScrapedItem,
    page_id: i64,
    attribute: &MetadataAttribute,
    scraper: &dyn ContentScraper,
    database: &Database,
    base_dir: &Path,
    store: &FileDownloadStore,
    torrent_only: bool,
) -> AppResult<()> {
    let url = attribute
        .source_url
        .as_deref()
        .ok_or("attachment has no download URL")?;
    let relative = stored_relative_path(database, page, page_id, url, &attribute.label)?;
    let path = if torrent_only {
        super::torrent_library::checked_path(
            &base_dir.canonicalize()?,
            &relative.to_string_lossy(),
        )?
    } else {
        base_dir.join(&relative)
    };
    let size = match tokio::fs::metadata(&path).await {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => return Err("attachment destination is not a file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    let data = if size > 0 {
        if size > 8 * 1024 * 1024 {
            return Err("existing attachment exceeds limit".into());
        }
        tokio::fs::read(&path).await?
    } else {
        let data = scraper.fetch_attachment(url).await?;
        if data.bytes.is_empty()
            || data
                .content_type
                .to_ascii_lowercase()
                .starts_with("text/html")
        {
            return Err(
                "the source returned an empty file or a web page; check access or download limits"
                    .into(),
            );
        }
        data.bytes
    };
    let torrent = is_torrent_file(&attribute.label)
        .then(|| TorrentMetainfo::parse(&data))
        .transpose()?;
    let magnet = torrent
        .as_ref()
        .filter(|_| !torrent_only)
        .map(TorrentMetainfo::magnet)
        .transpose()?;
    if size == 0 {
        store.write_bytes(&path.to_string_lossy(), &data).await?;
    }
    if let Some(torrent) = torrent {
        database.save_torrent_file(
            page_id,
            url,
            &attribute.label,
            &relative.to_string_lossy(),
            &torrent.file_names(),
            torrent_only,
        )?;
    } else {
        database.save_page_attachment(
            page_id,
            url,
            &attribute.label,
            &relative.to_string_lossy(),
        )?;
    }
    if let Some(magnet) = magnet {
        save_magnet(page, page_id, &magnet, database, base_dir, store).await?;
    }
    Ok(())
}

async fn save_magnet(
    page: &ScrapedItem,
    page_id: i64,
    magnet: &str,
    database: &Database,
    base_dir: &Path,
    store: &FileDownloadStore,
) -> AppResult<()> {
    if !valid_magnet(magnet) {
        return Err("invalid magnet reference".into());
    }
    let relative = stored_relative_path(database, page, page_id, magnet, "download.magnet.txt")?;
    let path = base_dir.join(&relative);
    let text = format!("{magnet}\n");
    let already_saved = tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == text.len() as u64)
        && tokio::fs::read(&path).await? == text.as_bytes();
    if !already_saved {
        store
            .write_bytes(&path.to_string_lossy(), text.as_bytes())
            .await?;
    }
    database.save_page_attachment(page_id, magnet, "Magnet", &relative.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_html_truncation_duplicate_info_and_excessive_nesting() {
        for input in [
            b"<html>Login</html>".as_slice(),
            b"d4:info",
            b"d4:infoi1ee",
            b"d4:infode4:infodee",
            b"d4:infodeeextra",
            b"d4:infod1:ai01eee",
        ] {
            assert!(torrent_magnet(input).is_err());
        }
        let deep = format!("d4:info{}{}e", "d1:al".repeat(70), "ee".repeat(70));
        assert!(torrent_magnet(deep.as_bytes()).is_err());
    }
    #[test]
    fn hashes_the_exact_info_bytes_and_validates_magnet_references() {
        let raw = b"d4:infod6:lengthi1e4:name4:demoee";
        let expected = format!(
            "magnet:?xt=urn:btih:{:x}",
            sha1::Sha1::digest(b"d6:lengthi1e4:name4:demoe")
        );
        assert_eq!(torrent_magnet(raw).unwrap(), expected);
        assert!(valid_magnet(&expected));
        for bad in [
            "https://example.test",
            "magnet:?xt=urn:btih:bad",
            "magnet://host?xt=urn:btih:0123456789012345678901234567890123456789",
        ] {
            assert!(!valid_magnet(bad));
        }
    }

    #[test]
    fn retains_trackers_and_uses_v2_or_hybrid_hashes_when_required() {
        let raw =
            b"d8:announce29:https://tracker.test/announce4:infod12:meta versioni2e4:name4:demoee";
        let magnet = torrent_magnet(raw).unwrap();
        assert!(magnet.starts_with("magnet:?xt=urn:btmh:1220"));
        assert!(valid_magnet(&magnet));
        let url = reqwest::Url::parse(&magnet).unwrap();
        assert!(url
            .query_pairs()
            .any(|(k, v)| k == "tr" && v == "https://tracker.test/announce"));
        assert!(!magnet.contains("urn:btih:"));
        let hybrid = b"d4:infod12:meta versioni2e4:name4:demo6:pieces0:ee";
        let magnet = torrent_magnet(hybrid).unwrap();
        assert!(magnet.contains("urn:btih:"));
        assert!(magnet.contains("urn:btmh:1220"));
    }
}

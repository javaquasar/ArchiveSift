//! Host-owned binary attachments and passive magnet references on content pages.
use std::path::{Path, PathBuf};

use sha1::Digest;

use crate::db::{Database, FileDownloadStore};
use crate::error::IncompleteAttachments;
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
    fn string<'a>(b: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
        let start = *i;
        while b.get(*i)?.is_ascii_digit() {
            *i += 1;
        }
        if *i == start || b.get(*i) != Some(&b':') || (*i > start + 1 && b[start] == b'0') {
            return None;
        }
        let length = std::str::from_utf8(&b[start..*i])
            .ok()?
            .parse::<usize>()
            .ok()?;
        *i += 1;
        let end = i.checked_add(length)?;
        let result = b.get(*i..end)?;
        *i = end;
        Some(result)
    }
    fn value(b: &[u8], i: &mut usize, depth: usize) -> Option<()> {
        if depth > 64 {
            return None;
        }
        match *b.get(*i)? {
            b'i' => {
                *i += 1;
                let start = *i;
                if b.get(*i) == Some(&b'-') {
                    *i += 1;
                }
                let digits = *i;
                while b.get(*i)?.is_ascii_digit() {
                    *i += 1;
                }
                if *i == digits
                    || b.get(*i) != Some(&b'e')
                    || (*i > digits + 1 && b[digits] == b'0')
                    || (digits != start && b[digits] == b'0')
                {
                    return None;
                }
                *i += 1;
            }
            b'l' | b'd' => {
                let dict = b[*i] == b'd';
                *i += 1;
                while b.get(*i) != Some(&b'e') {
                    if dict {
                        string(b, i)?;
                    }
                    value(b, i, depth + 1)?;
                }
                *i += 1;
            }
            b'0'..=b'9' => {
                string(b, i)?;
            }
            _ => return None,
        }
        Some(())
    }
    let invalid = || "attachment is not a valid torrent file";
    if bytes.first() != Some(&b'd') {
        return Err(invalid().into());
    }
    let mut index = 1;
    let mut info = None;
    let mut trackers = Vec::<String>::new();
    while bytes.get(index) != Some(&b'e') {
        let key = string(bytes, &mut index).ok_or_else(invalid)?;
        let start = index;
        value(bytes, &mut index, 0).ok_or_else(invalid)?;
        if key == b"info" {
            if info.is_some() || bytes[start] != b'd' {
                return Err(invalid().into());
            }
            info = Some(&bytes[start..index]);
        } else if key == b"announce" || key == b"announce-list" {
            let mut cursor = start;
            while cursor < index && trackers.len() < 32 {
                if bytes[cursor].is_ascii_digit() {
                    if let Some(raw) = string(bytes, &mut cursor) {
                        if let Ok(text) = std::str::from_utf8(raw) {
                            if reqwest::Url::parse(text).is_ok_and(|u| {
                                matches!(u.scheme(), "http" | "https" | "udp" | "wss")
                                    && u.host_str().is_some()
                            }) && !trackers.iter().any(|s| s == text)
                            {
                                trackers.push(text.to_string());
                            }
                        }
                    } else {
                        break;
                    }
                } else {
                    cursor += 1;
                }
            }
        }
    }
    if index + 1 != bytes.len() {
        return Err(invalid().into());
    }
    let info = info.ok_or_else(invalid)?;
    let mut cursor = 1;
    let mut version = None;
    let mut v1_pieces = false;
    while info.get(cursor) != Some(&b'e') {
        let key = string(info, &mut cursor).ok_or_else(invalid)?;
        let start = cursor;
        value(info, &mut cursor, 0).ok_or_else(invalid)?;
        if key == b"meta version" {
            version = Some(&info[start..cursor]);
        }
        if key == b"pieces" {
            v1_pieces = true;
        }
    }
    if version.is_some_and(|v| v != b"i2e") {
        return Err("unsupported torrent metadata version".into());
    }
    let mut magnet = if version.is_some() {
        format!("magnet:?xt=urn:btmh:1220{:x}", sha2::Sha256::digest(info))
    } else {
        format!("magnet:?xt=urn:btih:{:x}", sha1::Sha1::digest(info))
    };
    if version.is_some() && v1_pieces {
        magnet.push_str(&format!("&xt=urn:btih:{:x}", sha1::Sha1::digest(info)));
    }
    for tracker in trackers {
        let parameter =
            reqwest::Url::parse_with_params("https://example.invalid/", [("tr", tracker)])?;
        let query = parameter.query().unwrap();
        if magnet.len() + query.len() + 1 <= 16 * 1024 {
            magnet.push('&');
            magnet.push_str(query);
        }
    }
    Ok(magnet)
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
                    page, page_id, attribute, scraper, database, base_dir, &store,
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
            .attachments
            .iter()
            .any(|file| {
                is_torrent_file(&file.filename)
                    && base_dir
                        .join(&file.local_file_path)
                        .metadata()
                        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
            })
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

fn is_torrent_file(filename: &str) -> bool {
    Path::new(filename)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
}

async fn save_attachment(
    page: &ScrapedItem,
    page_id: i64,
    attribute: &MetadataAttribute,
    scraper: &dyn ContentScraper,
    database: &Database,
    base_dir: &Path,
    store: &FileDownloadStore,
) -> AppResult<()> {
    let url = attribute
        .source_url
        .as_deref()
        .ok_or("attachment has no download URL")?;
    let relative = stored_relative_path(database, page, page_id, url, &attribute.label)?;
    let path = base_dir.join(&relative);
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
    let magnet = is_torrent_file(&attribute.label)
        .then(|| torrent_magnet(&data))
        .transpose()?;
    if size == 0 {
        store.write_bytes(&path.to_string_lossy(), &data).await?;
    }
    database.save_page_attachment(page_id, url, &attribute.label, &relative.to_string_lossy())?;
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

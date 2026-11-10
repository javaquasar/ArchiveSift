use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::models::MetadataAttribute;

fn valid_year(value: &str) -> Option<&str> {
    let value = value.trim();
    let year = value.get(..4)?;
    if !year.bytes().all(|byte| byte.is_ascii_digit()) || year.starts_with('0') {
        return None;
    }
    if value.len() > 4 && !value[4..].starts_with(['-', '.', ' ']) {
        return None;
    }
    Some(year)
}

pub(crate) const RELEASE_DATE_KINDS: [&str; 4] =
    ["production_date", "date", "year", "release_date"];

pub(crate) fn production_date(attributes: &[MetadataAttribute]) -> Option<&str> {
    for kind in RELEASE_DATE_KINDS {
        if let Some(attribute) = attributes
            .iter()
            .find(|attribute| attribute.kind == kind && valid_year(&attribute.value).is_some())
        {
            return Some(attribute.value.trim());
        }
    }
    None
}

pub fn production_year(attributes: &[MetadataAttribute]) -> Option<&str> {
    production_date(attributes).and_then(valid_year)
}

/// Portable components, including Windows device names, even when built on Unix.
pub fn safe_component(value: &str, fallback: &str, limit: usize) -> String {
    let mut result: String = value
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(limit)
        .collect();
    result = result.trim_matches([' ', '.']).to_string();
    if result.is_empty() {
        return fallback.into();
    }
    let stem = result
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|n| {
            matches!(
                n,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    }) {
        result.insert(0, '_');
    }
    result
}

pub fn torrent_directory(
    site: &str,
    studio: &str,
    title: &str,
    url: &str,
    page_id: i64,
) -> PathBuf {
    torrent_directory_with_year(site, studio, title, url, page_id, None)
}

pub fn torrent_directory_with_year(
    site: &str,
    studio: &str,
    title: &str,
    url: &str,
    page_id: i64,
    year: Option<&str>,
) -> PathBuf {
    let parsed = url::Url::parse(url).ok();
    let identifier = parsed
        .as_ref()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| matches!(k.as_ref(), "t" | "id"))
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_else(|| page_id.to_string());
    let hash = format!("{:x}", Sha256::digest(url.as_bytes()));
    let mut title = safe_component(title, "untitled", 64);
    if let Some(year) = year.and_then(valid_year) {
        let suffix = format!("({year})");
        if !title.ends_with(&suffix) {
            title.push(' ');
            title.push_str(&suffix);
        }
    }
    PathBuf::from("torrents")
        .join(safe_component(site, "unknown_source", 32))
        .join(safe_component(studio, "unknown_studio", 48))
        .join(format!(
            "{} [{}-{}]",
            title,
            safe_component(&identifier, "item", 20),
            &hash[..8]
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_is_part_of_the_movie_name_and_survives_title_truncation() {
        let url = "https://example.test/topic?t=42";
        for title in ["Example Movie".to_owned(), "A".repeat(140)] {
            let path =
                torrent_directory_with_year("example", "Studio", &title, url, 1, Some("2026"));
            assert_eq!(path.components().count(), 4);
            assert!(path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains(" (2026) [42-"));
        }
        let path = torrent_directory_with_year(
            "example",
            "Studio",
            "Example (2026)",
            url,
            1,
            Some("2026"),
        );
        assert_eq!(path.to_string_lossy().matches("(2026)").count(), 1);
        for invalid in ["", "20266", "0000", "../../2026", "2026garbage"] {
            assert_eq!(
                torrent_directory_with_year("example", "Studio", "Example", url, 1, Some(invalid)),
                torrent_directory("example", "Studio", "Example", url, 1)
            );
        }
    }

    #[test]
    fn production_year_prefers_production_metadata_and_ignores_unrelated_numbers() {
        let attribute = |kind: &str, value: &str| MetadataAttribute {
            kind: kind.into(),
            label: kind.into(),
            value: value.into(),
            source_url: None,
            is_female: None,
        };
        let mut attributes = vec![
            attribute("resolution", "1080"),
            attribute("release_date", "2027-01-01"),
            attribute("date", "2026-06-23"),
            attribute("production_date", "2025-12-31"),
        ];
        assert_eq!(production_year(&attributes), Some("2025"));
        attributes.retain(|a| a.kind != "production_date");
        assert_eq!(production_year(&attributes), Some("2026"));
        assert_eq!(
            production_year(&[attribute("date", "bad"), attribute("year", "2024")]),
            Some("2024")
        );
        assert_eq!(production_year(&[attribute("resolution", "1080")]), None);
    }
}

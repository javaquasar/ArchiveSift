use sha2::{Digest, Sha256};
use std::path::PathBuf;

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
    PathBuf::from("torrents")
        .join(safe_component(site, "unknown_source", 32))
        .join(safe_component(studio, "unknown_studio", 48))
        .join(format!(
            "{} [{}-{}]",
            safe_component(title, "untitled", 64),
            safe_component(&identifier, "item", 20),
            &hash[..8]
        ))
}

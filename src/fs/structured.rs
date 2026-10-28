use std::path::PathBuf;

use crate::models::{MetadataAttribute, ScrapedImage, ScrapedItem};

use super::DirectoryStrategy;

/// Organizes images into a human-browsable directory tree:
/// `{site_key}/{actor}/{content_slug}/{NNN}_{filename}.{ext}`
///
/// This layout can be navigated without the application:
/// - Top level: one directory per site
/// - Second level: one directory per actress (first female actor), falling back
///   to first actor of any gender, then `unknown_actor`
/// - Third level: one directory per content item (derived from page title)
/// - Files: zero-padded sort-order prefix + sanitized original name
pub struct StructuredDirectoryStrategy;

impl DirectoryStrategy for StructuredDirectoryStrategy {
    fn relative_path(
        &self,
        page: &ScrapedItem,
        image: &ScrapedImage,
        original_filename: &str,
    ) -> PathBuf {
        let site = sanitize_component(&page.site_key);
        let actor = pick_folder_actor(&page.attributes);
        let slug = make_content_slug(&page.title);
        let slug = if slug.is_empty() {
            page.page_url
                .split('/')
                .filter(|segment| !segment.is_empty())
                .next_back()
                .map(sanitize_component)
                .filter(|segment| !segment.is_empty())
                .unwrap_or_else(|| "untitled".to_string())
        } else {
            slug
        };
        let n = (image.sort_order as usize) + 1;
        let file = format!("{:03}_{}", n, sanitize_filename(original_filename));
        PathBuf::from(site).join(actor).join(slug).join(file)
    }
}

/// Selects the folder name for the actor dimension of the path.
///
/// Priority:
///   1. First female actor (`is_female == Some(true)`, set by the scraper)
///   2. First actor of any gender (fallback when gender is unknown)
///   3. `"unknown_actor"` (no actor attributes at all)
fn pick_folder_actor(attributes: &[MetadataAttribute]) -> String {
    let actors: Vec<&MetadataAttribute> = attributes.iter().filter(|a| a.kind == "actor").collect();

    // 1. First female actor as determined by the site-specific scraper.
    if let Some(attr) = actors.iter().find(|a| a.is_female == Some(true)) {
        return sanitize_component(&attr.value);
    }

    // 2. First actor regardless of gender.
    if let Some(attr) = actors.first() {
        return sanitize_component(&attr.value);
    }

    // 3. No actors at all.
    "unknown_actor".to_string()
}

/// Returns the filesystem-safe directory name for an actor (same logic used when
/// images are first saved).  Used when renaming/moving files after a primary-actor change.
pub(crate) fn actor_folder_name(actor: &str) -> String {
    sanitize_component(actor)
}

/// Strips characters forbidden in directory/file names and lowercases.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ' ' | '\t' => '_',
            c => c.to_ascii_lowercase(),
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Derives a short filesystem-safe slug from a content title.
/// Lowercases, replaces spaces/runs with `_`, strips non-alphanumeric, truncates at 60 chars on word boundary.
fn make_content_slug(title: &str) -> String {
    // lowercase + replace whitespace runs with single underscore
    let mut slug = String::with_capacity(title.len());
    let mut last_was_sep = false;
    for c in title.chars() {
        if c.is_whitespace() || c == '-' {
            if !last_was_sep {
                slug.push('_');
                last_was_sep = true;
            }
        } else if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_sep = false;
        }
        // skip everything else
    }
    let slug = slug.trim_matches('_').to_string();

    if slug.len() <= 60 {
        return slug;
    }

    // Truncate at last underscore at or before position 60
    let truncated = &slug[..60];
    if let Some(pos) = truncated.rfind('_') {
        slug[..pos].to_string()
    } else {
        truncated.to_string()
    }
}

/// Like `sanitize_component` but preserves the last `.ext` dot.
fn sanitize_filename(name: &str) -> String {
    if let Some(dot_pos) = name.rfind('.') {
        let stem = sanitize_component(&name[..dot_pos]);
        let ext = name[dot_pos + 1..]
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if ext.is_empty() {
            stem
        } else {
            format!("{}.{}", stem, ext)
        }
    } else {
        sanitize_component(name)
    }
}

#[cfg(test)]
mod tests {
    use crate::models::MetadataAttribute;

    use super::*;

    // ── slug helpers ───────────────────────────────────────────────────────────

    #[test]
    fn slug_short_title() {
        assert_eq!(make_content_slug("Hot Summer Fun"), "hot_summer_fun");
    }

    #[test]
    fn slug_strips_special_chars() {
        assert_eq!(make_content_slug("A & B: C!"), "a_b_c");
    }

    #[test]
    fn non_latin_scene_title_uses_scene_id_for_directory() {
        let mut page = ScrapedItem {
            site_key: "example_site".to_string(),
            page_url: "https://source.example.test/scenes/00000000-0000-0000-0000-000000000001"
                .to_string(),
            title: "日本語".to_string(),
            raw_html: String::new(),
            images: Vec::new(),
            attributes: Vec::new(),
        };
        let image = ScrapedImage {
            source_url: "https://source.example.test/images/example".to_string(),
            preview_url: None,
            alt_text: None,
            width: None,
            height: None,
            sort_order: 0,
            local_file_path: None,
        };
        let path = StructuredDirectoryStrategy.relative_path(&page, &image, "example.jpg");
        assert!(path
            .to_string_lossy()
            .contains("00000000-0000-0000-0000-000000000001"));
        page.title = "Named Scene".to_string();
        let path = StructuredDirectoryStrategy.relative_path(&page, &image, "example.jpg");
        assert!(path.to_string_lossy().contains("named_scene"));
    }

    #[test]
    fn slug_truncates_at_word_boundary() {
        let title = "Redheaded Babe Penny Pax Gets Double Fucked and Jizzed in an MMF Threesome";
        let slug = make_content_slug(title);
        assert!(slug.len() <= 60, "slug too long: {}", slug);
        assert!(!slug.ends_with('_'));
    }

    #[test]
    fn sanitize_component_strips_forbidden() {
        assert_eq!(sanitize_component("Penny Pax"), "penny_pax");
        assert_eq!(sanitize_component("A/B\\C"), "a_b_c");
    }

    #[test]
    fn sanitize_filename_preserves_ext() {
        assert_eq!(sanitize_filename("Hello World.JPG"), "hello_world.jpg");
        assert_eq!(sanitize_filename("abc123.jpeg"), "abc123.jpeg");
    }

    // ── pick_folder_actor ──────────────────────────────────────────────────────
    // Gender is expressed via `is_female` field set by the scraper — structured.rs
    // has no knowledge of site-specific URL patterns.

    fn actor(value: &str, is_female: Option<bool>) -> MetadataAttribute {
        MetadataAttribute {
            kind: "actor".to_string(),
            label: "Models".to_string(),
            value: value.to_string(),
            source_url: None,
            is_female,
        }
    }

    #[test]
    fn picks_female_actor_when_only_female_present() {
        let attrs = vec![actor("Penny Pax", Some(true))];
        assert_eq!(pick_folder_actor(&attrs), "penny_pax");
    }

    #[test]
    fn picks_first_female_over_male_when_male_listed_first() {
        let attrs = vec![
            actor("John Doe", Some(false)),
            actor("Penny Pax", Some(true)),
            actor("Mia Malkova", Some(true)),
        ];
        assert_eq!(pick_folder_actor(&attrs), "penny_pax");
    }

    #[test]
    fn picks_first_female_when_multiple_females() {
        let attrs = vec![
            actor("Penny Pax", Some(true)),
            actor("Mia Malkova", Some(true)),
        ];
        assert_eq!(pick_folder_actor(&attrs), "penny_pax");
    }

    #[test]
    fn falls_back_to_first_actor_when_gender_unknown() {
        let attrs = vec![actor("Actor A", None), actor("Actor B", None)];
        assert_eq!(pick_folder_actor(&attrs), "actor_a");
    }

    #[test]
    fn falls_back_to_unknown_actor_when_no_actors() {
        let attrs: Vec<MetadataAttribute> = vec![];
        assert_eq!(pick_folder_actor(&attrs), "unknown_actor");
    }

    #[test]
    fn ignores_non_actor_attributes() {
        let attrs = vec![MetadataAttribute {
            kind: "tag".to_string(),
            label: "tag".to_string(),
            value: "outdoor".to_string(),
            source_url: None,
            is_female: None,
        }];
        assert_eq!(pick_folder_actor(&attrs), "unknown_actor");
    }

    // ── full path ──────────────────────────────────────────────────────────────

    #[test]
    fn structured_path_uses_female_actor() {
        use crate::models::{ScrapedImage, ScrapedItem};

        let page = ScrapedItem {
            site_key: "example_gallery".to_string(),
            page_url: "https://example.com/gallery".to_string(),
            title: "Hot Summer Fun".to_string(),
            raw_html: String::new(),
            images: vec![],
            attributes: vec![
                actor("John Doe", Some(false)),
                actor("Penny Pax", Some(true)),
            ],
        };
        let image = ScrapedImage {
            source_url: "https://example.com/img/abc.jpg".to_string(),
            preview_url: None,
            alt_text: None,
            width: None,
            height: None,
            sort_order: 0,
            local_file_path: None,
        };

        let strategy = StructuredDirectoryStrategy;
        let path = strategy.relative_path(&page, &image, "abc.jpg");
        assert_eq!(
            path,
            PathBuf::from("example_gallery/penny_pax/hot_summer_fun/001_abc.jpg")
        );
    }
}

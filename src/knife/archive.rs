use std::path::PathBuf;

use serde_json::Value;
use url::Url;

use super::{identity, KnifeProfile, MAX_NAME};
use crate::models::MetadataAttribute;

#[derive(Debug)]
pub(crate) struct ArchiveKnife {
    pub row: usize,
    pub id: Option<String>,
    pub manufacturer: String,
    pub model: String,
    pub steel: Option<String>,
    pub title: String,
    pub sources: Vec<String>,
    pub attributes: Vec<MetadataAttribute>,
    pub images: Vec<ArchiveImage>,
}

#[derive(Debug)]
pub(crate) struct ArchiveImage {
    /// Path below the archive's images folder, using the original filename.
    pub relative: PathBuf,
    pub url: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ImportResult {
    pub new_pages: usize,
    pub existing_pages: usize,
    pub skipped_records: usize,
    pub copied_files: usize,
    pub reused_files: usize,
    pub duplicate_files: usize,
    pub skipped_files: usize,
    pub errors: Vec<String>,
}

impl ArchiveKnife {
    pub fn from_json(row: usize, record: &Value, profile: KnifeProfile<'_>) -> Self {
        let KnifeProfile {
            manufacturer,
            model,
            steel,
        } = profile;
        let id = text(&record["id"], MAX_NAME * 2).map(identity);
        let title = text(&record["displayName"], MAX_NAME * 2)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{manufacturer} - {model}"));
        let mut sources = Vec::new();
        for url in record["sourceUrls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(web_url)
        {
            if !sources.contains(&url) {
                sources.push(url);
            }
        }
        let mut attributes = Vec::new();
        let mut add = |kind, label, value: Option<&str>| {
            if let Some(value) = value {
                attributes.push(attribute(kind, label, value));
            }
        };
        add("catalog_profile", "Catalog profile", Some(super::PROFILE));
        add("manufacturer", "Manufacturer", Some(manufacturer));
        add("model", "Model", Some(model));
        add("steel", "Blade steel", steel);
        add(
            "archive_id",
            "Archive ID",
            text(&record["id"], MAX_NAME * 2),
        );
        for (field, kind, label, limit) in [
            ("knifeType", "category", "Knife type", MAX_NAME),
            (
                "technicalDetails",
                "technical_details",
                "Technical details",
                32_768,
            ),
            ("description", "description", "Description", 32_768),
            ("notes", "notes", "Archive notes", 32_768),
            (
                "priceCurrency",
                "archived_price_currency",
                "Archived currency",
                12,
            ),
            (
                "priceParsedAt",
                "archived_price_date",
                "Archived price date",
                32,
            ),
            (
                "availabilityTextAtParse",
                "archived_availability",
                "Archived availability",
                MAX_NAME,
            ),
        ] {
            add(kind, label, text(&record[field], limit));
        }
        for (field, kind, label) in [
            ("priceAtParse", "archived_price", "Archived price"),
            ("bladeLengthMm", "blade_length_mm", "Blade length (mm)"),
            ("weightG", "weight_g", "Weight (g)"),
            (
                "availableAtParse",
                "archived_in_stock",
                "In stock at capture",
            ),
            (
                "discontinuedAtParse",
                "archived_discontinued",
                "Discontinued at capture",
            ),
        ] {
            let value = &record[field];
            let scalar = (value.is_number() || value.is_boolean()).then(|| value.to_string());
            add(
                kind,
                label,
                scalar.as_deref().or_else(|| text(value, MAX_NAME)),
            );
        }
        for tag in record["tags"].as_array().into_iter().flatten() {
            add("tag", "Tag", text(tag, MAX_NAME));
        }
        for url in &sources {
            let mut source = attribute("archive_source", "Archive source", url);
            source.source_url = Some(url.clone());
            attributes.push(source);
        }
        Self {
            row,
            id,
            title,
            sources,
            attributes,
            images: Vec::new(),
            manufacturer: manufacturer.into(),
            model: model.into(),
            steel: steel.map(str::to_owned),
        }
    }

    pub fn profile(&self) -> KnifeProfile<'_> {
        KnifeProfile {
            manufacturer: &self.manufacturer,
            model: &self.model,
            steel: self.steel.as_deref(),
        }
    }
}

fn text(value: &Value, limit: usize) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= limit)
}

fn attribute(kind: &str, label: &str, value: &str) -> MetadataAttribute {
    MetadataAttribute {
        kind: kind.into(),
        label: label.into(),
        value: value.into(),
        source_url: None,
        is_female: None,
    }
}

pub(super) fn web_url(value: &Value) -> Option<String> {
    let mut url = Url::parse(text(value, 2048)?).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_fragment(None);
    Some(url.into())
}

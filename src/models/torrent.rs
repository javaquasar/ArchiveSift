use super::MetadataAttribute;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TorrentQuery {
    pub text: String,
    pub site_key: String,
    pub studio: String,
    pub creators: Vec<String>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    pub year: String,
    pub offset: usize,
}

#[derive(Clone, Debug)]
pub struct TorrentImage {
    pub image_id: i64,
    pub source_url: String,
    pub local_file_path: Option<String>,
    pub thumbnail_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TorrentAttachment {
    pub source_url: String,
    pub filename: String,
    pub local_file_path: String,
}

#[derive(Clone, Debug)]
pub struct TorrentCard {
    pub page_id: i64,
    pub site_key: String,
    pub page_url: String,
    pub title: String,
    pub primary_studio: String,
    pub primary_image_id: Option<i64>,
    pub directory: Option<String>,
    pub folder_name: Option<String>,
    pub revision: i64,
    pub attributes: Vec<MetadataAttribute>,
    pub images: Vec<TorrentImage>,
    pub attachments: Vec<TorrentAttachment>,
}

impl TorrentCard {
    pub fn directory_name(&self) -> Option<&str> {
        self.directory.as_deref()?.rsplit(['/', '\\']).next()
    }

    pub fn primary_image(&self) -> Option<&TorrentImage> {
        self.images
            .iter()
            .find(|image| Some(image.image_id) == self.primary_image_id)
            .or_else(|| self.images.first())
    }

    pub fn values(&self, kind: &str) -> Vec<String> {
        self.attributes
            .iter()
            .filter(|a| a.kind == kind)
            .map(|a| a.value.clone())
            .collect()
    }

    pub fn value(&self, kind: &str) -> &str {
        self.attributes
            .iter()
            .find(|a| a.kind == kind && !a.value.trim().is_empty())
            .map_or("", |attribute| attribute.value.as_str())
    }

    pub fn content_title(&self) -> &str {
        match self.value("content_title") {
            "" => &self.title,
            title => title,
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TorrentField {
    Title,
    Release,
    Duration,
    OriginalLanguage,
    TranslationLanguage,
    Series,
    Episode,
    StudioCode,
}

impl TorrentField {
    pub const ALL: [Self; 8] = [
        Self::Title,
        Self::Release,
        Self::Duration,
        Self::OriginalLanguage,
        Self::TranslationLanguage,
        Self::Series,
        Self::Episode,
        Self::StudioCode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Catalog title",
            Self::Release => "Release date / year",
            Self::Duration => "Duration",
            Self::OriginalLanguage => "Original language",
            Self::TranslationLanguage => "Translation language",
            Self::Series => "Series",
            Self::Episode => "Part / episode",
            Self::StudioCode => "Studio code",
        }
    }

    pub(crate) fn kind(self) -> &'static str {
        match self {
            Self::Title => "content_title",
            Self::Release => "date",
            Self::Duration => "duration",
            Self::OriginalLanguage => "language",
            Self::TranslationLanguage => "translation_language",
            Self::Series => "series",
            Self::Episode => "episode",
            Self::StudioCode => "studio_code",
        }
    }

    pub fn value(self, card: &TorrentCard) -> &str {
        match self {
            Self::Release => crate::fs::torrent::production_date(&card.attributes)
                .or_else(|| {
                    crate::fs::torrent::RELEASE_DATE_KINDS
                        .into_iter()
                        .map(|kind| card.value(kind))
                        .find(|value| !value.is_empty())
                })
                .unwrap_or_default(),
            _ => card.value(self.kind()),
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TorrentEdit {
    pub primary_studio: String,
    #[serde(default)]
    pub folder_name: Option<String>,
    #[serde(default)]
    pub primary_image_id: Option<i64>,
    pub studios: Vec<String>,
    pub tags: Vec<String>,
    pub categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<TorrentField, String>,
    #[serde(default)]
    pub regenerate_folder: bool,
}

impl From<&TorrentCard> for TorrentEdit {
    fn from(card: &TorrentCard) -> Self {
        Self {
            primary_studio: card.primary_studio.clone(),
            folder_name: card.folder_name.clone(),
            primary_image_id: card.primary_image_id,
            studios: card.values("studio"),
            tags: card.values("tag"),
            categories: card.values("category"),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct TorrentPage {
    pub cards: Vec<TorrentCard>,
    pub total: usize,
}

#[derive(Clone, Debug)]
pub struct TorrentSummary {
    pub page_id: i64,
    pub site_key: String,
    pub primary_studio: String,
    pub content_title: String,
    pub image_count: usize,
    pub attachment_count: usize,
}

#[derive(Clone, Debug)]
pub struct TorrentSummaryPage {
    pub cards: Vec<TorrentSummary>,
    pub total: usize,
}

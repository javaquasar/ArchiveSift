use super::MetadataAttribute;

#[derive(Clone, Debug, Default)]
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

    pub fn content_title(&self) -> &str {
        self.attributes
            .iter()
            .find(|a| a.kind == "content_title" && !a.value.trim().is_empty())
            .map(|a| a.value.as_str())
            .unwrap_or(&self.title)
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

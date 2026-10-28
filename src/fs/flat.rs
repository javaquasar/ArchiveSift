use std::path::PathBuf;

use crate::models::{ScrapedImage, ScrapedItem};

use super::DirectoryStrategy;

/// Preserves the original flat download behavior: all images go into the
/// root of `base_dir` with actor/channel info appended before the extension.
///
/// Example: `abc.jpg Penny Pax [New Sensations].jpg`
pub struct FlatDirectoryStrategy;

impl DirectoryStrategy for FlatDirectoryStrategy {
    fn relative_path(
        &self,
        page: &ScrapedItem,
        _image: &ScrapedImage,
        original_filename: &str,
    ) -> PathBuf {
        let actors = page.attribute_values("actor").collect::<Vec<_>>();
        let channels = page.attribute_values("channel").collect::<Vec<_>>();
        let suffix = format!(" {} [{}]", join_values(&actors), join_values(&channels));
        PathBuf::from(insert_before_dot(original_filename, &suffix))
    }
}

fn join_values(values: &[&str]) -> String {
    if values.is_empty() {
        "Unknown".to_string()
    } else {
        values.join(", ")
    }
}

fn insert_before_dot(name: &str, insert: &str) -> String {
    if let Some(pos) = name.rfind('.') {
        let (stem, ext) = name.split_at(pos);
        format!("{}{}{}", stem, insert, ext)
    } else {
        name.to_string()
    }
}

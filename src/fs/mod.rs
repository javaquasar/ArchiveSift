mod flat;
pub mod structured;

use std::path::PathBuf;

use crate::models::{ScrapedImage, ScrapedItem};

/// Returns the path of a thumbnail **relative to the thumbs root directory**.
///
/// Thumbnails are sharded into 1 000-image buckets to avoid large flat
/// directories:
///
/// ```text
/// thumbs/
///   0/      ← image IDs   0 – 999
///   1/      ← image IDs 1000 – 1999
///   …
/// ```
///
/// Example: `thumb_rel_path(1234)` → `"1/1234.jpg"`
pub fn thumb_rel_path(image_id: i64) -> PathBuf {
    let bucket = image_id / 1000;
    PathBuf::from(bucket.to_string()).join(format!("{}.jpg", image_id))
}

pub use flat::FlatDirectoryStrategy;
pub use structured::StructuredDirectoryStrategy;

/// Controls how downloaded image files are organized on disk.
///
/// Implementations receive page and image metadata and return a path
/// **relative to the configured base download directory**. The caller
/// stores this relative path in the database, so the base directory
/// can be relocated without invalidating stored records.
pub trait DirectoryStrategy: Send + Sync {
    /// Returns the relative path (from `base_dir`) for a single image.
    ///
    /// `original_filename` is the bare filename extracted from the image URL
    /// (e.g. `"abc123.jpg"`).
    fn relative_path(
        &self,
        page: &ScrapedItem,
        image: &ScrapedImage,
        original_filename: &str,
    ) -> PathBuf;
}

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use serde::de::{DeserializeSeed, Error, SeqAccess, Visitor};
use serde_json::Value;

use super::{identity, image_extension, KnifeProfile, MAX_NAME};

const MAX_PREVIEW_ROWS: usize = 1000;

#[derive(Debug, Default)]
pub(crate) struct ImportPreview {
    pub records: usize,
    pub invalid_records: usize,
    pub duplicate_ids: usize,
    pub ready_files: usize,
    pub missing_files: usize,
    pub rejected_files: usize,
    pub conflicting_files: usize,
    pub rows: Vec<PreviewRow>,
}

#[derive(Debug, Default)]
pub(crate) struct PreviewRow {
    pub number: usize,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub steel: Option<String>,
    pub directory: Option<PathBuf>,
    pub duplicate_id: bool,
    pub files: usize,
    pub ready: usize,
    pub missing: usize,
    pub rejected: usize,
    pub conflicts: usize,
    pub first_destination: Option<PathBuf>,
}

/// Inspect the legacy index and its sibling images folder without changing either library.
pub(crate) fn preview(index: &Path, download_root: &Path) -> io::Result<ImportPreview> {
    let file = File::open(index)?;
    let images = index.parent().unwrap_or(Path::new("")).join("images");
    let images = match images.canonicalize() {
        Ok(path) => Some(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::with_capacity(128 * 1024, file);
    if reader.fill_buf()?.starts_with(b"\xef\xbb\xbf") {
        reader.consume(3);
    }
    let mut input = serde_json::Deserializer::from_reader(reader);
    let summary = PreviewVisitor {
        images,
        download_root,
    }
    .deserialize(&mut input)?;
    input.end()?;
    Ok(summary)
}

struct PreviewVisitor<'a> {
    images: Option<PathBuf>,
    download_root: &'a Path,
}

impl<'de> DeserializeSeed<'de> for PreviewVisitor<'_> {
    type Value = ImportPreview;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for PreviewVisitor<'_> {
    type Value = ImportPreview;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of knife records")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<ImportPreview, S::Error> {
        let mut summary = ImportPreview::default();
        let mut ids = HashSet::new();
        let mut manufacturers = HashMap::new();
        let mut models = HashMap::new();
        let mut destinations = HashSet::new();
        while let Some(record) = seq.next_element::<Value>()? {
            summary.records += 1;
            let value = |field, limit| {
                record
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && value.len() <= limit)
            };
            let manufacturer = value("company", MAX_NAME);
            let model = value("knifeName", MAX_NAME);
            let steel = value("steel", MAX_NAME);
            let duplicate_id =
                value("id", MAX_NAME * 2).is_some_and(|id| !ids.insert(identity(id)));
            let directory = manufacturer.zip(model).map(|(manufacturer, model)| {
                let brand_key = identity(manufacturer);
                let manufacturer = manufacturers
                    .entry(brand_key.clone())
                    .or_insert_with(|| manufacturer.to_owned());
                let directory = models
                    .entry((brand_key, identity(model)))
                    .or_insert_with(|| {
                        KnifeProfile {
                            manufacturer,
                            model,
                            steel,
                        }
                        .directory()
                    });
                directory.clone()
            });
            let refs = record
                .get("imageFiles")
                .and_then(Value::as_array)
                .filter(|files| !files.is_empty())
                .map(Vec::as_slice)
                .or_else(|| {
                    record
                        .get("imageFile")
                        .filter(|value| !value.is_null())
                        .map(std::slice::from_ref)
                })
                .unwrap_or_default();
            let mut row = PreviewRow {
                number: summary.records,
                manufacturer: manufacturer.map(str::to_owned),
                model: model.map(str::to_owned),
                steel: steel.map(str::to_owned),
                duplicate_id,
                files: refs.len(),
                ..Default::default()
            };
            for file in refs {
                let Some(relative) = file.as_str().and_then(safe_image_path) else {
                    row.rejected += 1;
                    continue;
                };
                let Some(images) = &self.images else {
                    row.missing += 1;
                    continue;
                };
                match images.join(&relative).canonicalize() {
                    Ok(path) if path.starts_with(images) && path.is_file() => {}
                    Ok(_) => {
                        row.rejected += 1;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        row.missing += 1;
                        continue;
                    }
                    Err(error) => return Err(S::Error::custom(error)),
                }
                let Some(directory) = &directory else {
                    continue;
                };
                let target =
                    directory.join(relative.file_name().expect("validated image filename"));
                if row.first_destination.is_none() {
                    row.first_destination = Some(target.clone());
                }
                let key = target.to_string_lossy().to_lowercase();
                let exists = self
                    .download_root
                    .join(&target)
                    .try_exists()
                    .map_err(S::Error::custom)?;
                if !destinations.insert(key) || exists {
                    row.conflicts += 1;
                } else {
                    row.ready += 1;
                }
            }
            summary.invalid_records += usize::from(directory.is_none());
            summary.duplicate_ids += usize::from(duplicate_id);
            summary.ready_files += row.ready;
            summary.missing_files += row.missing;
            summary.rejected_files += row.rejected;
            summary.conflicting_files += row.conflicts;
            row.directory = directory;
            if summary.rows.len() < MAX_PREVIEW_ROWS {
                summary.rows.push(row);
            }
        }
        Ok(summary)
    }
}

fn safe_image_path(value: &str) -> Option<PathBuf> {
    if value.is_empty() || value.len() > 2048 {
        return None;
    }
    let value = value.replace('\\', "/");
    if value.starts_with('/')
        || value.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part
                    .chars()
                    .any(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
                || part.ends_with(['.', ' '])
                || super::is_windows_device_name(part)
        })
    {
        return None;
    }
    let path = Path::new(&value);
    if !path
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        || !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(image_extension)
    {
        return None;
    }
    Some(path.components().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn preview_retains_names_and_reports_import_problems_without_writing() {
        let archive = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let images = archive.path().join("images");
        fs::create_dir_all(images.join("nested")).unwrap();
        fs::write(
            images.join("nested/Forge - Ridge - D2 - 001.png"),
            b"fixture",
        )
        .unwrap();
        fs::write(images.join("another.jpg"), b"fixture").unwrap();
        let index = archive.path().join("knives.json");
        let input = serde_json::json!([
            {"id":"ridge","company":"Forge","knifeName":"Ridge","steel":"D2","imageFiles":[
                "nested\\Forge - Ridge - D2 - 001.png","missing.jpg","../outside.jpg","another.jpg"]},
            {"id":"RIDGE","company":"FORGE","knifeName":"ridge","imageFile":"nested/Forge - Ridge - D2 - 001.png"},
            {"company":"X".repeat(300_000),"knifeName":"Unknown","imageFile":"X".repeat(300_000)},
            {"company":"Forge","knifeName":"Path checks","imageFiles":["C:\\Pictures\\a.png","/external.png","nested\\..\\another.jpg"]}
        ]);
        fs::write(&index, input.to_string()).unwrap();
        let model_directory = destination.path().join("knife").join("Forge").join("Ridge");
        fs::create_dir_all(&model_directory).unwrap();
        fs::write(model_directory.join("another.jpg"), b"pre-existing").unwrap();

        let report = preview(&index, destination.path()).unwrap();
        assert_eq!(report.records, 4);
        assert_eq!(report.duplicate_ids, 1);
        assert_eq!(report.rows[0].directory, report.rows[1].directory);
        assert!(safe_image_path("AUX.txt/cover.png").is_none());
        assert_eq!(report.invalid_records, 1);
        assert_eq!(report.ready_files, 1);
        assert_eq!(report.conflicting_files, 2);
        assert_eq!(report.rejected_files, 5);
        assert_eq!(report.missing_files, 1);
        assert_eq!(
            report.rows[0].first_destination,
            Some(
                PathBuf::from("knife")
                    .join("Forge")
                    .join("Ridge")
                    .join("Forge - Ridge - D2 - 001.png")
            )
        );
        assert_eq!(fs::read_dir(&model_directory).unwrap().count(), 1);
        assert_eq!(
            fs::read(model_directory.join("another.jpg")).unwrap(),
            b"pre-existing"
        );
        assert_eq!(fs::read_to_string(index).unwrap(), input.to_string());
    }

    #[test]
    fn preview_handles_missing_images_and_rejects_invalid_json() {
        let archive = tempfile::tempdir().unwrap();
        let index = archive.path().join("knives.json");
        fs::write(
            &index,
            r#"[{"company":"Forge","knifeName":"Ridge","imageFile":"cover.png"}]"#,
        )
        .unwrap();
        assert_eq!(preview(&index, archive.path()).unwrap().missing_files, 1);
        fs::write(&index, b"\xef\xbb\xbf[]").unwrap();
        assert_eq!(preview(&index, archive.path()).unwrap().records, 0);
        for invalid in ["{}", "[] trailing", "[1,"] {
            fs::write(&index, invalid).unwrap();
            assert!(preview(&index, archive.path()).is_err());
        }
    }

    #[test]
    #[ignore = "requires a local legacy archive"]
    fn preview_local_archive() {
        let index = PathBuf::from(std::env::var_os("ARCHIVESIFT_KNIFE_INDEX").unwrap());
        let root = PathBuf::from(std::env::var_os("ARCHIVESIFT_KNIFE_DOWNLOAD_ROOT").unwrap());
        let report = preview(&index, &root).unwrap();
        assert!(report.records > 0);
        println!("{} records, {} invalid, {} repeated IDs, {} files ready, {} missing, {} invalid paths, {} collisions",
            report.records, report.invalid_records, report.duplicate_ids, report.ready_files,
            report.missing_files, report.rejected_files, report.conflicting_files);
    }
}

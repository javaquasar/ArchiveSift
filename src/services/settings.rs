use std::path::{Path, PathBuf};

use crate::app::AppConfig;
use crate::db::Database;
use crate::AppResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageSettings {
    pub download_folder: String,
    pub database_path: String,
    pub thumbnail_folder: Option<String>,
}

impl From<&AppConfig> for StorageSettings {
    fn from(config: &AppConfig) -> Self {
        Self {
            download_folder: storage_path(&config.download_folder),
            database_path: storage_path(&config.database_path),
            thumbnail_folder: config.thumbnail_folder.as_deref().map(storage_path),
        }
    }
}

fn storage_path(value: &str) -> String {
    Path::new(value.trim())
        .components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

pub struct SettingsService;

impl SettingsService {
    pub fn new() -> Self {
        Self
    }

    pub fn list_recent_download_dirs(&self, config: &AppConfig) -> Vec<PathBuf> {
        config
            .download_folder_history
            .iter()
            .map(PathBuf::from)
            .collect()
    }

    pub fn set_active_download_dir(&self, config: &mut AppConfig, path: impl AsRef<Path>) {
        config.set_active_download_folder(path.as_ref().display().to_string());
    }

    /// Validate the selected library before persisting any settings. Run on a
    /// worker because creating/opening a database on a network drive can be slow.
    pub fn apply_storage_settings(
        &self,
        current: &AppConfig,
        settings: &StorageSettings,
        config_path: &Path,
    ) -> AppResult<AppConfig> {
        if settings.database_path.trim().is_empty() {
            return Err("Select a database file.".into());
        }
        if settings
            .thumbnail_folder
            .as_ref()
            .is_some_and(|s| s.trim().is_empty())
        {
            return Err("Select a thumbnail folder or use the default location.".into());
        }
        let mut config = current.clone();
        config
            .download_folder_history
            .insert(0, current.download_folder.clone());
        config.set_active_download_folder(storage_path(&settings.download_folder));
        config.database_path = storage_path(&settings.database_path);
        config.thumbnail_folder = settings.thumbnail_folder.as_deref().map(storage_path);

        validate_database_file(Path::new(&config.database_path))?;
        for folder in [
            Some(config.download_folder.as_str()),
            config.thumbnail_folder.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        {
            if Path::new(folder).exists() && !Path::new(folder).is_dir() {
                return Err(format!("Not a folder: {folder}").into());
            }
        }
        Database::new(&config.database_path).initialize()?;
        if config.has_download_folder() {
            std::fs::create_dir_all(&config.download_folder)?;
        }
        std::fs::create_dir_all(config.thumb_dir())?;
        config.save_to(config_path)?;
        Ok(config)
    }
}

fn validate_database_file(path: &Path) -> AppResult<()> {
    if !path.exists() {
        return Ok(());
    }
    if !path.is_file() {
        return Err("The database path must point to a file.".into());
    }
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if !tables.is_empty()
        && [
            "source_pages",
            "images",
            "page_images",
            "attributes",
            "page_attributes",
            "image_attributes",
        ]
        .iter()
        .any(|table| !tables.iter().any(|existing| existing == table))
    {
        return Err("This file is not an ArchiveSift library database.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_settings_create_a_library_and_roundtrip_custom_paths() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("settings/app-config.json");
        let old_folder = root.path().join("old-files");
        std::fs::create_dir(&old_folder).unwrap();
        let original_file = old_folder.join("keep.txt");
        std::fs::write(&original_file, b"existing content").unwrap();
        let current = AppConfig {
            download_folder: old_folder.display().to_string(),
            preset_tags: "outdoor".into(),
            ..AppConfig::default()
        };
        let settings = StorageSettings {
            download_folder: root.path().join("new-files").display().to_string(),
            database_path: root
                .path()
                .join("library")
                .join("archive.sqlite3")
                .display()
                .to_string(),
            thumbnail_folder: Some(root.path().join("previews").display().to_string()),
        };
        SettingsService::new()
            .apply_storage_settings(&current, &settings, &config_path)
            .unwrap();
        let restored = AppConfig::load_from(&config_path).unwrap();
        assert_eq!(StorageSettings::from(&restored), settings);
        assert_eq!(restored.preset_tags, current.preset_tags);
        assert_eq!(
            restored.download_folder_history[0],
            settings.download_folder
        );
        assert!(restored
            .download_folder_history
            .contains(&current.download_folder));
        assert!(restored.thumb_dir().is_dir());
        let db = rusqlite::Connection::open(&settings.database_path).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM source_pages", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(std::fs::read(original_file).unwrap(), b"existing content");
    }

    #[test]
    fn storage_settings_attach_existing_library_without_changing_records_or_files() {
        let root = tempfile::tempdir().unwrap();
        let database_path = root.path().join("existing.sqlite3");
        Database::new(&database_path).initialize().unwrap();
        let db = rusqlite::Connection::open(&database_path).unwrap();
        db.execute("INSERT INTO source_pages(site_key,page_url,title,raw_html) VALUES ('demo-site','https://demo.example.test/galleries/one','Existing page','')", []).unwrap();
        let settings = StorageSettings {
            download_folder: String::new(),
            database_path: database_path.display().to_string(),
            thumbnail_folder: None,
        };
        let updated = SettingsService::new()
            .apply_storage_settings(
                &AppConfig::default(),
                &settings,
                &root.path().join("config.json"),
            )
            .unwrap();
        assert!(!updated.has_download_folder());
        assert_eq!(updated.thumb_dir(), root.path().join("thumbs"));
        assert!(updated.thumb_dir().is_dir());
        assert_eq!(
            db.query_row("SELECT title FROM source_pages", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "Existing page"
        );
    }

    #[test]
    fn storage_settings_reject_invalid_paths_and_foreign_databases_without_saving() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.json");
        let current = AppConfig::default();
        current.save_to(&config_path).unwrap();
        let before = std::fs::read(&config_path).unwrap();
        let text_file = root.path().join("not-sqlite.db");
        std::fs::write(&text_file, b"not a database").unwrap();
        let foreign_path = root.path().join("foreign.sqlite3");
        let foreign = rusqlite::Connection::open(&foreign_path).unwrap();
        foreign
            .execute_batch("CREATE TABLE keep(value TEXT); INSERT INTO keep VALUES ('original');")
            .unwrap();
        let mut invalid = Vec::new();
        for path in [
            String::new(),
            root.path().display().to_string(),
            text_file.display().to_string(),
            foreign_path.display().to_string(),
        ] {
            invalid.push(StorageSettings {
                database_path: path,
                ..StorageSettings::from(&current)
            });
        }
        invalid.push(StorageSettings {
            database_path: root.path().join("new.sqlite3").display().to_string(),
            thumbnail_folder: Some("   ".to_string()),
            ..StorageSettings::from(&current)
        });
        invalid.push(StorageSettings {
            database_path: root.path().join("new.sqlite3").display().to_string(),
            download_folder: text_file.display().to_string(),
            ..StorageSettings::from(&current)
        });
        for settings in invalid {
            assert!(
                SettingsService::new()
                    .apply_storage_settings(&current, &settings, &config_path)
                    .is_err(),
                "Accepted {settings:?}"
            );
            assert_eq!(std::fs::read(&config_path).unwrap(), before);
        }
        assert_eq!(
            foreign
                .query_row("SELECT value FROM keep", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "original"
        );
        assert_eq!(
            foreign
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='source_pages'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(!root.path().join("new.sqlite3").exists());
    }

    #[test]
    fn storage_settings_report_config_write_failures() {
        let root = tempfile::tempdir().unwrap();
        let current = AppConfig::default();
        let before = serde_json::to_value(&current).unwrap();
        let settings = StorageSettings {
            download_folder: String::new(),
            database_path: root.path().join("library.sqlite3").display().to_string(),
            thumbnail_folder: None,
        };
        assert!(SettingsService::new()
            .apply_storage_settings(&current, &settings, root.path())
            .is_err());
        assert_eq!(serde_json::to_value(&current).unwrap(), before);
    }

    #[test]
    fn legacy_settings_without_thumbnail_override_keep_database_relative_location() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let mut json = serde_json::to_value(AppConfig::default()).unwrap();
        json.as_object_mut().unwrap().remove("thumbnail_folder");
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let restored = AppConfig::load_from(&path).unwrap();
        assert!(restored.thumbnail_folder.is_none());
        assert_eq!(restored.thumb_dir(), Path::new("data").join("thumbs"));
    }
}

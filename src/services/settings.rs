use std::path::{Path, PathBuf};

use crate::app::AppConfig;

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
}

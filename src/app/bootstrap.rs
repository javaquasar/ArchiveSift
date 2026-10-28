use crate::app::AppConfig;
use crate::ui::{egui_ui::EguiBackend, UiBackend};
use crate::AppResult;

/// The application entry point.
///
/// `App` is UI-agnostic: it holds configuration and delegates rendering to a
/// [`UiBackend`] implementation. Swap `EguiBackend` for a different backend
/// (e.g. `TauriBackend`) without touching this struct.
pub struct App {
    config: AppConfig,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    /// Runs the application with the default egui/eframe desktop backend.
    pub fn run(&self) -> AppResult<()> {
        EguiBackend.run(self.config.clone())
    }
}

impl Default for App {
    fn default() -> Self {
        let config = AppConfig::load().unwrap_or_default();
        Self::new(config)
    }
}

/// Convenience entry point: load config from disk and run with the egui backend.
pub fn run() -> AppResult<()> {
    App::default().run()
}

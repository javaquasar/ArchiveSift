use eframe::egui;

use crate::app::AppConfig;
use crate::models::InputMode;

use super::truncate;

/// Carries the user's choices from the home screen to the main app.
pub struct HomeTransition {
    pub input_mode: InputMode,
    pub download_folder: String,
}

/// State for the home / setup screen shown before the main window.
pub struct HomeScreenState {
    pub input_mode: InputMode,
    pub download_folder: String,
    pub download_folder_history: Vec<String>,
    pub error: Option<String>,
}

impl HomeScreenState {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            input_mode: config.input_mode.clone(),
            download_folder: config.download_folder.clone(),
            download_folder_history: config.download_folder_history.clone(),
            error: None,
        }
    }
}

/// Renders the home/setup screen each frame.
/// Returns `Some(HomeTransition)` once when the user clicks "Start Scraping".
pub fn render_home_screen(
    state: &mut HomeScreenState,
    ctx: &egui::Context,
) -> Option<HomeTransition> {
    let mut transition = None;

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.heading("ArchiveSift");
            ui.label("Configure your session before starting.");
            ui.add_space(20.0);

            ui.group(|ui| {
                ui.label("Input mode:");
                ui.radio_value(
                    &mut state.input_mode,
                    InputMode::ClipboardMonitor,
                    "Monitor clipboard (auto-add copied URLs)",
                );
                ui.radio_value(
                    &mut state.input_mode,
                    InputMode::ManualUrl,
                    "Manual URL input only",
                );
            });

            ui.add_space(12.0);

            ui.group(|ui| {
                ui.label("Base download directory:");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut state.download_folder);
                    if ui.button("Browse").clicked() {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            state.download_folder = folder.display().to_string();
                            state.error = None;
                        }
                    }
                });

                if !state.download_folder_history.is_empty() {
                    egui::ComboBox::from_label("Recent folders")
                        .selected_text(if state.download_folder.is_empty() {
                            "Choose a recent folder\u{2026}".to_string()
                        } else {
                            truncate(&state.download_folder, 50)
                        })
                        .show_ui(ui, |ui| {
                            for folder in state.download_folder_history.clone() {
                                if ui.selectable_label(false, &folder).clicked() {
                                    state.download_folder = folder;
                                    state.error = None;
                                }
                            }
                        });
                }
            });

            ui.add_space(12.0);

            if let Some(error) = &state.error {
                ui.colored_label(egui::Color32::RED, error);
            }

            let can_start = !state.download_folder.trim().is_empty();
            if ui
                .add_enabled(can_start, egui::Button::new("Start Scraping"))
                .clicked()
            {
                transition = Some(HomeTransition {
                    input_mode: state.input_mode.clone(),
                    download_folder: state.download_folder.clone(),
                });
            }

            if !can_start {
                ui.small("Select a download directory to continue.");
            }
        });
    });

    transition
}

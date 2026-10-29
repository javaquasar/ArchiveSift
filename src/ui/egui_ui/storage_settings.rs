use std::path::{Path, PathBuf};

use eframe::egui;

use crate::app::AppConfig;
use crate::services::StorageSettings;

pub(super) struct StorageSettingsEditor {
    pub(super) draft: StorageSettings,
    pub(super) applying: bool,
    pub(super) status: Option<Result<(), String>>,
}

impl StorageSettingsEditor {
    pub(super) fn new(config: &AppConfig) -> Self {
        Self {
            draft: StorageSettings::from(config),
            applying: false,
            status: None,
        }
    }

    pub(super) fn render(
        &mut self,
        ui: &mut egui::Ui,
        config: &AppConfig,
        busy: bool,
    ) -> Option<StorageSettings> {
        let mut apply = None;
        ui.heading("Settings");
        egui::ScrollArea::vertical()
            .id_salt("storage_settings")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("Storage");
                ui.label("Select the folders and database used by your library.");
                ui.label("Changing paths does not move existing files. When opening another library, select its database and download folder together.");
                ui.add_space(12.0);

                folder_field(ui, "Download folder", &mut self.draft.download_folder);
                let mut preview = config.clone();
                preview.download_folder = self.draft.download_folder.clone();
                preview.database_path = self.draft.database_path.clone();
                preview.thumbnail_folder = self.draft.thumbnail_folder.clone();
                ui.weak(format!("Files will be stored in: {}", native_text(&preview.effective_download_dir())));
                ui.add_space(12.0);

                ui.label("Database file");
                ui.add(egui::TextEdit::singleline(&mut self.draft.database_path).desired_width(f32::INFINITY));
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Choose existing database").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("SQLite database", &["sqlite3", "sqlite", "db"])
                            .pick_file()
                        {
                            self.draft.database_path = native_text(&path);
                        }
                    }
                    if ui.button("New database").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("SQLite database", &["sqlite3", "sqlite", "db"])
                            .set_file_name("archivesift.sqlite3")
                            .save_file()
                        {
                            self.draft.database_path = native_text(&path);
                        }
                    }
                });
                ui.add_space(12.0);

                let mut default_thumbnails = self.draft.thumbnail_folder.is_none();
                if ui.checkbox(&mut default_thumbnails, "Store thumbnails next to the database").changed() {
                    self.draft.thumbnail_folder = if default_thumbnails {
                        None
                    } else {
                        Some(native_text(&preview.thumb_dir()))
                    };
                }
                if let Some(folder) = &mut self.draft.thumbnail_folder {
                    folder_field(ui, "Thumbnail folder", folder);
                } else {
                    preview.database_path = self.draft.database_path.clone();
                    preview.thumbnail_folder = None;
                    ui.label("Thumbnail folder");
                    ui.add(egui::Label::new(native_text(&preview.thumb_dir())).wrap());
                }
                ui.add_space(16.0);

                let changed = self.draft != StorageSettings::from(config);
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!busy && !self.applying && changed, egui::Button::new("Save and apply")).clicked() {
                        self.status = None;
                        apply = Some(self.draft.clone());
                    }
                    if ui.add_enabled(!self.applying && changed, egui::Button::new("Discard changes")).clicked() {
                        *self = Self::new(config);
                    }
                });
                if self.applying {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Applying storage settings…");
                    });
                } else if busy {
                    ui.label("Wait for the current download, search, or database operation to finish before applying storage settings.");
                }
                if let Some(status) = &self.status {
                    match status {
                        Ok(()) => { ui.colored_label(egui::Color32::DARK_GREEN, "Storage settings saved and applied."); }
                        Err(error) => { ui.colored_label(egui::Color32::RED, error); }
                    }
                }

                ui.add_space(16.0);
                ui.separator();
                ui.heading("Settings file");
                let path = std::env::current_dir()
                    .unwrap_or_default()
                    .join(AppConfig::config_path());
                let path = native_text(&path);
                ui.add(egui::Label::new(&path).wrap());
                if ui.button("Copy config path").clicked() {
                    ui.ctx().copy_text(path);
                }
            });
        apply
    }
}

fn folder_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.push_id(label, |ui| {
        ui.label(label);
        ui.add(
            egui::TextEdit::singleline(value)
                .id(egui::Id::new(("storage_folder", label)))
                .desired_width(f32::INFINITY),
        );
        if ui.button("Browse folder").clicked() {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                *value = native_text(&folder);
            }
        }
    });
}

fn native_text(path: &Path) -> String {
    path.components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        editor: &mut StorageSettingsEditor,
        config: &AppConfig,
        context: &egui::Context,
        events: Vec<egui::Event>,
        busy: bool,
    ) -> (egui::FullOutput, Option<StorageSettings>) {
        let mut command = None;
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 1100.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    command = editor.render(ui, config, busy);
                });
            },
        );
        (output, command)
    }

    fn text_position(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        fn find(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.job.text == label => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, label)),
                _ => None,
            }
        }
        output
            .shapes
            .iter()
            .find_map(|shape| find(&shape.shape, label))
            .unwrap_or_else(|| panic!("Missing {label}"))
    }

    fn click(
        label: &str,
        output: &egui::FullOutput,
        editor: &mut StorageSettingsEditor,
        config: &AppConfig,
        context: &egui::Context,
        busy: bool,
    ) -> Option<StorageSettings> {
        let pos = text_position(output, label);
        let mut request = None;
        for pressed in [true, false] {
            let (_, command) = frame(
                editor,
                config,
                context,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                busy,
            );
            request = command.or(request);
        }
        request
    }

    #[test]
    fn storage_settings_editor_stages_typed_changes_and_emits_apply_request() {
        let config = AppConfig {
            download_folder: "original".to_string(),
            ..AppConfig::default()
        };
        let mut editor = StorageSettingsEditor::new(&config);
        let context = egui::Context::default();
        let _ = frame(&mut editor, &config, &context, Vec::new(), false);
        context.memory_mut(|memory| {
            memory.request_focus(egui::Id::new(("storage_folder", "Download folder")))
        });
        let chosen = native_text(&Path::new("chosen").join("files"));
        let (output, request) = frame(
            &mut editor,
            &config,
            &context,
            vec![
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        ctrl: true,
                        command: true,
                        ..Default::default()
                    },
                },
                egui::Event::Text(chosen.clone()),
            ],
            false,
        );
        assert!(request.is_none());
        assert_eq!(editor.draft.download_folder, chosen);
        assert_eq!(config.download_folder, "original");
        let requested = click(
            "Save and apply",
            &output,
            &mut editor,
            &config,
            &context,
            false,
        )
        .unwrap();
        assert_eq!(requested, editor.draft);
        assert_eq!(config.download_folder, "original");
    }

    #[test]
    fn storage_settings_editor_discards_changes_and_blocks_apply_during_operations() {
        let config = AppConfig::default();
        let mut editor = StorageSettingsEditor::new(&config);
        editor.draft.download_folder = "edited".to_string();
        let context = egui::Context::default();
        let (output, _) = frame(&mut editor, &config, &context, Vec::new(), true);
        assert!(click(
            "Save and apply",
            &output,
            &mut editor,
            &config,
            &context,
            true
        )
        .is_none());
        let (output, _) = frame(&mut editor, &config, &context, Vec::new(), false);
        assert!(click(
            "Discard changes",
            &output,
            &mut editor,
            &config,
            &context,
            false
        )
        .is_none());
        assert_eq!(editor.draft, StorageSettings::from(&config));
    }

    #[test]
    fn storage_settings_editor_can_switch_between_default_and_custom_thumbnails() {
        let config = AppConfig::default();
        let mut editor = StorageSettingsEditor::new(&config);
        let context = egui::Context::default();
        let (output, _) = frame(&mut editor, &config, &context, Vec::new(), false);
        click(
            "Store thumbnails next to the database",
            &output,
            &mut editor,
            &config,
            &context,
            false,
        );
        assert_eq!(
            editor.draft.thumbnail_folder.as_deref(),
            Some(native_text(&config.thumb_dir()).as_str())
        );
        let (output, _) = frame(&mut editor, &config, &context, Vec::new(), false);
        click(
            "Store thumbnails next to the database",
            &output,
            &mut editor,
            &config,
            &context,
            false,
        );
        assert!(editor.draft.thumbnail_folder.is_none());
        assert!(config.thumbnail_folder.is_none());
    }
}

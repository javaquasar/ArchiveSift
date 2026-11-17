use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use eframe::egui;

use crate::db::Database;
use crate::knife::archive::ImportResult;
use crate::knife::preview::{preview, ImportPreview};

enum Outcome {
    Preview(ImportPreview),
    Import(ImportResult),
}

#[derive(Clone, PartialEq)]
struct Destination {
    root: PathBuf,
    database: PathBuf,
}

#[derive(Default)]
pub(super) struct KnifeImport {
    index: String,
    worker: Option<Receiver<Result<Outcome, String>>>,
    importing: bool,
    destination: Option<Destination>,
    report: Option<Arc<ImportPreview>>,
    confirmation: bool,
    result: Option<ImportResult>,
    error: Option<String>,
    page: usize,
}

impl KnifeImport {
    pub fn importing(&self) -> bool {
        self.importing
    }

    pub fn poll(&mut self) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let outcome = match worker.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("The archive task stopped unexpectedly".into()),
        };
        self.worker = None;
        self.importing = false;
        match outcome {
            Ok(Outcome::Preview(report)) => self.report = Some(Arc::new(report)),
            Ok(Outcome::Import(result)) => {
                self.result = Some(result);
                return true;
            }
            Err(error) => self.error = Some(error),
        }
        false
    }

    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        download_root: PathBuf,
        database_path: PathBuf,
        blocked: bool,
    ) {
        let destination = Destination {
            root: download_root,
            database: database_path,
        };
        if self.worker.is_none()
            && self
                .destination
                .as_ref()
                .is_some_and(|target| target != &destination)
        {
            self.clear_preview();
        }
        ui.heading("Knife archive import");
        ui.label("Choose knives.json from the archive folder that contains images.");
        ui.add_enabled_ui(self.worker.is_none(), |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.index)
                            .hint_text("Path to knives.json")
                            .desired_width(350.0),
                    )
                    .changed()
                {
                    self.clear_preview();
                }
                if ui.button("Browse").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .pick_file()
                    {
                        self.index = path.display().to_string();
                        self.clear_preview();
                    }
                }
            });
            if ui
                .add_enabled(
                    !self.index.trim().is_empty(),
                    egui::Button::new("Preview import"),
                )
                .clicked()
            {
                self.start_preview(ui.ctx().clone(), destination.clone());
            }
        });
        if self.worker.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(if self.importing {
                    "Importing knife archive"
                } else {
                    "Reading archive"
                });
            });
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(result) = &self.result {
            render_result(ui, result);
        }
        let Some(report) = self.report.clone() else {
            return;
        };
        let ready = self.worker.is_none() && !blocked && !destination.root.as_os_str().is_empty();
        if ui
            .add_enabled(
                ready && !report.importable.is_empty(),
                egui::Button::new("Review and import…"),
            )
            .clicked()
        {
            self.confirmation = true;
        }
        if destination.root.as_os_str().is_empty() {
            ui.label("Set the download folder in Storage before importing.");
        }
        if self.confirmation {
            let mut confirm = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("confirm_knife_import")).show(ui.ctx(), |ui| {
                    ui.heading("Confirm knife import");
                    ui.label(format!("Import {} records into {}?", report.importable.len(), destination.root.display()));
                    ui.label("Available photos will be copied from the archive. Matching entries are checked again and conflicting files are skipped.");
                    ui.horizontal(|ui| {
                        confirm = ui.add_enabled(ready, egui::Button::new("Import")).clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if confirm {
                self.confirmation = false;
                self.error = None;
                self.result = None;
                let plan = report.clone();
                self.start(ui.ctx().clone(), true, move || {
                    Database::new(destination.database)
                        .import_knife_archive(&plan, &destination.root)
                        .map(Outcome::Import)
                        .map_err(|error| error.to_string())
                });
            } else if cancel || modal.should_close() {
                self.confirmation = false;
            }
        }
        render_report(ui, &report, &mut self.page);
    }

    fn clear_preview(&mut self) {
        self.report = None;
        self.destination = None;
        self.result = None;
        self.error = None;
        self.confirmation = false;
        self.page = 0;
    }

    fn start_preview(&mut self, context: egui::Context, destination: Destination) {
        let index = PathBuf::from(self.index.trim());
        self.clear_preview();
        self.destination = Some(destination.clone());
        self.start(context, false, move || {
            preview(&index, &destination.root)
                .map(Outcome::Preview)
                .map_err(|error| error.to_string())
        });
    }

    fn start(
        &mut self,
        context: egui::Context,
        importing: bool,
        task: impl FnOnce() -> Result<Outcome, String> + Send + 'static,
    ) {
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("knife-archive".into())
            .spawn(move || {
                let _ = sender.send(task());
                context.request_repaint();
            }) {
            Ok(_) => {
                self.worker = Some(receiver);
                self.importing = importing;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn text_position(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_position(shape, label)),
            _ => None,
        }
    }

    #[test]
    fn knife_import_requires_confirmation_and_cancel_leaves_the_library_unwritten() {
        let archive = tempfile::tempdir().unwrap();
        let library = tempfile::tempdir().unwrap();
        let index = archive.path().join("knives.json");
        let image = archive.path().join("images/cover.png");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        image::GrayImage::new(8, 8).save(&image).unwrap();
        fs::write(
            &index,
            r#"[{"id":"ridge","company":"Forge","knifeName":"Ridge","imageFile":"cover.png"}]"#,
        )
        .unwrap();
        let target = Destination {
            root: library.path().to_owned(),
            database: library.path().join("library.sqlite3"),
        };
        let mut panel = KnifeImport {
            index: index.display().to_string(),
            destination: Some(target.clone()),
            report: Some(Arc::new(preview(&index, library.path()).unwrap())),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let render = |panel: &mut KnifeImport, events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        panel.render(ui, target.root.clone(), target.database.clone(), false);
                    });
                },
            )
        };
        let click = |panel: &mut KnifeImport, button: &str| {
            let _ = render(panel, vec![]);
            let output = render(panel, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| text_position(&shape.shape, button))
                .unwrap();
            for pressed in [true, false] {
                let _ = render(
                    panel,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        };

        click(&mut panel, "Review and import…");
        assert!(panel.confirmation);
        assert!(!target.database.exists());
        assert!(panel.worker.is_none());
        click(&mut panel, "Cancel");
        assert!(!panel.confirmation);
        assert!(!target.database.exists());

        click(&mut panel, "Review and import…");
        click(&mut panel, "Import");
        assert!(panel.importing());
        let timeout = Instant::now() + Duration::from_secs(5);
        while panel.importing() && Instant::now() < timeout {
            panel.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(panel.result.unwrap().copied_files, 1, "{:?}", panel.error);
        assert!(target
            .root
            .join(Path::new("knife/Forge/Ridge/cover.png"))
            .is_file());
        assert!(image.is_file());
    }
}

fn render_result(ui: &mut egui::Ui, result: &ImportResult) {
    ui.label(format!(
        "{} new pages · {} matched existing pages · {} records skipped",
        result.new_pages, result.existing_pages, result.skipped_records
    ));
    ui.label(format!(
        "{} photos copied · {} reused · {} duplicates · {} files skipped",
        result.copied_files, result.reused_files, result.duplicate_files, result.skipped_files
    ));
    if !result.errors.is_empty() {
        ui.collapsing("Import issues", |ui| {
            for error in &result.errors {
                ui.label(error);
            }
        });
    }
}

fn render_report(ui: &mut egui::Ui, report: &ImportPreview, page: &mut usize) {
    const PAGE_SIZE: usize = 30;
    ui.label(format!(
        "{} records · {} invalid · {} repeated IDs",
        report.records, report.invalid_records, report.duplicate_ids
    ));
    ui.label(format!(
        "{} files ready · {} missing · {} rejected · {} conflicts",
        report.ready_files, report.missing_files, report.rejected_files, report.conflicting_files
    ));
    let pages = report.rows.len().div_ceil(PAGE_SIZE);
    if pages > 1 {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(*page > 0, egui::Button::new("Previous"))
                .clicked()
            {
                *page -= 1;
            }
            ui.label(format!("{} / {pages}", *page + 1));
            if ui
                .add_enabled(*page + 1 < pages, egui::Button::new("Next"))
                .clicked()
            {
                *page += 1;
            }
        });
    }
    if report.rows.len() < report.records {
        ui.label(format!("Showing the first {} records", report.rows.len()));
    }
    for row in report.rows.iter().skip(*page * PAGE_SIZE).take(PAGE_SIZE) {
        ui.separator();
        ui.strong(format!(
            "{} · {} / {}",
            row.number,
            row.manufacturer
                .as_deref()
                .unwrap_or("Invalid manufacturer"),
            row.model.as_deref().unwrap_or("Invalid model")
        ));
        if let Some(steel) = &row.steel {
            ui.label(steel);
        }
        if let Some(directory) = &row.directory {
            ui.label(directory.display().to_string());
        }
        if let Some(example) = &row.first_destination {
            ui.small(example.display().to_string());
        }
        ui.small(format!(
            "{} files · {} ready · {} missing · {} rejected · {} conflicts{}",
            row.files,
            row.ready,
            row.missing,
            row.rejected,
            row.conflicts,
            if row.duplicate_id {
                " · Repeated ID"
            } else {
                ""
            }
        ));
    }
}

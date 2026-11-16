use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;

use crate::knife::preview::{preview, ImportPreview};

#[derive(Default)]
pub(super) struct KnifeImport {
    index: String,
    pending: Option<Receiver<Result<ImportPreview, String>>>,
    report: Option<Result<ImportPreview, String>>,
    page: usize,
}

impl KnifeImport {
    pub fn render(&mut self, ui: &mut egui::Ui, download_root: PathBuf) {
        ui.heading("Knife import preview");
        ui.label("Choose knives.json from the archive folder that contains images.");
        ui.add_enabled_ui(self.pending.is_none(), |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.index)
                            .hint_text("Path to knives.json")
                            .desired_width(350.0),
                    )
                    .changed()
                {
                    self.report = None;
                }
                if ui.button("Browse").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .pick_file()
                    {
                        self.index = path.display().to_string();
                        self.report = None;
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
                self.start(ui.ctx().clone(), download_root);
            }
        });
        if let Some(pending) = &self.pending {
            let result = match pending.try_recv() {
                Ok(report) => Some(report),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err("The import preview stopped unexpectedly".into()))
                }
            };
            if let Some(result) = result {
                self.report = Some(result);
                self.pending = None;
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading archive");
                });
            }
        }
        match &self.report {
            Some(Ok(report)) => render_report(ui, report, &mut self.page),
            Some(Err(error)) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            None => {}
        }
    }

    fn start(&mut self, context: egui::Context, download_root: PathBuf) {
        let index = PathBuf::from(self.index.trim());
        let (sender, receiver) = mpsc::channel();
        self.report = None;
        self.page = 0;
        let thread = std::thread::Builder::new()
            .name("knife-import-preview".into())
            .spawn(move || {
                let _ =
                    sender.send(preview(&index, &download_root).map_err(|error| error.to_string()));
                context.request_repaint();
            });
        match thread {
            Ok(_) => self.pending = Some(receiver),
            Err(error) => self.report = Some(Err(error.to_string())),
        }
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

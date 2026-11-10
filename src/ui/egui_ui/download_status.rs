use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;

use super::background_executor::spawn_background;
use crate::app::AppConfig;
use crate::db::Database;

#[derive(Debug, PartialEq, Eq)]
enum ImageFiles {
    Unavailable,
    Available { saved: usize, total: usize },
}

fn count_files(root: &Path, paths: &[Option<String>]) -> ImageFiles {
    if !root.is_dir() {
        return ImageFiles::Unavailable;
    }
    let saved = paths
        .iter()
        .filter_map(Option::as_deref)
        .filter(|path| {
            !path.is_empty()
                && root
                    .join(path)
                    .metadata()
                    .is_ok_and(|file| file.is_file() && file.len() > 0)
        })
        .count();
    ImageFiles::Available {
        saved,
        total: paths.len(),
    }
}

#[derive(Default)]
pub(super) struct DownloadStatus {
    check_key: Option<(String, PathBuf, i64, String)>,
    receiver: Option<Receiver<Result<ImageFiles, String>>>,
    files: Option<Result<ImageFiles, String>>,
    pending: HashSet<String>,
    results: VecDeque<(String, String)>,
}

impl DownloadStatus {
    pub(super) fn queued(&mut self, url: String) {
        self.results.retain(|(page, _)| page != &url);
        self.pending.insert(url);
    }

    pub(super) fn is_pending(&self, url: &str) -> bool {
        self.pending.contains(url)
    }

    pub(super) fn clear_pending(&mut self, active: &[(String, String)]) {
        self.pending
            .retain(|url| active.iter().any(|(running, _)| running == url));
    }

    pub(super) fn complete(&mut self, url: &str, message: &str, show_result: bool) {
        if self.pending.remove(url) || show_result {
            self.results.retain(|(page, _)| page != url);
            self.results
                .push_front((url.to_owned(), message.to_owned()));
            self.results.truncate(16);
        }
        if self
            .check_key
            .as_ref()
            .is_some_and(|(_, _, _, checked)| checked == url)
        {
            self.check_key = None;
        }
    }

    pub(super) fn render(
        &mut self,
        ui: &mut egui::Ui,
        config: &AppConfig,
        page_id: i64,
        url: &str,
    ) -> bool {
        let root = config.effective_download_dir();
        let key = (&config.database_path, &root, page_id, url);
        if self
            .check_key
            .as_ref()
            .map(|(db, root, id, url)| (db, root, *id, url.as_str()))
            != Some(key)
        {
            let database = Database::new(&config.database_path);
            let ctx = ui.ctx().clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            self.check_key = Some((
                config.database_path.clone(),
                root.clone(),
                page_id,
                url.to_owned(),
            ));
            self.files = None;
            self.receiver = Some(receiver);
            if let Err(error) = spawn_background(move || {
                let files = database
                    .page_image_paths(page_id)
                    .map(|paths| count_files(&root, &paths))
                    .map_err(|error| error.to_string());
                let _ = sender.send(files);
                ctx.request_repaint();
            }) {
                self.files = Some(Err(error));
                self.receiver = None;
            }
        }
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok(files) => {
                    self.files = Some(files);
                    self.receiver = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.files = Some(Err("Image check did not finish.".into()));
                    self.receiver = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        let mut repair = false;
        ui.horizontal_wrapped(|ui| match &self.files {
            Some(Ok(ImageFiles::Available { saved, total })) => {
                if *total == 0 {
                    ui.weak("No images found");
                } else {
                    ui.label(format!("Images saved · {saved} of {total}"));
                }
                let pending = self.is_pending(url);
                repair = ui
                    .add_enabled(
                        !pending && config.has_download_folder(),
                        egui::Button::new(if pending {
                            "Repair queued"
                        } else if *total == 0 {
                            "Check for images"
                        } else if saved < total {
                            "Download missing images"
                        } else {
                            "Check for missing images"
                        }),
                    )
                    .clicked();
            }
            Some(Ok(ImageFiles::Unavailable)) => {
                ui.colored_label(ui.visuals().warn_fg_color, "Library storage unavailable");
                if ui.button("Check storage again").clicked() {
                    self.check_key = None;
                    ui.ctx().request_repaint();
                }
            }
            Some(Err(error)) => {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            None => {
                ui.spinner();
                ui.weak("Checking local images…");
            }
        });
        if let Some((_, message)) = self.results.iter().find(|(page, _)| page == url) {
            ui.weak(message);
        }
        repair
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_existing_originals_and_distinguishes_unavailable_storage() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("ready.jpg"), b"content").unwrap();
        std::fs::write(root.path().join("empty.jpg"), b"").unwrap();
        let paths = [
            Some("ready.jpg".into()),
            Some("empty.jpg".into()),
            Some("missing.jpg".into()),
            None,
        ];
        assert_eq!(
            count_files(root.path(), &paths),
            ImageFiles::Available { saved: 1, total: 4 }
        );
        assert_eq!(
            count_files(&root.path().join("offline"), &paths),
            ImageFiles::Unavailable
        );
    }

    #[test]
    fn completion_is_scoped_to_the_queued_page() {
        let mut status = DownloadStatus::default();
        status.queued("first".into());
        status.queued("second".into());
        status.complete("first", "Incomplete", false);
        assert!(status.is_pending("second"));
        assert!(!status.is_pending("first"));
        status.complete("second", "Completed", false);
        assert_eq!(status.results.len(), 2);
        status.queued("first".into());
        assert_eq!(
            status.results,
            VecDeque::from([("second".into(), "Completed".into())])
        );
    }
}

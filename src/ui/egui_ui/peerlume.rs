use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use zeroize::{Zeroize, Zeroizing};

use super::background_executor::spawn_background;
use crate::app::AppConfig;
use crate::secure_store::{Secret, SecretStore, SystemSecretStore};
use crate::services::peerlume::{
    self, client::CliTransport, Handoff, HandoffService, HandoffState, PeerLumeConfig, Preparation,
    PreparedTorrent,
};
use crate::ui::UiCommand;

enum Outcome {
    Message(String),
    Prepared(Preparation),
    History(Vec<Handoff>),
    Submitted(Vec<Handoff>, Vec<String>),
}

#[derive(Default)]
pub(super) struct PeerLumeUi {
    draft: Option<PeerLumeConfig>,
    config_identity: String,
    token: Zeroizing<String>,
    receiver: Option<Receiver<Result<Outcome, String>>>,
    status: Option<String>,
    error: Option<String>,
    open: bool,
    review: Vec<(bool, PreparedTorrent)>,
    review_errors: Vec<(i64, String)>,
    destination: String,
    start: bool,
    retry_failed: bool,
    history: Vec<Handoff>,
    last_refresh: Option<Instant>,
}

fn service(config: AppConfig) -> peerlume::Result<HandoffService<CliTransport>> {
    let transport = CliTransport::connect(&config.peerlume)?;
    HandoffService::new(config, transport)
}

impl PeerLumeUi {
    pub fn busy(&self) -> bool {
        self.receiver.is_some()
    }
    pub fn blocks_settings(&self) -> bool {
        self.busy() || !self.review.is_empty()
    }

    fn job(
        &mut self,
        ctx: &egui::Context,
        work: impl FnOnce() -> Result<Outcome, String> + Send + 'static,
    ) {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let repaint = ctx.clone();
        self.error = None;
        match spawn_background(move || {
            let _ = tx.send(work());
            repaint.request_repaint();
        }) {
            Ok(()) => self.receiver = Some(rx),
            Err(error) => self.error = Some(error),
        }
    }

    pub fn poll(&mut self, config: &AppConfig) {
        let identity = format!(
            "{}:{}",
            config.database_path,
            serde_json::to_string(&config.peerlume).unwrap_or_default()
        );
        if !self.busy() && self.config_identity != identity {
            self.config_identity = identity;
            self.draft = Some(config.peerlume.clone());
            self.token.zeroize();
            self.review.clear();
            self.history.clear();
            self.review_errors.clear();
            self.destination = config.peerlume.destination.clone();
            self.start = config.peerlume.start;
            self.status = None;
            self.error = None;
        }
        let message = self.receiver.as_ref().map(|rx| rx.try_recv());
        match message {
            Some(Ok(result)) => {
                self.receiver = None;
                match result {
                    Ok(Outcome::Message(text)) => self.status = Some(text),
                    Ok(Outcome::Prepared(preparation)) => {
                        if self.open {
                            self.review = preparation
                                .torrents
                                .into_iter()
                                .map(|item| (true, item))
                                .collect();
                            self.review_errors = preparation.errors;
                        }
                    }
                    Ok(Outcome::History(history)) => {
                        self.history = history;
                        self.last_refresh = Some(Instant::now());
                    }
                    Ok(Outcome::Submitted(history, errors)) => {
                        self.history = history;
                        self.review_errors.clear();
                        self.error = (!errors.is_empty()).then(|| errors.join("\n"));
                        self.status = Some("Handoff results saved. Accepted operations still require confirmation by PeerLume.".into());
                        self.last_refresh = Some(Instant::now());
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.receiver = None;
                self.error = Some(
                    "The PeerLume background task stopped. Refresh before trying again.".into(),
                );
            }
            _ => (),
        }
    }

    pub fn prepare(&mut self, ctx: &egui::Context, config: &AppConfig, ids: Vec<i64>) {
        self.open = true;
        if self.busy() {
            return;
        }
        if !self.review.is_empty() {
            self.error = Some(
                "Finish or discard the current review before preparing another selection.".into(),
            );
            return;
        }
        self.review.clear();
        self.review_errors.clear();
        self.destination = config.peerlume.destination.clone();
        self.start = config.peerlume.start;
        self.retry_failed = false;
        let config = config.clone();
        self.job(ctx, move || {
            service(config)
                .and_then(|s| s.prepare(&ids))
                .map(Outcome::Prepared)
                .map_err(|e| e.to_string())
        });
    }

    fn history(&mut self, ctx: &egui::Context, config: &AppConfig, refresh: bool) {
        self.open = true;
        let config = config.clone();
        self.job(ctx, move || {
            service(config)
                .and_then(|s| if refresh { s.refresh() } else { s.history() })
                .map(Outcome::History)
                .map_err(|e| e.to_string())
        });
    }

    pub fn settings(
        &mut self,
        ui: &mut egui::Ui,
        config: &AppConfig,
        busy: bool,
    ) -> Option<UiCommand> {
        let mut command = None;
        let locked = busy || self.busy() || !self.review.is_empty();
        ui.heading("Integrations · PeerLume");
        ui.label("Send collected torrent files to an already running PeerLume owner. Enable its restricted API and grant a dedicated client manage permission.");
        ui.add_enabled_ui(!locked, |ui| {
            let draft = self.draft.get_or_insert_with(|| config.peerlume.clone());
            ui.checkbox(&mut draft.enabled, "Enable PeerLume integration");
            path_field(ui, "Control executable", &mut draft.executable, false);
            path_field(ui, "PeerLume profile", &mut draft.profile, true);
            ui.horizontal(|ui| {
                ui.label("Payload destination (relative)");
                ui.text_edit_singleline(&mut draft.destination);
            });
            ui.checkbox(&mut draft.start, "Start downloading after confirmation");
            ui.weak("Destination must already exist inside PeerLume's granted download root. Payload files are separate from card artwork and metadata.");
            ui.horizontal(|ui| {
                ui.label("Control token");
                ui.add(egui::TextEdit::singleline(&mut *self.token).password(true).hint_text("Write-only · saved in OS credential store"));
            });
            let target = draft.clone();
            let has_token = !self.token.is_empty();
            let save_token = ui.add_enabled(has_token, egui::Button::new("Save token securely")).clicked();
            let forget_token = ui.button("Forget stored token").clicked();
            let test = ui.button("Test connection using these settings").clicked();
            let save = ui.button("Save PeerLume settings").clicked();
            if save {
                match target.validate() {
                    Err(error) => self.error = Some(error.to_string()),
                    Ok(()) => {
                        let mut next = config.clone();
                        next.peerlume = target.clone();
                        command = Some(UiCommand::ApplyConfiguration {
                            document: serde_json::to_value(next).unwrap(),
                            expected: serde_json::to_value(config).unwrap(),
                        });
                    }
                }
            }
            if save_token || forget_token {
                let token = Secret::new(std::mem::take(&mut *self.token).into_bytes());
                self.job(ui.ctx(), move || {
                    let target = target.secret_target().map_err(|e| e.to_string())?;
                    if save_token {
                        peerlume::client::validate_token(token.expose_secret()).map_err(|e| e.to_string())?;
                        SystemSecretStore.set(&target, &token).map_err(|e| e.to_string())?;
                    } else {
                        SystemSecretStore.delete(&target).map_err(|e| e.to_string())?;
                    }
                    Ok(Outcome::Message(if save_token { "Token saved securely for this profile." } else { "Stored token removed. Revoke it in PeerLume to revoke access." }.into()))
                });
            } else if test {
                self.job(ui.ctx(), move || {
                    let transport = CliTransport::connect(&target).map_err(|e| e.to_string())?;
                    peerlume::client::discover(&transport).map_err(|e| e.to_string())?;
                    Ok(Outcome::Message("Connected: protocol 1, preview/add capabilities and manage permission verified.".into()))
                });
            }
        });
        if ui
            .add_enabled(
                !locked && config.peerlume.enabled,
                egui::Button::new("Show PeerLume handoffs"),
            )
            .clicked()
        {
            self.history(ui.ctx(), config, false);
        }
        if self.busy() {
            ui.spinner();
        }
        if let Some(status) = &self.status {
            ui.label(status);
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }
        ui.separator();
        command
    }

    pub fn render(&mut self, ctx: &egui::Context, config: &AppConfig, storage_busy: bool) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        let mut submit = false;
        let mut refresh = false;
        let busy = self.busy();
        egui::Window::new("PeerLume · review and handoffs")
            .id(egui::Id::new("peerlume_handoffs"))
            .open(&mut open)
            .default_size([740.0, 560.0])
            .resizable(true)
            .vscroll(true)
            .show(ctx, |ui| {
                if busy { ui.horizontal(|ui| { ui.spinner(); ui.label("Working with PeerLume…"); }); }
                if let Some(status) = &self.status { ui.label(status); }
                if let Some(error) = &self.error { ui.colored_label(egui::Color32::RED, error); }
                egui::ScrollArea::vertical().id_salt("peerlume_review_scroll").max_height(300.0).show(ui, |ui| {
                    for (included, item) in &mut self.review {
                        ui.group(|ui| {
                            ui.add_enabled(!busy, egui::Checkbox::new(included, &item.title));
                            ui.label(format!("{} files · {} bytes · {}", item.preview.file_count, item.preview.total_bytes, if item.preview.private { "Private torrent" } else { "Public torrent" }));
                            for hash in [&item.preview.info_hash_v1, &item.preview.info_hash_v2].into_iter().flatten() { ui.monospace(hash); }
                        });
                    }
                    for (id, error) in &self.review_errors { ui.colored_label(egui::Color32::RED, format!("Card {id}: {error}")); }
                });
                if !self.review.is_empty() {
                    ui.add_enabled_ui(!busy && !storage_busy, |ui| {
                        ui.horizontal(|ui| { ui.label("Existing relative destination"); ui.text_edit_singleline(&mut self.destination); });
                        ui.horizontal(|ui| { ui.radio_value(&mut self.start, false, "Add paused"); ui.radio_value(&mut self.start, true, "Start now"); });
                        ui.checkbox(&mut self.retry_failed, "Retry confirmed failed handoffs with a new request");
                        ui.weak("Confirming can start network transfers in PeerLume. Existing torrents are linked without changing their destination or state.");
                        let valid = peerlume::relative_destination(&self.destination).is_ok() && self.review.iter().any(|(on,_)| *on);
                        submit = ui.add_enabled(valid, egui::Button::new("Confirm and send selected torrents")).clicked();
                        if ui.button("Discard review").clicked() { self.review.clear(); self.review_errors.clear(); }
                    });
                }
                ui.separator();
                refresh = ui.add_enabled(!busy && self.review.is_empty() && !storage_busy, egui::Button::new("Refresh handoff and transfer status")).clicked();
                ui.label("Recent handoffs for the selected profile (up to 1,000). Acknowledged add operations and current transfer observations are shown separately.");
                if let Some(at) = self.last_refresh { ui.weak(format!("Last loaded {} seconds ago. Refresh verifies the current owner state.", at.elapsed().as_secs())); }
                egui::ScrollArea::vertical().id_salt("peerlume_history_scroll").max_height(350.0).show_rows(ui, 144.0, self.history.len(), |ui, rows| {
                    for index in rows {
                        history_card(ui, &self.history[index]);
                    }
                });
            });
        self.open = open;
        if !open && !busy {
            self.review.clear();
            self.review_errors.clear();
        }
        if submit {
            let selected: Vec<_> = std::mem::take(&mut self.review)
                .into_iter()
                .filter_map(|(on, item)| on.then_some(item))
                .collect();
            let config = config.clone();
            let destination = self.destination.clone();
            let start = self.start;
            let retry_failed = self.retry_failed;
            self.job(ctx, move || {
                let service = service(config).map_err(|e| e.to_string())?;
                let mut errors = Vec::new();
                let mut chosen = Vec::new();
                for item in selected {
                    let result = if retry_failed {
                        service.retry_failed(&item, &destination, start)
                    } else {
                        service.submit(&item, &destination, start)
                    };
                    match result {
                        Ok(record) => chosen.push(record),
                        Err(error) => errors.push(format!("Card {}: {error}", item.page_id)),
                    }
                }
                let mut history = service.history().map_err(|e| e.to_string())?;
                for record in chosen.into_iter().rev() {
                    history.retain(|previous| previous.id != record.id);
                    history.insert(0, record);
                }
                Ok(Outcome::Submitted(history, errors))
            });
        } else if self.open && !busy && self.review.is_empty() && !storage_busy {
            let active = self.history.iter().any(|item| {
                !matches!(item.state, HandoffState::Failed)
                    && !matches!(
                        item.transfer,
                        Some(peerlume::TransferState::Seeding | peerlume::TransferState::Removed)
                    )
            });
            if refresh
                || (active
                    && self.error.is_none()
                    && self
                        .last_refresh
                        .is_some_and(|at| at.elapsed() >= Duration::from_secs(5)))
            {
                self.history(ctx, config, true);
            }
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}

fn history_card(ui: &mut egui::Ui, record: &Handoff) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(&record.title).truncate()).on_hover_text(&record.title);
        let transfer = record.transfer.map_or("Not observed".into(), |s| format!("{s:?}"));
        ui.add(egui::Label::new(format!("Handoff: {:?} · Transfer: {transfer}", record.state)).truncate());
        ui.add(egui::Label::new(format!("{} / {} bytes · Destination: {}", record.downloaded_bytes, record.total_bytes, record.destination)).truncate())
            .on_hover_text(&record.destination);
        if let Some(id) = &record.operation_id {
            ui.small(format!("Operation: {id}"));
        }
        if record.missing {
            let text = "Torrent is not in the latest PeerLume snapshot. It will not be re-added automatically.";
            ui.add(egui::Label::new(egui::RichText::new(text).color(egui::Color32::YELLOW)).truncate()).on_hover_text(text);
        }
        if let Some(error) = &record.error {
            ui.add(egui::Label::new(egui::RichText::new(error).color(egui::Color32::RED)).truncate()).on_hover_text(error);
        }
    });
}

fn path_field(ui: &mut egui::Ui, label: &str, value: &mut String, folder: bool) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(ui.available_width() - 80.0));
        if ui.button("Browse…").clicked() {
            let dialog = rfd::FileDialog::new();
            let picked = if folder {
                dialog.pick_folder()
            } else {
                dialog.pick_file()
            };
            if let Some(path) = picked {
                *value = path.display().to_string();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        panel: &mut PeerLumeUi,
        config: &AppConfig,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        settings: bool,
        busy: bool,
    ) -> (egui::FullOutput, Option<UiCommand>) {
        let mut command = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 1100.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                if settings {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        command = panel.settings(ui, config, busy);
                    });
                } else {
                    panel.render(ctx, config, busy);
                }
            },
        );
        (output, command)
    }
    fn position(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        fn find(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(text) if text.galley.job.text == label => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| find(s, label)),
                _ => None,
            }
        }
        output
            .shapes
            .iter()
            .find_map(|s| find(&s.shape, label))
            .unwrap_or_else(|| panic!("Missing {label}"))
    }
    fn click(
        panel: &mut PeerLumeUi,
        config: &AppConfig,
        ctx: &egui::Context,
        label: &str,
        settings: bool,
        busy: bool,
    ) -> Option<UiCommand> {
        frame(panel, config, ctx, vec![], settings, busy);
        let (output, _) = frame(panel, config, ctx, vec![], settings, busy);
        let pos = position(&output, label);
        let mut result = None;
        for pressed in [true, false] {
            let (_, command) = frame(
                panel,
                config,
                ctx,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                settings,
                busy,
            );
            result = command.or(result);
        }
        result
    }

    #[test]
    fn peerlume_settings_stage_changes_without_serializing_the_token_and_lock_while_busy() {
        let config = AppConfig::default();
        let ctx = egui::Context::default();
        let mut panel = PeerLumeUi::default();
        panel.poll(&config);
        panel.draft.as_mut().unwrap().destination = "chosen/path".into();
        *panel.token = "fixture-write-only-input".into();
        let command = click(
            &mut panel,
            &config,
            &ctx,
            "Save PeerLume settings",
            true,
            false,
        )
        .unwrap();
        let UiCommand::ApplyConfiguration { document, expected } = command else {
            panic!("Unexpected command")
        };
        assert_eq!(document["peerlume"]["destination"], "chosen/path");
        assert_eq!(expected["peerlume"]["destination"], "incoming");
        assert!(!document.to_string().contains("fixture-write-only"));
        assert!(!panel.busy());
        assert!(click(
            &mut panel,
            &config,
            &ctx,
            "Save PeerLume settings",
            true,
            true
        )
        .is_none());
    }

    #[test]
    fn peerlume_review_requires_inclusion_and_explicit_confirmation() {
        let config = AppConfig::default(); // A confirm job cannot contact a real owner.
        let ctx = egui::Context::default();
        let mut panel = PeerLumeUi::default();
        panel.poll(&config);
        panel.open = true;
        panel.review = vec![(
            false,
            crate::services::peerlume::tests::prepared_for_ui_test(),
        )];
        assert!(panel.blocks_settings());
        click(
            &mut panel,
            &config,
            &ctx,
            "Confirm and send selected torrents",
            false,
            false,
        );
        assert!(!panel.busy());
        panel.review[0].0 = true;
        click(
            &mut panel,
            &config,
            &ctx,
            "Confirm and send selected torrents",
            false,
            true,
        );
        assert!(!panel.busy());
        click(
            &mut panel,
            &config,
            &ctx,
            "Confirm and send selected torrents",
            false,
            false,
        );
        assert!(panel.busy());
        assert!(panel.review.is_empty());
    }

    #[test]
    fn peerlume_closed_preparation_does_not_leave_settings_locked() {
        let config = AppConfig::default();
        let mut panel = PeerLumeUi::default();
        panel.poll(&config);
        let (tx, rx) = mpsc::sync_channel(1);
        panel.receiver = Some(rx);
        tx.send(Ok(Outcome::Prepared(Preparation {
            torrents: vec![crate::services::peerlume::tests::prepared_for_ui_test()],
            errors: vec![],
        })))
        .unwrap();
        panel.open = false;
        panel.poll(&config);
        assert!(!panel.blocks_settings());
        assert!(panel.review.is_empty());
    }
}

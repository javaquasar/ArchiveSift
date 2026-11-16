use eframe::egui;

use crate::db::image_access::ImageAccessGroup;
use crate::ui::{ImageAccessAction, UiCommand};

#[derive(Default)]
pub(super) struct ImageAccessPanel {
    database_path: String,
    loading: bool,
    refresh: bool,
    groups: Vec<ImageAccessGroup>,
    message: Option<String>,
}

impl ImageAccessPanel {
    pub(super) fn invalidate(&mut self) {
        self.refresh = true;
    }

    pub(super) fn finish(
        &mut self,
        database_path: &str,
        result: Result<Vec<ImageAccessGroup>, String>,
        message: Option<String>,
    ) {
        if self.database_path != database_path {
            return;
        }
        self.loading = false;
        match result {
            Ok(groups) => {
                self.groups = groups;
                if message.is_some() {
                    self.message = message;
                }
            }
            Err(error) => self.message = Some(error),
        }
    }

    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        database_path: &str,
        can_retry: bool,
    ) -> Option<UiCommand> {
        if self.database_path != database_path {
            *self = Self {
                database_path: database_path.into(),
                refresh: true,
                ..Self::default()
            };
        }
        let mut command = None;
        ui.horizontal(|ui| {
            ui.heading("Image host permissions");
            if ui
                .add_enabled(!self.loading, egui::Button::new("Refresh permissions"))
                .clicked()
            {
                self.refresh = true;
            }
            if self.loading {
                ui.spinner();
            }
        });
        if let Some(message) = &self.message {
            ui.label(message);
        }
        if self.groups.is_empty() && !self.loading {
            ui.weak("No image hosts need review.");
        }
        egui::ScrollArea::vertical()
            .id_salt("image-host-permissions")
            .max_height(280.0)
            .show(ui, |ui| {
                for group in &self.groups {
                    let status = if group.approved {
                        "Allowed"
                    } else {
                        "Needs permission"
                    };
                    ui.push_id((&group.extension_id, &group.origin), |ui| {
                        egui::CollapsingHeader::new(format!(
                            "{} · {} · {} cards",
                            group.origin,
                            status,
                            group.cards.len(),
                        ))
                        .show(ui, |ui| {
                            ui.weak(&group.extension_id);
                            ui.horizontal_wrapped(|ui| {
                                let mut set_action = |action| {
                                    command = Some(UiCommand::ChangeImageAccess {
                                        extension_id: group.extension_id.clone(),
                                        origin: group.origin.clone(),
                                        action,
                                    });
                                };
                                if group.approved {
                                    if ui
                                        .add_enabled(
                                            can_retry && !self.loading && !group.cards.is_empty(),
                                            egui::Button::new("Retry all cards"),
                                        )
                                        .clicked()
                                    {
                                        set_action(ImageAccessAction::Retry);
                                    }
                                    if ui
                                        .add_enabled(
                                            !self.loading,
                                            egui::Button::new("Revoke permission"),
                                        )
                                        .clicked()
                                    {
                                        set_action(ImageAccessAction::Revoke);
                                    }
                                } else if ui
                                    .add_enabled(
                                        can_retry && !self.loading,
                                        egui::Button::new("Allow host and retry all"),
                                    )
                                    .clicked()
                                {
                                    set_action(ImageAccessAction::ApproveAndRetry);
                                }
                            });
                            for (id, card) in &group.cards {
                                ui.push_id(id, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        if ui.button("Open card").clicked() {
                                            command = Some(UiCommand::OpenQueuedCard {
                                                url: card.page_url.clone(),
                                            });
                                        }
                                        if group.approved
                                            && ui
                                                .add_enabled(
                                                    can_retry && !self.loading,
                                                    egui::Button::new("Retry card"),
                                                )
                                                .clicked()
                                        {
                                            command = Some(UiCommand::RepairPageImages {
                                                url: card.page_url.clone(),
                                            });
                                        }
                                        ui.label(&card.title);
                                    });
                                    egui::CollapsingHeader::new(format!(
                                        "Resources ({})",
                                        card.resources.len()
                                    ))
                                    .show(ui, |ui| {
                                        for (url, reason) in &card.resources {
                                            let detail = match reason.as_str() {
                                                "redirect" => "Redirect",
                                                "viewer" => "Image viewer",
                                                "download" => "Image download",
                                                _ => "Discovered image",
                                            };
                                            ui.add(
                                                egui::Label::new(format!("{detail} · {url}"))
                                                    .wrap()
                                                    .selectable(true),
                                            );
                                        }
                                    });
                                });
                            }
                        });
                    });
                }
            });
        if command.is_none() && self.refresh && !self.loading && !database_path.is_empty() {
            self.loading = true;
            self.refresh = false;
            command = Some(UiCommand::LoadImageAccess);
        }
        if matches!(command, Some(UiCommand::ChangeImageAccess { .. })) {
            self.loading = true;
        }
        command
    }
}

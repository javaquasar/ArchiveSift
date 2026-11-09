use crate::{
    app::AppConfig,
    scraper::extensions::{
        manager::{
            global_manager, updates::UpdateSource, CheckReport, CheckState, LocalState,
            PreparedPackage,
        },
        manifest::RuntimeKind,
    },
    ui::UiCommand,
};
use eframe::egui;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
};

#[derive(Default)]
pub(super) struct ExtensionsManagerUi {
    worker: Option<Receiver<Result<Reply, String>>>,
    prepared: Option<PreparedPackage>,
    report: Option<CheckReport>,
    checks: BTreeMap<String, CheckReport>,
    state: Option<LocalState>,
    state_attempted: bool,
    pub(super) changed: bool,
    approve: bool,
    native: bool,
    message: Option<Result<String, String>>,
    source_id: Option<String>,
    source_json: String,
    token: zeroize::Zeroizing<String>,
    rollback: Option<(String, String)>,
}
enum Reply {
    Preview(PreparedPackage, CheckReport),
    Changed,
    Installed(String, CheckReport),
    State(LocalState),
    Checked(String, CheckReport),
}
impl ExtensionsManagerUi {
    pub(super) fn busy(&self) -> bool {
        self.worker.is_some()
    }
    pub(super) fn blocks_settings(&self) -> bool {
        self.worker.is_some()
    }
    fn start(
        &mut self,
        ctx: &egui::Context,
        job: impl FnOnce() -> Result<Reply, String> + Send + 'static,
    ) {
        if self.worker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        self.worker = Some(rx);
        std::thread::spawn(move || {
            let result = job();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    pub(super) fn poll(&mut self, ctx: &egui::Context, _config: &AppConfig) {
        if let Some(rx) = &self.worker {
            match rx.try_recv() {
                Ok(result) => {
                    self.worker = None;
                    match result {
                        Ok(Reply::Preview(package, report)) => {
                            self.prepared = Some(package);
                            self.report = Some(report);
                            self.approve = false;
                            self.native = false;
                            self.message = None;
                        }
                        Ok(Reply::Changed) => {
                            self.changed = true;
                            self.state = None;
                            self.state_attempted = false;
                            self.prepared = None;
                            self.checks.clear();
                            self.message=Some(Ok("Extension registry updated. Running downloads retain their original version.".into()));
                        }
                        Ok(Reply::Installed(id, report)) => {
                            self.invalidate();
                            self.prepared = None;
                            self.report = None;
                            self.checks.insert(id, report);
                            self.message=Some(Ok("Package installed. Complete missing settings or sign in before downloading.".into()));
                        }
                        Ok(Reply::State(state)) => self.state = Some(state),
                        Ok(Reply::Checked(id, report)) => {
                            self.checks.insert(id, report);
                        }
                        Err(e) => self.message = Some(Err(e)),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.message = Some(Err("Extension task ended unexpectedly".into()));
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
            }
        }
    }
    pub(super) fn invalidate(&mut self) {
        self.changed = true;
        self.state = None;
        self.state_attempted = false;
        self.checks.clear();
    }

    pub(super) fn preview(
        &mut self,
        ctx: &egui::Context,
        path: PathBuf,
        config: &AppConfig,
        legacy: bool,
    ) {
        let settings = config.extension_settings.clone();
        self.start(ctx, move || {
            let manager = global_manager();
            let package = if legacy {
                manager.import_legacy(&path)
            } else {
                manager.prepare(&path)
            }
            .map_err(|e| e.to_string())?;
            let runtime = super::shared_async_runtime()?;
            let report = runtime
                .block_on(manager.check_prepared(&package, settings.get(&package.manifest.id)));
            Ok(Reply::Preview(package, report))
        });
    }
    pub(super) fn render(
        &mut self,
        ui: &mut egui::Ui,
        config: &AppConfig,
        blocked: bool,
        mut credentials: impl FnMut(&mut egui::Ui, &str, bool) -> Option<UiCommand>,
    ) -> Option<UiCommand> {
        let mut command = None;
        ui.heading("Extensions");
        ui.label("Install, check and switch parser versions without restarting. Settings and granted permissions are stored locally.");
        let directory = crate::scraper::extensions::default_plugin_directory();
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::Label::new(directory.to_string_lossy()).wrap());
            if ui.button("Copy folder path").clicked() {
                ui.ctx().copy_text(directory.to_string_lossy().into_owned());
            }
        });
        let busy = self.busy() || blocked;
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Install extension…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("ArchiveSift extension", &["asift-plugin", "zip"])
                        .pick_file()
                    {
                        self.preview(ui.ctx(), path, config, false);
                    }
                }
                if ui.button("Reload packages").clicked() {
                    self.start(ui.ctx(), || {
                        global_manager().reload().map_err(|e| e.to_string())?;
                        Ok(Reply::Changed)
                    });
                }
            });
        });
        if !busy {
            if let Some(path) = ui.ctx().input(|i| {
                i.raw.dropped_files.iter().find_map(|f| {
                    f.path.clone().filter(|p| {
                        p.extension()
                            .is_some_and(|e| e == "asift-plugin" || e == "zip")
                    })
                })
            }) {
                self.preview(ui.ctx(), path, config, false);
            }
        }
        if self.worker.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking extension…");
            });
        }
        if let Some(message) = &self.message {
            match message {
                Ok(m) => {
                    ui.label(m);
                }
                Err(m) => {
                    ui.colored_label(egui::Color32::RED, m);
                }
            }
        }
        if !self.state_attempted && self.worker.is_none() {
            self.state_attempted = true;
            self.start(ui.ctx(), || {
                Ok(Reply::State(
                    global_manager().state().map_err(|e| e.to_string())?,
                ))
            });
        }
        if let Some(package) = &self.prepared {
            let manifest = &package.manifest;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.heading(format!(
                    "Review: {} {}",
                    manifest.display_name, manifest.version
                ));
                ui.label(format!(
                    "ID: {} · API {} · {:?}",
                    manifest.id, manifest.api, manifest.runtime
                ));
                ui.label(format!(
                    "Package SHA-256: {}",
                    package.package.artifact_digest
                ));
                ui.add(
                    egui::Label::new(match &package.package.signer {
                        Some(key) => format!("Ed25519 publisher key: {key}"),
                        None => "Unsigned local package — approve only a source you trust".into(),
                    })
                    .wrap(),
                );
                ui.label("Requested network access (including file downloads):");
                let previous = self
                    .state
                    .as_ref()
                    .and_then(|s| s.plugins.get(&manifest.id))
                    .and_then(|p| p.versions.get(&p.active));
                for origin in &manifest.network_origins {
                    let added = previous.is_some_and(|p| !p.origins.contains(origin));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{}{origin}",
                                if added { "New permission: " } else { "" }
                            ))
                            .monospace(),
                        )
                        .wrap(),
                    );
                }
                if previous.is_some_and(|p| p.publisher != package.package.signer) {
                    ui.colored_label(
                        egui::Color32::ORANGE,
                        "Publisher changed. Review the full key before approving this package.",
                    );
                }
                ui.collapsing("Required settings and schema", |ui| {
                    ui.add(
                        egui::Label::new(
                            serde_json::to_string_pretty(package.settings.schema.value())
                                .unwrap_or_default(),
                        )
                        .wrap(),
                    );
                });
                if package
                    .package
                    .files
                    .contains_key("credential-bindings.json")
                {
                    ui.label("Uses declared references to this extension's OS-stored credentials.");
                }
                if let Some(report) = &self.report {
                    show_report(ui, report);
                }
                ui.checkbox(
                    &mut self.approve,
                    "Approve this exact package, publisher and network permissions",
                );
                if manifest.runtime == RuntimeKind::Process {
                    ui.checkbox(
                        &mut self.native,
                        "Allow native code to run with my account permissions",
                    );
                }
            });
            let allowed = self.approve
                && (manifest.runtime == RuntimeKind::Wasm || self.native)
                && !busy
                && self.report.as_ref().is_some_and(|r| {
                    matches!(
                        r.state,
                        CheckState::Ready
                            | CheckState::MissingSettings
                            | CheckState::AuthorizationRequired
                    )
                });
            let mut install = false;
            let mut cancel = false;
            ui.horizontal(|ui| {
                install = ui
                    .add_enabled(allowed, egui::Button::new("Install and activate"))
                    .clicked();
                cancel = ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked();
            });
            if install {
                let package = self.prepared.take().unwrap();
                let native = self.native;
                let report = self.report.clone().expect("reviewed package report");
                let settings = config.extension_settings.get(&package.manifest.id).cloned();
                self.start(ui.ctx(), move || {
                    global_manager()
                        .install(
                            &package,
                            package.package.signer.as_deref(),
                            package.package.signer.is_none(),
                            native,
                            settings.as_ref(),
                        )
                        .map_err(|e| e.to_string())?;
                    Ok(Reply::Installed(package.manifest.id.clone(), report))
                });
            } else if cancel {
                self.prepared = None;
                self.report = None;
            }
        }
        ui.separator();
        let registry = crate::scraper::ScraperRegistry::with_all_scrapers();
        if registry.extension_statuses().is_empty() {
            ui.label("No external packages installed. The built-in demo parser is available.");
        }
        ui.scope(|ui| {
            for status in registry.extension_statuses() {
                ui.push_id(&status.directory, |ui| {
                    extension_frame(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("{} {}", status.id, status.version))
                                    .strong(),
                            )
                            .wrap(),
                        );
                        ui.add(egui::Label::new(&status.message).wrap());
                        if let Some(action) = credentials(ui, &status.id, busy || self.busy()) {
                            command = Some(action);
                        }
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(status.directory.to_string_lossy()).small(),
                            )
                            .wrap(),
                        );
                        let installed = self
                            .state
                            .as_ref()
                            .and_then(|s| s.plugins.get(&status.id))
                            .cloned();
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                if ui.button("Check extension").clicked() {
                                    let id = status.id.clone();
                                    let settings = config.extension_settings.get(&id).cloned();
                                    self.start(ui.ctx(), move || {
                                        let runtime = super::shared_async_runtime()?;
                                        Ok(Reply::Checked(
                                            id.clone(),
                                            runtime.block_on(
                                                global_manager().check(&id, settings.as_ref()),
                                            ),
                                        ))
                                    });
                                }
                                if let Some(installed) = &installed {
                                    if ui
                                        .button(if installed.enabled {
                                            "Disable"
                                        } else {
                                            "Enable"
                                        })
                                        .clicked()
                                    {
                                        let id = status.id.clone();
                                        let enabled = !installed.enabled;
                                        self.start(ui.ctx(), move || {
                                            global_manager()
                                                .set_enabled(&id, enabled)
                                                .map_err(|e| e.to_string())?;
                                            Ok(Reply::Changed)
                                        });
                                    }
                                    if ui
                                        .add_enabled(
                                            installed.previous.is_some(),
                                            egui::Button::new("Roll back…"),
                                        )
                                        .clicked()
                                    {
                                        if let Some(previous) = &installed.previous {
                                            self.rollback =
                                                Some((status.id.clone(), previous.clone()));
                                        }
                                    }
                                    if ui.button("Update source…").clicked() {
                                        self.source_id = Some(status.id.clone());
                                        self.source_json =
                                            serde_json::to_string_pretty(&installed.source)
                                                .unwrap_or_default();
                                    }
                                    if ui
                                        .add_enabled(
                                            installed.source.is_some(),
                                            egui::Button::new("Check for updates"),
                                        )
                                        .clicked()
                                    {
                                        let id = status.id.clone();
                                        let settings = config.extension_settings.clone();
                                        self.start(ui.ctx(), move || {
                                            let manager = global_manager();
                                            let runtime = super::shared_async_runtime()?;
                                            let package = runtime
                                                .block_on(manager.check_update(&id))
                                                .map_err(|e| e.to_string())?;
                                            let report = runtime.block_on(
                                                manager.check_prepared(&package, settings.get(&id)),
                                            );
                                            Ok(Reply::Preview(package, report))
                                        });
                                    }
                                } else if ui.button("Import legacy package…").clicked() {
                                    self.preview(ui.ctx(), status.directory.clone(), config, true);
                                }
                            });
                        });
                        if let Some(report) = self.checks.get(&status.id) {
                            show_report(ui, report);
                        }
                        if let Some(installed) = &installed {
                            ui.collapsing("Installed versions", |ui| {
                                for (key, version) in &installed.versions {
                                    ui.label(format!(
                                        "{}{} · {}",
                                        version.version,
                                        if key == &installed.active {
                                            " (active)"
                                        } else if Some(key) == installed.previous.as_ref() {
                                            " (previous)"
                                        } else {
                                            ""
                                        },
                                        &version.digest[..12]
                                    ));
                                    if ui
                                        .add_enabled(
                                            !busy && key != &installed.active,
                                            egui::Button::new("Use this version…"),
                                        )
                                        .clicked()
                                    {
                                        self.rollback = Some((status.id.clone(), key.clone()));
                                    }
                                }
                            });
                        }
                    });
                });
            }
        });
        if let Some((id, version)) = &self.rollback {
            let id = id.clone();
            let version = version.clone();
            let mut open = true;
            egui::Window::new("Confirm extension version").open(&mut open).show(ui.ctx(),|ui| {
                ui.add(egui::Label::new(format!("Activate {id} version {version} and restore its saved settings?")).wrap());
                ui.label("Only this extension's settings will be restored. Credentials stay in the system password store.");
                if ui.add_enabled(!busy,egui::Button::new("Confirm version change")).clicked() {
                    command=Some(UiCommand::SwitchExtensionVersion {id:id.clone(),version:version.clone(),expected:serde_json::to_value(config).unwrap()}); self.rollback=None;
                }
            });
            if !open {
                self.rollback = None;
            }
        }
        if let Some(id) = self.source_id.clone() {
            let mut open = true;
            egui::Window::new("Private update source").open(&mut open).show(ui.ctx(),|ui|{
            ui.label("Local folder: {\"kind\":\"directory\",\"path\":\"absolute path\"}");
            ui.label("GitHub Releases: {\"kind\":\"github\",\"repository\":\"owner/repository\",\"asset_prefix\":\"parser-\"}");
            ui.label("Use null to remove the source. Repository names stay in local state.");
            ui.add(egui::TextEdit::multiline(&mut self.source_json).desired_width(540.0).desired_rows(6));
            ui.label("GitHub token (optional replacement; stored only in the system password store)");ui.add(egui::TextEdit::singleline(&mut *self.token).password(true).char_limit(8192));
            if ui.add_enabled(!busy,egui::Button::new("Save source")).clicked(){
                match serde_json::from_str::<Option<UpdateSource>>(&self.source_json){Ok(source)=>{let mut token=zeroize::Zeroizing::new(String::new());std::mem::swap(&mut token,&mut self.token);let id=id.clone();self.start(ui.ctx(),move||{
                    if let Some(s)=&source{s.validate().map_err(|e|e.to_string())?;if !token.is_empty(){use crate::secure_store::SecretStore;let target=s.credential_target().ok_or("This source does not use a token")?;crate::secure_store::SystemSecretStore.set(&target,&crate::secure_store::Secret::new(token.as_bytes().to_vec())).map_err(|e|e.to_string())?;}}
                    global_manager().set_source(&id,source).map_err(|e|e.to_string())?;Ok(Reply::Changed)
                });self.source_id=None;},Err(e)=>self.message=Some(Err(e.to_string()))}
            }
        });
            if !open {
                self.source_id = None;
                use zeroize::Zeroize;
                self.token.zeroize();
            }
        }
        command
    }
}
fn show_report(ui: &mut egui::Ui, report: &CheckReport) {
    ui.strong(format!("{:?}", report.state));
    for check in &report.checks {
        ui.add(egui::Label::new(check).wrap());
    }
}

pub(super) fn extension_frame(
    ui: &mut egui::Ui,
    body: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let frame = egui::Frame::group(ui.style());
    let width = (ui.available_width() - frame.total_margin().sum().x).max(0.0);
    frame
        .show(ui, |ui| {
            ui.set_width(width);
            body(ui);
        })
        .response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_state_load_is_shown_without_spawning_a_worker_on_every_frame() {
        let ctx = egui::Context::default();
        let mut editor = ExtensionsManagerUi {
            state_attempted: true,
            message: Some(Err("Invalid local extension state".into())),
            ..Default::default()
        };
        for _ in 0..3 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    editor.render(ui, &AppConfig::default(), false, |_, _, _| None);
                });
            });
            assert!(editor.worker.is_none());
            assert!(editor.message.as_ref().unwrap().is_err());
        }
        editor.invalidate();
        assert!(!editor.state_attempted);
    }
}

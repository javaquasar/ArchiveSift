use eframe::egui;
use zeroize::{Zeroize, Zeroizing};

use crate::app::AppConfig;
use crate::scraper::extensions::accounts::{
    AccountCommand, AccountError, AccountOutcome, AccountProvider, SignInRequest,
};
use crate::scraper::ScraperRegistry;
use crate::secure_store::Secret;

pub(super) struct AccountSettingsEditor {
    entries: Vec<AccountEntry>,
}
struct AccountEntry {
    provider: AccountProvider,
    username: String,
    password: Zeroizing<String>,
    remember_password: bool,
    saved_username: String,
    password_saved: bool,
    busy: bool,
    status: Option<Result<String, String>>,
}

impl AccountSettingsEditor {
    pub(super) fn installed(config: &AppConfig) -> Self {
        let providers = ScraperRegistry::with_all_scrapers()
            .extension_statuses()
            .iter()
            .filter(|status| status.loaded)
            .filter_map(|status| {
                AccountProvider::from_directory(&status.directory)
                    .ok()
                    .flatten()
            })
            .collect();
        Self::new(providers, config)
    }
    fn new(providers: Vec<AccountProvider>, config: &AppConfig) -> Self {
        Self {
            entries: providers
                .into_iter()
                .map(|provider| {
                    let saved = config
                        .extension_accounts
                        .get(&provider.id)
                        .cloned()
                        .unwrap_or_default();
                    let remember_password = saved.username.is_empty() || saved.remember_password;
                    AccountEntry {
                        provider,
                        username: saved.username.clone(),
                        saved_username: saved.username,
                        password: Zeroizing::new(String::new()),
                        password_saved: saved.remember_password,
                        remember_password,
                        busy: false,
                        status: None,
                    }
                })
                .collect(),
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.entries.iter().any(|entry| entry.busy)
    }

    pub(super) fn render(&mut self, ui: &mut egui::Ui, blocked: bool) -> Option<AccountCommand> {
        if self.entries.is_empty() {
            return None;
        }
        let mut command = None;
        ui.heading("Accounts");
        ui.label("Sign in to installed extensions. Passwords and sessions are stored in the system password store.");
        for entry in &mut self.entries {
            let id = egui::Id::new(&entry.provider.id);
            ui.push_id(id, |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.strong(&entry.provider.display_name);
                    ui.label(entry.provider.origin());
                    ui.add_enabled_ui(!blocked && !entry.busy, |ui| {
                        ui.label("Username");
                        ui.add(
                            egui::TextEdit::singleline(&mut entry.username)
                                .desired_width(f32::INFINITY)
                                .char_limit(512),
                        );
                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut *entry.password)
                                .password(true)
                                .hint_text(
                                    if entry.password_saved
                                        && entry.username == entry.saved_username
                                    {
                                        "Saved password"
                                    } else {
                                        "Enter password"
                                    },
                                )
                                .desired_width(f32::INFINITY)
                                .char_limit(1024),
                        );
                        ui.checkbox(&mut entry.remember_password, "Remember password");
                        ui.horizontal_wrapped(|ui| {
                            let can_sign_in = !entry.username.trim().is_empty()
                                && (!entry.password.is_empty()
                                    || (entry.password_saved
                                        && entry.username == entry.saved_username));
                            if ui
                                .add_enabled(can_sign_in, egui::Button::new("Sign in"))
                                .clicked()
                            {
                                command = Some(entry.sign_in());
                            }
                            if ui.button("Check session").clicked() {
                                command = Some(AccountCommand::Check(entry.provider.clone()));
                                entry.busy = true;
                                entry.status = None;
                            }
                            if ui.button("Forget account").clicked() {
                                command = Some(AccountCommand::Forget(entry.provider.clone()));
                                entry.busy = true;
                                entry.status = None;
                                entry.password.zeroize();
                            }
                        });
                    });
                    if entry.busy {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Checking account…");
                        });
                    }
                    if let Some(status) = &entry.status {
                        match status {
                            Ok(text) => {
                                ui.colored_label(egui::Color32::DARK_GREEN, text);
                            }
                            Err(text) => {
                                ui.colored_label(egui::Color32::RED, text);
                            }
                        }
                    }
                });
            });
            ui.add_space(8.0);
        }
        ui.separator();
        ui.add_space(8.0);
        command
    }

    pub(super) fn finish(&mut self, id: &str, result: Result<AccountOutcome, String>) {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.provider.id == id)
        else {
            return;
        };
        entry.busy = false;
        entry.status = Some(match result {
            Err(error) => Err(error),
            Ok(outcome) => {
                entry.password_saved = outcome.password_saved;
                if outcome.forgotten {
                    entry.username.clear();
                    entry.saved_username.clear();
                    entry.password.zeroize();
                    Ok("Saved account and session removed.".into())
                } else {
                    if let Some(account) = outcome.account {
                        entry.saved_username = account.username;
                    }
                    Ok(if outcome.password_saved {
                        "Signed in. Password saved in the system password store.".into()
                    } else {
                        "Session is active.".into()
                    })
                }
            }
        });
    }
}

impl AccountEntry {
    fn sign_in(&mut self) -> AccountCommand {
        self.busy = true;
        self.status = None;
        AccountCommand::SignIn(SignInRequest {
            provider: self.provider.clone(),
            username: self.username.trim().into(),
            password: (!self.password.is_empty())
                .then(|| Secret::new(std::mem::take(&mut *self.password).into_bytes())),
            remember_password: self.remember_password,
        })
    }
}

pub(super) fn apply_account_result(
    config: &mut AppConfig,
    id: &str,
    result: Result<AccountOutcome, AccountError>,
    path: &std::path::Path,
) -> Result<AccountOutcome, String> {
    let outcome = result.map_err(|error| error.to_string())?;
    if outcome.forgotten || outcome.account.is_some() {
        let mut next = config.clone();
        if outcome.forgotten {
            next.extension_accounts.remove(id);
        } else if let Some(account) = &outcome.account {
            next.extension_accounts.insert(id.into(), account.clone());
        }
        next.save_to(path).map_err(|_| "The account was updated in the password store, but the settings file could not be saved.".to_string())?;
        *config = next;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ExtensionAccount;

    #[test]
    fn account_result_merges_into_current_settings_and_persists_no_password() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let mut config = AppConfig {
            download_folder: "current-library".into(),
            ..Default::default()
        };
        let outcome = AccountOutcome {
            account: Some(ExtensionAccount {
                username: "example-user".into(),
                remember_password: true,
            }),
            password_saved: true,
            forgotten: false,
        };
        apply_account_result(&mut config, "demo.parser", Ok(outcome), &path).unwrap();
        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.download_folder, "current-library");
        assert_eq!(
            loaded.extension_accounts["demo.parser"].username,
            "example-user"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("\"password\""));
        apply_account_result(
            &mut config,
            "demo.parser",
            Ok(AccountOutcome {
                account: None,
                password_saved: false,
                forgotten: true,
            }),
            &path,
        )
        .unwrap();
        assert!(AppConfig::load_from(&path)
            .unwrap()
            .extension_accounts
            .is_empty());
    }

    #[test]
    fn failed_sign_in_does_not_save_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        assert!(apply_account_result(
            &mut AppConfig::default(),
            "demo.parser",
            Err(AccountError::SignInFailed),
            &path
        )
        .is_err());
        assert!(!path.exists());
    }

    fn editor(config: &AppConfig) -> AccountSettingsEditor {
        let directory = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({"schema":1,"api":"1.0","id":"demo.account","version":"1.0.0","display_name":"Demo account","source_key":"demo-account","runtime":"wasm","entry":"adapter.wasm","sha256":"0".repeat(64),"routes":[{"host":"example.test","path_prefix":"/content","kind":"content"}],"network_origins":["https://example.test"]});
        let form = serde_json::json!({"schema":1,"login_url":"https://example.test/login","check_url":"https://example.test/home","form_selector":"form","username_field":"username","password_field":"password","session_cookie":"session","session_paths":["/content"],"success_selector":"#logout"});
        std::fs::write(
            directory.path().join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("account.json"),
            serde_json::to_vec(&form).unwrap(),
        )
        .unwrap();
        AccountSettingsEditor::new(
            vec![AccountProvider::from_directory(directory.path())
                .unwrap()
                .unwrap()],
            config,
        )
    }

    fn frame(
        editor: &mut AccountSettingsEditor,
        context: &egui::Context,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<AccountCommand>) {
        let mut command = None;
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| command = editor.render(ui, false));
            },
        );
        (output, command)
    }

    fn position(output: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
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
    }

    fn click(
        editor: &mut AccountSettingsEditor,
        context: &egui::Context,
        output: &egui::FullOutput,
        label: &str,
    ) -> Option<AccountCommand> {
        let pos = position(output, label).expect("button rendered");
        let mut command = None;
        for pressed in [true, false] {
            let (_, result) = frame(
                editor,
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
            );
            command = result.or(command);
        }
        command
    }

    #[test]
    fn rendered_account_sign_in_masks_and_clears_password_and_disables_repeat_requests() {
        let mut editor = editor(&AppConfig::default());
        editor.entries[0].username = "example-user".into();
        *editor.entries[0].password = "fixture-password".into();
        let context = egui::Context::default();
        let (output, _) = frame(&mut editor, &context, vec![]);
        for label in [
            "Accounts",
            "Username",
            "Password",
            "Remember password",
            "Sign in",
            "Check session",
            "Forget account",
        ] {
            assert!(position(&output, label).is_some());
        }
        assert!(position(&output, "fixture-password").is_none());
        let AccountCommand::SignIn(request) =
            click(&mut editor, &context, &output, "Sign in").unwrap()
        else {
            panic!("sign-in request expected")
        };
        assert_eq!(request.password.unwrap().bytes(), b"fixture-password");
        assert!(editor.entries[0].password.is_empty());
        assert!(editor.busy());
        let (output, _) = frame(&mut editor, &context, vec![]);
        assert!(click(&mut editor, &context, &output, "Sign in").is_none());
        editor.finish("demo.account", Err("Sign-in failed.".into()));
        assert!(!editor.busy());
    }

    #[test]
    fn saved_password_is_reused_only_for_the_saved_username() {
        let mut config = AppConfig::default();
        config.extension_accounts.insert(
            "demo.account".into(),
            ExtensionAccount {
                username: "saved-user".into(),
                remember_password: true,
            },
        );
        let mut editor = editor(&config);
        let context = egui::Context::default();
        editor.entries[0].username = "other-user".into();
        let (output, _) = frame(&mut editor, &context, vec![]);
        assert!(click(&mut editor, &context, &output, "Sign in").is_none());
        editor.entries[0].username = "saved-user".into();
        let (output, _) = frame(&mut editor, &context, vec![]);
        let AccountCommand::SignIn(request) =
            click(&mut editor, &context, &output, "Sign in").unwrap()
        else {
            panic!("sign-in request expected")
        };
        assert!(request.password.is_none());
        editor.finish(
            "demo.account",
            Ok(AccountOutcome {
                account: None,
                forgotten: true,
                password_saved: false,
            }),
        );
        assert!(editor.entries[0].username.is_empty());
        assert!(!editor.entries[0].password_saved);
    }
}

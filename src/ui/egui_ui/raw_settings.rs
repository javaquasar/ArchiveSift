use crate::app::AppConfig;
use crate::scraper::extensions::settings::SettingsCatalog;
use crate::services::{
    config_completion::{self, Completion},
    configuration,
};
use crate::ui::UiCommand;
use archive_sift_scraper_api::settings_schema::{SettingsSchema, ValidationIssue};
use eframe::egui;
use serde_json::Value;
use std::collections::BTreeMap;
use zeroize::{Zeroize, Zeroizing};

pub(super) struct RawSettingsEditor {
    pub(super) draft: String,
    initial: String,
    baseline: Value,
    catalog: SettingsCatalog,
    schema: Option<SettingsSchema>,
    issues: Vec<ValidationIssue>,
    parsed: Option<Value>,
    syntax_error: Option<String>,
    pub(super) status: Option<String>,
    cursor: usize,
    suggestions: Vec<Completion>,
    show_suggestions: bool,
    selected_suggestion: usize,
    secrets: BTreeMap<String, SecretInput>,
}
struct SecretInput {
    text: Zeroizing<String>,
    pending: bool,
    status: Option<String>,
}
impl RawSettingsEditor {
    pub(super) fn installed(config: &AppConfig) -> Self {
        Self::new(config, SettingsCatalog::installed())
    }
    pub(super) fn new(config: &AppConfig, catalog: SettingsCatalog) -> Self {
        let baseline = serde_json::to_value(config).unwrap();
        let document =
            configuration::document(config, &catalog).unwrap_or_else(|_| baseline.clone());
        let draft = serde_json::to_string_pretty(&document).unwrap();
        let mut editor = Self {
            initial: draft.clone(),
            draft,
            baseline,
            schema: configuration::schema(&catalog).ok(),
            catalog,
            issues: vec![],
            parsed: None,
            syntax_error: None,
            status: None,
            cursor: 0,
            suggestions: vec![],
            show_suggestions: false,
            selected_suggestion: 0,
            secrets: BTreeMap::new(),
        };
        for target in editor.catalog.secret_targets() {
            editor.secrets.insert(
                target,
                SecretInput {
                    text: Zeroizing::new(String::new()),
                    pending: false,
                    status: None,
                },
            );
        }
        editor.validate();
        editor
    }
    fn validate(&mut self) {
        self.parsed = None;
        self.syntax_error = None;
        self.issues.clear();
        match configuration::parse_json(&self.draft) {
            Err(error) => self.syntax_error = Some(error),
            Ok(value) => {
                if let Err(issues) = configuration::validate(&value, &self.catalog) {
                    self.issues = issues;
                }
                self.parsed = Some(value);
            }
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.secrets.values().any(|s| s.pending)
    }
    pub(super) fn secret_finished(&mut self, target: &str, result: Result<(), String>) {
        if let Some(input) = self.secrets.get_mut(target) {
            input.pending = false;
            input.status = Some(
                result
                    .err()
                    .unwrap_or_else(|| "Credential saved in the system password store.".into()),
            );
        }
    }

    pub(super) fn render(
        &mut self,
        ui: &mut egui::Ui,
        config: &AppConfig,
        busy: bool,
        applying: bool,
    ) -> Option<UiCommand> {
        let mut command = None;
        ui.label("Full JSON configuration · Ctrl+Space for completion · Enter/Tab to accept · Escape to close suggestions");
        ui.label("Plugin sections are described by installed settings.schema.json files. Credential values stay in the system password store.");
        let stale = serde_json::to_value(config).ok().as_ref() != Some(&self.baseline);
        let mut requested =
            ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Space));
        ui.horizontal_wrapped(|ui| {
            if ui.button("Format JSON").clicked() {
                if let Some(value) = &self.parsed {
                    self.draft = serde_json::to_string_pretty(value).unwrap();
                    self.validate();
                }
            }
            if ui.button("Complete").clicked() {
                requested = true;
            }
            if ui
                .add_enabled(!applying, egui::Button::new("Discard draft"))
                .clicked()
            {
                *self = Self::new(config, self.catalog.clone());
            }
            if ui
                .add_enabled(
                    !busy && !applying,
                    egui::Button::new("Reload saved settings"),
                )
                .clicked()
            {
                command = Some(UiCommand::ReloadConfiguration);
            }
        });
        if stale {
            ui.colored_label(
                egui::Color32::DARK_RED,
                "Settings changed while this document was open. Reload before saving.",
            );
        }
        let accept = self.show_suggestions
            && !self.suggestions.is_empty()
            && ui.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
            });
        if self.show_suggestions {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                self.selected_suggestion =
                    (self.selected_suggestion + 1).min(self.suggestions.len().saturating_sub(1));
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                self.selected_suggestion = self.selected_suggestion.saturating_sub(1);
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                self.show_suggestions = false;
            }
        }
        let mut output = egui::TextEdit::multiline(&mut self.draft)
            .id(egui::Id::new("full_settings_json"))
            .code_editor()
            .desired_width(f32::INFINITY)
            .desired_rows(25)
            .interactive(!applying)
            .show(ui);
        if let Some(range) = output.cursor_range {
            self.cursor = range.primary.ccursor.index;
        }
        if output.response.changed() {
            self.validate();
            self.show_suggestions = output.response.has_focus();
            self.selected_suggestion = 0;
        }
        if requested {
            self.show_suggestions = true;
            output.response.request_focus();
        }
        let mut apply_suggestion = accept.then_some(self.selected_suggestion);
        if self.show_suggestions {
            self.suggestions = self
                .schema
                .as_ref()
                .map(|s| config_completion::complete(&self.draft, self.cursor, s))
                .unwrap_or_default();
            let position = output
                .cursor_range
                .map(|r| {
                    output.galley_pos
                        + output.galley.pos_from_cursor(&r.primary).min.to_vec2()
                        + egui::vec2(0.0, 20.0)
                })
                .unwrap_or(output.response.rect.left_bottom());
            if !self.suggestions.is_empty() {
                egui::Area::new("settings_completions".into())
                    .order(egui::Order::Foreground)
                    .fixed_pos(position)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(600.0);
                            egui::ScrollArea::vertical()
                                .max_height(210.0)
                                .show(ui, |ui| {
                                    for (i, suggestion) in self.suggestions.iter().enumerate() {
                                        if ui
                                            .selectable_label(
                                                i == self.selected_suggestion,
                                                &suggestion.label,
                                            )
                                            .on_hover_text(&suggestion.detail)
                                            .clicked()
                                        {
                                            apply_suggestion = Some(i);
                                        }
                                    }
                                });
                        });
                    });
            }
        }
        if let Some(index) = apply_suggestion {
            if let Some(suggestion) = self.suggestions.get(index) {
                (self.draft, self.cursor) = config_completion::apply(&self.draft, suggestion);
                output
                    .state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(self.cursor),
                    )));
                output.state.store(ui.ctx(), output.response.id);
                output.response.request_focus();
                self.show_suggestions = false;
                self.validate();
            }
        }
        if let Some(error) = &self.syntax_error {
            ui.colored_label(egui::Color32::RED, error);
        }
        for issue in &self.issues {
            ui.colored_label(egui::Color32::RED, issue.to_string());
        }
        if self.syntax_error.is_none() && self.issues.is_empty() {
            ui.colored_label(egui::Color32::DARK_GREEN, "Configuration is valid.");
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !busy
                        && !applying
                        && !stale
                        && self.syntax_error.is_none()
                        && self.issues.is_empty(),
                    egui::Button::new("Save full configuration"),
                )
                .clicked()
            {
                command = Some(UiCommand::ApplyConfiguration {
                    document: self.parsed.clone().unwrap(),
                    expected: self.baseline.clone(),
                });
            }
            if applying {
                ui.spinner();
                ui.label("Applying configuration…");
            }
            if self.draft != self.initial {
                ui.weak("Unsaved changes");
            }
        });
        if let Some(status) = &self.status {
            ui.label(status);
        }
        ui.collapsing("Plugin requirements and credentials", |ui| {
            for plugin in self.catalog.plugins.values() {
                ui.strong(format!("{} {}", plugin.id, plugin.version));
                if !plugin.declared {
                    ui.weak("Legacy package: no configurable guest settings declared.");
                }
                if let Some(properties) = plugin.schema.value()["properties"].as_object() {
                    for (name, node) in properties {
                        let required = plugin.schema.value()["required"]
                            .as_array()
                            .is_some_and(|r| r.iter().any(|r| r == name));
                        ui.label(format!(
                            "{} · {}{}",
                            name,
                            node["type"].as_str().unwrap_or("value"),
                            if required { " · required" } else { "" }
                        ));
                        if let Some(description) = node["description"].as_str() {
                            ui.weak(description);
                        }
                    }
                }
            }
            for (target, input) in &mut self.secrets {
                ui.push_id(target, |ui| {
                    ui.label(target);
                    ui.add(
                        egui::TextEdit::singleline(&mut *input.text)
                            .password(true)
                            .hint_text("New credential value")
                            .char_limit(8192),
                    );
                    if ui
                        .add_enabled(
                            !busy && !applying && !input.pending && !input.text.is_empty(),
                            egui::Button::new("Store credential"),
                        )
                        .clicked()
                    {
                        let value =
                            crate::secure_store::Secret::new(input.text.as_bytes().to_vec());
                        input.text.zeroize();
                        input.pending = true;
                        input.status = None;
                        command = Some(UiCommand::StoreSettingSecret {
                            target: target.clone(),
                            value,
                        });
                    }
                    if let Some(status) = &input.status {
                        ui.label(status);
                    }
                });
            }
        });
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(
        editor: &mut RawSettingsEditor,
        config: &AppConfig,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        busy: bool,
    ) -> (egui::FullOutput, Option<UiCommand>) {
        let mut command = None;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 1200.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| command = editor.render(ui, config, busy, false));
            },
        );
        (output, command)
    }
    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    fn position(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        fn find(shape: &egui::Shape, label: &str) -> Option<egui::Pos2> {
            match shape {
                egui::Shape::Text(t) if t.galley.job.text == label => {
                    Some(t.pos + t.galley.size() * 0.5)
                }
                egui::Shape::Vec(v) => v.iter().find_map(|s| find(s, label)),
                _ => None,
            }
        }
        output
            .shapes
            .iter()
            .find_map(|s| find(&s.shape, label))
            .unwrap_or_else(|| panic!("missing {label}"))
    }
    fn click(
        editor: &mut RawSettingsEditor,
        config: &AppConfig,
        ctx: &egui::Context,
        label: &str,
        busy: bool,
    ) -> Option<UiCommand> {
        render(editor, config, ctx, vec![], busy);
        let (output, _) = render(editor, config, ctx, vec![], busy);
        let pos = position(&output, label);
        let mut result = None;
        for pressed in [true, false] {
            let (_, command) = render(
                editor,
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
                busy,
            );
            result = command.or(result);
        }
        result
    }
    #[test]
    fn valid_json_edits_emit_a_complete_configuration_and_invalid_or_busy_edits_do_not() {
        let config = AppConfig::default();
        let ctx = egui::Context::default();
        let mut editor = RawSettingsEditor::new(&config, SettingsCatalog::default());
        let mut value = editor.parsed.clone().unwrap();
        value["preset_tags"] = serde_json::json!("manual");
        editor.draft = serde_json::to_string_pretty(&value).unwrap();
        editor.validate();
        assert!(click(&mut editor, &config, &ctx, "Save full configuration", true).is_none());
        let command = click(&mut editor, &config, &ctx, "Save full configuration", false).unwrap();
        let UiCommand::ApplyConfiguration { document, expected } = command else {
            panic!("wrong command")
        };
        assert_eq!(document["preset_tags"], "manual");
        assert_eq!(expected, serde_json::to_value(&config).unwrap());
        editor.draft = "{\"search_columns\":0}".into();
        editor.validate();
        assert!(!editor.issues.is_empty());
        assert!(click(&mut editor, &config, &ctx, "Save full configuration", false).is_none());
        editor.draft = "{".into();
        editor.validate();
        assert!(editor.syntax_error.is_some());
        assert!(click(&mut editor, &config, &ctx, "Save full configuration", false).is_none());
    }
    #[test]
    fn keyboard_completion_uses_the_json_caret_and_accepts_with_enter() {
        let config = AppConfig::default();
        let ctx = egui::Context::default();
        let mut editor = RawSettingsEditor::new(&config, SettingsCatalog::default());
        editor.draft = "{\"database_pa".into();
        editor.validate();
        render(&mut editor, &config, &ctx, vec![], false);
        let id = egui::Id::new("full_settings_json");
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(editor.draft.chars().count()),
            )));
        state.store(&ctx, id);
        ctx.memory_mut(|m| m.request_focus(id));
        render(
            &mut editor,
            &config,
            &ctx,
            vec![key(egui::Key::Space, egui::Modifiers::CTRL)],
            false,
        );
        assert!(editor
            .suggestions
            .iter()
            .any(|s| s.label == "database_path"));
        assert!(editor.show_suggestions);
        render(
            &mut editor,
            &config,
            &ctx,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
            false,
        );
        assert!(editor
            .draft
            .contains("\"database_path\": \"data/archivesift.sqlite3\""));
        assert!(!editor.show_suggestions);
    }
    #[test]
    fn changed_active_settings_require_reload_before_saving() {
        let config = AppConfig::default();
        let ctx = egui::Context::default();
        let mut editor = RawSettingsEditor::new(&config, SettingsCatalog::default());
        let mut changed = config.clone();
        changed.search_columns = 6;
        assert!(click(
            &mut editor,
            &changed,
            &ctx,
            "Save full configuration",
            false
        )
        .is_none());
        click(&mut editor, &changed, &ctx, "Discard draft", false);
        assert_eq!(editor.parsed.as_ref().unwrap()["search_columns"], 6);
        assert!(matches!(
            click(&mut editor, &changed, &ctx, "Reload saved settings", false),
            Some(UiCommand::ReloadConfiguration)
        ));
    }
}

//! Shared asynchronous attribution filter for images and torrent cards.
use crate::ui::UiCommand;
use eframe::egui;

#[derive(Default, Clone)]
pub(super) struct CreatorFilter {
    pub text: String,
    suggestions: Vec<String>,
    requested: Option<(String, String)>,
}

fn parts(value: &str) -> Vec<(usize, &str)> {
    let mut result = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (index, character) in value.char_indices() {
        if character == '"' {
            quoted = !quoted;
        }
        if character == ',' && !quoted {
            result.push((start, &value[start..index]));
            start = index + 1;
        }
    }
    result.push((start, &value[start..]));
    result
}

fn name(value: &str) -> String {
    let value = value.trim();
    if let Some(value) = value.strip_prefix('"') {
        value
            .strip_suffix('"')
            .unwrap_or(value)
            .replace("\"\"", "\"")
    } else {
        value.into()
    }
}

fn last_token(value: &str) -> String {
    name(parts(value).last().unwrap().1)
}

impl CreatorFilter {
    pub fn values(&self) -> Vec<String> {
        parts(&self.text)
            .into_iter()
            .map(|(_, token)| name(token))
            .filter(|value| !value.is_empty())
            .collect()
    }

    fn request(&mut self, database: &str) -> Option<UiCommand> {
        let query = last_token(&self.text);
        let key = (database.to_owned(), query.clone());
        if self.requested.as_ref() == Some(&key) {
            return None;
        }
        self.suggestions.clear();
        self.requested = Some(key);
        Some(UiCommand::SuggestCreators {
            database_path: database.into(),
            query,
        })
    }

    pub fn input(
        &mut self,
        ui: &mut egui::Ui,
        size: [f32; 2],
        database: &str,
    ) -> Option<UiCommand> {
        let response = ui.add_sized(
            size,
            egui::TextEdit::singleline(&mut self.text)
                .hint_text("Director, author or photographer")
                .char_limit(2048),
        );
        if response.gained_focus() {
            self.requested = None;
        }
        if response.changed() || response.gained_focus() {
            self.request(database)
        } else {
            None
        }
    }

    pub fn set_suggestions(&mut self, database: &str, query: &str, values: Vec<String>) {
        if self
            .requested
            .as_ref()
            .is_some_and(|(db, token)| db == database && token == query)
            && last_token(&self.text) == query
        {
            let tokens = parts(&self.text);
            let previous = tokens
                .iter()
                .take(tokens.len().saturating_sub(1))
                .map(|(_, token)| name(token))
                .collect::<Vec<_>>();
            self.suggestions = values
                .into_iter()
                .filter(|value| {
                    !previous
                        .iter()
                        .any(|name| name.to_lowercase() == value.to_lowercase())
                })
                .collect();
        }
    }

    pub fn has_suggestions(&self) -> bool {
        !self.suggestions.is_empty()
    }

    fn select(&mut self, value: &str) {
        let start = parts(&self.text).last().unwrap().0;
        let encoded = if value.contains([',', '"']) {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value.into()
        };
        self.text = if start == 0 {
            encoded
        } else {
            format!("{} {}", self.text[..start].trim_end(), encoded)
        };
        self.suggestions.clear();
        self.requested = None;
    }

    pub fn render_suggestions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.small("Suggestions:");
            for value in self.suggestions.clone() {
                if ui.small_button(&value).clicked() {
                    self.select(&value);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_queries_databases_and_cleared_filters_cannot_restore_suggestions() {
        let mut filter = CreatorFilter::default();
        filter.text = "Old".into();
        filter.request("first.sqlite3");
        filter.text = "New".into();
        filter.request("second.sqlite3");
        filter.set_suggestions("first.sqlite3", "New", vec!["Wrong database".into()]);
        filter.set_suggestions("second.sqlite3", "Old", vec!["Wrong query".into()]);
        assert!(!filter.has_suggestions());
        filter.set_suggestions("second.sqlite3", "New", vec!["New Creator".into()]);
        assert!(filter.has_suggestions());
        filter = CreatorFilter::default();
        filter.set_suggestions("second.sqlite3", "", vec!["Stale".into()]);
        assert!(!filter.has_suggestions());
    }

    #[test]
    fn selected_names_with_commas_and_quotes_remain_single_people() {
        let mut filter = CreatorFilter::default();
        filter.text = "First Creator, Do".into();
        filter.select("Doe, Jane");
        assert_eq!(filter.values(), ["First Creator", "Doe, Jane"]);
        filter.text.push_str(", Ja");
        assert!(
            matches!(filter.request("library.sqlite3"), Some(UiCommand::SuggestCreators { query, .. }) if query == "Ja")
        );
        filter.select("Jane \"JJ\" Example");
        assert_eq!(
            filter.values(),
            ["First Creator", "Doe, Jane", "Jane \"JJ\" Example"]
        );
        assert_eq!(last_token("First Creator, \"Doe, Ja"), "Doe, Ja");
    }

    #[test]
    fn selecting_a_name_preserves_previous_people_and_suppresses_duplicates() {
        let mut filter = CreatorFilter::default();
        filter.text = "First Creator, Sec".into();
        assert!(
            matches!(filter.request("library.sqlite3"), Some(UiCommand::SuggestCreators {database_path, query}) if database_path == "library.sqlite3" && query == "Sec")
        );
        filter.set_suggestions(
            "library.sqlite3",
            "Sec",
            vec!["FIRST CREATOR".into(), "Second Creator".into()],
        );
        assert_eq!(filter.suggestions, ["Second Creator"]);
        filter.select("Second Creator");
        assert_eq!(filter.values(), ["First Creator", "Second Creator"]);
        filter.set_suggestions("library.sqlite3", "Sec", vec!["Stale".into()]);
        assert!(!filter.has_suggestions());
    }
}

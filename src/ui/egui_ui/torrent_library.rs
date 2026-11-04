use std::sync::mpsc::{self, Receiver, SyncSender};

use super::background_executor::spawn_background;
use super::preview_loader::{ImageLoadMode, PreviewLoader, PreviewSource};
use super::texture_cache::SizedLruCache;
use crate::app::AppConfig;
use crate::db::Database;
use crate::models::{TorrentCard, TorrentEdit, TorrentImage, TorrentPage, TorrentQuery};
use crate::services::torrent_library::{MoveProgress, TorrentLibrary, TorrentMovePlan};
use eframe::egui;

#[derive(Default)]
struct Filters {
    text: String,
    site: String,
    studio: String,
    tags: String,
    categories: String,
    year: String,
    offset: usize,
}
impl Filters {
    fn query(&self) -> TorrentQuery {
        let values = |s: &str| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };
        TorrentQuery {
            text: self.text.clone(),
            site_key: self.site.clone(),
            studio: self.studio.clone(),
            tags: values(&self.tags),
            categories: values(&self.categories),
            year: self.year.trim().into(),
            offset: self.offset,
        }
    }
}

#[derive(Default)]
struct Suggestions {
    studios: Vec<String>,
    tags: Vec<String>,
    categories: Vec<String>,
}
enum Outcome {
    Recovered,
    Loaded(TorrentPage, Suggestions),
    Planned(TorrentMovePlan),
    Saved(TorrentCard),
}
enum Message {
    Progress(MoveProgress),
    Finished(Result<Outcome, String>),
}

pub(super) struct TorrentLibraryUi {
    filters: Filters,
    page: TorrentPage,
    selected: Option<TorrentCard>,
    edit: TorrentEdit,
    inputs: [String; 3],
    suggestions: Suggestions,
    organize: bool,
    plan: Option<TorrentMovePlan>,
    receiver: Option<Receiver<Message>>,
    pub(super) busy: bool,
    loaded: bool,
    reload: bool,
    status: Option<String>,
    error: Option<String>,
    changed: bool,
    recovering: bool,
    pub(super) startup_error: Option<String>,
    loader: Option<PreviewLoader>,
    textures: SizedLruCache<String, egui::TextureHandle>,
    image_popup: Option<TorrentImage>,
}

impl Default for TorrentLibraryUi {
    fn default() -> Self {
        Self {
            filters: Filters::default(),
            page: TorrentPage {
                cards: vec![],
                total: 0,
            },
            selected: None,
            edit: TorrentEdit::default(),
            inputs: Default::default(),
            suggestions: Suggestions::default(),
            organize: false,
            plan: None,
            receiver: None,
            busy: false,
            loaded: false,
            reload: false,
            status: None,
            error: None,
            changed: false,
            recovering: false,
            startup_error: None,
            loader: None,
            textures: SizedLruCache::new(96, 48 * 1024 * 1024),
            image_popup: None,
        }
    }
}

impl TorrentLibraryUi {
    pub(super) fn recover_on_start(&mut self, ctx: &egui::Context, config: &AppConfig) {
        self.recovering = true;
        let library = TorrentLibrary::new(
            Database::new(&config.database_path),
            config.effective_download_dir(),
        );
        self.job(ctx, move |_, _| {
            library
                .recover()
                .map(|_| Outcome::Recovered)
                .map_err(|e| e.to_string())
        });
    }

    /// Polled even while a different tab is visible, so completion unlocks settings.
    pub(super) fn poll(&mut self) -> bool {
        while let Some(message) = self.receiver.as_ref().and_then(|r| r.try_recv().ok()) {
            match message {
                Message::Progress(p) => {
                    self.status = Some(format!("Moving files: {} / {}", p.completed, p.total))
                }
                Message::Finished(result) => {
                    self.busy = false;
                    self.receiver = None;
                    match result {
                        Ok(Outcome::Recovered) => (),
                        Err(error) => {
                            if self.recovering {
                                self.startup_error = Some(error.clone());
                            }
                            self.error = Some(error);
                            self.plan = None;
                            self.status = None;
                        }
                        Ok(Outcome::Loaded(page, suggestions)) => {
                            let selected = self.selected.as_ref().map(|c| c.page_id);
                            self.page = page;
                            self.suggestions = suggestions;
                            self.loaded = true;
                            let card = self
                                .page
                                .cards
                                .iter()
                                .find(|c| Some(c.page_id) == selected)
                                .or(self.page.cards.first())
                                .cloned();
                            if let Some(card) = card {
                                self.select(card);
                            } else {
                                self.selected = None;
                            }
                        }
                        Ok(Outcome::Planned(plan)) => {
                            self.plan = Some(plan);
                            self.status = None;
                        }
                        Ok(Outcome::Saved(card)) => {
                            self.select(card);
                            self.plan = None;
                            self.changed = true;
                            self.reload = true;
                            self.status = Some("Card saved. File paths updated.".into());
                            self.textures.clear();
                            if let Some(loader) = &mut self.loader {
                                loader.advance_generation();
                            }
                        }
                    }
                    self.recovering = false;
                }
            }
        }
        std::mem::take(&mut self.changed)
    }

    fn select(&mut self, card: TorrentCard) {
        self.edit = TorrentEdit::from(&card);
        self.selected = Some(card);
        self.inputs = Default::default();
        self.organize = false;
        self.plan = None;
        self.image_popup = None;
    }

    fn job(
        &mut self,
        ctx: &egui::Context,
        work: impl FnOnce(SyncSender<Message>, egui::Context) -> Result<Outcome, String>
            + Send
            + 'static,
    ) {
        let (tx, rx) = mpsc::sync_channel(8);
        self.receiver = Some(rx);
        self.busy = true;
        self.error = None;
        let ctx = ctx.clone();
        spawn_background(move || {
            let result = work(tx.clone(), ctx.clone());
            let _ = tx.send(Message::Finished(result));
            ctx.request_repaint();
        });
    }

    fn load(&mut self, ctx: &egui::Context, config: &AppConfig) {
        self.loaded = true;
        let database = Database::new(&config.database_path);
        let root = config.effective_download_dir();
        let query = self.filters.query();
        self.reload = false;
        self.job(ctx, move |_, _| {
            let run = || -> crate::AppResult<Outcome> {
                database.initialize()?;
                TorrentLibrary::new(database.clone(), root).recover()?;
                let page = database.search_torrents(&query)?;
                let suggestions = Suggestions {
                    studios: database.suggest_attribute_values("studio", "", 500)?,
                    tags: database.suggest_attribute_values("tag", "", 500)?,
                    categories: database.suggest_attribute_values("category", "", 500)?,
                };
                Ok(Outcome::Loaded(page, suggestions))
            };
            run().map_err(|e| e.to_string())
        });
    }

    pub(super) fn render(&mut self, ui: &mut egui::Ui, config: &AppConfig) {
        if (!self.loaded || self.reload) && !self.busy {
            self.load(ui.ctx(), config);
        }
        self.install_previews(ui.ctx());
        ui.add_enabled_ui(!self.busy && self.plan.is_none(), |ui| {
            egui::Grid::new("torrent_filters")
                .num_columns(4)
                .show(ui, |ui| {
                    for (label, value) in [
                        ("Text", &mut self.filters.text),
                        ("Source", &mut self.filters.site),
                        ("Studio", &mut self.filters.studio),
                        ("Year", &mut self.filters.year),
                        ("Tags (comma separated)", &mut self.filters.tags),
                        ("Categories (comma separated)", &mut self.filters.categories),
                    ] {
                        ui.label(label);
                        ui.add(egui::TextEdit::singleline(value).desired_width(180.0));
                        if matches!(label, "Source" | "Year" | "Categories (comma separated)") {
                            ui.end_row();
                        }
                    }
                });
            if ui.button("Find torrents / Refresh").clicked() {
                self.filters.offset = 0;
                self.load(ui.ctx(), config);
            }
        });
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    self.status
                        .as_deref()
                        .unwrap_or("Loading / checking files…"),
                );
            });
        } else if let Some(status) = &self.status {
            ui.label(status);
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }
        ui.horizontal(|ui| {
            ui.label(format!("{} torrent cards", self.page.total));
            if ui
                .add_enabled(
                    !self.busy && self.plan.is_none() && self.filters.offset > 0,
                    egui::Button::new("Previous"),
                )
                .clicked()
            {
                self.filters.offset = self.filters.offset.saturating_sub(40);
                self.load(ui.ctx(), config);
            }
            if ui
                .add_enabled(
                    !self.busy && self.plan.is_none() && self.filters.offset + 40 < self.page.total,
                    egui::Button::new("Next"),
                )
                .clicked()
            {
                self.filters.offset += 40;
                self.load(ui.ctx(), config);
            }
        });
        ui.separator();
        let height = ui.available_height();
        ui.columns(2, |columns| {
            egui::ScrollArea::vertical()
                .id_salt("torrent_cards")
                .max_height(height)
                .show(&mut columns[0], |ui| {
                    let mut choose = None;
                    for card in &self.page.cards {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if ui
                                .add_enabled(
                                    !self.busy && self.plan.is_none(),
                                    egui::Button::new(&card.title).wrap().selected(
                                        self.selected
                                            .as_ref()
                                            .is_some_and(|c| c.page_id == card.page_id),
                                    ),
                                )
                                .clicked()
                            {
                                choose = Some(card.clone());
                            }
                            ui.label(format!(
                                "{} · {}",
                                card.site_key,
                                if card.primary_studio.is_empty() {
                                    "Unknown studio"
                                } else {
                                    &card.primary_studio
                                }
                            ));
                            ui.label(format!(
                                "{} attachments · {} images",
                                card.attachments.len(),
                                card.images.len()
                            ));
                        });
                    }
                    if let Some(card) = choose {
                        self.select(card);
                    }
                    if self.page.total == 0 {
                        ui.label(
                            "Download a page containing a torrent or magnet to create a card.",
                        );
                    }
                });
            egui::ScrollArea::vertical()
                .id_salt("torrent_details")
                .max_height(height)
                .show(&mut columns[1], |ui| self.details(ui, config));
        });
        self.confirm(ui.ctx(), config);
        self.preview_window(ui.ctx(), config);
    }

    fn details(&mut self, ui: &mut egui::Ui, config: &AppConfig) {
        let Some(card) = self.selected.clone() else {
            return;
        };
        ui.add(egui::Label::new(&card.title).wrap().selectable(true));
        ui.hyperlink_to("Open source page", &card.page_url);
        let primary = self
            .edit
            .primary_image_id
            .and_then(|id| card.images.iter().find(|image| image.image_id == id))
            .or_else(|| card.images.first());
        if let Some(image) = primary {
            self.preview(ui, config, image, egui::vec2(280.0, 200.0));
            self.primary_image_checkbox(ui, image.image_id);
            ui.weak("Choose one main image, then Review and save. With no selection, the first image is used.");
        }
        ui.horizontal_wrapped(|ui| {
            for image in card
                .images
                .iter()
                .filter(|image| Some(image.image_id) != primary.map(|image| image.image_id))
            {
                ui.vertical(|ui| {
                    ui.set_max_width(110.0);
                    self.preview(ui, config, image, egui::vec2(70.0, 60.0));
                    self.primary_image_checkbox(ui, image.image_id);
                });
            }
        });
        for kind in ["date", "year", "release_date", "duration", "actor"] {
            let values = card.values(kind);
            if !values.is_empty() {
                ui.label(format!("{}: {}", kind, values.join(", ")));
            }
        }
        for kind in ["description", "full_description"] {
            if let Some(a) = card.attributes.iter().find(|a| a.kind == kind) {
                ui.collapsing(
                    if kind == "description" {
                        "Description"
                    } else {
                        "Full description"
                    },
                    |ui| {
                        if ui.button("Copy text").clicked() {
                            ui.ctx().copy_text(a.value.clone());
                        }
                        ui.add(egui::Label::new(&a.value).wrap().selectable(true));
                    },
                );
            }
        }
        ui.separator();
        ui.add_enabled_ui(!self.busy && self.plan.is_none(), |ui| {
            egui::ComboBox::from_label("Primary studio")
                .selected_text(if self.edit.primary_studio.is_empty() {
                    "Unknown studio"
                } else {
                    &self.edit.primary_studio
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.edit.primary_studio,
                        String::new(),
                        "Unknown studio",
                    );
                    for studio in &self.edit.studios {
                        ui.selectable_value(&mut self.edit.primary_studio, studio.clone(), studio);
                    }
                });
            values_editor(
                ui,
                "Studios",
                &mut self.edit.studios,
                &mut self.inputs[0],
                &self.suggestions.studios,
            );
            values_editor(
                ui,
                "Tags",
                &mut self.edit.tags,
                &mut self.inputs[1],
                &self.suggestions.tags,
            );
            values_editor(
                ui,
                "Categories",
                &mut self.edit.categories,
                &mut self.inputs[2],
                &self.suggestions.categories,
            );
            if card.directory.is_none() {
                ui.checkbox(
                    &mut self.organize,
                    "Organize existing files into a studio folder",
                );
            }
            if self.edit.primary_studio != card.primary_studio {
                ui.label(
                    "Saving the new primary studio will move this card's files after confirmation.",
                );
            }
            if ui.button("Review and save").clicked() {
                let library = TorrentLibrary::new(
                    Database::new(&config.database_path),
                    config.effective_download_dir(),
                );
                let edit = self.edit.clone();
                let organize = self.organize;
                let id = card.page_id;
                let revision = card.revision;
                self.job(ui.ctx(), move |_, _| {
                    library
                        .prepare(id, revision, edit, organize)
                        .map(Outcome::Planned)
                        .map_err(|e| e.to_string())
                });
            }
        });
        if let Some(directory) = &card.directory {
            ui.label("Current folder:");
            ui.add(egui::Label::new(directory).wrap().selectable(true));
            if ui.button("Open folder").clicked() {
                super::app_window::open_folder(&config.effective_download_dir().join(directory));
            }
        }
        ui.collapsing("Files", |ui| {
            for attachment in &card.attachments {
                ui.horizontal_wrapped(|ui| {
                    ui.label(&attachment.filename);
                    if ui.button("Show file").clicked() {
                        super::app_window::open_in_explorer(
                            &config
                                .effective_download_dir()
                                .join(&attachment.local_file_path),
                        );
                    }
                    if attachment.source_url.starts_with("magnet:")
                        && ui.button("Copy magnet").clicked()
                    {
                        ui.ctx().copy_text(attachment.source_url.clone());
                    }
                });
            }
        });
    }

    fn confirm(&mut self, ctx: &egui::Context, config: &AppConfig) {
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let mut cancel = false;
        egui::Window::new("Confirm card changes")
            .collapsible(false)
            .resizable(true)
            .default_width(650.0)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Primary studio: {}",
                    if plan.edit.primary_studio.is_empty() {
                        "Unknown studio"
                    } else {
                        &plan.edit.primary_studio
                    }
                ));
                let image_number = self.selected.as_ref().and_then(|card| {
                    card.images
                        .iter()
                        .position(|image| Some(image.image_id) == plan.edit.primary_image_id)
                });
                ui.label(match image_number {
                    Some(index) => format!("Main image: image {}", index + 1),
                    None => "Main image: automatic (first image)".into(),
                });
                ui.label("From:");
                ui.add(
                    egui::Label::new(
                        plan.from_directory
                            .as_deref()
                            .unwrap_or("Existing individual file locations"),
                    )
                    .wrap()
                    .selectable(true),
                );
                ui.label("To:");
                ui.add(
                    egui::Label::new(
                        plan.to_directory
                            .as_deref()
                            .map(|p| {
                                config
                                    .effective_download_dir()
                                    .join(p)
                                    .display()
                                    .to_string()
                            })
                            .unwrap_or_else(|| "Keep current locations".into()),
                    )
                    .wrap()
                    .selectable(true),
                );
                ui.label(format!(
                    "{} files to move · {} bytes",
                    plan.files.len(),
                    plan.files.iter().map(|f| f.size).sum::<u64>()
                ));
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .show(ui, |ui| {
                        for file in &plan.files {
                            ui.label(format!("{} → {}", file.from, file.to));
                        }
                    });
                ui.add_enabled_ui(!self.busy, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button(if plan.files.is_empty() {
                                "Confirm and save"
                            } else {
                                "Confirm and move"
                            })
                            .clicked()
                        {
                            let library = TorrentLibrary::new(
                                Database::new(&config.database_path),
                                config.effective_download_dir(),
                            );
                            let plan = plan.clone();
                            self.job(ctx, move |tx, ctx| {
                                library
                                    .apply(plan, |p| {
                                        let _ = tx.try_send(Message::Progress(p));
                                        ctx.request_repaint();
                                    })
                                    .map(Outcome::Saved)
                                    .map_err(|e| e.to_string())
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        if cancel {
            self.plan = None;
        }
    }

    fn install_previews(&mut self, ctx: &egui::Context) {
        let Some(loader) = &mut self.loader else {
            return;
        };
        let ready = loader.drain_ready_up_to(4);
        if ready.len() == 4 {
            ctx.request_repaint();
        }
        for preview in ready {
            let texture = ctx.load_texture(&preview.key, preview.image, Default::default());
            self.textures
                .insert(preview.key, texture, preview.estimated_bytes);
        }
    }

    fn primary_image_checkbox(&mut self, ui: &mut egui::Ui, image_id: i64) {
        ui.push_id(("primary_image", image_id), |ui| {
            let mut selected = self.edit.primary_image_id == Some(image_id);
            if ui.add_enabled(
                !self.busy && self.plan.is_none(),
                egui::Checkbox::new(&mut selected, "Main image"),
            ).on_hover_text("Choose this image as the card preview. Save with Review and save; clear the choice to use the first image automatically.").changed() {
                self.edit.primary_image_id = selected.then_some(image_id);
            }
        });
    }

    fn preview(
        &mut self,
        ui: &mut egui::Ui,
        config: &AppConfig,
        image: &TorrentImage,
        size: egui::Vec2,
    ) {
        let key = format!(
            "torrent:{}:{}",
            image.image_id,
            image.local_file_path.as_deref().unwrap_or("")
        );
        if let Some(texture) = self.textures.get(&key) {
            if ui
                .add(
                    egui::Image::new(texture)
                        .max_size(size)
                        .sense(egui::Sense::click()),
                )
                .clicked()
            {
                self.image_popup = Some(image.clone());
            }
        } else {
            self.loader.get_or_insert_with(PreviewLoader::new).request(
                PreviewSource {
                    key,
                    image_id: image.image_id,
                    base_dir: config.effective_download_dir(),
                    thumb_dir: config.thumb_dir(),
                    local_file_path: image.local_file_path.clone(),
                    thumbnail_path: image.thumbnail_path.clone(),
                    mode: ImageLoadMode::Preview,
                    max_side: None,
                },
                ui.ctx(),
            );
            ui.label("Image preview…");
        }
    }
    fn preview_window(&mut self, ctx: &egui::Context, config: &AppConfig) {
        let Some(image) = self.image_popup.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new("Torrent image")
            .open(&mut open)
            .show(ctx, |ui| {
                self.preview(ui, config, &image, egui::vec2(800.0, 700.0));
                self.primary_image_checkbox(ui, image.image_id);
            });
        if !open {
            self.image_popup = None;
        }
    }
}

fn values_editor(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut Vec<String>,
    input: &mut String,
    suggestions: &[String],
) {
    ui.push_id(label, |ui| {
        ui.strong(label);
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (i, value) in values.iter().enumerate() {
                if ui.button(format!("{value} ×")).clicked() {
                    remove = Some(i);
                }
            }
        });
        if let Some(i) = remove {
            values.remove(i);
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(input)
                    .hint_text(format!("Add {}", label.to_lowercase()))
                    .char_limit(256),
            );
            if ui
                .add_enabled(!input.trim().is_empty(), egui::Button::new("Add"))
                .clicked()
            {
                if !values.iter().any(|s| s.eq_ignore_ascii_case(input.trim())) {
                    values.push(input.trim().into());
                }
                input.clear();
            }
        });
        if !input.is_empty() {
            let query = input.to_lowercase();
            ui.horizontal_wrapped(|ui| {
                for s in suggestions
                    .iter()
                    .filter(|s| s.to_lowercase().contains(&query) && !values.contains(s))
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                {
                    if ui.small_button(&s).clicked() {
                        values.push(s);
                        input.clear();
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MetadataAttribute, ScrapedItem};

    fn fixture() -> (
        tempfile::TempDir,
        AppConfig,
        Database,
        TorrentLibraryUi,
        egui::Context,
    ) {
        let root = tempfile::tempdir().unwrap();
        let config = AppConfig {
            database_path: root.path().join("library.sqlite3").display().to_string(),
            download_folder: root.path().join("files").display().to_string(),
            clipboard_listener_enabled: false,
            ..Default::default()
        };
        let db = Database::new(&config.database_path);
        db.initialize().unwrap();
        std::fs::create_dir_all(config.effective_download_dir()).unwrap();
        let page = ScrapedItem {
            site_key: "fixture".into(),
            page_url: "https://example.test/topic?t=1".into(),
            title: "Full torrent heading [2025]".into(),
            raw_html: String::new(),
            images: vec![],
            attributes: vec![MetadataAttribute {
                kind: "magnet".into(),
                label: "Magnet".into(),
                value: "magnet:?xt=urn:btih:0123456789012345678901234567890123456789".into(),
                source_url: None,
                is_female: None,
            }],
        };
        let id = db.upsert_scraped_item(&page).unwrap().page_id;
        let card = db.torrent_card(id).unwrap();
        let path =
            std::path::Path::new(card.directory.as_ref().unwrap()).join("release.magnet.txt");
        let absolute = config.effective_download_dir().join(&path);
        std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        std::fs::write(&absolute, b"fixture").unwrap();
        db.save_page_attachment(id, "fixture", "Magnet", &path.to_string_lossy())
            .unwrap();
        let card = db.torrent_card(id).unwrap();
        let mut editor = TorrentLibraryUi {
            loaded: true,
            ..Default::default()
        };
        editor.page = db.search_torrents(&TorrentQuery::default()).unwrap();
        editor.select(card);
        (root, config, db, editor, egui::Context::default())
    }
    fn render(
        editor: &mut TorrentLibraryUi,
        config: &AppConfig,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 1000.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| editor.render(ui, config));
            },
        )
    }
    fn texts(output: &egui::FullOutput) -> Vec<(String, egui::Pos2)> {
        fn visit(shape: &egui::Shape, items: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Text(t) => {
                    items.push((t.galley.job.text.clone(), t.pos + t.galley.size() * 0.5))
                }
                egui::Shape::Vec(shapes) => {
                    for s in shapes {
                        visit(s, items)
                    }
                }
                _ => (),
            }
        }
        let mut items = vec![];
        for s in &output.shapes {
            visit(&s.shape, &mut items);
        }
        items
    }
    fn click(editor: &mut TorrentLibraryUi, config: &AppConfig, ctx: &egui::Context, label: &str) {
        render(editor, config, ctx, vec![]);
        let output = render(editor, config, ctx, vec![]);
        let position = texts(&output)
            .into_iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("missing {label}"))
            .1;
        click_at(editor, config, ctx, position);
    }
    fn click_at(
        editor: &mut TorrentLibraryUi,
        config: &AppConfig,
        ctx: &egui::Context,
        position: egui::Pos2,
    ) {
        for pressed in [true, false] {
            render(
                editor,
                config,
                ctx,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
    }

    fn click_main_image(
        editor: &mut TorrentLibraryUi,
        config: &AppConfig,
        ctx: &egui::Context,
        index: usize,
    ) {
        render(editor, config, ctx, vec![]);
        let output = render(editor, config, ctx, vec![]);
        let position = texts(&output)
            .into_iter()
            .filter(|(text, _)| text == "Main image")
            .nth(index)
            .expect("main image checkbox")
            .1;
        click_at(editor, config, ctx, position);
    }

    fn finish(editor: &mut TorrentLibraryUi) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut changed = false;
        while editor.busy {
            changed |= editor.poll();
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        changed
    }

    #[test]
    fn card_editor_renders_primary_studio_tags_categories_and_full_heading() {
        let (_root, config, _db, mut editor, ctx) = fixture();
        let _ = render(&mut editor, &config, &ctx, vec![]);
        let output = render(&mut editor, &config, &ctx, vec![]);
        let labels = texts(&output);
        for expected in [
            "Full torrent heading [2025]",
            "Primary studio",
            "Studios",
            "Tags",
            "Categories",
            "Review and save",
            "Year",
        ] {
            assert!(
                labels.iter().any(|(text, _)| text == expected),
                "missing {expected}"
            );
        }
        editor.inputs[1] = "Custom tag".into();
        editor.inputs[2] = "My category".into();
        // Local draft changes never write to the database before confirmation.
        assert!(editor.selected.as_ref().unwrap().values("tag").is_empty());
    }

    #[test]
    fn main_image_checkboxes_are_exclusive_and_save_only_after_confirmation() {
        let (_root, config, db, mut editor, ctx) = fixture();
        let card = editor.selected.clone().unwrap();
        let page = ScrapedItem {
            site_key: card.site_key.clone(),
            page_url: card.page_url.clone(),
            title: card.title.clone(),
            raw_html: String::new(),
            attributes: card.attributes.clone(),
            images: (0..2)
                .map(|index| crate::models::ScrapedImage {
                    source_url: format!("https://example.test/image-{index}.png"),
                    preview_url: None,
                    alt_text: None,
                    width: None,
                    height: None,
                    sort_order: index,
                    local_file_path: None,
                })
                .collect(),
        };
        let persisted = db.upsert_scraped_item(&page).unwrap();
        for (index, image) in page.images.iter().enumerate() {
            let id = persisted.image_ids_by_url[&image.source_url];
            let path = std::path::Path::new(card.directory.as_ref().unwrap())
                .join(format!("images/{index}.png"));
            let absolute = config.effective_download_dir().join(&path);
            std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            image::DynamicImage::new_rgb8(2, 2).save(&absolute).unwrap();
            db.update_image_artifacts(card.page_id, id, &path.to_string_lossy(), None)
                .unwrap();
            let key = format!("torrent:{id}:{}", path.to_string_lossy());
            let texture = ctx.load_texture(
                &key,
                egui::ColorImage::from_rgba_unmultiplied([2, 2], &[255; 16]),
                Default::default(),
            );
            editor.textures.insert(key, texture, 16);
        }
        let card = db.torrent_card(card.page_id).unwrap();
        let first = card.images[0].image_id;
        let second = card.images[1].image_id;
        editor.page = db.search_torrents(&TorrentQuery::default()).unwrap();
        editor.select(card.clone());
        click_main_image(&mut editor, &config, &ctx, 1);
        assert_eq!(editor.edit.primary_image_id, Some(second));
        assert_eq!(
            db.torrent_card(card.page_id).unwrap().primary_image_id,
            None
        );
        click_main_image(&mut editor, &config, &ctx, 1);
        assert_eq!(editor.edit.primary_image_id, Some(first));
        click_main_image(&mut editor, &config, &ctx, 0);
        assert_eq!(editor.edit.primary_image_id, None);
        click_main_image(&mut editor, &config, &ctx, 1);
        assert_eq!(editor.edit.primary_image_id, Some(second));
        click(&mut editor, &config, &ctx, "Review and save");
        finish(&mut editor);
        assert_eq!(
            editor.plan.as_ref().unwrap().edit.primary_image_id,
            Some(second)
        );
        assert!(editor.plan.as_ref().unwrap().files.is_empty());
        click(&mut editor, &config, &ctx, "Cancel");
        assert_eq!(
            db.torrent_card(card.page_id).unwrap().primary_image_id,
            None
        );
        assert_eq!(editor.edit.primary_image_id, Some(second));
        click(&mut editor, &config, &ctx, "Review and save");
        finish(&mut editor);
        click(&mut editor, &config, &ctx, "Confirm and save");
        assert!(finish(&mut editor));
        assert_eq!(
            db.torrent_card(card.page_id).unwrap().primary_image_id,
            Some(second)
        );
        assert_eq!(
            editor
                .selected
                .as_ref()
                .unwrap()
                .primary_image()
                .unwrap()
                .image_id,
            second
        );
    }

    #[test]
    fn cancelling_confirmation_keeps_original_files_then_confirming_moves_them() {
        let (_root, config, db, mut editor, ctx) = fixture();
        let old = editor.selected.clone().unwrap();
        editor.edit.primary_studio = "Selected studio".into();
        editor.edit.tags = vec!["Manual tag".into()];
        editor.edit.categories = vec!["Collection".into()];
        click(&mut editor, &config, &ctx, "Review and save");
        finish(&mut editor);
        assert!(editor.plan.is_some());
        click(&mut editor, &config, &ctx, "Cancel");
        assert!(editor.plan.is_none());
        assert!(config
            .effective_download_dir()
            .join(&old.attachments[0].local_file_path)
            .is_file());
        assert!(db
            .torrent_card(old.page_id)
            .unwrap()
            .primary_studio
            .is_empty());
        click(&mut editor, &config, &ctx, "Review and save");
        finish(&mut editor);
        click(&mut editor, &config, &ctx, "Confirm and move");
        assert!(editor.busy);
        assert!(finish(&mut editor));
        let saved = db.torrent_card(old.page_id).unwrap();
        assert_eq!(saved.primary_studio, "Selected studio");
        assert_eq!(saved.values("tag"), ["Manual tag"]);
        assert_eq!(saved.values("category"), ["Collection"]);
        assert!(!config
            .effective_download_dir()
            .join(&old.attachments[0].local_file_path)
            .exists());
        assert!(config
            .effective_download_dir()
            .join(&saved.attachments[0].local_file_path)
            .is_file());
    }

    #[test]
    fn a_failed_initial_query_waits_for_an_explicit_retry() {
        let (_root, mut config, _db, mut editor, ctx) = fixture();
        config.database_path = std::env::temp_dir().display().to_string();
        editor.loaded = false;
        render(&mut editor, &config, &ctx, vec![]);
        finish(&mut editor);
        assert!(editor.error.is_some());
        render(&mut editor, &config, &ctx, vec![]);
        assert!(!editor.busy);
    }
}

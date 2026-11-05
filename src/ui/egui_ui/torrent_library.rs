use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;

use super::background_executor::spawn_background;
use super::creator_filter::CreatorFilter;
use super::preview_loader::{ImageLoadMode, PreviewLoader, PreviewSource};
use super::texture_cache::SizedLruCache;
use crate::app::AppConfig;
use crate::db::Database;
use crate::models::{TorrentCard, TorrentEdit, TorrentImage, TorrentQuery, TorrentSummaryPage};
use crate::services::torrent_library::{MoveProgress, TorrentLibrary, TorrentMovePlan};
use eframe::egui;

#[derive(Default)]
struct Filters {
    text: String,
    site: String,
    studio: String,
    creators: CreatorFilter,
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
            creators: self.creators.values(),
            tags: values(&self.tags),
            categories: values(&self.categories),
            year: self.year.trim().into(),
            offset: self.offset,
        }
    }
}

#[derive(Default, Clone)]
struct Suggestions {
    studios: Vec<String>,
    tags: Vec<String>,
    categories: Vec<String>,
}
enum Outcome {
    Recovered,
    Loaded(TorrentSummaryPage, Suggestions, Option<TorrentCard>),
    Selected(TorrentCard),
    Planned(TorrentMovePlan),
    Saved(TorrentCard),
}
enum Message {
    Progress(MoveProgress),
    Finished(Result<Outcome, String>),
}

pub(super) struct TorrentLibraryUi {
    handoff: Option<Vec<i64>>,
    checked_cards: std::collections::BTreeSet<i64>,
    pending_command: Option<crate::ui::UiCommand>,
    filters: Filters,
    page: TorrentSummaryPage,
    selected: Option<Arc<TorrentCard>>,
    edit: TorrentEdit,
    folder_name: String,
    inputs: [String; 3],
    suggestions: Suggestions,
    refresh_suggestions: bool,
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
            handoff: None,
            checked_cards: Default::default(),
            pending_command: None,
            filters: Filters::default(),
            page: TorrentSummaryPage {
                cards: vec![],
                total: 0,
            },
            selected: None,
            edit: TorrentEdit::default(),
            folder_name: String::new(),
            inputs: Default::default(),
            suggestions: Suggestions::default(),
            refresh_suggestions: true,
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
    pub(super) fn take_handoff(&mut self) -> Option<Vec<i64>> {
        self.handoff.take()
    }
    pub(super) fn take_command(&mut self) -> Option<crate::ui::UiCommand> {
        self.pending_command.take()
    }

    pub(super) fn set_creator_suggestions(
        &mut self,
        database: &str,
        query: &str,
        values: Vec<String>,
    ) {
        self.filters
            .creators
            .set_suggestions(database, query, values);
    }

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
                        Ok(Outcome::Loaded(page, suggestions, card)) => {
                            self.page = page;
                            self.suggestions = suggestions;
                            self.refresh_suggestions = false;
                            self.loaded = true;
                            if let Some(card) = card {
                                self.select(card);
                            } else {
                                self.selected = None;
                            }
                        }
                        Ok(Outcome::Selected(card)) => {
                            self.select(card);
                            self.status = None;
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
                            self.refresh_suggestions = true;
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

    fn select(&mut self, card: impl Into<Arc<TorrentCard>>) {
        let card = card.into();
        self.edit = TorrentEdit::from(card.as_ref());
        self.folder_name = card.directory_name().unwrap_or("").to_owned();
        self.selected = Some(card);
        self.inputs = Default::default();
        self.organize = false;
        self.plan = None;
        self.image_popup = None;
    }

    fn draft_edit(&self, card: &TorrentCard) -> TorrentEdit {
        let mut edit = self.edit.clone();
        if self.folder_name != card.directory_name().unwrap_or("") {
            edit.folder_name = Some(self.folder_name.clone());
        }
        edit
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
        if let Err(error) = spawn_background(move || {
            let result = work(tx.clone(), ctx.clone());
            let _ = tx.send(Message::Finished(result));
            ctx.request_repaint();
        }) {
            self.busy = false;
            self.receiver = None;
            if self.recovering {
                self.startup_error = Some(error.clone());
            }
            self.recovering = false;
            self.error = Some(error);
            self.status = None;
        }
    }

    fn load(&mut self, ctx: &egui::Context, config: &AppConfig) {
        self.loaded = true;
        let database = Database::new(&config.database_path);
        let root = config.effective_download_dir();
        let query = self.filters.query();
        let selected = self.selected.as_ref().map(|card| card.page_id);
        let cached_suggestions = (!self.refresh_suggestions).then(|| self.suggestions.clone());
        self.reload = false;
        self.job(ctx, move |_, _| {
            let run = || -> crate::AppResult<Outcome> {
                database.initialize()?;
                TorrentLibrary::new(database.clone(), root).recover()?;
                let page = database.search_torrent_summaries(&query)?;
                let card = page
                    .cards
                    .iter()
                    .find(|card| Some(card.page_id) == selected)
                    .or_else(|| page.cards.first())
                    .map(|card| database.torrent_card(card.page_id))
                    .transpose()?;
                let suggestions = if let Some(cached) = cached_suggestions {
                    cached
                } else {
                    Suggestions {
                        studios: database.suggest_attribute_values("studio", "", 500)?,
                        tags: database.suggest_attribute_values("tag", "", 500)?,
                        categories: database.suggest_attribute_values("category", "", 500)?,
                    }
                };
                Ok(Outcome::Loaded(page, suggestions, card))
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
                    ui.label("Director / Author");
                    if let Some(command) = self.filters.creators.input(
                        ui,
                        [180.0, ui.spacing().interact_size.y],
                        &config.database_path,
                    ) {
                        self.pending_command = Some(command);
                    }
                    ui.end_row();
                    if self.filters.creators.has_suggestions() {
                        ui.label("");
                        self.filters.creators.render_suggestions(ui);
                        ui.end_row();
                    }
                });
            if ui.button("Find torrents / Refresh").clicked() {
                self.filters.offset = 0;
                self.refresh_suggestions = true;
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
        ui.horizontal(|ui| {
            ui.label(format!("{} selected", self.checked_cards.len()));
            if ui
                .add_enabled(
                    !self.busy
                        && config.peerlume.enabled
                        && !self.checked_cards.is_empty()
                        && self.checked_cards.len() <= 100,
                    egui::Button::new("Download selected in PeerLume"),
                )
                .clicked()
            {
                self.handoff = Some(self.checked_cards.iter().copied().collect());
            }
            if ui.button("Clear selection").clicked() {
                self.checked_cards.clear();
            }
        });
        let height = ui.available_height();
        ui.columns(2, |columns| {
            egui::ScrollArea::vertical()
                .id_salt("torrent_cards")
                .max_height(height)
                .show(&mut columns[0], |ui| {
                    let mut choose = None;
                    for card in &self.page.cards {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            let mut checked = self.checked_cards.contains(&card.page_id);
                            if ui.checkbox(&mut checked, "Select for PeerLume").changed() {
                                if checked {
                                    self.checked_cards.insert(card.page_id);
                                } else {
                                    self.checked_cards.remove(&card.page_id);
                                }
                            }
                            ui.set_width(ui.available_width());
                            if ui
                                .add_enabled(
                                    !self.busy && self.plan.is_none(),
                                    egui::Button::new(&card.content_title).wrap().selected(
                                        self.selected
                                            .as_ref()
                                            .is_some_and(|c| c.page_id == card.page_id),
                                    ),
                                )
                                .clicked()
                            {
                                choose = Some(card.page_id);
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
                                card.attachment_count, card.image_count
                            ));
                        });
                    }
                    if let Some(id) = choose {
                        let database = Database::new(&config.database_path);
                        self.job(ui.ctx(), move |_, _| {
                            database
                                .torrent_card(id)
                                .map(Outcome::Selected)
                                .map_err(|e| e.to_string())
                        });
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
        ui.add(
            egui::Label::new(card.content_title())
                .wrap()
                .selectable(true),
        );
        if card.content_title() != card.title {
            ui.collapsing("Full page title", |ui| {
                ui.add(egui::Label::new(&card.title).wrap().selectable(true));
            });
        }
        ui.hyperlink_to("Open source page", &card.page_url);
        if ui
            .add_enabled(
                !self.busy && config.peerlume.enabled,
                egui::Button::new("Download in PeerLume"),
            )
            .clicked()
        {
            self.handoff = Some(vec![card.page_id]);
        }
        if !config.peerlume.enabled {
            ui.weak("Enable the PeerLume integration in Settings to hand off torrents.");
        }
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
        let columns = (ui.available_width() / 118.0).floor().max(1.0) as usize;
        let primary_index = primary.and_then(|image| {
            card.images
                .iter()
                .position(|entry| entry.image_id == image.image_id)
        });
        let count = card
            .images
            .len()
            .saturating_sub(usize::from(primary_index.is_some()));
        egui::ScrollArea::vertical()
            .id_salt(("torrent_thumbnails", card.page_id))
            .max_height(280.0)
            .show_rows(ui, 92.0, count.div_ceil(columns), |ui, rows| {
                for row in rows {
                    ui.horizontal(|ui| {
                        for slot in row * columns..((row + 1) * columns).min(count) {
                            let index = slot
                                + usize::from(primary_index.is_some_and(|index| slot >= index));
                            let image = &card.images[index];
                            ui.allocate_ui(egui::vec2(110.0, 92.0), |ui| {
                                self.preview(ui, config, image, egui::vec2(70.0, 60.0));
                                self.primary_image_checkbox(ui, image.image_id);
                            });
                        }
                    });
                }
            });
        for kind in [
            "date",
            "year",
            "release_date",
            "duration",
            "actor",
            "director",
            "author",
            "photographer",
            "creator",
        ] {
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
            ui.label("Folder name");
            ui.add(
                egui::TextEdit::singleline(&mut self.folder_name)
                    .desired_width(f32::INFINITY)
                    .hint_text("Generated from title and year")
                    .char_limit(120),
            );
            ui.weak(
                "Review and save previews the new folder. Confirm and move transfers all files.",
            );
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
                let edit = self.draft_edit(&card);
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

    #[test]
    fn torrent_filters_reuse_creator_names_across_pagination() {
        let mut filters = Filters::default();
        filters.creators.text = "Alice Creator, Bob Author".into();
        filters.offset = 40;
        filters.site = "example-source".into();
        let query = filters.query();
        assert_eq!(query.creators, ["Alice Creator", "Bob Author"]);
        assert_eq!(query.offset, 40);
        assert_eq!(query.site_key, "example-source");
    }

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
        editor.page = db
            .search_torrent_summaries(&TorrentQuery::default())
            .unwrap();
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
            "Folder name",
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
        editor.page = db
            .search_torrent_summaries(&TorrentQuery::default())
            .unwrap();
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
    fn selecting_a_summary_loads_only_the_chosen_full_card_in_the_background() {
        let (_root, config, db, mut editor, ctx) = fixture();
        let previous = Arc::clone(editor.selected.as_ref().unwrap());
        let page = ScrapedItem {
            site_key: "example".into(),
            page_url: "https://example.test/second".into(),
            title: "Another card".into(),
            raw_html: String::new(),
            images: vec![],
            attributes: previous.attributes.clone(),
        };
        let id = db.upsert_scraped_item(&page).unwrap().page_id;
        editor.page = db
            .search_torrent_summaries(&TorrentQuery::default())
            .unwrap();
        click(&mut editor, &config, &ctx, "Another card");
        assert!(editor.busy);
        assert!(Arc::ptr_eq(&previous, editor.selected.as_ref().unwrap()));
        finish(&mut editor);
        assert_eq!(editor.selected.as_ref().unwrap().page_id, id);
        assert_eq!(editor.selected.as_ref().unwrap().title, "Another card");
    }

    #[test]
    fn large_cards_share_their_snapshot_and_only_render_visible_thumbnail_rows() {
        let (_root, config, db, mut editor, ctx) = fixture();
        let card = editor.selected.as_ref().unwrap();
        let mut page = ScrapedItem {
            site_key: card.site_key.clone(),
            page_url: card.page_url.clone(),
            title: card.title.clone(),
            raw_html: String::new(),
            attributes: card.attributes.clone(),
            images: vec![],
        };
        page.images = (0..200)
            .map(|id| crate::models::ScrapedImage {
                source_url: format!("https://example.test/large/{id}.jpg"),
                preview_url: None,
                alt_text: None,
                width: None,
                height: None,
                sort_order: id,
                local_file_path: None,
            })
            .collect();
        let id = db.upsert_scraped_item(&page).unwrap().page_id;
        editor.page = db
            .search_torrent_summaries(&TorrentQuery::default())
            .unwrap();
        editor.select(db.torrent_card(id).unwrap());
        let snapshot = Arc::clone(editor.selected.as_ref().unwrap());
        let _ = render(&mut editor, &config, &ctx, vec![]);
        let output = render(&mut editor, &config, &ctx, vec![]);
        let count = texts(&output)
            .iter()
            .filter(|(text, _)| text.as_str() == "Main image")
            .count();
        assert!(
            count > 0 && count < 30,
            "rendered {count} checkboxes for 200 images"
        );
        assert!(Arc::ptr_eq(&snapshot, editor.selected.as_ref().unwrap()));
        assert_eq!(snapshot.images.len(), 200);
    }

    #[test]
    fn folder_rename_is_drafted_cancelled_and_confirmed_through_the_ui() {
        let (_root, config, db, mut editor, ctx) = fixture();
        let old = editor.selected.clone().unwrap();
        assert_eq!(editor.draft_edit(&old).folder_name, None);
        editor.folder_name = "Renamed release (2025)".into();
        click(&mut editor, &config, &ctx, "Review and save");
        finish(&mut editor);
        let plan = editor.plan.as_ref().unwrap();
        let destination = config
            .effective_download_dir()
            .join(plan.to_directory.as_ref().unwrap());
        assert!(!destination.exists());
        assert_eq!(
            db.torrent_card(old.page_id).unwrap().directory,
            old.directory
        );
        click(&mut editor, &config, &ctx, "Cancel");
        assert!(editor.plan.is_none());
        assert_eq!(editor.folder_name, "Renamed release (2025)");
        assert_eq!(db.torrent_card(old.page_id).unwrap().folder_name, None);
        click(&mut editor, &config, &ctx, "Review and save");
        finish(&mut editor);
        click(&mut editor, &config, &ctx, "Confirm and move");
        assert!(finish(&mut editor));
        let saved = db.torrent_card(old.page_id).unwrap();
        assert_eq!(saved.directory_name(), Some("Renamed release (2025)"));
        assert_eq!(saved.folder_name.as_deref(), Some("Renamed release (2025)"));
        assert_eq!(saved.primary_studio, old.primary_studio);
        assert!(destination.is_dir());
        assert!(!config
            .effective_download_dir()
            .join(old.directory.as_ref().unwrap())
            .exists());
        assert!(config
            .effective_download_dir()
            .join(&saved.attachments[0].local_file_path)
            .is_file());
        assert_eq!(editor.folder_name, "Renamed release (2025)");
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

    #[test]
    fn peerlume_card_and_batch_buttons_only_request_review_and_respect_the_enable_switch() {
        let (_root, mut config, _db, mut editor, ctx) = fixture();
        let id = editor.selected.as_ref().unwrap().page_id;
        click(&mut editor, &config, &ctx, "Download in PeerLume");
        assert!(editor.take_handoff().is_none());
        config.peerlume.enabled = true;
        click(&mut editor, &config, &ctx, "Download in PeerLume");
        assert_eq!(editor.take_handoff(), Some(vec![id]));
        click(&mut editor, &config, &ctx, "Select for PeerLume");
        click(&mut editor, &config, &ctx, "Download selected in PeerLume");
        assert_eq!(editor.take_handoff(), Some(vec![id]));
        click(&mut editor, &config, &ctx, "Clear selection");
        assert!(editor.checked_cards.is_empty());
        assert!(editor.receiver.is_none());
    }
}

use archive_sift::{
    db::Database,
    fs::torrent::{safe_component, torrent_directory},
    models::{
        MetadataAttribute, ScrapedImage, ScrapedItem, SearchQuery, TorrentEdit, TorrentQuery,
    },
    services::torrent_library::TorrentLibrary,
};
use std::path::{Path, PathBuf};

fn attribute(kind: &str, value: &str) -> MetadataAttribute {
    MetadataAttribute {
        kind: kind.into(),
        label: kind.into(),
        value: value.into(),
        source_url: None,
        is_female: None,
    }
}
fn page() -> ScrapedItem {
    ScrapedItem {
        site_key: "example-source".into(),
        page_url: "https://example.test/topic?t=42".into(),
        title: "Complete heading [2025, 1080p]".into(),
        raw_html: String::new(),
        images: vec![ScrapedImage {
            source_url: "https://example.test/image.png".into(),
            preview_url: None,
            alt_text: None,
            width: None,
            height: None,
            sort_order: 0,
            local_file_path: None,
        }],
        attributes: vec![
            attribute("studio", "Studio First"),
            attribute("studio", "Studio Second"),
            attribute("actor", "Actor One"),
            attribute("actor", "Actor Two"),
            attribute("actor", "Actor Three"),
            attribute("content_title", "Example release"),
            attribute("date", "2025-06-23"),
            attribute("tag", "Original"),
            attribute("tag", "Remove me"),
            attribute(
                "magnet",
                "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
            ),
        ],
    }
}

struct Library {
    root: tempfile::TempDir,
    db: Database,
    base: PathBuf,
    page: ScrapedItem,
    id: i64,
}
impl Library {
    fn new(legacy: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("downloads");
        std::fs::create_dir_all(&base).unwrap();
        let db = Database::new(root.path().join("archive.sqlite3"));
        db.initialize().unwrap();
        let page = page();
        let persisted = db.upsert_scraped_item(&page).unwrap();
        let id = persisted.page_id;
        let card = db.torrent_card(id).unwrap();
        let directory = if legacy {
            PathBuf::from("old/source/actor/release")
        } else {
            PathBuf::from(card.directory.unwrap())
        };
        let image = directory.join("images/001.png");
        let torrent = directory.join("release.torrent");
        let magnet = directory.join("release.magnet.txt");
        for (path, bytes) in [
            (&image, b"image bytes".as_slice()),
            (&torrent, b"torrent bytes"),
            (&magnet, b"magnet bytes"),
        ] {
            let absolute = base.join(path);
            std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            std::fs::write(absolute, bytes).unwrap();
        }
        let image_id = persisted.image_ids_by_url[&page.images[0].source_url];
        db.update_image_artifacts(id, image_id, &image.to_string_lossy(), Some("0/1.jpg"))
            .unwrap();
        db.save_page_attachment(
            id,
            "https://example.test/download/42",
            "release.torrent",
            &torrent.to_string_lossy(),
        )
        .unwrap();
        db.save_page_attachment(
            id,
            "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
            "Magnet",
            &magnet.to_string_lossy(),
        )
        .unwrap();
        if legacy {
            rusqlite::Connection::open(root.path().join("archive.sqlite3"))
                .unwrap()
                .execute(
                    "UPDATE torrent_cards SET directory=NULL WHERE page_id=?1",
                    [id],
                )
                .unwrap();
        }
        Self {
            root,
            db,
            base,
            page,
            id,
        }
    }
    fn service(&self) -> TorrentLibrary {
        TorrentLibrary::new(self.db.clone(), &self.base)
    }
    fn add_second_image(&mut self) -> i64 {
        let mut image = self.page.images[0].clone();
        image.source_url = "https://example.test/second.png".into();
        image.sort_order = 1;
        self.page.images.push(image);
        let persisted = self.db.upsert_scraped_item(&self.page).unwrap();
        let id = persisted.image_ids_by_url["https://example.test/second.png"];
        let path = PathBuf::from(self.db.torrent_card(self.id).unwrap().directory.unwrap())
            .join("images/002.png");
        std::fs::write(self.base.join(&path), b"second image bytes").unwrap();
        self.db
            .update_image_artifacts(self.id, id, &path.to_string_lossy(), None)
            .unwrap();
        id
    }
    fn plan(&self, studio: &str) -> archive_sift::services::torrent_library::TorrentMovePlan {
        let card = self.db.torrent_card(self.id).unwrap();
        let mut edit = TorrentEdit::from(&card);
        edit.primary_studio = studio.into();
        self.service()
            .prepare(self.id, card.revision, edit, false)
            .unwrap()
    }
}

#[test]
fn changing_signed_urls_uses_the_stable_preview_without_merging_explicit_variants() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::new(root.path().join("archive.sqlite3"));
    db.initialize().unwrap();
    let mut page = page();
    let first = "https://cdn.example.test/media/photo.jpg?signature=first&expires=1";
    let second = "https://cdn.example.test/media/photo.jpg?signature=second&expires=2";
    page.images[0].source_url = first.into();
    page.images[0].preview_url = Some("https://cdn.example.test/thumb/photo.jpg".into());
    let saved = db.upsert_scraped_item(&page).unwrap();
    let card = db.torrent_card(saved.page_id).unwrap();
    let library = TorrentLibrary::new(db.clone(), root.path().join("downloads"));
    let edit = TorrentEdit {
        primary_image_id: Some(card.images[0].image_id),
        ..TorrentEdit::from(&card)
    };
    let plan = library
        .prepare(card.page_id, card.revision, edit, false)
        .unwrap();
    library.apply(plan, |_| {}).unwrap();
    page.images[0].source_url = second.into();
    db.upsert_scraped_item(&page).unwrap();
    let updated = db.torrent_card(saved.page_id).unwrap();
    assert_eq!(updated.images.len(), 1);
    assert_eq!(updated.images[0].source_url, second);
    assert_eq!(updated.primary_image_id, Some(updated.images[0].image_id));
    let mut previous = page.images[0].clone();
    previous.source_url = first.into();
    page.images.push(previous);
    db.upsert_scraped_item(&page).unwrap();
    assert_eq!(db.torrent_card(saved.page_id).unwrap().images.len(), 2);
    page.images.truncate(1);
    page.images[0].source_url = "https://cdn.example.test/other/photo.jpg?signature=third".into();
    db.upsert_scraped_item(&page).unwrap();
    assert_eq!(db.torrent_card(saved.page_id).unwrap().images.len(), 3);
}

#[test]
fn original_replaces_its_preview_without_losing_cover_favorites_tags_or_other_pages() {
    let library = Library::new(false);
    let card = library.db.torrent_card(library.id).unwrap();
    let old_id = card.images[0].image_id;
    let old_path = card.images[0].local_file_path.clone().unwrap();
    library.db.set_favorite(old_id, true).unwrap();
    library
        .db
        .add_image_attributes(&[old_id], "tag", "Tag", &["Manual image tag".into()])
        .unwrap();
    let edit = TorrentEdit {
        primary_image_id: Some(old_id),
        ..TorrentEdit::from(&card)
    };
    let plan = library
        .service()
        .prepare(library.id, card.revision, edit, false)
        .unwrap();
    library.service().apply(plan, |_| {}).unwrap();
    let mut other = library.page.clone();
    other.page_url = "https://example.test/topic?t=99".into();
    let other_id = library.db.upsert_scraped_item(&other).unwrap().page_id;
    library
        .db
        .update_image_artifacts(other_id, old_id, &old_path, None)
        .unwrap();
    let mut original = library.page.clone();
    original.images[0].preview_url = Some(original.images[0].source_url.clone());
    original.images[0].source_url = "https://example.test/original.png".into();
    let saved = library.db.upsert_scraped_item(&original).unwrap();
    let original_id = saved.image_ids_by_url["https://example.test/original.png"];
    let card = library.db.torrent_card(library.id).unwrap();
    assert_eq!(card.images.len(), 1);
    assert_eq!(card.images[0].image_id, original_id);
    assert_eq!(card.primary_image_id, Some(original_id));
    assert!(
        card.images[0].local_file_path.is_none(),
        "the original must be downloaded instead of reusing a thumbnail"
    );
    let favorites = library
        .db
        .search_images(&SearchQuery {
            page_id: Some(library.id),
            favorites_only: true,
            tags: vec!["Manual image tag".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(favorites.len(), 1);
    assert_eq!(favorites[0].image_id, original_id);
    let other = library.db.torrent_card(other_id).unwrap();
    assert_eq!(other.images[0].image_id, old_id);
    assert_eq!(
        other.images[0].local_file_path.as_deref(),
        Some(old_path.as_str())
    );
    assert!(library.base.join(&old_path).is_file());
    library.db.upsert_scraped_item(&original).unwrap();
    assert_eq!(library.db.torrent_card(library.id).unwrap().images.len(), 1);
    original.images.push(library.page.images[0].clone());
    library.db.upsert_scraped_item(&original).unwrap();
    assert_eq!(
        library.db.torrent_card(library.id).unwrap().images.len(),
        2,
        "an explicitly requested separate preview must remain"
    );
}

#[test]
fn failed_preview_upgrade_rolls_back_links_and_new_image_records() {
    let library = Library::new(false);
    let before = library.db.torrent_card(library.id).unwrap();
    let connection =
        rusqlite::Connection::open(library.root.path().join("archive.sqlite3")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_preview_delete BEFORE DELETE ON page_images BEGIN SELECT RAISE(ABORT,'fixture preview replacement failure'); END;").unwrap();
    let mut original = library.page.clone();
    original.images[0].preview_url = Some(original.images[0].source_url.clone());
    original.images[0].source_url = "https://example.test/original.png".into();
    assert!(library.db.upsert_scraped_item(&original).is_err());
    let after = library.db.torrent_card(library.id).unwrap();
    assert_eq!(after.images[0].image_id, before.images[0].image_id);
    assert_eq!(
        after.images[0].local_file_path,
        before.images[0].local_file_path
    );
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM images WHERE source_url='https://example.test/original.png'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn primary_image_is_per_card_and_survives_reparse_restart_and_studio_moves() {
    let mut library = Library::new(false);
    let second = library.add_second_image();
    let original = library.db.torrent_card(library.id).unwrap();
    assert_eq!(original.primary_image_id, None);
    let first = original.primary_image().unwrap().image_id;
    let edit = TorrentEdit {
        primary_image_id: Some(second),
        ..TorrentEdit::from(&original)
    };
    let plan = library
        .service()
        .prepare(library.id, original.revision, edit, false)
        .unwrap();
    assert!(plan.files.is_empty());
    let saved = library.service().apply(plan, |_| {}).unwrap();
    assert_eq!(saved.primary_image().unwrap().image_id, second);
    assert_eq!(
        saved.images[0].image_id, first,
        "manual selection must not rewrite parser image order"
    );
    let reopened = Database::new(library.root.path().join("archive.sqlite3"));
    reopened.initialize().unwrap();
    assert_eq!(
        reopened.torrent_card(library.id).unwrap().primary_image_id,
        Some(second)
    );
    library.page.images.swap(0, 1);
    for (index, image) in library.page.images.iter_mut().enumerate() {
        image.sort_order = index as i64;
    }
    library.db.upsert_scraped_item(&library.page).unwrap();
    assert_eq!(
        library
            .db
            .torrent_card(library.id)
            .unwrap()
            .primary_image_id,
        Some(second)
    );
    let mut other_page = library.page.clone();
    other_page.page_url = "https://example.test/topic?t=43".into();
    let other_id = library.db.upsert_scraped_item(&other_page).unwrap().page_id;
    let other = library.db.torrent_card(other_id).unwrap();
    assert_eq!(other.primary_image_id, None);
    let edit = TorrentEdit {
        primary_image_id: Some(first),
        ..TorrentEdit::from(&other)
    };
    let plan = library
        .service()
        .prepare(other_id, other.revision, edit, false)
        .unwrap();
    library.service().apply(plan, |_| {}).unwrap();
    let moved = library
        .service()
        .apply(library.plan("Studio Second"), |_| {})
        .unwrap();
    assert_eq!(moved.primary_image_id, Some(second));
    let cover = moved.primary_image().unwrap();
    assert!(cover
        .local_file_path
        .as_ref()
        .unwrap()
        .starts_with(moved.directory.as_ref().unwrap()));
    assert!(library
        .base
        .join(cover.local_file_path.as_ref().unwrap())
        .is_file());
    assert_eq!(
        library.db.torrent_card(other_id).unwrap().primary_image_id,
        Some(first)
    );
    let edit = TorrentEdit {
        primary_image_id: None,
        ..TorrentEdit::from(&moved)
    };
    let plan = library
        .service()
        .prepare(library.id, moved.revision, edit, false)
        .unwrap();
    assert!(plan.files.is_empty());
    let automatic = library.service().apply(plan, |_| {}).unwrap();
    assert_eq!(automatic.primary_image_id, None);
    assert_eq!(
        automatic.primary_image().unwrap().image_id,
        automatic.images[0].image_id
    );
}

#[test]
fn primary_image_must_belong_to_card_and_deletion_restores_a_safe_fallback() {
    let mut library = Library::new(false);
    let second = library.add_second_image();
    let card = library.db.torrent_card(library.id).unwrap();
    let first = card.images[0].image_id;
    let mut foreign = library.page.clone();
    foreign.page_url = "https://example.test/topic?t=44".into();
    foreign.images.truncate(1);
    foreign.images[0].source_url = "https://example.test/foreign.png".into();
    let foreign = library.db.upsert_scraped_item(&foreign).unwrap();
    let foreign_id = foreign.image_ids_by_url["https://example.test/foreign.png"];
    for invalid in [i64::MAX, foreign_id] {
        let edit = TorrentEdit {
            primary_image_id: Some(invalid),
            ..TorrentEdit::from(&card)
        };
        assert!(library
            .service()
            .prepare(library.id, card.revision, edit, false)
            .is_err());
    }
    let edit = TorrentEdit {
        primary_image_id: Some(second),
        ..TorrentEdit::from(&card)
    };
    let plan = library
        .service()
        .prepare(library.id, card.revision, edit, false)
        .unwrap();
    library.service().apply(plan, |_| {}).unwrap();
    let plan = library.plan("Studio Second");
    let destination = library.base.join(plan.to_directory.as_ref().unwrap());
    library.db.delete_images(&[second]).unwrap();
    let fallback = library.db.torrent_card(library.id).unwrap();
    assert_eq!(fallback.primary_image_id, None);
    assert_eq!(fallback.primary_image().unwrap().image_id, first);
    assert!(library.service().apply(plan, |_| {}).is_err());
    assert!(!destination.exists());
    assert_eq!(
        library.db.torrent_card(library.id).unwrap().primary_studio,
        "Studio First"
    );
    library.db.delete_images(&[first]).unwrap();
    assert!(library
        .db
        .torrent_card(library.id)
        .unwrap()
        .primary_image()
        .is_none());
}

#[test]
fn studio_move_keeps_complete_card_sidecars_and_search_paths_consistent() {
    let library = Library::new(false);
    let old = library.db.torrent_card(library.id).unwrap();
    let old_folder = library.base.join(old.directory.as_ref().unwrap());
    std::fs::write(old_folder.join("notes.txt"), b"User notes").unwrap();
    let plan = library.plan("Studio Second");
    assert_eq!(plan.files.len(), 4);
    assert!(!plan.to_directory.as_ref().unwrap().contains("2025"));
    assert!(old_folder.is_dir(), "preview must not move files");
    let mut edit = plan.edit.clone();
    edit.tags.push("Custom".into());
    edit.categories = vec!["Collection".into()];
    let plan = library
        .service()
        .prepare(library.id, old.revision, edit, false)
        .unwrap();
    let new = library.service().apply(plan, |_| {}).unwrap();
    assert_eq!(new.primary_studio, "Studio Second");
    assert!(!old_folder.exists());
    assert_eq!(
        std::fs::read(
            library
                .base
                .join(new.directory.as_ref().unwrap())
                .join("notes.txt")
        )
        .unwrap(),
        b"User notes"
    );
    assert_eq!(new.values("actor").len(), 3);
    assert_eq!(new.values("date"), ["2025-06-23"]);
    assert_eq!(new.images[0].thumbnail_path, old.images[0].thumbnail_path);
    for path in new
        .images
        .iter()
        .filter_map(|i| i.local_file_path.as_ref())
        .chain(new.attachments.iter().map(|a| &a.local_file_path))
    {
        assert!(library.base.join(path).is_file());
    }
    let query = TorrentQuery {
        site_key: "example-source".into(),
        studio: "Studio Second".into(),
        tags: vec!["Custom".into()],
        categories: vec!["Collection".into()],
        year: "2025".into(),
        ..Default::default()
    };
    assert_eq!(library.db.search_torrents(&query).unwrap().total, 1);
    assert_eq!(
        library
            .db
            .search_torrents(&TorrentQuery {
                year: "2024".into(),
                ..query.clone()
            })
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        library
            .db
            .search_torrents(&TorrentQuery {
                site_key: "other".into(),
                ..query.clone()
            })
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        library
            .db
            .search_torrents(&TorrentQuery {
                tags: vec!["absent".into()],
                ..query
            })
            .unwrap()
            .total,
        0
    );
    let images = library
        .db
        .search_images(&SearchQuery {
            tags: vec!["Custom".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(images[0].local_file_path, new.images[0].local_file_path);
    assert_eq!(images[0].display_source(), Some("Studio Second"));
}

#[test]
fn metadata_edits_and_removals_survive_reparse_without_implicitly_moving_legacy_files() {
    let library = Library::new(true);
    let old = library.db.torrent_card(library.id).unwrap();
    let mut edit = TorrentEdit::from(&old);
    edit.tags.retain(|s| s != "Remove me");
    edit.tags.extend(["Custom".into(), " custom ".into()]);
    edit.categories = vec!["Saved collection".into()];
    let plan = library
        .service()
        .prepare(library.id, old.revision, edit, false)
        .unwrap();
    assert!(plan.files.is_empty());
    assert!(plan.to_directory.is_none());
    library.service().apply(plan, |_| {}).unwrap();
    let mut reparsed = library.page.clone();
    reparsed.attributes.push(attribute("tag", "New parsed tag"));
    library.db.upsert_scraped_item(&reparsed).unwrap();
    let new = library.db.torrent_card(library.id).unwrap();
    assert!(new.directory.is_none());
    assert_eq!(new.images[0].local_file_path, old.images[0].local_file_path);
    assert_eq!(new.values("tag"), ["Custom", "New parsed tag", "Original"]);
    assert_eq!(new.values("category"), ["Saved collection"]);
    let plan = library.plan("New manual studio");
    let moved = library.service().apply(plan, |_| {}).unwrap();
    library.db.upsert_scraped_item(&library.page).unwrap();
    let reparsed = library.db.torrent_card(library.id).unwrap();
    assert_eq!(reparsed.primary_studio, "New manual studio");
    assert_eq!(reparsed.directory, moved.directory);
    assert!(reparsed
        .values("studio")
        .contains(&"New manual studio".into()));
}

#[test]
fn legacy_files_move_only_after_an_explicit_organize_confirmation() {
    let library = Library::new(true);
    let card = library.db.torrent_card(library.id).unwrap();
    let plan = library
        .service()
        .prepare(card.page_id, card.revision, TorrentEdit::from(&card), true)
        .unwrap();
    assert_eq!(plan.files.len(), 3);
    assert!(card
        .attachments
        .iter()
        .all(|a| library.base.join(&a.local_file_path).is_file()));
    let saved = library.service().apply(plan, |_| {}).unwrap();
    assert!(saved.directory.unwrap().starts_with("torrents"));
    for a in &saved.attachments {
        assert!(library.base.join(&a.local_file_path).is_file());
    }
}

#[test]
fn conflicting_destination_changed_files_and_stale_cards_do_not_move_anything() {
    let library = Library::new(false);
    let old = library.db.torrent_card(library.id).unwrap();
    let plan = library.plan("Studio Second");
    let destination = library.base.join(plan.to_directory.as_ref().unwrap());
    std::fs::create_dir_all(&destination).unwrap();
    std::fs::write(destination.join("keep.txt"), b"keep").unwrap();
    assert!(library.service().apply(plan, |_| {}).is_err());
    assert!(library.base.join(old.directory.as_ref().unwrap()).is_dir());
    assert_eq!(
        std::fs::read(destination.join("keep.txt")).unwrap(),
        b"keep"
    );
    let plan = library.plan("Studio Third");
    let path = library.base.join(&plan.files[0].from);
    std::fs::write(&path, b"changed").unwrap();
    assert!(library.service().apply(plan, |_| {}).is_err());
    assert!(path.is_file());
    let plan = library.plan("Studio Fourth");
    library.db.upsert_scraped_item(&library.page).unwrap();
    assert!(library.service().apply(plan, |_| {}).is_err());
    assert_eq!(
        library.db.torrent_card(library.id).unwrap().primary_studio,
        "Studio First"
    );
}

#[test]
fn failed_database_commit_rolls_back_all_files_and_metadata() {
    let library = Library::new(false);
    let old = library.db.torrent_card(library.id).unwrap();
    let mut plan = library.plan("Studio Second");
    plan.edit.primary_image_id = Some(old.images[0].image_id);
    let destination = library.base.join(plan.to_directory.as_ref().unwrap());
    let connection =
        rusqlite::Connection::open(library.root.path().join("archive.sqlite3")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_card_update BEFORE UPDATE ON torrent_cards BEGIN SELECT RAISE(ABORT,'test commit failure'); END;").unwrap();
    assert!(library.service().apply(plan, |_| {}).is_err());
    assert_eq!(
        library.db.torrent_card(library.id).unwrap().primary_studio,
        old.primary_studio
    );
    assert_eq!(
        library
            .db
            .torrent_card(library.id)
            .unwrap()
            .primary_image_id,
        old.primary_image_id
    );
    assert!(!destination.exists());
    for a in old.attachments {
        assert!(library.base.join(a.local_file_path).is_file());
    }
    let journals: i64 = connection
        .query_row("SELECT count(*) FROM torrent_moves", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journals, 0);
}

#[cfg(windows)]
#[test]
fn a_file_open_without_delete_sharing_rolls_back_the_partially_moved_legacy_card() {
    use std::os::windows::fs::OpenOptionsExt;
    let library = Library::new(true);
    let old = library.db.torrent_card(library.id).unwrap();
    let plan = library.plan("Studio Second");
    let locked_path = library.base.join(
        &old.attachments
            .iter()
            .find(|a| a.filename.ends_with(".torrent"))
            .unwrap()
            .local_file_path,
    );
    let locked = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1 | 2)
        .open(&locked_path)
        .unwrap();
    assert!(library.service().apply(plan, |_| {}).is_err());
    drop(locked);
    assert_eq!(
        library.db.torrent_card(library.id).unwrap().primary_studio,
        old.primary_studio
    );
    for path in old
        .images
        .iter()
        .filter_map(|i| i.local_file_path.as_ref())
        .chain(old.attachments.iter().map(|a| &a.local_file_path))
    {
        assert!(library.base.join(path).is_file());
    }
}

#[test]
fn a_file_shared_with_another_page_is_copied_and_the_other_card_stays_readable() {
    let library = Library::new(false);
    let first = library.db.torrent_card(library.id).unwrap();
    let mut another = library.page.clone();
    another.page_url = "https://example.test/topic?t=43".into();
    let persisted = library.db.upsert_scraped_item(&another).unwrap();
    let image_id = persisted.image_ids_by_url[&another.images[0].source_url];
    let original = first.images[0].local_file_path.as_ref().unwrap();
    library
        .db
        .update_image_artifacts(persisted.page_id, image_id, original, None)
        .unwrap();
    let saved = library
        .service()
        .apply(library.plan("Studio Second"), |_| {})
        .unwrap();
    assert!(library.base.join(original).is_file());
    assert!(library
        .base
        .join(saved.images[0].local_file_path.as_ref().unwrap())
        .is_file());
    assert_eq!(
        library.db.torrent_card(persisted.page_id).unwrap().images[0]
            .local_file_path
            .as_ref(),
        Some(original)
    );
}

#[test]
fn portable_paths_preserve_unicode_and_reject_windows_device_names_and_collisions() {
    for value in ["CON", "nul.txt", "COM1", "LPT9", "COM¹"] {
        assert!(safe_component(value, "fallback", 40).starts_with('_'));
    }
    assert_eq!(
        safe_component(" . ", "unknown_studio", 48),
        "unknown_studio"
    );
    assert_eq!(
        safe_component("Студия / Test: one?", "fallback", 48),
        "Студия _ Test_ one_"
    );
    let a = torrent_directory(
        "source",
        "Студия",
        "Example",
        "https://example.test/topic?t=42",
        1,
    );
    let b = torrent_directory(
        "source",
        "Студия",
        "Example",
        "https://example.test/other?t=42",
        1,
    );
    assert_ne!(a, b);
    assert_eq!(a.components().count(), 4);
    assert!(a.to_string_lossy().contains("Студия"));
    #[cfg(windows)]
    assert!(!a.to_string_lossy().contains('/'));
    #[cfg(not(windows))]
    assert!(!a.to_string_lossy().contains('\\'));
    let library = Library::new(false);
    let card = library.db.torrent_card(library.id).unwrap();
    let connection =
        rusqlite::Connection::open(library.root.path().join("archive.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE page_attachments SET local_file_path='../outside' WHERE filename='Magnet'",
            [],
        )
        .unwrap();
    assert!(library
        .service()
        .prepare(
            card.page_id,
            card.revision,
            TorrentEdit {
                primary_studio: "Other".into(),
                ..TorrentEdit::from(&card)
            },
            false
        )
        .is_err());
}

#[test]
fn torrent_cards_without_images_are_searchable_and_pagination_is_bounded() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::new(root.path().join("archive.sqlite3"));
    db.initialize().unwrap();
    let mut item = page();
    item.images.clear();
    item.attributes.retain(|a| a.kind != "studio");
    for i in 0..43 {
        item.page_url = format!("https://example.test/topic?t={i}");
        db.upsert_scraped_item(&item).unwrap();
    }
    let first = db.search_torrents(&TorrentQuery::default()).unwrap();
    assert_eq!(first.total, 43);
    assert_eq!(first.cards.len(), 40);
    assert!(first.cards[0].images.is_empty());
    assert!(Path::new(first.cards[0].directory.as_ref().unwrap())
        .components()
        .any(|p| p.as_os_str() == "unknown_studio"));
    let last = db
        .search_torrents(&TorrentQuery {
            offset: 40,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(last.cards.len(), 3);
    assert!(last
        .cards
        .iter()
        .all(|c| !first.cards.iter().any(|f| f.page_id == c.page_id)));
    assert!(db
        .search_torrents(&TorrentQuery {
            year: "bad".into(),
            ..Default::default()
        })
        .is_err());
}

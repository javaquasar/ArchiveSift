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
    let plan = library.plan("Studio Second");
    let destination = library.base.join(plan.to_directory.as_ref().unwrap());
    let connection =
        rusqlite::Connection::open(library.root.path().join("archive.sqlite3")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_card_update BEFORE UPDATE ON torrent_cards BEGIN SELECT RAISE(ABORT,'test commit failure'); END;").unwrap();
    assert!(library.service().apply(plan, |_| {}).is_err());
    assert_eq!(
        library.db.torrent_card(library.id).unwrap().primary_studio,
        old.primary_studio
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

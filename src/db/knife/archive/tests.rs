use super::*;
use crate::knife::preview::preview;
use crate::models::SearchQuery;

fn png(path: &Path, side: u32, color: u8) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    image::GrayImage::from_pixel(side, side, image::Luma([color]))
        .save(path)
        .unwrap();
}

fn index(archive: &Path, records: serde_json::Value) -> PathBuf {
    let index = archive.join("knives.json");
    fs::write(&index, records.to_string()).unwrap();
    index
}

fn database(library: &Path) -> Database {
    let db = Database::new(library.join("library.sqlite3"));
    db.initialize().unwrap();
    db
}

fn count(db: &Database, table: &str) -> i64 {
    db.connect()
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn live_attribute(kind: &str, value: &str) -> MetadataAttribute {
    MetadataAttribute {
        kind: kind.into(),
        label: kind.into(),
        value: value.into(),
        source_url: None,
        is_female: None,
    }
}

#[test]
fn archive_import_merges_a_live_page_uses_local_files_and_supports_manufacturer_search() {
    let archive = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let database = database(library.path());
    let page_url = "https://shop.example.test/products/ridge";
    let live = ScrapedItem {
        site_key: "existing-source".into(),
        page_url: page_url.into(),
        title: "Live title".into(),
        raw_html: String::new(),
        attributes: vec![
            live_attribute("catalog_profile", "knife"),
            live_attribute("manufacturer", "Forge, Inc."),
            live_attribute("model", "Ridge"),
            live_attribute("steel", "D2"),
            live_attribute("price", "110"),
        ],
        images: vec![ScrapedImage {
            source_url: "https://shop.example.test/images/1000x1000/cover.png".into(),
            preview_url: None,
            alt_text: None,
            width: Some(8),
            height: Some(8),
            sort_order: 0,
            local_file_path: None,
        }],
    };
    let persisted = database.upsert_scraped_item(&live).unwrap();
    let image_id = persisted.image_ids_by_url[&live.images[0].source_url];
    let saved = database
        .knife_image_paths(persisted.page_id)
        .unwrap()
        .remove(&image_id)
        .unwrap();
    png(&library.path().join(&saved), 8, 64);
    database
        .update_image_file_location(persisted.page_id, image_id, &portable_text(&saved))
        .unwrap();

    let images = archive.path().join("images");
    png(&images.join("legacy-cover.png"), 4, 64);
    png(&images.join("nested/Detail.png"), 8, 128);
    png(&images.join("Repeated.png"), 4, 255);
    fs::copy(
        images.join("nested/Detail.png"),
        images.join("Alternate name.png"),
    )
    .unwrap();
    png(&images.join("Profile.png"), 8, 192);
    let path = index(
        archive.path(),
        serde_json::json!([
            {
                "id":"ridge","company":"FORGE, INC.","knifeName":"Ridge, Horn Handle","steel":"D2",
                "sourceUrls":[format!("{page_url}#photos")], "imageFiles":["legacy-cover.png","nested\\Detail.png"],
                "imageUrls":["https://shop.example.test/images/800x800/cover.png","https://shop.example.test/images/800x800/detail.png"],
                "priceAtParse":90, "technicalDetails":"Blade Length: 108 mm", "notes":"Archive notes"
            },
            {
                "id":"RIDGE","company":"Forge, Inc.","knifeName":"Ridge", "imageFile":"Repeated.png"
            },
            {
                "id":"ridge-variant","company":"Forge, Inc.","knifeName":"Ridge","steel":"CPM 3V",
                "sourceUrls":["https://retailer.example.test/ridge"],"imageFiles":["Alternate name.png","Profile.png"]
            }
        ]),
    );
    let plan = preview(&path, library.path()).unwrap();
    assert_eq!(plan.duplicate_ids, 1);
    assert!(!library
        .path()
        .join("knife/Forge, Inc./Ridge/Detail.png")
        .exists());
    assert_eq!(count(&database, "source_pages"), 1);

    let result = database
        .import_knife_archive(&plan, library.path())
        .unwrap();
    assert!(result.errors.is_empty(), "{result:?}");
    assert_eq!(
        (
            result.new_pages,
            result.existing_pages,
            result.skipped_records
        ),
        (1, 1, 1)
    );
    assert_eq!((result.copied_files, result.duplicate_files), (2, 2));
    assert_eq!(count(&database, "knife_models"), 1);
    assert_eq!(count(&database, "page_images"), 3);
    assert_eq!(count(&database, "knife_sources"), 2);
    let folder = library.path().join("knife/Forge, Inc./Ridge");
    assert_eq!(
        fs::read(folder.join("Detail.png")).unwrap(),
        fs::read(images.join("nested/Detail.png")).unwrap()
    );
    assert!(folder.join("Profile.png").is_file());
    assert!(!folder.join("Repeated.png").exists());
    assert_eq!(fs::read_dir(&folder).unwrap().count(), 3);
    let conn = database.connect().unwrap();
    for (kind, expected) in [
        ("price", "110"),
        ("archived_price", "90"),
        ("technical_details", "Blade Length: 108 mm"),
    ] {
        let value: String = conn
            .query_row(
                "SELECT a.value FROM page_attributes pa JOIN attributes a ON a.id = pa.attribute_id
             WHERE pa.page_id = ?1 AND a.kind = ?2",
                params![persisted.page_id, kind],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(value, expected, "{kind}");
    }

    assert_eq!(
        database.suggest_knife_manufacturers("iNc.", 8).unwrap(),
        ["Forge, Inc."]
    );
    let query = SearchQuery {
        manufacturers: vec!["  FORGE,   inc. ".into()],
        ..Default::default()
    };
    assert_eq!(database.search_images(&query).unwrap().len(), 3);
    let query = SearchQuery {
        site_key: Some("existing-source".into()),
        ..query
    };
    assert_eq!(database.search_images(&query).unwrap().len(), 2);
    let query = SearchQuery {
        manufacturers: vec!["Other manufacturer".into()],
        ..query
    };
    assert!(database.search_images(&query).unwrap().is_empty());

    let repeated = database
        .import_knife_archive(&plan, library.path())
        .unwrap();
    assert!(repeated.errors.is_empty(), "{repeated:?}");
    assert_eq!(
        (
            repeated.new_pages,
            repeated.existing_pages,
            repeated.copied_files
        ),
        (0, 2, 0)
    );
    assert_eq!(count(&database, "page_images"), 3);
    assert_eq!(fs::read_dir(folder).unwrap().count(), 3);
}

#[test]
fn archive_import_skips_paths_and_identity_conflicts_and_keeps_existing_files() {
    let archive = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let database = database(library.path());
    let occupied_url = "https://example.test/other-content";
    database
        .upsert_scraped_item(&ScrapedItem {
            site_key: "fixture".into(),
            page_url: occupied_url.into(),
            title: "Other content".into(),
            raw_html: String::new(),
            images: Vec::new(),
            attributes: Vec::new(),
        })
        .unwrap();
    let images = archive.path().join("images");
    png(&images.join("cover.png"), 8, 64);
    png(&images.join("same.png"), 8, 128);
    png(&archive.path().join("outside.png"), 8, 192);
    fs::write(images.join("invalid.png"), b"not an image").unwrap();
    let target = library.path().join("knife/Forge/Ridge");
    png(&target.join("cover.png"), 8, 0);
    fs::copy(images.join("same.png"), target.join("same.png")).unwrap();
    let prior = fs::read(target.join("cover.png")).unwrap();
    let index = index(
        archive.path(),
        serde_json::json!([
            {"id":"offline","company":"Forge","knifeName":"Ridge","imageFiles":["cover.png","same.png","invalid.png","missing.png","../outside.png"]},
            {"id":"collision","company":"Forge","knifeName":"Other", "sourceUrls":[occupied_url], "imageFile":"cover.png"}
        ]),
    );
    let plan = preview(&index, library.path()).unwrap();
    let result = database
        .import_knife_archive(&plan, library.path())
        .unwrap();
    assert_eq!((result.new_pages, result.skipped_records), (1, 1));
    assert!(result.errors[0].contains("outside the knife catalog"));
    assert_eq!(
        (
            result.copied_files,
            result.reused_files,
            result.skipped_files
        ),
        (0, 1, 5)
    );
    assert_eq!(fs::read(target.join("cover.png")).unwrap(), prior);
    assert_eq!(count(&database, "page_images"), 1);

    fs::remove_file(target.join("cover.png")).unwrap();
    let result = database
        .import_knife_archive(&plan, library.path())
        .unwrap();
    assert_eq!(
        (result.new_pages, result.existing_pages, result.copied_files),
        (0, 1, 1)
    );
    assert_eq!(
        fs::read(target.join("cover.png")).unwrap(),
        fs::read(images.join("cover.png")).unwrap()
    );
    assert_eq!(count(&database, "page_images"), 2);
}

#[test]
fn archive_import_rolls_back_new_files_when_a_record_cannot_commit() {
    let archive = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let database = database(library.path());
    let conn = database.writer_connection().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_archive_image BEFORE INSERT ON page_images BEGIN
           SELECT RAISE(ABORT, 'fixture database error');
         END;",
    )
    .unwrap();
    drop(conn);
    let images = archive.path().join("images");
    png(&images.join("cover.png"), 8, 64);
    let index = index(
        archive.path(),
        serde_json::json!([
            {"id":"ridge","company":"Forge","knifeName":"Ridge","imageFile":"cover.png"}
        ]),
    );
    let plan = preview(&index, library.path()).unwrap();
    let result = database
        .import_knife_archive(&plan, library.path())
        .unwrap();
    assert_eq!(result.skipped_records, 1);
    assert!(result.errors[0].contains("fixture database error"));
    assert_eq!(count(&database, "source_pages"), 0);
    assert_eq!(count(&database, "knife_import_keys"), 0);
    assert_eq!(count(&database, "knife_models"), 0);
    assert!(!library.path().join("knife/Forge/Ridge/cover.png").exists());
    assert!(images.join("cover.png").is_file());

    database
        .writer_connection()
        .unwrap()
        .execute("DROP TRIGGER reject_archive_image", [])
        .unwrap();
    let retry = database
        .import_knife_archive(&plan, library.path())
        .unwrap();
    assert_eq!((retry.new_pages, retry.copied_files), (1, 1));
    assert!(library.path().join("knife/Forge/Ridge/cover.png").is_file());
}

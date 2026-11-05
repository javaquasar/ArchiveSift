use archive_sift::{
    db::{Database, QueryCancellation},
    models::{MetadataAttribute, ScrapedImage, ScrapedItem, SearchQuery, TorrentQuery},
    services::SearchService,
};
use std::collections::BTreeSet;

fn attribute(kind: &str, value: &str) -> MetadataAttribute {
    MetadataAttribute {
        kind: kind.into(),
        label: kind.into(),
        value: value.into(),
        source_url: None,
        is_female: None,
    }
}

fn page(id: &str, film: bool, people: &[(&str, &str)], images: &[&str]) -> ScrapedItem {
    let mut attributes = people
        .iter()
        .map(|(kind, value)| attribute(kind, value))
        .collect::<Vec<_>>();
    attributes.push(attribute("tag", "Outdoor"));
    if film {
        attributes.push(attribute(
            "magnet",
            "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
        ));
    }
    ScrapedItem {
        site_key: if film { "film-source" } else { "photo-source" }.into(),
        page_url: format!("https://example.test/content/{id}"),
        title: format!("Example {id}"),
        raw_html: String::new(),
        attributes,
        images: images
            .iter()
            .enumerate()
            .map(|(index, url)| ScrapedImage {
                source_url: (*url).into(),
                preview_url: None,
                alt_text: None,
                width: None,
                height: None,
                sort_order: index as i64,
                local_file_path: None,
            })
            .collect(),
    }
}

#[test]
fn one_attribution_filter_searches_films_and_photos_without_leaking_shared_pages() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::new(root.path().join("library.sqlite3"));
    db.initialize().unwrap();
    let shared = "https://example.test/shared.jpg";
    let film = db
        .upsert_scraped_item(&page(
            "film",
            true,
            &[("director", "Alice Creator")],
            &[shared, "https://example.test/front.jpg"],
        ))
        .unwrap();
    let photo = db
        .upsert_scraped_item(&page(
            "photo",
            false,
            &[("photographer", "ALICE CREATOR")],
            &["https://example.test/photo.jpg"],
        ))
        .unwrap();
    let other = db
        .upsert_scraped_item(&page(
            "other-film",
            true,
            &[("director", "Different Creator")],
            &[shared],
        ))
        .unwrap();
    db.upsert_scraped_item(&page(
        "actor-only",
        true,
        &[("actor", "Alice Creator")],
        &["https://example.test/actor.jpg"],
    ))
    .unwrap();
    let query = SearchQuery {
        creators: vec!["alice creator".into()],
        ..Default::default()
    };
    let images = db.search_images(&query).unwrap();
    assert_eq!(images.len(), 3);
    assert!(images
        .iter()
        .all(|image| [film.page_id, photo.page_id].contains(&image.page_id())));
    assert!(!images.iter().any(|image| image.page_id() == other.page_id));
    let films = db
        .search_torrents(&TorrentQuery {
            creators: query.creators.clone(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(films.total, 1);
    assert_eq!(films.cards[0].page_id, film.page_id);
    assert_eq!(
        db.search_images(&SearchQuery {
            site_key: Some("film-source".into()),
            tags: vec!["Outdoor".into()],
            ..query.clone()
        })
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        db.search_images(&SearchQuery {
            tags: vec!["Missing".into()],
            ..query.clone()
        })
        .unwrap()
        .len(),
        0
    );
    assert_eq!(
        db.search_torrents(&TorrentQuery {
            creators: query.creators.clone(),
            site_key: "photo-source".into(),
            ..Default::default()
        })
        .unwrap()
        .total,
        0
    );

    let mut cursor = None;
    let mut found = Vec::new();
    loop {
        let result = db
            .search_images_page_lightweight(
                &query,
                cursor.as_ref(),
                1,
                &QueryCancellation::default(),
            )
            .unwrap();
        found.extend(
            result
                .items
                .iter()
                .map(|image| (image.page_id(), image.image_id)),
        );
        cursor = result.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(found.len(), 3);
    assert_eq!(found.into_iter().collect::<BTreeSet<_>>().len(), 3);
}

#[test]
fn image_attribution_is_local_to_the_image_and_matches_parent_torrent_cards() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::new(root.path().join("library.sqlite3"));
    db.initialize().unwrap();
    let urls = [
        "https://example.test/one.jpg",
        "https://example.test/two.jpg",
    ];
    let film = db
        .upsert_scraped_item(&page("film", true, &[], &urls))
        .unwrap();
    let image_id = film.image_ids_by_url[urls[0]];
    db.add_image_attributes(&[image_id], "author", "Author", &["Image Author".into()])
        .unwrap();
    let found = db
        .search_images(&SearchQuery {
            creators: vec!["IMAGE AUTHOR".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].image_id, image_id);
    assert_eq!(
        db.search_torrents(&TorrentQuery {
            creators: vec!["image author".into()],
            ..Default::default()
        })
        .unwrap()
        .total,
        1
    );
    assert_eq!(
        db.search_images(&SearchQuery {
            creators: vec!["unknown".into()],
            ..Default::default()
        })
        .unwrap()
        .len(),
        0
    );
    assert_eq!(
        db.search_images(&SearchQuery {
            creators: vec!["   ".into()],
            ..Default::default()
        })
        .unwrap()
        .len(),
        2
    );
}

#[test]
fn attribution_names_support_unicode_multiple_people_and_literal_punctuation() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::new(root.path().join("library.sqlite3"));
    db.initialize().unwrap();
    db.upsert_scraped_item(&page(
        "photo",
        false,
        &[("author", "Борис Автор")],
        &["https://example.test/photo.jpg"],
    ))
    .unwrap();
    db.upsert_scraped_item(&page(
        "film",
        true,
        &[("director", "БОРИС АВТОР")],
        &["https://example.test/film.jpg"],
    ))
    .unwrap();
    let literal = "O'Name_%'; DROP TABLE attributes; --";
    db.upsert_scraped_item(&page(
        "literal",
        true,
        &[("creator", literal)],
        &["https://example.test/literal.jpg"],
    ))
    .unwrap();
    assert_eq!(
        db.search_images(&SearchQuery {
            creators: vec!["борис автор".into()],
            ..Default::default()
        })
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        db.search_torrents(&TorrentQuery {
            creators: vec!["борис автор".into()],
            ..Default::default()
        })
        .unwrap()
        .total,
        1
    );
    assert_eq!(
        db.search_images(&SearchQuery {
            creators: vec![literal.into()],
            ..Default::default()
        })
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        db.search_images(&SearchQuery {
            creators: vec!["борис автор".into(), literal.into()],
            ..Default::default()
        })
        .unwrap()
        .len(),
        3
    );
    assert!(db
        .search_images(&SearchQuery {
            creators: vec!["' OR 1=1 --".into()],
            ..Default::default()
        })
        .unwrap()
        .is_empty());
}

#[test]
fn completion_merges_roles_orders_prefixes_and_ignores_unused_names() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("library.sqlite3");
    let db = Database::new(&path);
    db.initialize().unwrap();
    db.upsert_scraped_item(&page(
        "one",
        true,
        &[
            ("director", "Alice Creator"),
            ("author", "ALICE CREATOR"),
            ("photographer", "Creator Zed"),
            ("actor", "Actor only"),
        ],
        &["https://example.test/one.jpg"],
    ))
    .unwrap();
    db.upsert_scraped_item(&page(
        "two",
        false,
        &[("author", "Борис Автор"), ("creator", "50% Name")],
        &["https://example.test/two.jpg"],
    ))
    .unwrap();
    rusqlite::Connection::open(path).unwrap().execute("INSERT INTO attributes(kind,label,value,normalized_value) VALUES('director','Director','Forgotten Creator','forgotten creator')",[]).unwrap();
    assert_eq!(
        db.suggest_creators("cre", 8).unwrap(),
        ["Creator Zed", "Alice Creator"]
    );
    assert_eq!(db.suggest_creators("ALICE", 8).unwrap(), ["Alice Creator"]);
    assert_eq!(db.suggest_creators("бор", 8).unwrap(), ["Борис Автор"]);
    assert_eq!(db.suggest_creators("%", 8).unwrap(), ["50% Name"]);
    assert!(db.suggest_creators("Forgotten", 8).unwrap().is_empty());
    assert!(db.suggest_creators("Actor only", 8).unwrap().is_empty());
    assert_eq!(db.suggest_creators("", 1).unwrap().len(), 1);
    assert_eq!(
        SearchService::new(db).suggest_creators("alice").unwrap(),
        ["Alice Creator"]
    );
}

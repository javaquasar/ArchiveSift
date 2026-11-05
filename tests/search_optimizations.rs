use archive_sift::{
    db::{Database, QueryCancellation},
    models::{MetadataAttribute, ScrapedImage, ScrapedItem, SearchQuery, TorrentQuery},
    services::SearchService,
};
use rusqlite::{params, Connection};

fn attribute(kind: &str, value: &str) -> MetadataAttribute {
    MetadataAttribute {
        kind: kind.into(),
        label: kind.into(),
        value: value.into(),
        source_url: None,
        is_female: None,
    }
}

fn page(number: usize, images: usize, title: &str) -> ScrapedItem {
    ScrapedItem {
        site_key: format!("example-{}", number % 2),
        page_url: format!("https://example.test/topic?id={number}"),
        title: title.into(),
        raw_html: String::new(),
        images: (0..images)
            .map(|index| ScrapedImage {
                source_url: format!("https://example.test/{number}/{index}.jpg"),
                preview_url: None,
                alt_text: None,
                width: None,
                height: None,
                sort_order: index as i64,
                local_file_path: None,
            })
            .collect(),
        attributes: vec![
            attribute("tag", "Selected"),
            attribute("director", "Example Creator"),
            attribute("content_title", &format!("Movie {number}")),
            attribute("description", &"Full description ".repeat(2000)),
            attribute(
                "magnet",
                "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
            ),
        ],
    }
}

fn ids(db: &Database, query: &SearchQuery, size: usize) -> Vec<(i64, i64)> {
    let mut cursor = None;
    let mut all = Vec::new();
    loop {
        let page = db
            .search_images_page_lightweight(
                query,
                cursor.as_ref(),
                size,
                &QueryCancellation::default(),
            )
            .unwrap();
        all.extend(
            page.items
                .iter()
                .map(|image| (image.page_id(), image.image_id)),
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    all
}

#[test]
fn adaptive_fts_matches_the_previous_query_for_empty_rare_and_common_terms() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("library.sqlite3");
    let db = Database::new(&path);
    db.initialize().unwrap();
    let first = page(1, 3, "Common needle 50% \"quote\" кино");
    let saved = db.upsert_scraped_item(&first).unwrap();
    db.upsert_scraped_item(&page(2, 160, "Common title"))
        .unwrap();
    let mut shared = page(3, 1, "Other heading");
    shared.images[0].source_url = first.images[0].source_url.clone();
    db.upsert_scraped_item(&shared).unwrap();
    SearchService::new(db.clone())
        .add_tags_to_images(
            &[saved.image_ids_by_url[&first.images[0].source_url]],
            &["Needle".into()],
        )
        .unwrap();
    let connection = Connection::open(&path).unwrap();
    for text in [
        "absent-token",
        "needle",
        "common",
        "50%",
        "\"quote\"",
        "кино",
    ] {
        let literal = format!("\"{}\"", text.replace('"', "\"\""));
        let mut statement = connection.prepare("SELECT sp.id,i.id FROM images i JOIN page_images pi ON pi.image_id=i.id JOIN source_pages sp ON sp.id=pi.page_id
            WHERE (sp.id IN (SELECT rowid FROM page_search_fts WHERE page_search_fts MATCH ?1)
               OR i.id IN (SELECT rowid FROM image_search_fts WHERE image_search_fts MATCH ?1))
            ORDER BY sp.scraped_at DESC,sp.id DESC,pi.sort_order ASC,i.id ASC").unwrap();
        let expected = statement
            .query_map([literal], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<(i64, i64)>>>()
            .unwrap();
        for size in [1, 17, 120] {
            assert_eq!(
                ids(
                    &db,
                    &SearchQuery {
                        text: text.into(),
                        ..Default::default()
                    },
                    size
                ),
                expected,
                "{text}, page size {size}"
            );
        }
    }
    let all = ids(
        &db,
        &SearchQuery {
            text: "needle".into(),
            ..Default::default()
        },
        17,
    );
    let filtered = ids(
        &db,
        &SearchQuery {
            text: "needle".into(),
            site_key: Some("example-1".into()),
            tags: vec!["Selected".into()],
            creators: vec!["Example Creator".into()],
            page_id: Some(saved.page_id),
            ..Default::default()
        },
        1,
    );
    assert_eq!(
        filtered,
        all.into_iter()
            .filter(|(page, _)| *page == saved.page_id)
            .collect::<Vec<_>>()
    );
    assert!(ids(
        &db,
        &SearchQuery {
            text: "needle".into(),
            favorites_only: true,
            ..Default::default()
        },
        17
    )
    .is_empty());
    assert!(!ids(
        &db,
        &SearchQuery {
            text: "co".into(),
            ..Default::default()
        },
        17
    )
    .is_empty());
    let cancellation = QueryCancellation::default();
    cancellation.cancel();
    assert!(db
        .search_images_page_lightweight(
            &SearchQuery {
                text: "absent-token".into(),
                ..Default::default()
            },
            None,
            120,
            &cancellation
        )
        .is_err());
    assert!(db
        .search_images_by_ids_cancellable(
            &[saved.image_ids_by_url[&first.images[0].source_url]],
            &cancellation
        )
        .is_err());
}

#[test]
fn lightweight_torrent_pages_and_batched_cards_preserve_filters_order_and_details() {
    let root = tempfile::tempdir().unwrap();
    let db = Database::new(root.path().join("library.sqlite3"));
    db.initialize().unwrap();
    for number in 0..43 {
        db.upsert_scraped_item(&page(number, number % 3, "Complete heading"))
            .unwrap();
    }
    for query in [
        TorrentQuery::default(),
        TorrentQuery {
            offset: 40,
            ..Default::default()
        },
        TorrentQuery {
            site_key: "example-1".into(),
            creators: vec!["Example Creator".into()],
            tags: vec!["Selected".into()],
            ..Default::default()
        },
        TorrentQuery {
            text: "absent-token".into(),
            ..Default::default()
        },
    ] {
        let summaries = db.search_torrent_summaries(&query).unwrap();
        let full = db.search_torrents(&query).unwrap();
        assert_eq!(summaries.total, full.total);
        assert_eq!(summaries.cards.len(), full.cards.len());
        for (summary, card) in summaries.cards.iter().zip(&full.cards) {
            let individual = db.torrent_card(card.page_id).unwrap();
            assert_eq!(summary.page_id, card.page_id);
            assert_eq!(summary.content_title, individual.content_title());
            assert_eq!(summary.image_count, individual.images.len());
            assert_eq!(summary.attachment_count, individual.attachments.len());
            assert_eq!(card.values("description"), individual.values("description"));
            assert_eq!(
                card.images.iter().map(|i| i.image_id).collect::<Vec<_>>(),
                individual
                    .images
                    .iter()
                    .map(|i| i.image_id)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                card.attachments
                    .iter()
                    .map(|a| &a.filename)
                    .collect::<Vec<_>>(),
                individual
                    .attachments
                    .iter()
                    .map(|a| &a.filename)
                    .collect::<Vec<_>>()
            );
        }
    }
    let connection = Connection::open(root.path().join("library.sqlite3")).unwrap();
    let id = db
        .search_torrent_summaries(&TorrentQuery::default())
        .unwrap()
        .cards[0]
        .page_id;
    connection.execute("INSERT INTO page_attachments(page_id,source_url,filename,local_file_path) VALUES(?1,'https://example.test/file','Example.torrent','files/Example.torrent')",params![id]).unwrap();
    assert_eq!(
        db.search_torrent_summaries(&TorrentQuery::default())
            .unwrap()
            .cards[0]
            .attachment_count,
        1
    );
    assert_eq!(
        db.search_torrents(&TorrentQuery::default()).unwrap().cards[0].attachments[0].filename,
        "Example.torrent"
    );
}

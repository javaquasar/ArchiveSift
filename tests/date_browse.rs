use archive_sift::{
    db::Database,
    models::{DateBrowseDirection, DateGroup, DateGroupCursor},
};

fn database() -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    let database = Database::new(root.path().join("library.sqlite3"));
    database.initialize().unwrap();
    let connection = rusqlite::Connection::open(root.path().join("library.sqlite3")).unwrap();
    for (id, site, date, favorites) in [
        (1, "source-a", "2026-01-01 00:00:00", 0),
        (2, "source-b", "2026-01-02 00:00:00", 0),
        (3, "source-a", "2026-01-03 00:00:00", 5),
        (4, "source-b", "2026-01-04 00:00:00", 1),
        (5, "source-a", "2026-01-05 00:00:00", 2),
        (6, "source-a", "2026-01-05 00:00:00", 0),
        (7, "source-b", "2026-01-06 00:00:00", 0),
        (8, "source-a", "2026-01-07 00:00:00", 7),
    ] {
        connection.execute(
            "INSERT INTO source_pages(id,site_key,page_url,title,raw_html,scraped_at,favorite_count) VALUES(?1,?2,?3,?4,'',?5,?6)",
            rusqlite::params![id, site, format!("https://example.test/{site}/{id}"), format!("Page {id}"), date, favorites],
        ).unwrap();
    }
    (root, database)
}

fn cursor(group: &DateGroup) -> DateGroupCursor {
    DateGroupCursor {
        page_id: group.page_id,
        scraped_at: group.scraped_at.clone(),
    }
}

#[test]
fn date_browse_source_filter_applies_to_counts_both_directions_ties_and_wraparound() {
    let (_root, database) = database();
    for (ascending, direction, expected) in [
        (true, DateBrowseDirection::Next, [1, 5, 6, 1]),
        (true, DateBrowseDirection::Previous, [1, 6, 5, 1]),
        (false, DateBrowseDirection::Next, [6, 5, 1, 6]),
        (false, DateBrowseDirection::Previous, [6, 1, 5, 6]),
    ] {
        let mut page = database
            .get_date_group_page_for_site(
                None,
                DateBrowseDirection::Initial,
                ascending,
                Some(3),
                Some("source-a"),
            )
            .unwrap();
        let mut visited = vec![page.group.as_ref().unwrap().page_id];
        for _ in 0..3 {
            assert_eq!(page.total, 3);
            page = database
                .get_date_group_page_for_site(
                    Some(&cursor(page.group.as_ref().unwrap())),
                    direction,
                    ascending,
                    Some(3),
                    Some("source-a"),
                )
                .unwrap();
            visited.push(page.group.as_ref().unwrap().page_id);
        }
        assert_eq!(page.total, 3);
        assert_eq!(visited, expected);
    }
    let second = database
        .get_date_group_page_for_site(
            None,
            DateBrowseDirection::Initial,
            false,
            Some(3),
            Some("source-b"),
        )
        .unwrap();
    assert_eq!(second.total, 3);
    assert_eq!(second.group.unwrap().page_id, 7);
}

#[test]
fn date_browse_all_sites_and_favorite_filter_remain_independent() {
    let (_root, database) = database();
    for site in [None, Some(""), Some(" \t ")] {
        let page = database
            .get_date_group_page_for_site(None, DateBrowseDirection::Initial, false, None, site)
            .unwrap();
        assert_eq!(page.total, 8);
        assert_eq!(page.group.unwrap().page_id, 8);
    }
    let all = database
        .get_date_group_page(None, DateBrowseDirection::Initial, false, Some(3))
        .unwrap();
    assert_eq!(all.total, 6);
    assert_eq!(all.group.unwrap().page_id, 7);
    let selected = database
        .get_date_group_page_for_site(
            None,
            DateBrowseDirection::Initial,
            false,
            None,
            Some(" source-a "),
        )
        .unwrap();
    assert_eq!(selected.total, 5);
    assert_eq!(selected.group.unwrap().page_id, 8);
    for site in ["missing-source", "source-a' OR 1=1 --"] {
        let empty = database
            .get_date_group_page_for_site(
                None,
                DateBrowseDirection::Initial,
                false,
                None,
                Some(site),
            )
            .unwrap();
        assert_eq!(empty.total, 0);
        assert!(empty.group.is_none());
    }
}

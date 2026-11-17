use crate::db::Database;
use crate::models::{
    DateBrowseDirection, MetadataAttribute, ScrapedImage, ScrapedItem, SearchQuery,
};

#[test]
fn catalog_groups_span_live_and_imported_sources_without_including_unrelated_pages() {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::new(temp.path().join("library.sqlite3"));
    database.initialize().unwrap();
    for (site, profile) in [
        ("first-source", Some("knife")),
        ("second-source", Some("KNIFE")),
        ("knife-archive", Some("knife")),
        ("first-source", None),
        ("another-source", Some("catalog-sample")),
    ] {
        let url = format!(
            "https://{site}.example.test/{}",
            profile.unwrap_or("unrelated")
        );
        let mut attributes = vec![
            MetadataAttribute {
                kind: "manufacturer".into(),
                label: "Manufacturer".into(),
                value: "Example Forge".into(),
                source_url: None,
                is_female: None,
            },
            MetadataAttribute {
                kind: "model".into(),
                label: "Model".into(),
                value: "Ridge".into(),
                source_url: None,
                is_female: None,
            },
        ];
        if let Some(profile) = profile {
            attributes.push(MetadataAttribute {
                kind: "catalog_profile".into(),
                label: "Catalog profile".into(),
                value: profile.into(),
                source_url: None,
                is_female: None,
            });
        }
        let page = ScrapedItem {
            page_url: url.clone(),
            title: "Fixture".into(),
            site_key: site.into(),
            raw_html: String::new(),
            attributes,
            images: vec![ScrapedImage {
                source_url: format!("{url}/photo.jpg"),
                preview_url: None,
                alt_text: None,
                width: None,
                height: None,
                sort_order: 0,
                local_file_path: None,
            }],
        };
        database.upsert_scraped_item(&page).unwrap();
    }
    assert_eq!(
        database.library_profiles().unwrap(),
        ["catalog-sample", "knife"]
    );
    let knives = SearchQuery {
        catalog_profile: Some(" KNIFE ".into()),
        ..Default::default()
    };
    assert_eq!(database.search_images(&knives).unwrap().len(), 3);
    let site = SearchQuery {
        site_key: Some("first-source".into()),
        ..Default::default()
    };
    assert_eq!(database.search_images(&site).unwrap().len(), 2);
    let other = SearchQuery {
        catalog_profile: Some("catalog-sample".into()),
        ..Default::default()
    };
    assert_eq!(database.search_images(&other).unwrap().len(), 1);
    let recent = database
        .get_date_group_page_for_scope(
            None,
            DateBrowseDirection::Initial,
            false,
            None,
            None,
            Some("knife"),
        )
        .unwrap();
    assert_eq!(recent.total, 3);
    assert!(recent.group.is_some());
}

#[test]
#[ignore = "imports selected source records from a local archive into a temporary library"]
fn imports_local_archive_samples_from_selected_sources() {
    use crate::knife::preview::preview;
    use std::path::PathBuf;

    let index = PathBuf::from(std::env::var_os("ARCHIVESIFT_KNIFE_INDEX").unwrap());
    let sources = std::env::var("ARCHIVESIFT_KNIFE_SAMPLE_HOSTS").unwrap();
    let library = tempfile::tempdir().unwrap();
    let mut plan = preview(&index, library.path()).unwrap();
    let mut selected = Vec::new();
    for host in sources.split(',') {
        let index = plan
            .importable
            .iter()
            .position(|record| {
                !record.images.is_empty()
                    && record.sources.iter().any(|source| {
                        url::Url::parse(source).ok().is_some_and(|url| {
                            url.host_str()
                                .is_some_and(|name| name.trim_start_matches("www.") == host)
                        })
                    })
            })
            .unwrap_or_else(|| panic!("no archived record with local photos for {host}"));
        selected.push(plan.importable.remove(index));
    }
    plan.records = selected.len();
    plan.importable = selected;
    let database = Database::new(library.path().join("library.sqlite3"));
    let report = database
        .import_knife_archive(&plan, library.path())
        .unwrap();
    assert_eq!(report.new_pages, plan.records, "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");
    let images = database
        .search_images(&SearchQuery {
            catalog_profile: Some("knife".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(images.len(), report.copied_files);
    let pages = images
        .iter()
        .map(|item| item.page_id())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(pages.len(), plan.records);
    for item in &images {
        let path = item.local_file_path.as_ref().unwrap();
        assert!(library.path().join(path).is_file());
    }
    println!(
        "{} source records and {} local photos imported",
        plan.records,
        images.len()
    );
}

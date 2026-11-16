use super::*;
use crate::fs::torrent::{valid_year, RELEASE_DATE_KINDS};
use crate::fs::torrent_metainfo::TorrentMetainfo;
use crate::models::{
    torrent_year_bounds, TorrentAttachment, TorrentCard, TorrentEdit, TorrentFacet, TorrentField,
    TorrentImage, TorrentPage, TorrentQuery, TorrentSummary, TorrentSummaryPage,
    TORRENT_UNAVAILABLE_NOTICE, TORRENT_VIDEO_FIELDS,
};
use rusqlite::functions::FunctionFlags;
use std::io::Read;

fn fold(value: &str) -> String {
    value.trim().to_lowercase()
}

fn fold_video(value: &str) -> String {
    fold(value).replace('.', "").replace('×', "x")
}

fn read_torrent_names(root: &Path, relative: &str) -> Option<Vec<String>> {
    const LIMIT: u64 = 8 * 1024 * 1024;
    let portable = relative.replace('\\', "/");
    let path = Path::new(&portable);
    if !crate::fs::torrent::is_torrent_file(&portable)
        || !path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let path = root.join(path).canonicalize().ok()?;
    if !path.starts_with(root) {
        return None;
    }
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > LIMIT {
        return None;
    }
    Some(TorrentMetainfo::parse(&bytes).ok()?.file_names())
}

pub(super) fn initialize(connection: &mut Connection) -> AppResult<()> {
    let tx = connection.transaction()?;
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS torrent_cards (
            page_id INTEGER PRIMARY KEY REFERENCES source_pages(id) ON DELETE CASCADE,
            primary_studio TEXT NOT NULL DEFAULT '', directory TEXT,
            folder_name TEXT,
            primary_image_id INTEGER REFERENCES images(id) ON DELETE SET NULL,
            revision INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS page_attribute_overrides (
            page_id INTEGER NOT NULL REFERENCES source_pages(id) ON DELETE CASCADE,
            kind TEXT NOT NULL, normalized_value TEXT NOT NULL, value TEXT NOT NULL,
            removed INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(page_id,kind,normalized_value)
        );
        CREATE TABLE IF NOT EXISTS torrent_moves (
            page_id INTEGER PRIMARY KEY REFERENCES source_pages(id) ON DELETE CASCADE,
            payload TEXT NOT NULL, committed INTEGER NOT NULL DEFAULT 0
        );
        INSERT OR IGNORE INTO torrent_cards(page_id,primary_studio)
        SELECT sp.id, COALESCE((
            SELECT a.value FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
            WHERE pa.page_id=sp.id AND a.kind='studio' ORDER BY a.id LIMIT 1
        ), '') FROM source_pages sp WHERE
            EXISTS(SELECT 1 FROM page_attachments f WHERE f.page_id=sp.id) OR
            EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
                WHERE pa.page_id=sp.id AND a.kind IN ('attachment','magnet'));
        INSERT OR IGNORE INTO app_schema_migrations(migration_key) VALUES('torrent-library-v1');
    "#,
    )?;
    let has_primary_image = tx
        .prepare("PRAGMA table_info(torrent_cards)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "primary_image_id");
    if !has_primary_image {
        tx.execute_batch("ALTER TABLE torrent_cards ADD COLUMN primary_image_id INTEGER REFERENCES images(id) ON DELETE SET NULL;")?;
    }
    let has_folder_name = tx
        .prepare("PRAGMA table_info(torrent_cards)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "folder_name");
    if !has_folder_name {
        tx.execute_batch("ALTER TABLE torrent_cards ADD COLUMN folder_name TEXT;")?;
    }
    let attachment_columns = tx
        .prepare("PRAGMA table_info(page_attachments)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !attachment_columns
        .iter()
        .any(|name| name == "torrent_file_names")
    {
        tx.execute_batch("ALTER TABLE page_attachments ADD COLUMN torrent_file_names TEXT;")?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_torrent_cards_primary_image ON torrent_cards(primary_image_id) WHERE primary_image_id IS NOT NULL;
         INSERT OR IGNORE INTO app_schema_migrations(migration_key) VALUES('torrent-primary-image-v1');",
    )?;
    // Earlier builds stored these releases as ordinary image pages. Give
    // them a studio destination without touching already downloaded files.
    let missing = {
        let mut query = tx.prepare(
            "SELECT DISTINCT sp.id, sp.site_key, sp.page_url, sp.title
             FROM attributes a JOIN page_attributes pa ON pa.attribute_id=a.id
             JOIN source_pages sp ON sp.id=pa.page_id
             LEFT JOIN torrent_cards tc ON tc.page_id=sp.id
             WHERE a.kind='notice' AND a.label=?1 AND tc.page_id IS NULL",
        )?;
        let rows = query
            .query_map([TORRENT_UNAVAILABLE_NOTICE], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let ids = missing.iter().map(|(id, _, _, _)| *id).collect::<Vec<_>>();
    let mut metadata = batch_load_page_attributes(&tx, &ids)?;
    for (page_id, site, url, title) in missing {
        register_studio_card(
            &tx,
            page_id,
            &site,
            &url,
            &title,
            &metadata.remove(&page_id).unwrap_or_default(),
        )?;
        apply_overrides(&tx, page_id)?;
        refresh_page_search_index(&tx, page_id)?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn after_scrape(
    tx: &Transaction<'_>,
    page: &ScrapedItem,
    page_id: i64,
) -> AppResult<()> {
    if page.attributes.iter().any(|a| {
        matches!(a.kind.as_str(), "attachment" | "magnet")
            || (a.kind == "notice" && a.label == TORRENT_UNAVAILABLE_NOTICE)
    }) {
        register_studio_card(
            tx,
            page_id,
            &page.site_key,
            &page.page_url,
            &page.title,
            &page.attributes,
        )?;
    }
    tx.execute(
        "UPDATE torrent_cards SET revision=revision+1 WHERE page_id=?1",
        [page_id],
    )?;
    apply_overrides(tx, page_id)
}

fn register_studio_card(
    tx: &Transaction<'_>,
    page_id: i64,
    site: &str,
    url: &str,
    title: &str,
    attributes: &[MetadataAttribute],
) -> AppResult<()> {
    let value = |kind: &str| {
        attributes
            .iter()
            .find(|a| a.kind == kind && !a.value.trim().is_empty())
            .map(|a| a.value.as_str())
    };
    let studio = value("studio").unwrap_or_default();
    let directory = crate::fs::torrent::torrent_directory_with_year(
        site,
        studio,
        value("content_title").unwrap_or(title),
        url,
        page_id,
        crate::fs::torrent::production_year(attributes),
    );
    tx.execute(
        "INSERT OR IGNORE INTO torrent_cards(page_id,primary_studio,directory) VALUES(?1,?2,?3)",
        params![page_id, studio, directory.to_string_lossy()],
    )?;
    Ok(())
}

fn apply_overrides(tx: &Transaction<'_>, page_id: i64) -> AppResult<()> {
    tx.execute(
        "DELETE FROM page_attributes WHERE page_id=?1 AND attribute_id IN (
        SELECT a.id FROM attributes a JOIN page_attribute_overrides o
        ON o.kind=a.kind AND (o.normalized_value=a.normalized_value OR o.normalized_value='')
        WHERE o.page_id=?1 AND o.removed=1)",
        [page_id],
    )?;
    let mut q = tx.prepare(
        "SELECT kind,value FROM page_attribute_overrides WHERE page_id=?1 AND removed=0",
    )?;
    let additions = q
        .query_map([page_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(q);
    for (kind, value) in additions {
        let existing = tx
            .query_row(
                "SELECT id FROM attributes WHERE kind=?1 AND normalized_value=?2",
                params![kind, value.trim().to_ascii_lowercase()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => id,
            None => upsert_attribute(
                tx,
                &MetadataAttribute {
                    label: kind.clone(),
                    kind,
                    value,
                    source_url: None,
                    is_female: None,
                },
            )?,
        };
        tx.execute(
            "INSERT OR IGNORE INTO page_attributes(page_id,attribute_id) VALUES(?1,?2)",
            params![page_id, id],
        )?;
    }
    tx.execute("DELETE FROM page_attributes WHERE page_id=?1 AND attribute_id IN (SELECT id FROM attributes WHERE kind='primary_studio')", [page_id])?;
    let studio: Option<String> = tx
        .query_row(
            "SELECT primary_studio FROM torrent_cards WHERE page_id=?1",
            [page_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(value) = studio.filter(|s| !s.is_empty()) {
        let id = upsert_attribute(
            tx,
            &MetadataAttribute {
                kind: "primary_studio".into(),
                label: "Primary studio".into(),
                value,
                source_url: None,
                is_female: None,
            },
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO page_attributes(page_id,attribute_id) VALUES(?1,?2)",
            params![page_id, id],
        )?;
    }
    Ok(())
}

fn replace_field(
    tx: &Transaction<'_>,
    page_id: i64,
    kind: &str,
    values: &[String],
) -> AppResult<()> {
    tx.execute(
        "DELETE FROM page_attribute_overrides WHERE page_id=?1 AND kind=?2",
        params![page_id, kind],
    )?;
    // An empty key with removed=1 suppresses the entire scraped field.
    tx.execute(
        "INSERT INTO page_attribute_overrides(page_id,kind,normalized_value,value,removed)
                VALUES(?1,?2,'','',1)",
        params![page_id, kind],
    )?;
    for value in values.iter().filter(|value| !value.trim().is_empty()) {
        tx.execute(
            "INSERT INTO page_attribute_overrides(page_id,kind,normalized_value,value,removed)
                    VALUES(?1,?2,?3,?4,0)",
            params![page_id, kind, value.trim().to_ascii_lowercase(), value],
        )?;
    }
    Ok(())
}

fn read_attachment(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<TorrentAttachment> {
    let contents = row
        .get::<_, Option<String>>(start + 3)?
        .map(|json| {
            serde_json::from_str(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    start + 3,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?
        .unwrap_or_default();
    Ok(TorrentAttachment {
        source_url: row.get(start)?,
        filename: row.get(start + 1)?,
        local_file_path: row.get(start + 2)?,
        contents,
    })
}

fn read_cards(connection: &Connection, ids: &[i64]) -> AppResult<Vec<TorrentCard>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let mut statement = connection.prepare(&format!("SELECT sp.id,sp.site_key,sp.page_url,sp.title,tc.primary_studio,tc.directory,tc.revision,tc.primary_image_id,tc.folder_name FROM torrent_cards tc JOIN source_pages sp ON sp.id=tc.page_id WHERE tc.page_id IN ({placeholders})"))?;
    let headers = statement
        .query_map(params_from_iter(ids), |r| {
            Ok(TorrentCard {
                page_id: r.get(0)?,
                site_key: r.get(1)?,
                page_url: r.get(2)?,
                title: r.get(3)?,
                primary_studio: r.get(4)?,
                directory: r.get(5)?,
                revision: r.get(6)?,
                primary_image_id: r.get(7)?,
                folder_name: r.get(8)?,
                attributes: vec![],
                images: vec![],
                attachments: vec![],
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut cards = headers
        .into_iter()
        .map(|card| (card.page_id, card))
        .collect::<HashMap<_, _>>();
    for (id, attributes) in batch_load_page_attributes(connection, ids)? {
        if let Some(card) = cards.get_mut(&id) {
            card.attributes = attributes;
        }
    }
    let mut statement = connection.prepare(&format!("SELECT pi.page_id,pi.image_id,i.source_url,pi.local_file_path,pi.thumbnail_path FROM page_images pi JOIN images i ON i.id=pi.image_id WHERE pi.page_id IN ({placeholders}) ORDER BY pi.page_id,pi.sort_order,pi.image_id"))?;
    let rows = statement.query_map(params_from_iter(ids), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            TorrentImage {
                image_id: r.get(1)?,
                source_url: r.get(2)?,
                local_file_path: r.get(3)?,
                thumbnail_path: r.get(4)?,
            },
        ))
    })?;
    for row in rows {
        let (id, image) = row?;
        if let Some(card) = cards.get_mut(&id) {
            card.images.push(image);
        }
    }
    let mut statement = connection.prepare(&format!("SELECT page_id,source_url,filename,local_file_path,torrent_file_names FROM page_attachments WHERE page_id IN ({placeholders}) ORDER BY page_id,filename,source_url"))?;
    let rows = statement.query_map(params_from_iter(ids), |r| {
        Ok((r.get::<_, i64>(0)?, read_attachment(r, 1)?))
    })?;
    for row in rows {
        let (id, attachment) = row?;
        if let Some(card) = cards.get_mut(&id) {
            card.attachments.push(attachment);
        }
    }
    ids.iter()
        .map(|id| {
            cards
                .remove(id)
                .ok_or_else(|| "card changed; reload results".into())
        })
        .collect()
}

fn read_card(connection: &Connection, page_id: i64) -> AppResult<TorrentCard> {
    let mut card = connection.query_row("SELECT sp.id,sp.site_key,sp.page_url,sp.title,tc.primary_studio,tc.directory,tc.revision,tc.primary_image_id,tc.folder_name FROM torrent_cards tc JOIN source_pages sp ON sp.id=tc.page_id WHERE tc.page_id=?1", [page_id], |r| Ok(TorrentCard {
        page_id:r.get(0)?,site_key:r.get(1)?,page_url:r.get(2)?,title:r.get(3)?,primary_studio:r.get(4)?,directory:r.get(5)?,revision:r.get(6)?,primary_image_id:r.get(7)?,folder_name:r.get(8)?,attributes:vec![],images:vec![],attachments:vec![]
    }))?;
    card.attributes = batch_load_page_attributes(connection, &[page_id])?
        .remove(&page_id)
        .unwrap_or_default();
    let mut q = connection.prepare("SELECT pi.image_id,i.source_url,pi.local_file_path,pi.thumbnail_path FROM page_images pi JOIN images i ON i.id=pi.image_id WHERE pi.page_id=?1 ORDER BY pi.sort_order,pi.image_id")?;
    card.images = q
        .query_map([page_id], |r| {
            Ok(TorrentImage {
                image_id: r.get(0)?,
                source_url: r.get(1)?,
                local_file_path: r.get(2)?,
                thumbnail_path: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut q = connection.prepare("SELECT source_url,filename,local_file_path,torrent_file_names FROM page_attachments WHERE page_id=?1 ORDER BY filename,source_url")?;
    card.attachments = q
        .query_map([page_id], |r| read_attachment(r, 0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn searchable_page(db: &Database, id: usize, fields: &[(&str, &str)]) -> i64 {
        let page = ScrapedItem {
            site_key: "example-source".into(),
            page_url: format!("https://example.test/release/{id}"),
            title: format!("Example release {id}"),
            raw_html: String::new(),
            images: vec![],
            attributes: fields
                .iter()
                .chain([("notice", "unavailable")].iter())
                .map(|&(kind, value)| MetadataAttribute {
                    kind: kind.into(),
                    label: if kind == "notice" {
                        TORRENT_UNAVAILABLE_NOTICE
                    } else {
                        kind
                    }
                    .into(),
                    value: value.into(),
                    source_url: None,
                    is_female: None,
                })
                .collect(),
        };
        db.upsert_scraped_item(&page).unwrap().page_id
    }

    #[test]
    fn legacy_payload_file_search_indexes_only_files_inside_the_library() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::new(root.path().join("search.sqlite3"));
        db.initialize().unwrap();
        let base = root.path().join("library");
        fs::create_dir_all(base.join("nested")).unwrap();
        let inside = searchable_page(&db, 1, &[("quality", "WEB-DL"), ("audio_codec", "AAC")]);
        let outside = searchable_page(&db, 2, &[("quality", "WEB-DL"), ("audio_codec", "AAC")]);
        for file in [
            base.join("nested/legacy.torrent"),
            root.path().join("outside.torrent"),
        ] {
            fs::write(file, b"d4:infod6:lengthi1e4:name16:feature-clip.mp4ee").unwrap();
        }
        db.save_page_attachment(
            inside,
            "https://example.test/1",
            "legacy.torrent",
            r"nested\legacy.torrent",
        )
        .unwrap();
        db.save_page_attachment(
            outside,
            "https://example.test/2",
            "outside.torrent",
            "../outside.torrent",
        )
        .unwrap();
        let query = TorrentQuery {
            filename: "CLIP.MP4".into(),
            video: "web-dl aac".into(),
            ..Default::default()
        };
        assert!(db.torrent_card(inside).unwrap().attachments[0]
            .contents
            .is_empty());
        let result = db
            .search_torrent_summaries_with_storage(&query, Some(&base), None)
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.cards[0].page_id, inside);
        assert_eq!(
            db.torrent_card(inside).unwrap().attachments[0].contents,
            ["feature-clip.mp4"]
        );
        assert!(db.torrent_card(outside).unwrap().attachments[0]
            .contents
            .is_empty());
        assert_eq!(db.search_torrents(&query).unwrap().total, 1);
    }

    #[test]
    fn torrent_video_terms_match_structured_data_and_older_full_descriptions() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::new(root.path().join("search.sqlite3"));
        db.initialize().unwrap();
        let modern = searchable_page(
            &db,
            1,
            &[
                ("resolution", "3840×2160"),
                ("video_codec", "H.264/AVC"),
                ("audio_codec", "AAC"),
                ("video_filename", "Proper Cut.mkv"),
            ],
        );
        searchable_page(
            &db,
            2,
            &[("full_description", "Video: 3840x2160 H.264\nAudio: AAC")],
        );
        searchable_page(
            &db,
            3,
            &[
                ("resolution", "3840x2160"),
                ("video_codec", "H.264"),
                ("audio_codec", "FLAC"),
            ],
        );
        let mut query = TorrentQuery {
            video: "3840x2160 H264 aac".into(),
            ..Default::default()
        };
        assert_eq!(db.search_torrents(&query).unwrap().total, 2);
        query.filename = "per cut.MKV".into();
        let result = db.search_torrents(&query).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.cards[0].page_id, modern);
    }

    #[test]
    fn torrent_facet_suggestions_match_effective_studios_tags_and_genres() {
        let root = tempfile::tempdir().unwrap();
        let db = Database::new(root.path().join("search.sqlite3"));
        db.initialize().unwrap();
        searchable_page(
            &db,
            1,
            &[
                ("studio", "Primary Studio"),
                ("tag", "Drama Camera"),
                ("genre", "Drama"),
            ],
        );
        searchable_page(&db, 2, &[("tag", "Drama Legacy")]);
        let choices = |facet, prefix| db.suggest_torrent_facet(facet, prefix, 8).unwrap();
        assert_eq!(choices(TorrentFacet::Studio, "ARY st"), ["Primary Studio"]);
        assert_eq!(
            choices(TorrentFacet::Tag, "dram"),
            ["Drama Camera", "Drama Legacy"]
        );
        assert_eq!(
            choices(TorrentFacet::Genre, "dram"),
            ["Drama", "Drama Legacy"]
        );
    }

    #[test]
    fn previously_unclassified_releases_can_be_organized_into_their_recorded_studio() {
        use crate::services::torrent_library::TorrentLibrary;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("legacy.sqlite3");
        let base = root.path().join("downloads");
        let db = Database::new(&path);
        db.initialize().unwrap();
        let mut page = ScrapedItem {
            site_key: "example-source".into(),
            page_url: "https://example.test/release/42".into(),
            title: "Legacy release".into(),
            raw_html: String::new(),
            images: vec![ScrapedImage {
                source_url: "https://example.test/image.jpg".into(),
                preview_url: None,
                alt_text: None,
                width: None,
                height: None,
                sort_order: 0,
                local_file_path: None,
            }],
            attributes: [
                ("studio", "Studio", "Archive Studio"),
                ("actor", "Actor", "Example Actor"),
                (
                    "notice",
                    TORRENT_UNAVAILABLE_NOTICE,
                    "The release has no visible download link.",
                ),
            ]
            .into_iter()
            .map(|(kind, label, value)| MetadataAttribute {
                kind: kind.into(),
                label: label.into(),
                value: value.into(),
                source_url: None,
                is_female: None,
            })
            .collect(),
        };
        let persisted = db.upsert_scraped_item(&page).unwrap();
        let mut connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "DELETE FROM torrent_cards WHERE page_id=?1",
                [persisted.page_id],
            )
            .unwrap();
        let legacy = PathBuf::from("example-source/example_actor/legacy/001.jpg");
        std::fs::create_dir_all(base.join(legacy.parent().unwrap())).unwrap();
        std::fs::write(base.join(&legacy), b"legacy image").unwrap();
        db.update_image_file_location(
            persisted.page_id,
            persisted.image_ids_by_url[&page.images[0].source_url],
            &legacy.to_string_lossy(),
        )
        .unwrap();

        page.page_url = "https://example.test/gallery/1".into();
        page.attributes.last_mut().unwrap().label = "Image preview fallback".into();
        page.attributes.last_mut().unwrap().value = "The preview was retained.".into();
        let gallery_id = db.upsert_scraped_item(&page).unwrap().page_id;
        initialize(&mut connection).unwrap();
        initialize(&mut connection).unwrap();
        assert!(!db.is_torrent_page(gallery_id).unwrap());
        let card = db.torrent_card(persisted.page_id).unwrap();
        assert_eq!(card.primary_studio, "Archive Studio");
        assert_eq!(
            card.images[0].local_file_path.as_deref(),
            Some(legacy.to_string_lossy().as_ref())
        );
        assert!(base.join(&legacy).is_file());

        let directory = base.join(card.directory.as_ref().unwrap());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("description.txt"),
            b"Already in the studio directory",
        )
        .unwrap();
        let library = TorrentLibrary::new(db.clone(), base.clone());
        let plan = library
            .prepare(card.page_id, card.revision, TorrentEdit::from(&card), true)
            .unwrap();
        assert_eq!(plan.to_directory, card.directory);
        assert_eq!(plan.files.len(), 1);
        let saved = library.apply(plan, |_| {}).unwrap();
        let saved_path = base.join(saved.images[0].local_file_path.as_ref().unwrap());
        assert!(saved_path.starts_with(directory.join("images")));
        assert_eq!(std::fs::read(saved_path).unwrap(), b"legacy image");
        assert_eq!(
            std::fs::read(directory.join("description.txt")).unwrap(),
            b"Already in the studio directory"
        );
        assert!(!base.join(legacy).exists());
    }

    #[test]
    fn primary_image_migration_preserves_existing_cards_and_is_repeatable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("legacy.sqlite3");
        let db = Database::new(&path);
        db.initialize().unwrap();
        let mut connection = Connection::open(&path).unwrap();
        connection.execute_batch("DROP TABLE torrent_cards;
            CREATE TABLE torrent_cards(page_id INTEGER PRIMARY KEY REFERENCES source_pages(id) ON DELETE CASCADE, primary_studio TEXT NOT NULL DEFAULT '', directory TEXT, revision INTEGER NOT NULL DEFAULT 0);
            INSERT INTO source_pages(id,site_key,page_url,title,raw_html) VALUES(7,'example','https://example.test/7','Legacy','');
            INSERT INTO torrent_cards(page_id,primary_studio,directory,revision) VALUES(7,'Selected studio','old/release',9);").unwrap();
        initialize(&mut connection).unwrap();
        initialize(&mut connection).unwrap();
        let card = db.torrent_card(7).unwrap();
        assert_eq!(card.primary_studio, "Selected studio");
        assert_eq!(card.directory.as_deref(), Some("old/release"));
        assert_eq!(card.revision, 9);
        assert_eq!(card.primary_image_id, None);
        assert_eq!(card.folder_name, None);
        assert!(card.primary_image().is_none());
    }

    #[test]
    fn migration_discovers_legacy_attachment_only_pages_without_touching_their_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("legacy.sqlite3");
        let db = Database::new(&path);
        db.initialize().unwrap();
        let mut connection = Connection::open(&path).unwrap();
        connection.execute_batch("DROP TABLE torrent_moves; DROP TABLE page_attribute_overrides; DROP TABLE torrent_cards;
            INSERT INTO source_pages(id,site_key,page_url,title,raw_html) VALUES(7,'example','https://example.test/7','Legacy','');
            INSERT INTO page_attachments(page_id,source_url,filename,local_file_path) VALUES(7,'https://example.test/file','release.torrent','old/location/release.torrent');").unwrap();
        initialize(&mut connection).unwrap();
        initialize(&mut connection).unwrap();
        let card = db.torrent_card(7).unwrap();
        assert!(card.directory.is_none());
        assert!(card.images.is_empty());
        assert_eq!(
            card.attachments[0].local_file_path,
            "old/location/release.torrent"
        );
        assert_eq!(
            db.search_torrents(&TorrentQuery::default()).unwrap().total,
            1
        );
    }
}

impl Database {
    pub(crate) fn suggest_torrent_facet(
        &self,
        facet: TorrentFacet,
        query: &str,
        limit: usize,
    ) -> AppResult<Vec<String>> {
        let connection = self.connect()?;
        connection.create_scalar_function(
            "torrent_fold",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| Ok(fold(ctx.get_raw(0).as_str()?)),
        )?;
        let mut sql = String::from(
            "WITH facets(value) AS (
             SELECT a.value FROM attributes a WHERE a.kind=?1 AND EXISTS(
                 SELECT 1 FROM page_attributes pa JOIN torrent_cards tc ON tc.page_id=pa.page_id
                 WHERE pa.attribute_id=a.id)",
        );
        match facet {
            TorrentFacet::Studio => {
                sql.push_str(" UNION ALL SELECT primary_studio FROM torrent_cards")
            }
            TorrentFacet::Tag => {}
            TorrentFacet::Genre => sql.push_str(
                " UNION ALL SELECT a.value FROM attributes a WHERE a.kind='tag' AND EXISTS(
                    SELECT 1 FROM page_attributes pa JOIN torrent_cards tc ON tc.page_id=pa.page_id
                    WHERE pa.attribute_id=a.id AND NOT EXISTS(
                        SELECT 1 FROM page_attributes pg JOIN attributes g ON g.id=pg.attribute_id
                        WHERE pg.page_id=pa.page_id AND g.kind='genre' AND trim(g.value)<>''))",
            ),
        }
        sql.push_str(
            ") SELECT min(value) FROM facets
             WHERE trim(value)<>'' AND instr(torrent_fold(value),?2)>0
             GROUP BY torrent_fold(value)
             ORDER BY instr(torrent_fold(value),?2)=1 DESC, torrent_fold(value) LIMIT ?3",
        );
        let mut statement = connection.prepare(&sql)?;
        let values = statement
            .query_map(params![facet.kind(), fold(query), limit as i64], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(values)
    }

    fn index_legacy_torrent_names(
        &self,
        root: &Path,
        site: &str,
        cancellation: Option<&QueryCancellation>,
    ) -> AppResult<()> {
        let Ok(root) = root.canonicalize() else {
            return Ok(());
        };
        let connection = self.connect()?;
        let guard = cancellation
            .map(|token| token.register(&connection))
            .transpose()?;
        let paths = {
            let mut statement = connection.prepare(
                "SELECT f.page_id, f.source_url, f.local_file_path
                 FROM page_attachments f JOIN source_pages sp ON sp.id=f.page_id
                 WHERE f.torrent_file_names IS NULL AND lower(substr(f.filename,-8))='.torrent'
                   AND (?1='' OR sp.site_key=?1)",
            )?;
            let paths = statement
                .query_map([site], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            paths
        };
        drop(guard);
        drop(connection);
        let mut indexed = Vec::new();
        for (id, source, path) in paths {
            if cancellation.is_some_and(QueryCancellation::is_cancelled) {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                    None,
                )
                .into());
            }
            if let Some(names) = read_torrent_names(&root, &path) {
                indexed.push((id, source, path, serde_json::to_string(&names)?));
            }
        }
        if !indexed.is_empty() {
            let mut writer = self.writer_connection()?;
            let tx = writer.transaction()?;
            {
                let mut update = tx.prepare(
                    "UPDATE page_attachments SET torrent_file_names=?4
                     WHERE page_id=?1 AND source_url=?2 AND local_file_path=?3
                       AND torrent_file_names IS NULL",
                )?;
                for (id, source, path, names) in indexed {
                    update.execute(params![id, source, path, names])?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }

    pub(crate) fn is_torrent_page(&self, page_id: i64) -> AppResult<bool> {
        Ok(self.connect()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM torrent_cards WHERE page_id=?1)",
            [page_id],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn archive_path_is_shared(&self, page_id: i64, path: &str) -> AppResult<bool> {
        Ok(self.connect()?.query_row("SELECT EXISTS(SELECT 1 FROM page_images WHERE page_id<>?1 AND replace(local_file_path,char(92),'/')=?2 UNION ALL SELECT 1 FROM page_attachments WHERE page_id<>?1 AND replace(local_file_path,char(92),'/')=?2)", params![page_id,path.replace('\\',"/")], |r| r.get(0))?)
    }

    pub fn torrent_card(&self, page_id: i64) -> AppResult<TorrentCard> {
        let connection = self.connect()?;
        read_card(&connection, page_id)
    }

    pub fn torrent_card_cancellable(
        &self,
        page_id: i64,
        cancellation: &QueryCancellation,
    ) -> AppResult<TorrentCard> {
        let connection = self.connect()?;
        let _guard = cancellation.register(&connection)?;
        read_card(&connection, page_id)
    }

    pub fn search_torrents(&self, query: &TorrentQuery) -> AppResult<TorrentPage> {
        let (ids, total) = self.torrent_search_ids(query, None, None)?;
        let connection = self.connect()?;
        let cards = read_cards(&connection, &ids)?;
        Ok(TorrentPage { cards, total })
    }

    pub fn search_torrent_summaries(&self, query: &TorrentQuery) -> AppResult<TorrentSummaryPage> {
        self.search_torrent_summaries_with_storage(query, None, None)
    }

    pub fn search_torrent_summaries_cancellable(
        &self,
        query: &TorrentQuery,
        cancellation: &QueryCancellation,
    ) -> AppResult<TorrentSummaryPage> {
        self.search_torrent_summaries_with_storage(query, None, Some(cancellation))
    }

    pub(crate) fn search_torrent_summaries_with_storage(
        &self,
        query: &TorrentQuery,
        root: Option<&Path>,
        cancellation: Option<&QueryCancellation>,
    ) -> AppResult<TorrentSummaryPage> {
        let (ids, total) = self.torrent_search_ids(query, root, cancellation)?;
        if ids.is_empty() {
            return Ok(TorrentSummaryPage {
                cards: vec![],
                total,
            });
        }
        let connection = self.connect()?;
        let _guard = cancellation
            .map(|token| token.register(&connection))
            .transpose()?;
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut statement = connection.prepare(&format!(
            "SELECT sp.id,sp.site_key,tc.primary_studio,
             COALESCE((SELECT value FROM page_attribute_overrides
               WHERE page_id=sp.id AND kind='content_title' AND removed=0 LIMIT 1),
               (SELECT a.value FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
               WHERE pa.page_id=sp.id AND a.kind='content_title' AND trim(a.value)<>'' ORDER BY a.id LIMIT 1),sp.title),
             (SELECT COUNT(*) FROM page_images pi WHERE pi.page_id=sp.id),
             (SELECT COUNT(*) FROM page_attachments f WHERE f.page_id=sp.id),
             sp.page_url
             FROM torrent_cards tc JOIN source_pages sp ON sp.id=tc.page_id WHERE sp.id IN ({placeholders})
             ORDER BY sp.scraped_at DESC,sp.id DESC"))?;
        let cards = statement
            .query_map(params_from_iter(&ids), |r| {
                Ok(TorrentSummary {
                    page_id: r.get(0)?,
                    site_key: r.get(1)?,
                    primary_studio: r.get(2)?,
                    content_title: r.get(3)?,
                    image_count: r.get(4)?,
                    attachment_count: r.get(5)?,
                    page_url: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(TorrentSummaryPage { cards, total })
    }

    fn torrent_search_ids(
        &self,
        query: &TorrentQuery,
        root: Option<&Path>,
        cancellation: Option<&QueryCancellation>,
    ) -> AppResult<(Vec<i64>, usize)> {
        let year_bounds = torrent_year_bounds(&query.year)?;
        let filename = fold(&query.filename);
        if let Some(root) = root.filter(|_| !filename.is_empty()) {
            self.index_legacy_torrent_names(root, query.site_key.trim(), cancellation)?;
        }
        let connection = self.connect()?;
        let _guard = cancellation
            .map(|token| token.register(&connection))
            .transpose()?;
        let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
        connection.create_scalar_function("torrent_fold", 1, flags, |ctx| {
            Ok(fold(ctx.get_raw(0).as_str()?))
        })?;
        let mut sql = String::from(
            " FROM torrent_cards tc JOIN source_pages sp ON sp.id=tc.page_id WHERE 1=1",
        );
        let mut values = Vec::<String>::new();
        if !query.text.trim().is_empty() {
            sql.push_str(" AND (instr(lower(sp.title),lower(?))>0 OR EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id WHERE pa.page_id=sp.id AND instr(a.normalized_value,lower(?))>0))
");
            values.extend([query.text.trim().into(), query.text.trim().into()]);
        }
        let title = query.title.trim();
        if !title.is_empty() {
            sql.push_str(
                " AND (instr(torrent_fold(sp.title),?)>0 OR EXISTS(
                    SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
                    WHERE pa.page_id=sp.id AND a.kind='content_title'
                      AND instr(torrent_fold(a.value),?)>0))",
            );
            let title = title.to_lowercase();
            values.extend([title.clone(), title]);
        }
        if !filename.is_empty() {
            sql.push_str(
                " AND (EXISTS(SELECT 1 FROM page_attachments f WHERE f.page_id=sp.id
                    AND (instr(torrent_fold(f.filename),?)>0 OR EXISTS(
                      SELECT 1 FROM json_each(f.torrent_file_names) n
                      WHERE instr(torrent_fold(n.value),?)>0)))
                    OR EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
                      WHERE pa.page_id=sp.id AND a.kind='video_filename' AND instr(torrent_fold(a.value),?)>0))",
            );
            values.extend([filename.clone(), filename.clone(), filename]);
        }
        let mut video_terms = query
            .video
            .split_whitespace()
            .map(fold_video)
            .filter(|term| !term.is_empty())
            .peekable();
        if video_terms.peek().is_some() {
            connection.create_scalar_function("torrent_video_fold", 1, flags, |ctx| {
                Ok(fold_video(ctx.get_raw(0).as_str()?))
            })?;
            let kinds = TORRENT_VIDEO_FIELDS
                .map(|(kind, _)| format!("'{kind}'"))
                .join(",");
            for term in video_terms {
                sql.push_str(&format!(
                    " AND EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
                      WHERE pa.page_id=sp.id AND a.kind IN ({kinds}, 'full_description')
                        AND instr(torrent_video_fold(a.value),?)>0)"
                ));
                values.push(term);
            }
        }
        if !query.site_key.trim().is_empty() {
            sql.push_str(" AND sp.site_key=?");
            values.push(query.site_key.trim().into());
        }
        if !query.studio.trim().is_empty() {
            sql.push_str(" AND (lower(tc.primary_studio)=lower(?) OR EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id WHERE pa.page_id=sp.id AND a.kind='studio' AND a.normalized_value=lower(?)))");
            values.extend([query.studio.trim().into(), query.studio.trim().into()]);
        }
        super::creators::append_filter(
            self,
            &mut sql,
            &mut values,
            "sp.id",
            None,
            &query.creators,
        )?;
        for actor in query
            .actors
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
        {
            sql.push_str(
                " AND EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
                    WHERE pa.page_id=sp.id AND a.kind='actor' AND instr(torrent_fold(a.value),?)>0)",
            );
            values.push(actor.to_lowercase());
        }
        for (kind, entries) in [("tag", &query.tags), ("category", &query.categories)] {
            for entry in entries.iter().filter(|s| !s.trim().is_empty()) {
                sql.push_str(" AND EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id WHERE pa.page_id=sp.id AND a.kind=? AND torrent_fold(a.value)=?)");
                values.extend([kind.into(), entry.trim().to_lowercase()]);
            }
        }
        for genre in query
            .genres
            .iter()
            .map(|genre| genre.trim())
            .filter(|genre| !genre.is_empty())
        {
            // Older parsers stored genres as tags. Prefer explicit genres whenever present.
            sql.push_str(
                " AND EXISTS(SELECT 1 FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
                    WHERE pa.page_id=sp.id AND torrent_fold(a.value)=? AND (
                        a.kind='genre' OR (a.kind='tag' AND NOT EXISTS(
                            SELECT 1 FROM page_attributes pg JOIN attributes g ON g.id=pg.attribute_id
                            WHERE pg.page_id=sp.id AND g.kind='genre' AND trim(g.value)<>''
                        ))
                    ))",
            );
            values.push(genre.to_lowercase());
        }
        if let Some((from, to)) = year_bounds {
            connection.create_scalar_function("torrent_year", 1, flags, |ctx| {
                Ok(valid_year(ctx.get_raw(0).as_str()?).map(str::to_owned))
            })?;
            let kinds = RELEASE_DATE_KINDS.map(|kind| format!("'{kind}'")).join(",");
            let priority = RELEASE_DATE_KINDS
                .iter()
                .enumerate()
                .map(|(index, kind)| format!(" WHEN '{kind}' THEN {index}"))
                .collect::<String>();
            sql.push_str(&format!(
                " AND (SELECT torrent_year(a.value) AS release_year
                    FROM page_attributes pa JOIN attributes a ON a.id=pa.attribute_id
                    WHERE pa.page_id=sp.id AND a.kind IN ({kinds}) AND release_year IS NOT NULL
                    ORDER BY CASE a.kind{priority} END, a.id LIMIT 1) BETWEEN ? AND ?"
            ));
            values.extend([from.into(), to.into()]);
        }
        if query.missing_torrent {
            sql.push_str(
                " AND NOT EXISTS(SELECT 1 FROM page_attachments f
                WHERE f.page_id=sp.id AND length(f.filename)>8
                  AND lower(substr(f.filename,-8))='.torrent' AND trim(f.local_file_path)<>''",
            );
            if let Some(root) = root {
                if !root.is_dir() {
                    return Err("Library storage unavailable.".into());
                }
                let root = root.to_path_buf();
                let checked = std::cell::RefCell::new(HashMap::<String, bool>::new());
                connection.create_scalar_function(
                    "torrent_exists",
                    1,
                    FunctionFlags::SQLITE_UTF8,
                    move |ctx| {
                        let path = ctx.get_raw(0).as_str()?;
                        let mut checked = checked.borrow_mut();
                        Ok(*checked.entry(path.to_owned()).or_insert_with(|| {
                            crate::fs::torrent::local_torrent_file_exists(&root, path)
                        }))
                    },
                )?;
                sql.push_str(" AND torrent_exists(f.local_file_path)");
            }
            sql.push(')');
        }
        let total: usize = connection.query_row(
            &format!("SELECT COUNT(*){sql}"),
            params_from_iter(&values),
            |r| r.get(0),
        )?;
        let mut q = connection.prepare(&format!(
            "SELECT sp.id{sql} ORDER BY sp.scraped_at DESC,sp.id DESC LIMIT 40 OFFSET {}",
            query.offset
        ))?;
        let ids = q
            .query_map(params_from_iter(&values), |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((ids, total))
    }

    pub(crate) fn update_torrent_primary_image(
        &self,
        page_id: i64,
        revision: i64,
        image_id: Option<i64>,
    ) -> AppResult<()> {
        let connection = self.writer_connection()?;
        let updated = connection.execute(
            "UPDATE torrent_cards SET primary_image_id = ?3, revision = revision + 1
             WHERE page_id = ?1 AND revision = ?2
             AND NOT EXISTS (SELECT 1 FROM torrent_moves WHERE page_id = ?1)
             AND (?3 IS NULL OR EXISTS (
                 SELECT 1 FROM page_images WHERE page_id = ?1 AND image_id = ?3
             ))",
            params![page_id, revision, image_id],
        )?;
        if updated != 1 {
            return Err(
                "The card or its images changed. Reopen the card before choosing its main image."
                    .into(),
            );
        }
        Ok(())
    }

    pub(crate) fn torrent_directory(&self, page_id: i64) -> AppResult<Option<PathBuf>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT directory FROM torrent_cards WHERE page_id=?1",
                [page_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .map(PathBuf::from))
    }
    pub(crate) fn attachment_path(
        &self,
        page_id: i64,
        source_url: &str,
    ) -> AppResult<Option<PathBuf>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT local_file_path FROM page_attachments WHERE page_id=?1 AND source_url=?2",
                params![page_id, source_url],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(PathBuf::from))
    }
    pub(crate) fn torrent_journals(&self) -> AppResult<Vec<(i64, String, bool)>> {
        let connection = self.connect()?;
        let mut q = connection
            .prepare("SELECT page_id,payload,committed FROM torrent_moves ORDER BY page_id")?;
        let result = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
    pub(crate) fn begin_torrent_move(
        &self,
        page_id: i64,
        revision: i64,
        payload: &str,
    ) -> AppResult<()> {
        let mut connection = self.writer_connection()?;
        let tx = connection.transaction()?;
        let current: i64 = tx.query_row(
            "SELECT revision FROM torrent_cards WHERE page_id=?1",
            [page_id],
            |r| r.get(0),
        )?;
        if current != revision {
            return Err("card changed; reopen the editor and confirm the updated plan".into());
        }
        tx.execute(
            "INSERT INTO torrent_moves(page_id,payload) VALUES(?1,?2)",
            params![page_id, payload],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn finish_torrent_move(
        &self,
        page_id: i64,
        directory: Option<&str>,
        edit: &TorrentEdit,
        image_paths: &[(i64, String)],
        attachment_paths: &[(String, String)],
    ) -> AppResult<()> {
        let mut connection = self.writer_connection()?;
        let tx = connection.transaction()?;
        let card = read_card(&tx, page_id)?;
        if edit
            .primary_image_id
            .is_some_and(|id| !card.images.iter().any(|image| image.image_id == id))
        {
            return Err("primary image must belong to this torrent card".into());
        }
        for (kind, new) in [
            ("studio", &edit.studios),
            ("tag", &edit.tags),
            ("category", &edit.categories),
        ]
        .into_iter()
        .chain(edit.actors.iter().map(|actors| ("actor", actors)))
        {
            let old = card.values(kind);
            let normalize = |s: &str| s.trim().to_ascii_lowercase();
            for value in old.iter().filter(|v| {
                !normalize(v).is_empty() && !new.iter().any(|n| normalize(n) == normalize(v))
            }) {
                tx.execute("INSERT INTO page_attribute_overrides(page_id,kind,normalized_value,value,removed) VALUES(?1,?2,?3,?4,1) ON CONFLICT(page_id,kind,normalized_value) DO UPDATE SET removed=1,value=excluded.value", params![page_id,kind,normalize(value),value])?;
            }
            for value in new.iter().filter(|v| {
                !old.iter().any(|o| {
                    if kind == "actor" {
                        o == *v
                    } else {
                        normalize(o) == normalize(v)
                    }
                })
            }) {
                tx.execute("INSERT INTO page_attribute_overrides(page_id,kind,normalized_value,value,removed) VALUES(?1,?2,?3,?4,0) ON CONFLICT(page_id,kind,normalized_value) DO UPDATE SET removed=0,value=excluded.value", params![page_id,kind,normalize(value),value])?;
            }
        }
        if let Some(directors) = edit
            .directors
            .as_ref()
            .filter(|values| **values != card.values("director"))
        {
            replace_field(&tx, page_id, "director", directors)?;
        }
        for (kind, value) in [
            ("description", &edit.description),
            ("full_description", &edit.full_description),
        ] {
            if let Some(value) = value
                .as_ref()
                .filter(|value| value.as_str() != card.value(kind))
            {
                replace_field(&tx, page_id, kind, std::slice::from_ref(value))?;
            }
        }
        for (&field, value) in &edit.details {
            if field.value(&card) == value {
                continue;
            }
            let values = std::slice::from_ref(value);
            if field == TorrentField::Release {
                let active_kind = if value.len() == 4 { "year" } else { "date" };
                for kind in crate::fs::torrent::RELEASE_DATE_KINDS {
                    replace_field(
                        &tx,
                        page_id,
                        kind,
                        if kind == active_kind { values } else { &[] },
                    )?;
                }
            } else {
                replace_field(&tx, page_id, field.kind(), values)?;
            }
        }
        tx.execute("UPDATE torrent_cards SET primary_studio=?2,directory=?3,primary_image_id=?4,folder_name=?5,revision=revision+1 WHERE page_id=?1", params![page_id,edit.primary_studio,directory,edit.primary_image_id,edit.folder_name])?;
        apply_overrides(&tx, page_id)?;
        for (id, path) in image_paths {
            tx.execute("UPDATE page_images SET local_file_path=?3,updated_at=CURRENT_TIMESTAMP WHERE page_id=?1 AND image_id=?2", params![page_id,id,path])?;
        }
        for (source, path) in attachment_paths {
            tx.execute(
                "UPDATE page_attachments SET local_file_path=?3 WHERE page_id=?1 AND source_url=?2",
                params![page_id, source, path],
            )?;
        }
        refresh_page_search_index(&tx, page_id)?;
        tx.execute(
            "UPDATE torrent_moves SET committed=1 WHERE page_id=?1",
            [page_id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn clear_torrent_journal(&self, page_id: i64) -> AppResult<()> {
        self.writer_connection()?
            .execute("DELETE FROM torrent_moves WHERE page_id=?1", [page_id])?;
        Ok(())
    }
}

use std::collections::BTreeMap;

use rusqlite::{params, Connection};
use url::Url;

use crate::image_access::{public_image_origin, ImageAccessIssue};
use crate::AppResult;

use super::Database;

#[derive(Debug, Default)]
pub(crate) struct ImageAccessCard {
    pub title: String,
    pub page_url: String,
    pub resources: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub(crate) struct ImageAccessGroup {
    pub extension_id: String,
    pub origin: String,
    pub approved: bool,
    pub cards: BTreeMap<i64, ImageAccessCard>,
}

pub(super) fn initialize(connection: &Connection) -> AppResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS image_origin_grants (
             extension_id TEXT NOT NULL,
             origin TEXT NOT NULL,
             granted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
             PRIMARY KEY (extension_id, origin)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS image_access_issues (
             page_id INTEGER NOT NULL REFERENCES source_pages(id) ON DELETE CASCADE,
             extension_id TEXT NOT NULL,
             origin TEXT NOT NULL,
             resource TEXT NOT NULL,
             reason TEXT NOT NULL,
             PRIMARY KEY (page_id, extension_id, origin, resource)
         ) WITHOUT ROWID;

         CREATE INDEX IF NOT EXISTS idx_image_access_issues_origin
             ON image_access_issues(extension_id, origin, page_id);",
    )?;
    Ok(())
}

impl Database {
    pub(crate) fn seed_legacy_image_access(
        &self,
        registry: &crate::scraper::ScraperRegistry,
    ) -> AppResult<()> {
        let reader = self.connect()?;
        let mut seeded = reader
            .prepare("SELECT EXISTS(SELECT 1 FROM app_schema_migrations WHERE migration_key=?1)")?;
        let mut pending = Vec::new();
        for scraper in registry.extensions() {
            let Some(extension_id) = scraper.extension_id() else {
                continue;
            };
            let key = format!("image-access-v1/{extension_id}");
            if !seeded.query_row([&key], |row| row.get::<_, bool>(0))? {
                pending.push((scraper, extension_id, key));
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        let mut connection = self.writer_connection()?;
        let tx = connection.transaction()?;
        for (scraper, extension_id, key) in pending {
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO app_schema_migrations(migration_key) VALUES (?1)",
                [&key],
            )?;
            if inserted == 0 {
                continue;
            }
            let mut read = tx.prepare(
                "SELECT p.id,p.page_url,a.value
                 FROM attributes a JOIN page_attributes pa ON pa.attribute_id=a.id
                 JOIN source_pages p ON p.id=pa.page_id
                 WHERE a.kind='notice' AND a.label='Image host permission' AND p.site_key=?1",
            )?;
            let mut write = tx.prepare(
                "INSERT OR IGNORE INTO image_access_issues(page_id,extension_id,origin,resource,reason)
                 VALUES (?1,?2,?3,?4,?5)",
            )?;
            let mut rows = read.query([scraper.site_key()])?;
            while let Some(row) = rows.next()? {
                let url: String = row.get(1)?;
                if registry
                    .scraper_for_url(&url)
                    .and_then(|active| active.extension_id())
                    != Some(extension_id)
                {
                    continue;
                }
                let value: String = row.get(2)?;
                let issue = ImageAccessIssue::for_reference(&value, "discovered");
                let Some(issue) = issue else { continue };
                if scraper
                    .asset_origins()
                    .is_some_and(|origins| origins.contains(&issue.origin))
                {
                    continue;
                }
                write.execute(params![
                    row.get::<_, i64>(0)?,
                    extension_id,
                    issue.origin,
                    issue.resource,
                    issue.reason
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn image_origin_grants(&self, extension_id: &str) -> AppResult<Vec<String>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT origin FROM image_origin_grants WHERE extension_id = ?1 ORDER BY origin",
        )?;
        let rows = statement.query_map([extension_id], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub(crate) fn update_image_access_issues(
        &self,
        page_id: i64,
        extension_id: &str,
        issues: &[ImageAccessIssue],
        replace: bool,
    ) -> AppResult<()> {
        let mut connection = self.writer_connection()?;
        let tx = connection.transaction()?;
        if replace {
            tx.execute(
                "DELETE FROM image_access_issues WHERE page_id = ?1 AND extension_id = ?2",
                params![page_id, extension_id],
            )?;
        }
        {
            let mut statement = tx.prepare(
                "INSERT INTO image_access_issues(page_id,extension_id,origin,resource,reason)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT (page_id,extension_id,origin,resource) DO UPDATE SET reason=excluded.reason",
            )?;
            for issue in issues {
                statement.execute(params![
                    page_id,
                    extension_id,
                    issue.origin,
                    issue.resource,
                    issue.reason
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn image_access_groups(&self) -> AppResult<Vec<ImageAccessGroup>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "WITH entries AS (
                 SELECT extension_id,origin FROM image_origin_grants
                 UNION SELECT extension_id,origin FROM image_access_issues
             )
             SELECT e.extension_id,e.origin,g.origin IS NOT NULL,p.id,p.title,p.page_url,i.resource,i.reason
             FROM entries e
             LEFT JOIN image_access_issues i USING(extension_id,origin)
             LEFT JOIN image_origin_grants g USING(extension_id,origin)
             LEFT JOIN source_pages p ON p.id=i.page_id
             ORDER BY e.extension_id,e.origin,p.id,i.resource",
        )?;
        let mut rows = statement.query([])?;
        let mut groups: BTreeMap<_, ImageAccessGroup> = BTreeMap::new();
        while let Some(row) = rows.next()? {
            let extension_id: String = row.get(0)?;
            let origin: String = row.get(1)?;
            let approved = row.get(2)?;
            let group = groups
                .entry((extension_id.clone(), origin.clone()))
                .or_insert_with(|| ImageAccessGroup {
                    extension_id,
                    origin,
                    approved,
                    cards: BTreeMap::new(),
                });
            if let Some(id) = row.get::<_, Option<i64>>(3)? {
                let card = match group.cards.entry(id) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(ImageAccessCard {
                            title: row.get(4)?,
                            page_url: row.get(5)?,
                            resources: Vec::new(),
                        })
                    }
                };
                card.resources.push((row.get(6)?, row.get(7)?));
            }
        }
        Ok(groups.into_values().collect())
    }

    /// Persist an explicit user decision. A grant may only originate from a recorded issue.
    pub(crate) fn set_image_origin_grant(
        &self,
        extension_id: &str,
        origin: &str,
        approve: bool,
    ) -> AppResult<()> {
        if public_image_origin(&Url::parse(origin)?).as_deref() != Some(origin) {
            return Err("Invalid image origin".into());
        }
        let connection = self.writer_connection()?;
        if approve {
            let updated = connection.execute(
                "INSERT INTO image_origin_grants(extension_id, origin)
                 SELECT ?1, ?2 WHERE EXISTS (
                     SELECT 1 FROM image_access_issues WHERE extension_id=?1 AND origin=?2
                 ) ON CONFLICT(extension_id, origin) DO NOTHING",
                params![extension_id, origin],
            )?;
            if updated == 0 && !connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM image_origin_grants WHERE extension_id=?1 AND origin=?2)",
                params![extension_id, origin],
                |r| r.get::<_, bool>(0),
            )? {
                return Err("This extension no longer has images waiting for that host".into());
            }
        } else {
            connection.execute(
                "DELETE FROM image_origin_grants WHERE extension_id=?1 AND origin=?2",
                params![extension_id, origin],
            )?;
        }
        Ok(())
    }

    pub(crate) fn image_origin_cards(
        &self,
        extension_id: &str,
        origin: &str,
    ) -> AppResult<Vec<String>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT p.page_url FROM image_access_issues i
             JOIN image_origin_grants g USING (extension_id, origin)
             JOIN source_pages p ON p.id=i.page_id
             WHERE i.extension_id=?1 AND i.origin=?2 ORDER BY p.page_url",
        )?;
        let rows = statement.query_map(params![extension_id, origin], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ScrapedItem;

    #[test]
    fn image_grants_and_resource_queue_persist_per_extension_and_card() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join("library.db")
            .to_string_lossy()
            .into_owned();
        let db = Database::new(&path);
        db.initialize().unwrap();
        let pages: Vec<_> = ["one", "two"]
            .iter()
            .map(|card| {
                db.upsert_scraped_item(&ScrapedItem {
                    site_key: "demo".into(),
                    page_url: format!("https://demo.example.test/{card}"),
                    title: card.to_string(),
                    raw_html: String::new(),
                    images: vec![],
                    attributes: vec![],
                })
                .unwrap()
                .page_id
            })
            .collect();

        let origin = "https://cdn.example.test";
        let issue = ImageAccessIssue::for_url(
            Url::parse(&format!("{origin}/cover.png?token=example")).unwrap(),
            "discovered",
        )
        .unwrap();
        assert!(db.set_image_origin_grant("demo.one", origin, true).is_err());
        for page in &pages {
            db.update_image_access_issues(
                *page,
                "demo.one",
                &[issue.clone(), issue.clone()],
                false,
            )
            .unwrap();
        }
        db.update_image_access_issues(pages[1], "demo.two", std::slice::from_ref(&issue), false)
            .unwrap();
        db.set_image_origin_grant("demo.one", origin, true).unwrap();
        let db = Database::new(&path);
        assert_eq!(db.image_origin_cards("demo.one", origin).unwrap().len(), 2);
        assert!(db.image_origin_grants("demo.two").unwrap().is_empty());
        assert!(db
            .image_origin_cards("demo.two", origin)
            .unwrap()
            .is_empty());
        let groups = db.image_access_groups().unwrap();
        assert_eq!(
            groups[0].cards[&pages[0]].resources,
            vec![(format!("{origin}/cover.png"), "discovered".into())]
        );
        assert!(groups[0].approved);
        assert!(!groups[1].approved);
        db.update_image_access_issues(pages[0], "demo.one", &[], true)
            .unwrap();
        db.update_image_access_issues(pages[1], "demo.one", &[], false)
            .unwrap();
        assert_eq!(db.image_origin_cards("demo.one", origin).unwrap().len(), 1);
        db.set_image_origin_grant("demo.one", origin, false)
            .unwrap();
        assert!(db.image_origin_grants("demo.one").unwrap().is_empty());
        assert_eq!(db.image_access_groups().unwrap().len(), 2);
    }
}

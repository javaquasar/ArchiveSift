use super::{Error, Handoff, HandoffState, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::path::Path;

pub(super) struct Journal {
    connection: Connection,
}
impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path).map_err(|_| Error::Persistence)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| Error::Persistence)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS peerlume_handoffs (
            id TEXT PRIMARY KEY, profile TEXT NOT NULL, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS peerlume_handoff_profile ON peerlume_handoffs(profile);
            CREATE TABLE IF NOT EXISTS peerlume_handoff_hashes (
              profile TEXT NOT NULL, hash TEXT NOT NULL, handoff_id TEXT NOT NULL,
              PRIMARY KEY(profile,hash));
            CREATE TABLE IF NOT EXISTS peerlume_handoff_cards (
              handoff_id TEXT NOT NULL, page_id INTEGER NOT NULL, PRIMARY KEY(handoff_id,page_id));")
            .map_err(|_| Error::Persistence)?;
        Ok(Self { connection })
    }

    pub fn reserve(&self, record: Handoff, retry_failed: bool) -> Result<(Handoff, bool)> {
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
            .map_err(|_| Error::Persistence)?;
        let mut existing: Option<String> = None;
        for hash in record.hashes.keys() {
            let id: Option<String> = tx
                .query_row(
                    "SELECT handoff_id FROM peerlume_handoff_hashes WHERE profile=?1 AND hash=?2",
                    params![record.profile, hash],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| Error::Persistence)?;
            if let Some(id) = id {
                if existing.as_ref().is_some_and(|previous| previous != &id) {
                    return Err(Error::ReconciliationRequired);
                }
                existing = Some(id);
            }
        }
        if let Some(id) = existing {
            tx.execute(
                "INSERT OR IGNORE INTO peerlume_handoff_cards(handoff_id,page_id) VALUES (?1,?2)",
                params![id, record.page_id],
            )
            .map_err(|_| Error::Persistence)?;
            let json: String = tx
                .query_row(
                    "SELECT record FROM peerlume_handoffs WHERE id=?1",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(|_| Error::Persistence)?;
            let previous: Handoff = serde_json::from_str(&json).map_err(|_| Error::Persistence)?;
            if !retry_failed || previous.state != HandoffState::Failed {
                tx.commit().map_err(|_| Error::Persistence)?;
                return Ok((previous, false));
            }
            // Only a newly reviewed and explicitly confirmed retry can release
            // a failed reservation. Uncertain delivery never enters this path.
            tx.execute(
                "DELETE FROM peerlume_handoff_hashes WHERE profile=?1 AND handoff_id=?2",
                params![record.profile, id],
            )
            .map_err(|_| Error::Persistence)?;
        }
        let json = serde_json::to_string(&record).map_err(|_| Error::Persistence)?;
        tx.execute(
            "INSERT INTO peerlume_handoffs(id,profile,record) VALUES (?1,?2,?3)",
            params![record.id, record.profile, json],
        )
        .map_err(|_| Error::Persistence)?;
        for hash in record.hashes.keys() {
            tx.execute(
                "INSERT INTO peerlume_handoff_hashes(profile,hash,handoff_id) VALUES (?1,?2,?3)",
                params![record.profile, hash, record.id],
            )
            .map_err(|_| Error::Persistence)?;
        }
        tx.execute(
            "INSERT INTO peerlume_handoff_cards(handoff_id,page_id) VALUES (?1,?2)",
            params![record.id, record.page_id],
        )
        .map_err(|_| Error::Persistence)?;
        tx.commit().map_err(|_| Error::Persistence)?;
        Ok((record, true))
    }
    pub fn save(&self, record: &Handoff) -> Result<()> {
        let json = serde_json::to_string(record).map_err(|_| Error::Persistence)?;
        self.connection
            .execute(
                "UPDATE peerlume_handoffs SET record=?1 WHERE id=?2 AND profile=?3",
                params![json, record.id, record.profile],
            )
            .map_err(|_| Error::Persistence)?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Handoff> {
        let json: String = self
            .connection
            .query_row(
                "SELECT record FROM peerlume_handoffs WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .map_err(|_| Error::Persistence)?;
        serde_json::from_str(&json).map_err(|_| Error::Persistence)
    }
    pub fn list(&self, profile: &str) -> Result<Vec<Handoff>> {
        let mut query = self.connection.prepare("SELECT record FROM peerlume_handoffs WHERE profile=?1 ORDER BY rowid DESC LIMIT 1000").map_err(|_| Error::Persistence)?;
        let rows = query
            .query_map([profile], |row| row.get::<_, String>(0))
            .map_err(|_| Error::Persistence)?;
        rows.map(|row| {
            serde_json::from_str(&row.map_err(|_| Error::Persistence)?)
                .map_err(|_| Error::Persistence)
        })
        .collect()
    }
}

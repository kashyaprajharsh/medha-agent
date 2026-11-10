//! Pinned chats, kept beside the log so every app that lists the folder's chats sees them.

use super::*;
use std::collections::HashSet;

/// A pinned chat is marked in `store_meta` under this prefix.
pub(crate) const PINNED: &str = "pinned:";

impl SqliteLog {
    pub fn set_pinned(&self, chat: Ulid, pinned: bool) -> Result<(), StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let key = format!("{PINNED}{chat}");
        let changed = if pinned {
            conn.execute(
                "INSERT OR REPLACE INTO store_meta (key, value) VALUES (?1, '1')",
                [key],
            )
        } else {
            conn.execute("DELETE FROM store_meta WHERE key = ?1", [key])
        };
        changed
            .map(|_| ())
            .map_err(|error| StoreError::Db(error.to_string()))
    }

    pub fn pinned_chats(&self) -> Result<HashSet<Ulid>, StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let mut stmt = conn
            .prepare("SELECT substr(key, ?1) FROM store_meta WHERE substr(key, 1, ?2) = ?3")
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![PINNED.len() + 1, PINNED.len(), PINNED],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| StoreError::Db(error.to_string()))?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|id| Ulid::from_string(&id).ok())
            .collect())
    }
}

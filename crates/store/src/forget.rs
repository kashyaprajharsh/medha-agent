//! Deleting a chat from a log whose every row is chained to the one before.

use super::*;
use std::collections::HashSet;

/// A chat being deleted is marked in `store_meta` under this prefix with when it was, so every list leaves it out.
pub(crate) const DELETING: &str = "deleting:";
/// A mark nobody finished or undid, as after a crash, stops hiding its chat after this long.
pub(crate) const DELETING_FOR: f64 = 60.0;

/// What a deleted chat leaves to be cleaned up outside the log.
pub struct Forgotten {
    /// Stored outputs that only the deleted rows named.
    pub artifacts: Vec<String>,
}

pub(crate) fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64())
}

fn failed(error: impl std::fmt::Display) -> StoreError {
    StoreError::Db(error.to_string())
}

/// The chat and every sub-agent it started, however deep, in the order found.
fn family(conn: &Connection, chat: Ulid) -> Result<Vec<Ulid>, StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(payload, '$.child') FROM events
             WHERE session_id = ?1 AND kind = 'agent.spawned' ORDER BY rowid",
        )
        .map_err(failed)?;
    let mut found = vec![chat];
    let mut next = 0;
    while let Some(parent) = found.get(next).copied() {
        next += 1;
        let children = stmt
            .query_map([parent.to_string()], |row| row.get::<_, Option<String>>(0))
            .map_err(failed)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(failed)?;
        for child in children.into_iter().flatten() {
            if let Ok(child) = Ulid::from_string(&child)
                && !found.contains(&child)
            {
                found.push(child);
            }
        }
    }
    Ok(found)
}

/// Every stored-output name in `text`: a run of exactly 64 hex digits.
fn named_outputs(text: &str, into: &mut HashSet<String>) {
    for run in text.split(|c: char| !c.is_ascii_hexdigit()) {
        if run.len() == 64 {
            into.insert(run.to_ascii_lowercase());
        }
    }
}

/// A kept row as the chain hashes it: its kind and payload exactly as stored.
fn hashed(row: &Row, prev: [u8; 32]) -> Result<[u8; 32], StoreError> {
    let corrupt = || StoreError::Db(format!("corrupt event row {}", row.id));
    let event = Event {
        id: Ulid::from_string(&row.id).map_err(|_| corrupt())?,
        session_id: Ulid::from_string(&row.session_id).map_err(|_| corrupt())?,
        parent_id: row
            .parent_id
            .as_deref()
            .and_then(|parent| Ulid::from_string(parent).ok()),
        // Never hashed: the chain covers the kind as stored.
        kind: EventKind::Session,
        payload: serde_json::Value::Null,
        trust: TrustLabel::parse(&row.trust).ok_or_else(corrupt)?,
        provenance: Provenance {
            source: row.provenance.clone(),
        },
        prev_hash: prev,
        hash_version: row.hash_version,
        ts: row.ts,
    };
    Ok(chain_hash_stored(
        &prev,
        &event,
        &row.kind,
        row.payload.as_bytes(),
    ))
}

impl SqliteLog {
    /// Leaves these chats out of every list until they are deleted or shown again.
    pub fn hide_chats(&self, chats: &[Ulid]) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| failed("lock poisoned"))?;
        for chat in chats {
            conn.execute(
                "INSERT OR REPLACE INTO store_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("{DELETING}{chat}"), now().to_string()],
            )
            .map_err(failed)?;
        }
        Ok(())
    }

    pub fn show_chats(&self, chats: &[Ulid]) -> Result<(), StoreError> {
        let conn = self.conn.lock().map_err(|_| failed("lock poisoned"))?;
        for chat in chats {
            conn.execute(
                "DELETE FROM store_meta WHERE key = ?1",
                [format!("{DELETING}{chat}")],
            )
            .map_err(failed)?;
        }
        Ok(())
    }

    pub fn chat_with_agents(&self, chat: Ulid) -> Result<Vec<Ulid>, StoreError> {
        let conn = self.conn.lock().map_err(|_| failed("lock poisoned"))?;
        family(&conn, chat)
    }

    /// Deletes `chat` and its sub-agents, keeping their memory saves, and links the rest anew.
    /// Refused unless the sub-agents are still `expected`, so none the caller did not lock goes.
    pub fn forget_chat(&self, chat: Ulid, expected: &[Ulid]) -> Result<Forgotten, StoreError> {
        let mut conn = self.conn.lock().map_err(|_| failed("lock poisoned"))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(failed)?;
        let chats = family(&tx, chat)?;
        if chats != expected {
            return Err(failed(
                "the chat changed while it was being deleted; try again",
            ));
        }
        // The whole chain is proven before any link changes, so nothing tampered is blessed.
        let (count, head) = validated_chain(&tx)?;
        check_anchor(&tx, count, &head)?;
        let leaving: HashSet<String> = chats.iter().map(Ulid::to_string).collect();
        let (mut gone, mut kept_names) = (HashSet::new(), HashSet::new());
        let (mut prev, mut kept) = ([0u8; 32], 0u64);
        for (rowid, row, stored) in load_chain_rows(&tx)? {
            if leaving.contains(&row.session_id) && row.kind != "memory.write" {
                named_outputs(&row.payload, &mut gone);
                tx.execute("DELETE FROM events WHERE rowid = ?1", [rowid])
                    .map_err(failed)?;
                continue;
            }
            named_outputs(&row.payload, &mut kept_names);
            let hash = hashed(&row, prev)?;
            if row.prev_hash != prev || stored != hash {
                tx.execute(
                    "UPDATE events SET prev_hash = ?1, hash = ?2 WHERE rowid = ?3",
                    rusqlite::params![prev.to_vec(), hash.to_vec(), rowid],
                )
                .map_err(failed)?;
            }
            prev = hash;
            kept += 1;
        }
        for session in &leaving {
            tx.execute(
                "DELETE FROM events_fts WHERE session_id = ?1 AND kind <> 'memory.write'",
                [session],
            )
            .map_err(failed)?;
            tx.execute(
                "DELETE FROM store_meta WHERE key = ?1",
                [format!("{DELETING}{session}")],
            )
            .map_err(failed)?;
        }
        set_chain_anchor(&tx, kept, &prev)?;
        tx.commit().map_err(failed)?;
        // This connection's proof described the old chain.
        *self
            .verified_version
            .lock()
            .map_err(|_| failed("lock poisoned"))? = None;
        let mut artifacts: Vec<String> = gone.difference(&kept_names).cloned().collect();
        artifacts.sort();
        Ok(Forgotten { artifacts })
    }
}

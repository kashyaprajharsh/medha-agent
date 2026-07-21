//! Persistent event log (§4.2). A SQLite (WAL) implementation of the kernel's
//! `EventLog` trait, with a real SHA-256 hash chain making the log
//! tamper-evident (P3). Drop-in replacement for the in-memory log (P8); state
//! is still a projection of these events.

use async_trait::async_trait;
use kernel::events::chain_hash;
use kernel::{
    ArtifactStore, Event, EventKind, EventLog, KernelError, Provenance, SessionMeta, TrustLabel,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use ulid::Ulid;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SessionSearchHit {
    pub session_id: Ulid,
    pub event_id: Ulid,
    pub kind: String,
    pub snippet: String,
    pub ts: f64,
    pub source: String,
}

fn search_text(kind: &EventKind, payload: &serde_json::Value) -> Option<String> {
    let text = match kind {
        EventKind::UserMessage | EventKind::ModelText => payload.get("text")?.as_str()?.to_string(),
        EventKind::ModelMessage => {
            let message: kernel::ModelMessage = serde_json::from_value(payload.clone()).ok()?;
            message
                .parts
                .iter()
                .filter_map(|part| match part {
                    kernel::ContentPart::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        }
        EventKind::ToolObs => match payload.get("payload")? {
            serde_json::Value::String(text) => text.clone(),
            value => serde_json::to_string(value).ok()?,
        },
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

fn event_source(event: &Event) -> &'static str {
    if event.provenance.source == "automation"
        || event.payload.get("source").and_then(|value| value.as_str()) == Some("automation")
    {
        "automation"
    } else {
        "interactive"
    }
}

fn fts_match_expr(raw: &str) -> Option<String> {
    let terms = raw
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn backfill_event_fts(conn: &mut Connection) -> Result<(), StoreError> {
    let done = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = 'event_fts_backfilled_v1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .is_some();
    if done {
        return Ok(());
    }

    let tx = conn
        .transaction()
        .map_err(|error| StoreError::Db(error.to_string()))?;
    tx.execute("DELETE FROM events_fts", [])
        .map_err(|error| StoreError::Db(error.to_string()))?;
    let rows = {
        let mut stmt = tx
            .prepare(
                "SELECT id, session_id, kind, payload, provenance, ts FROM events ORDER BY rowid",
            )
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                ))
            })
            .map_err(|error| StoreError::Db(error.to_string()))?;
        mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| StoreError::Db(error.to_string()))?
    };
    for (event_id, session_id, kind, payload, provenance, ts) in rows {
        let Some(kind_value) = EventKind::parse(&kind) else {
            continue;
        };
        let payload_value = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
        let Some(text) = search_text(&kind_value, &payload_value) else {
            continue;
        };
        let source = if provenance == "automation"
            || payload_value.get("source").and_then(|value| value.as_str()) == Some("automation")
        {
            "automation"
        } else {
            "interactive"
        };
        tx.execute(
            "INSERT INTO events_fts (event_id, session_id, kind, text, source, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![event_id, session_id, kind, text, source, ts],
        )
        .map_err(|error| StoreError::Db(error.to_string()))?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO store_meta (key, value) VALUES ('event_fts_backfilled_v1', '1')",
        [],
    )
    .map_err(|error| StoreError::Db(error.to_string()))?;
    tx.commit()
        .map_err(|error| StoreError::Db(error.to_string()))
}

/// Content-addressed blob store on disk (§4.2/§4.5). Blobs live under a dir,
/// named by their SHA-256 hash, so identical content is stored once.
pub struct FileArtifactStore {
    dir: PathBuf,
}

impl FileArtifactStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(Self { dir })
    }

    /// Reject anything but a hex hash, so a hash can never escape the dir.
    fn safe_path(&self, hash: &str) -> Result<PathBuf, String> {
        if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid artifact hash".into());
        }
        Ok(self.dir.join(hash))
    }
}

impl ArtifactStore for FileArtifactStore {
    fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let mut h = Sha256::new();
        h.update(bytes);
        let hash = format!("{:x}", h.finalize());
        let path = self.dir.join(&hash);
        if !path.exists() {
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        }
        Ok(hash)
    }

    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String> {
        let data = std::fs::read(self.safe_path(hash)?).map_err(|e| e.to_string())?;
        let start = offset.min(data.len());
        let end = match len {
            Some(l) => (start + l).min(data.len()),
            None => data.len(),
        };
        Ok(data[start..end].to_vec())
    }

    fn size(&self, hash: &str) -> Result<usize, String> {
        let meta = std::fs::metadata(self.safe_path(hash)?).map_err(|e| e.to_string())?;
        Ok(meta.len() as usize)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("db: {0}")]
    Db(String),
}

pub struct SqliteLog {
    conn: Mutex<Connection>,
}

impl SqliteLog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let mut conn = Connection::open(path).map_err(|e| StoreError::Db(e.to_string()))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                rowid      INTEGER PRIMARY KEY AUTOINCREMENT,
                id         TEXT NOT NULL,
                session_id TEXT NOT NULL,
                parent_id  TEXT,
                kind       TEXT NOT NULL,
                payload    TEXT NOT NULL,
                trust      TEXT NOT NULL,
                provenance TEXT NOT NULL,
                prev_hash  BLOB NOT NULL,
                hash       BLOB NOT NULL,
                ts         REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
            CREATE TABLE IF NOT EXISTS store_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
                event_id UNINDEXED,
                session_id UNINDEXED,
                kind UNINDEXED,
                text,
                source UNINDEXED,
                ts UNINDEXED
            );",
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;
        backfill_event_fts(&mut conn)?;

        // The chain head is read from the DB inside each append's transaction
        // (see `append`), not cached — so a second MEDHA process on the same
        // workspace can't append against a stale head and corrupt the chain (K10).
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Verify the tamper-evident hash chain over the ENTIRE log (all sessions,
    /// in append/rowid order — the chain is global). For each event we confirm
    /// both that its `prev_hash` links to the running hash AND that recomputing
    /// its hash reproduces the stored `hash` column, so a direct edit to any row
    /// — including the last — is detected. Call this on open / session resume.
    pub fn verify(&self) -> Result<(), StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, parent_id, kind, payload, trust, provenance, prev_hash, hash, ts
                 FROM events ORDER BY rowid ASC",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    Row {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        parent_id: r.get(2)?,
                        kind: r.get(3)?,
                        payload: r.get(4)?,
                        trust: r.get(5)?,
                        provenance: r.get(6)?,
                        prev_hash: r.get(7)?,
                        ts: r.get(9)?,
                    },
                    r.get::<_, Vec<u8>>(8)?,
                ))
            })
            .map_err(|e| StoreError::Db(e.to_string()))?;

        let mut prev = [0u8; 32];
        for (index, row) in rows.enumerate() {
            let (row, stored_hash) = row.map_err(|e| StoreError::Db(e.to_string()))?;
            let event = row
                .into_event()
                .ok_or_else(|| StoreError::Db(format!("corrupt event row at index {index}")))?;
            if event.prev_hash != prev {
                return Err(StoreError::Db(format!(
                    "hash chain broken at event index {index}: prev_hash does not link"
                )));
            }
            let computed = chain_hash(&prev, &event);
            if computed.as_slice() != stored_hash.as_slice() {
                return Err(StoreError::Db(format!(
                    "hash chain broken at event index {index}: content does not match stored hash"
                )));
            }
            prev = computed;
        }
        Ok(())
    }

    /// List every session in the log, newest activity first — for the resume
    /// picker. Title is the first user message (truncated); a session with no
    /// user message (pre-logging, or empty) shows a placeholder.
    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>, StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT e1.session_id, MIN(e1.ts), MAX(e1.ts), COUNT(*),
                    (SELECT e2.payload FROM events e2
                     WHERE e2.session_id = e1.session_id AND e2.kind = 'user.message'
                     ORDER BY e2.rowid ASC LIMIT 1)
                 FROM events e1
                 GROUP BY e1.session_id
                 ORDER BY MAX(e1.ts) DESC",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| StoreError::Db(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows {
            let (id, started, last, count, first_user) =
                row.map_err(|e| StoreError::Db(e.to_string()))?;
            let Ok(id) = Ulid::from_string(&id) else {
                continue;
            };
            let title = first_user
                .as_deref()
                .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_owned))
                .map(|t| {
                    let t = t.trim().replace('\n', " ");
                    if t.chars().count() > 72 {
                        format!("{}…", t.chars().take(71).collect::<String>())
                    } else {
                        t
                    }
                })
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "(no user message)".to_string());
            out.push(SessionMeta {
                id,
                title,
                started_ts: started,
                last_ts: last,
                events: count as u64,
            });
        }
        Ok(out)
    }

    pub fn all_events(&self) -> Result<Vec<Event>, StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, parent_id, kind, payload, trust, provenance, prev_hash, ts
                 FROM events ORDER BY rowid ASC",
            )
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Row {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    parent_id: row.get(2)?,
                    kind: row.get(3)?,
                    payload: row.get(4)?,
                    trust: row.get(5)?,
                    provenance: row.get(6)?,
                    prev_hash: row.get(7)?,
                    ts: row.get(8)?,
                })
            })
            .map_err(|error| StoreError::Db(error.to_string()))?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(Row::into_event)
            .collect())
    }

    /// Search text-bearing events. Interactive sessions rank ahead of
    /// automation, which remains searchable rather than disappearing.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SessionSearchHit>, StoreError> {
        let Some(match_expr) = fts_match_expr(query) else {
            return Ok(Vec::new());
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, event_id, kind,
                        snippet(events_fts, 3, '[', ']', '…', 24), ts, source
                 FROM events_fts
                 WHERE events_fts MATCH ?1
                 ORDER BY CASE source WHEN 'automation' THEN 1 ELSE 0 END,
                          bm25(events_fts)
                 LIMIT ?2",
            )
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![match_expr, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let mut hits = Vec::new();
        for row in rows {
            let (session_id, event_id, kind, snippet, ts, source) =
                row.map_err(|error| StoreError::Db(error.to_string()))?;
            let (Ok(session_id), Ok(event_id)) =
                (Ulid::from_string(&session_id), Ulid::from_string(&event_id))
            else {
                continue;
            };
            hits.push(SessionSearchHit {
                session_id,
                event_id,
                kind,
                snippet,
                ts,
                source,
            });
        }
        Ok(hits)
    }

    /// Return a chronological event window centered on one event.
    pub fn window(
        &self,
        session_id: Ulid,
        around_event_id: Ulid,
        radius: usize,
    ) -> Result<Vec<Event>, StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let anchor = conn
            .query_row(
                "SELECT rowid FROM events WHERE session_id = ?1 AND id = ?2",
                rusqlite::params![session_id.to_string(), around_event_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT rowid, id, session_id, parent_id, kind, payload, trust, provenance, prev_hash, ts
                 FROM events
                 WHERE session_id = ?1
                 ORDER BY ABS(rowid - ?2), rowid
                 LIMIT ?3",
            )
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    session_id.to_string(),
                    anchor,
                    radius.saturating_mul(2).saturating_add(1) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        Row {
                            id: row.get(1)?,
                            session_id: row.get(2)?,
                            parent_id: row.get(3)?,
                            kind: row.get(4)?,
                            payload: row.get(5)?,
                            trust: row.get(6)?,
                            provenance: row.get(7)?,
                            prev_hash: row.get(8)?,
                            ts: row.get(9)?,
                        },
                    ))
                },
            )
            .map_err(|error| StoreError::Db(error.to_string()))?;
        let mut events = rows
            .filter_map(Result::ok)
            .filter_map(|(rowid, row)| row.into_event().map(|event| (rowid, event)))
            .collect::<Vec<_>>();
        events.sort_by_key(|(rowid, _)| *rowid);
        Ok(events.into_iter().map(|(_, event)| event).collect())
    }

    /// First and last text-bearing user/model events for discover-mode context.
    pub fn bookends(&self, session_id: Ulid, count: usize) -> Result<Vec<Event>, StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let read = |direction: &str| -> Result<Vec<(i64, Event)>, StoreError> {
            let sql = format!(
                "SELECT rowid, id, session_id, parent_id, kind, payload, trust, provenance, prev_hash, ts
                 FROM events WHERE session_id = ?1 AND kind IN ('user.message', 'model.text')
                 ORDER BY rowid {direction} LIMIT ?2"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|error| StoreError::Db(error.to_string()))?;
            let rows = stmt
                .query_map(
                    rusqlite::params![session_id.to_string(), count as i64],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            Row {
                                id: row.get(1)?,
                                session_id: row.get(2)?,
                                parent_id: row.get(3)?,
                                kind: row.get(4)?,
                                payload: row.get(5)?,
                                trust: row.get(6)?,
                                provenance: row.get(7)?,
                                prev_hash: row.get(8)?,
                                ts: row.get(9)?,
                            },
                        ))
                    },
                )
                .map_err(|error| StoreError::Db(error.to_string()))?;
            Ok(rows
                .filter_map(Result::ok)
                .filter_map(|(rowid, row)| row.into_event().map(|event| (rowid, event)))
                .collect())
        };
        let mut events = read("ASC")?;
        events.extend(read("DESC")?);
        events.sort_by_key(|(rowid, _)| *rowid);
        events.dedup_by_key(|(rowid, _)| *rowid);
        Ok(events.into_iter().map(|(_, event)| event).collect())
    }
}

#[async_trait]
impl EventLog for SqliteLog {
    async fn append(&self, mut e: Event) -> Result<Event, KernelError> {
        use rusqlite::OptionalExtension;
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| KernelError::Log("poisoned".into()))?;
        // Read the chain head and insert inside ONE `IMMEDIATE` transaction, so
        // the read-then-append is atomic against any other writer — including a
        // second MEDHA process on the same DB. Trusting an in-memory cached head
        // (the old design) let two processes both link off the same hash and
        // fail `verify()` as "tampering" (K10). SQLite's write lock serializes.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|err| KernelError::Log(err.to_string()))?;

        let head: Option<Vec<u8>> = tx
            .query_row(
                "SELECT hash FROM events ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|err| KernelError::Log(err.to_string()))?;
        let mut prev = [0u8; 32];
        if let Some(h) = head {
            if h.len() == 32 {
                prev.copy_from_slice(&h);
            }
        }
        e.prev_hash = prev;
        let hash = chain_hash(&e.prev_hash, &e);

        tx.execute(
            "INSERT INTO events
                (id, session_id, parent_id, kind, payload, trust, provenance, prev_hash, hash, ts)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            rusqlite::params![
                e.id.to_string(),
                e.session_id.to_string(),
                e.parent_id.map(|p| p.to_string()),
                e.kind.as_str(),
                e.payload.to_string(),
                e.trust.as_str(),
                &e.provenance.source,
                e.prev_hash.to_vec(),
                hash.to_vec(),
                e.ts,
            ],
        )
        .map_err(|err| KernelError::Log(err.to_string()))?;
        if let Some(text) = search_text(&e.kind, &e.payload) {
            tx.execute(
                "INSERT INTO events_fts (event_id, session_id, kind, text, source, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    e.id.to_string(),
                    e.session_id.to_string(),
                    e.kind.as_str(),
                    text,
                    event_source(&e),
                    e.ts,
                ],
            )
            .map_err(|err| KernelError::Log(err.to_string()))?;
        }
        tx.commit()
            .map_err(|err| KernelError::Log(err.to_string()))?;
        Ok(e)
    }

    async fn events(&self, session: Ulid) -> Vec<Event> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, session_id, parent_id, kind, payload, trust, provenance, prev_hash, ts
             FROM events WHERE session_id = ?1 ORDER BY rowid ASC",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map([session.to_string()], |r| {
            Ok(Row {
                id: r.get(0)?,
                session_id: r.get(1)?,
                parent_id: r.get(2)?,
                kind: r.get(3)?,
                payload: r.get(4)?,
                trust: r.get(5)?,
                provenance: r.get(6)?,
                prev_hash: r.get(7)?,
                ts: r.get(8)?,
            })
        });
        let mut out = Vec::new();
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                if let Some(ev) = row.into_event() {
                    out.push(ev);
                }
            }
        }
        out
    }

    /// Expose the session list through the trait so any surface (TUI picker,
    /// REPL, CLI) can browse sessions generically. Errors degrade to an empty
    /// list rather than failing the caller.
    async fn sessions(&self) -> Vec<SessionMeta> {
        self.list_sessions().unwrap_or_default()
    }
}

struct Row {
    id: String,
    session_id: String,
    parent_id: Option<String>,
    kind: String,
    payload: String,
    trust: String,
    provenance: String,
    prev_hash: Vec<u8>,
    ts: f64,
}

impl Row {
    fn into_event(self) -> Option<Event> {
        let mut prev = [0u8; 32];
        if self.prev_hash.len() == 32 {
            prev.copy_from_slice(&self.prev_hash);
        }
        Some(Event {
            id: Ulid::from_string(&self.id).ok()?,
            session_id: Ulid::from_string(&self.session_id).ok()?,
            parent_id: self.parent_id.and_then(|p| Ulid::from_string(&p).ok()),
            kind: EventKind::parse(&self.kind)?,
            payload: serde_json::from_str(&self.payload).unwrap_or(serde_json::Value::Null),
            trust: TrustLabel::parse(&self.trust)?,
            provenance: Provenance {
                source: self.provenance,
            },
            prev_hash: prev,
            ts: self.ts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_sessions_titles_and_orders_newest_first() {
        let dir = std::env::temp_dir().join(format!("medha-sess-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = SqliteLog::open(&db).unwrap();

        let s1 = kernel::Session {
            id: Ulid::new(),
            done: false,
            ..Default::default()
        };
        log.append(Event::user_message(&s1, "first task here"))
            .await
            .unwrap();
        log.append(Event::model_text(&s1, "ok")).await.unwrap();
        let s2 = kernel::Session {
            id: Ulid::new(),
            done: false,
            ..Default::default()
        };
        log.append(Event::user_message(&s2, "second task"))
            .await
            .unwrap();

        let sessions = log.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        // Newest activity first.
        assert_eq!(sessions[0].id, s2.id);
        assert_eq!(sessions[0].title, "second task");
        assert_eq!(sessions[1].id, s1.id);
        assert_eq!(sessions[1].title, "first task here");
        assert_eq!(sessions[1].events, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_window_and_bookends_return_verbatim_prior_session_events() {
        let dir = std::env::temp_dir().join(format!("medha-session-search-{}", Ulid::new()));
        let log = SqliteLog::open(dir.join("events.db")).unwrap();
        let session = kernel::Session::new();
        let first = log
            .append(Event::user_message(
                &session,
                "We chose quoted-values for the cache-key.",
            ))
            .await
            .unwrap();
        let answer = log
            .append(Event::model_text(
                &session,
                "Yes — keep the cache-key byte-for-byte.",
            ))
            .await
            .unwrap();
        log.append(Event::user_message(&session, "Anything else?"))
            .await
            .unwrap();

        let hits = log.search("quoted-values", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, first.id);
        assert!(hits[0].snippet.contains("quoted-values"));
        assert!(log.search("", 10).unwrap().is_empty());

        let window = log.window(session.id, answer.id, 1).unwrap();
        assert_eq!(window.len(), 3);
        assert_eq!(
            window[0].payload["text"],
            "We chose quoted-values for the cache-key."
        );
        assert_eq!(window[1].id, answer.id);
        let bookends = log.bookends(session.id, 1).unwrap();
        assert_eq!(bookends.len(), 2);
        assert_eq!(bookends[0].id, first.id);
        assert_eq!(bookends[1].payload["text"], "Anything else?");
        assert_eq!(log.all_events().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn automation_hits_are_demoted_not_excluded() {
        let dir = std::env::temp_dir().join(format!("medha-session-source-{}", Ulid::new()));
        let log = SqliteLog::open(dir.join("events.db")).unwrap();
        let automated = kernel::Session::new();
        let mut cron = Event::user_message(&automated, "shared retrieval phrase");
        cron.provenance.source = "automation".into();
        log.append(cron).await.unwrap();
        let interactive = kernel::Session::new();
        log.append(Event::user_message(&interactive, "shared retrieval phrase"))
            .await
            .unwrap();

        let hits = log.search("shared retrieval phrase", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, interactive.id);
        assert_eq!(hits[0].source, "interactive");
        assert_eq!(hits[1].source, "automation");
    }

    #[tokio::test]
    async fn opening_an_old_database_backfills_the_fts_mirror_once() {
        let dir = std::env::temp_dir().join(format!("medha-session-backfill-{}", Ulid::new()));
        let db = dir.join("events.db");
        let session = kernel::Session::new();
        {
            let log = SqliteLog::open(&db).unwrap();
            log.append(Event::model_text(&session, "backfill-only phrase"))
                .await
                .unwrap();
        }
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("DELETE FROM events_fts", []).unwrap();
            conn.execute(
                "DELETE FROM store_meta WHERE key = 'event_fts_backfilled_v1'",
                [],
            )
            .unwrap();
        }
        let reopened = SqliteLog::open(&db).unwrap();
        assert_eq!(reopened.search("backfill-only", 10).unwrap().len(), 1);
        drop(reopened);
        let reopened_again = SqliteLog::open(&db).unwrap();
        assert_eq!(reopened_again.search("backfill-only", 10).unwrap().len(), 1);
    }

    #[test]
    fn artifacts_roundtrip_and_reject_traversal() {
        let dir = std::env::temp_dir().join(format!("medha-art-{}", Ulid::new()));
        let store = FileArtifactStore::open(&dir).unwrap();
        let hash = store.put(b"hello world").unwrap();
        assert_eq!(store.size(&hash).unwrap(), 11);
        assert_eq!(store.get(&hash, 0, Some(5)).unwrap(), b"hello");
        assert_eq!(store.get(&hash, 6, None).unwrap(), b"world");
        // identical content → same hash (dedup)
        assert_eq!(store.put(b"hello world").unwrap(), hash);
        // non-hex hash is rejected (no path traversal)
        assert!(store.get("../etc/passwd", 0, None).is_err());
    }

    #[tokio::test]
    async fn fork_persists_a_branch_and_leaves_the_original_intact() {
        let dir = std::env::temp_dir().join(format!("medha-fork-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = SqliteLog::open(&db).unwrap();
        let s = kernel::Session {
            id: Ulid::new(),
            done: false,
            ..Default::default()
        };

        log.append(Event::user_message(&s, "one")).await.unwrap();
        let cut = log.append(Event::user_message(&s, "two")).await.unwrap();
        log.append(Event::model_text(&s, "answer")).await.unwrap();

        // Fork before "two": the branch keeps only "one".
        let branch_id = log.fork(s.id, cut.id).await.unwrap();
        let branch = log.events(branch_id).await;
        assert_eq!(branch.len(), 1);
        assert_eq!(
            branch[0].payload.get("text").and_then(|v| v.as_str()),
            Some("one")
        );
        assert_eq!(branch[0].session_id, branch_id);

        // Original session untouched; whole log still verifies (fork appended a
        // valid continuation of the global chain, tamper-evidence intact).
        assert_eq!(log.events(s.id).await.len(), 3);
        log.verify().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn memory_write_kind_round_trips_through_persistence() {
        let dir = std::env::temp_dir().join(format!("medha-memkind-{}", Ulid::new()));
        let db = dir.join("events.db");
        let s = kernel::Session {
            id: Ulid::new(),
            done: false,
            ..Default::default()
        };
        let payload = serde_json::json!({ "op": "write", "entry": { "name": "e1" } });

        let log = SqliteLog::open(&db).unwrap();
        log.append(kernel::Event {
            id: Ulid::new(),
            session_id: s.id,
            parent_id: None,
            kind: EventKind::MemoryWrite,
            payload: payload.clone(),
            trust: TrustLabel::Memory,
            provenance: kernel::Provenance {
                source: "test".into(),
            },
            prev_hash: [0u8; 32],
            ts: 0.0,
        })
        .await
        .unwrap();

        let reopened = SqliteLog::open(&db).unwrap();
        let events = reopened.events(s.id).await;
        assert_eq!(
            events.len(),
            1,
            "unknown-kind rows are silently dropped by Row::into_event — this must not be one"
        );
        assert_eq!(events[0].kind, EventKind::MemoryWrite);
        assert_eq!(events[0].payload, payload);
        reopened.verify().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ordered_provider_state_survives_sqlite_reload() {
        let dir = std::env::temp_dir().join(format!("medha-state-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = SqliteLog::open(&db).unwrap();
        let session = kernel::Session::new();
        let message = kernel::ModelMessage {
            role: kernel::Role::Assistant,
            parts: vec![kernel::ContentPart::Reasoning(kernel::ReasoningPart {
                text: Some("visible summary".into()),
                provider_state: vec![kernel::ProviderState {
                    protocol: kernel::Protocol::AnthropicMessages,
                    kind: "thinking-signature".into(),
                    value: serde_json::json!({"signature": "opaque-value"}),
                }],
            })],
        };
        log.append(Event::model_message(&session, &message))
            .await
            .unwrap();
        drop(log);

        let reopened = SqliteLog::open(&db).unwrap();
        let reloaded = reopened.events(session.id).await;
        assert_eq!(reloaded[0].kind, EventKind::ModelMessage);
        let projected = kernel::project_ordered_messages(&reloaded);
        assert_eq!(projected, vec![message]);
        let kernel::ContentPart::Reasoning(reasoning) = &projected[0].parts[0] else {
            panic!("reasoning part changed during persistence");
        };
        assert_eq!(
            reasoning.provider_state[0].value["signature"],
            "opaque-value"
        );
        drop(reopened);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn persists_and_chains() {
        let dir = std::env::temp_dir().join(format!("medha-store-{}", Ulid::new()));
        let db = dir.join("events.db");
        let session = Ulid::new();
        let s = kernel::Session {
            id: session,
            done: false,
            ..Default::default()
        };

        // Append two events, then drop and reopen → events must persist.
        {
            let log = SqliteLog::open(&db).unwrap();
            log.append(Event::model_text(&s, "first")).await.unwrap();
            log.append(Event::model_text(&s, "second")).await.unwrap();
        }
        let log = SqliteLog::open(&db).unwrap();
        let events = log.events(session).await;
        assert_eq!(events.len(), 2, "events persist across reopen");

        // Hash chain links: event 2's prev_hash == event 1's hash.
        let h1 = chain_hash(&events[0].prev_hash, &events[0]);
        assert_eq!(events[1].prev_hash, h1, "chain is linked");
        assert_ne!(events[0].prev_hash, [0u8; 32].map(|_| 1u8)); // sanity
    }

    #[tokio::test]
    async fn two_instances_on_one_db_keep_the_chain_intact() {
        // Simulates two MEDHA processes sharing a workspace: each opens its own
        // SqliteLog (own connection, no shared cache) and appends interleaved.
        // The head is read inside each append's IMMEDIATE txn (K10), so the chain
        // stays linked and verify() passes — the old cached-head design corrupted
        // it here.
        let dir = std::env::temp_dir().join(format!("medha-k10-{}", Ulid::new()));
        let db = dir.join("events.db");
        let a = SqliteLog::open(&db).unwrap();
        let b = SqliteLog::open(&db).unwrap();
        let s = kernel::Session {
            id: Ulid::new(),
            done: false,
            ..Default::default()
        };

        for i in 0..6 {
            let log = if i % 2 == 0 { &a } else { &b };
            log.append(Event::model_text(&s, &format!("event {i}")))
                .await
                .unwrap();
        }

        // Either handle sees all six, and the global chain verifies clean.
        assert_eq!(a.events(s.id).await.len(), 6);
        SqliteLog::open(&db).unwrap().verify().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn verify_detects_tampering() {
        let dir = std::env::temp_dir().join(format!("medha-verify-{}", Ulid::new()));
        let db = dir.join("events.db");
        let s = kernel::Session {
            id: Ulid::new(),
            done: false,
            ..Default::default()
        };

        {
            let log = SqliteLog::open(&db).unwrap();
            log.append(Event::model_text(&s, "first")).await.unwrap();
            log.append(Event::model_text(&s, "second")).await.unwrap();
        }

        // An untouched log verifies clean.
        SqliteLog::open(&db).unwrap().verify().unwrap();

        // Tamper directly with a stored payload (as an attacker editing the DB would).
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE events SET payload = ?1 WHERE rowid = (SELECT MIN(rowid) FROM events)",
                rusqlite::params![r#"{"text":"tampered"}"#],
            )
            .unwrap();
        }

        // Verification now fails.
        let err = SqliteLog::open(&db).unwrap().verify().unwrap_err();
        assert!(format!("{err}").contains("hash chain broken"), "got: {err}");
    }
}

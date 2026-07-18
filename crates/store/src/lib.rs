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
        let conn = Connection::open(path).map_err(|e| StoreError::Db(e.to_string()))?;
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
            CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);",
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;

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
        let s = kernel::Session { id: Ulid::new(), done: false, ..Default::default() };
        let payload = serde_json::json!({ "op": "write", "entry": { "name": "e1" } });

        let log = SqliteLog::open(&db).unwrap();
        log.append(kernel::Event {
            id: Ulid::new(),
            session_id: s.id,
            parent_id: None,
            kind: EventKind::MemoryWrite,
            payload: payload.clone(),
            trust: TrustLabel::Memory,
            provenance: kernel::Provenance { source: "test".into() },
            prev_hash: [0u8; 32],
            ts: 0.0,
        })
        .await
        .unwrap();

        let reopened = SqliteLog::open(&db).unwrap();
        let events = reopened.events(s.id).await;
        assert_eq!(events.len(), 1, "unknown-kind rows are silently dropped by Row::into_event — this must not be one");
        assert_eq!(events[0].kind, EventKind::MemoryWrite);
        assert_eq!(events[0].payload, payload);
        reopened.verify().unwrap();
        std::fs::remove_dir_all(&dir).ok();
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

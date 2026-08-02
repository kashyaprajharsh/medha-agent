//! Persistent event log (§4.2). A SQLite (WAL) implementation of the kernel's
//! `EventLog` trait, with a real SHA-256 hash chain making the log
//! tamper-evident (P3). Drop-in replacement for the in-memory log (P8); state
//! is still a projection of these events.

use async_trait::async_trait;
use kernel::events::{EVENT_HASH_VERSION, chain_hash};
use kernel::{
    ArtifactStore, Event, EventKind, EventLog, KernelError, MutationLease, Provenance, SessionMeta,
    TrustLabel,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ulid::Ulid;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

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

fn verify_chain_anchor(conn: &Connection, rows: &[(i64, Row, Vec<u8>)]) -> Result<(), StoreError> {
    let (count, head) = validated_chain(rows)?;
    let (anchored_count, anchored_head) = read_chain_anchor(conn)?.ok_or_else(|| {
        StoreError::Db("event-chain anchor is missing; refusing unanchored log".into())
    })?;
    if anchored_count != count || anchored_head != head {
        return Err(StoreError::Db(format!(
            "event-chain anchor mismatch: expected {anchored_count} rows/head {}, \
             found {count} rows/head {}",
            hash_hex(&anchored_head),
            hash_hex(&head)
        )));
    }
    Ok(())
}

/// Reconstruct the search index only from a chain-verified snapshot. The
/// caller holds one `IMMEDIATE` transaction through both this rebuild and the
/// result query, closing the otherwise exploitable rebuild/query race.
fn rebuild_verified_event_fts(conn: &Connection) -> Result<(), StoreError> {
    let rows = load_chain_rows(conn)?;
    verify_chain_anchor(conn, &rows)?;
    conn.execute("DELETE FROM events_fts", [])
        .map_err(|error| StoreError::Db(error.to_string()))?;
    for (_, row, _) in rows {
        let event = row
            .into_event()
            .ok_or_else(|| StoreError::Db("verified event could not be decoded".into()))?;
        let Some(text) = search_text(&event.kind, &event.payload) else {
            continue;
        };
        conn.execute(
            "INSERT INTO events_fts (event_id, session_id, kind, text, source, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                event.id.to_string(),
                event.session_id.to_string(),
                event.kind.as_str(),
                text,
                event_source(&event),
                event.ts,
            ],
        )
        .map_err(|error| StoreError::Db(error.to_string()))?;
    }
    Ok(())
}

const CHAIN_HEAD_KEY: &str = "event_chain_v2_head";
const CHAIN_COUNT_KEY: &str = "event_chain_v2_count";

fn hash_hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_hash_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut hash = [0u8; 32];
    for (index, slot) in hash.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

fn meta_value(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT value FROM store_meta WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| StoreError::Db(error.to_string()))
}

fn set_chain_anchor(conn: &Connection, count: u64, head: &[u8; 32]) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![CHAIN_COUNT_KEY, count.to_string()],
    )
    .map_err(|error| StoreError::Db(error.to_string()))?;
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![CHAIN_HEAD_KEY, hash_hex(head)],
    )
    .map_err(|error| StoreError::Db(error.to_string()))?;
    Ok(())
}

fn read_chain_anchor(conn: &Connection) -> Result<Option<(u64, [u8; 32])>, StoreError> {
    match (
        meta_value(conn, CHAIN_COUNT_KEY)?,
        meta_value(conn, CHAIN_HEAD_KEY)?,
    ) {
        (None, None) => Ok(None),
        (Some(count), Some(head)) => {
            let count = count.parse::<u64>().map_err(|_| {
                StoreError::Db("event-chain anchor has an invalid row count".into())
            })?;
            let head = parse_hash_hex(&head)
                .ok_or_else(|| StoreError::Db("event-chain anchor has an invalid hash".into()))?;
            Ok(Some((count, head)))
        }
        _ => Err(StoreError::Db(
            "event-chain anchor is incomplete (head/count mismatch)".into(),
        )),
    }
}

/// Content-addressed blob store on disk (§4.2/§4.5). Blobs live under a dir,
/// named by their SHA-256 hash, so identical content is stored once.
pub struct FileArtifactStore {
    dir: PathBuf,
}

/// One artifact read can never allocate more than this. Callers page with
/// `offset`; `None` means "one bounded page", not "allocate the whole blob".
pub const MAX_ARTIFACT_READ_BYTES: usize = 1024 * 1024;

fn file_digest(file: &mut File) -> Result<String, String> {
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn open_verified_artifact(path: &Path, expected_hash: &str) -> Result<File, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let actual = file_digest(&mut file)?;
    if !actual.eq_ignore_ascii_case(expected_hash) {
        return Err(format!(
            "artifact integrity check failed: expected {expected_hash}, found {actual}"
        ));
    }
    Ok(file)
}

/// Open an already-published artifact with write access before flushing it.
///
/// On Windows `File::sync_all` maps to `FlushFileBuffers`, which rejects a
/// read-only handle with `ERROR_ACCESS_DENIED`. Reads should stay read-only,
/// but the post-publication durability barrier needs this separate handle.
fn open_verified_artifact_for_sync(path: &Path, expected_hash: &str) -> Result<File, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let actual = file_digest(&mut file)?;
    if !actual.eq_ignore_ascii_case(expected_hash) {
        return Err(format!(
            "artifact integrity check failed: expected {expected_hash}, found {actual}"
        ));
    }
    Ok(file)
}

struct TemporaryArtifact(PathBuf);

impl Drop for TemporaryArtifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl FileArtifactStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(Self { dir })
    }

    /// Reject anything but a hex hash, so a hash can never escape the dir.
    fn safe_path(&self, hash: &str) -> Result<PathBuf, String> {
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("invalid artifact hash".into());
        }
        Ok(self.dir.join(hash.to_ascii_lowercase()))
    }
}

impl ArtifactStore for FileArtifactStore {
    fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let mut h = Sha256::new();
        h.update(bytes);
        let hash = format!("{:x}", h.finalize());
        let path = self.dir.join(&hash);
        if open_verified_artifact(&path, &hash).is_ok() {
            return Ok(hash);
        }

        let temporary = TemporaryArtifact(self.dir.join(format!(".{hash}.{}.tmp", Ulid::new())));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary.0)
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        if !file_digest(&mut file)?.eq_ignore_ascii_case(&hash) {
            return Err("temporary artifact failed its digest verification".into());
        }
        drop(file);

        // Same-directory rename is atomic. Concurrent writers publish identical
        // bytes because the destination name is the content digest.
        if let Err(error) = atomic_replace(&temporary.0, &path) {
            if open_verified_artifact(&path, &hash).is_err() {
                return Err(format!("could not atomically publish artifact: {error}"));
            }
        }
        let final_file = open_verified_artifact_for_sync(&path, &hash)?;
        final_file.sync_all().map_err(|e| e.to_string())?;
        #[cfg(unix)]
        File::open(&self.dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(hash)
    }

    fn get(&self, hash: &str, offset: usize, len: Option<usize>) -> Result<Vec<u8>, String> {
        let path = self.safe_path(hash)?;
        let mut file = open_verified_artifact(&path, hash)?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        let start = u64::try_from(offset).unwrap_or(u64::MAX).min(size);
        let requested = len.unwrap_or(MAX_ARTIFACT_READ_BYTES);
        let bounded = requested.min(MAX_ARTIFACT_READ_BYTES);
        let remaining = size.saturating_sub(start);
        let to_read = u64::try_from(bounded).unwrap_or(u64::MAX).min(remaining);
        file.seek(SeekFrom::Start(start))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(usize::try_from(to_read).unwrap_or(0));
        file.take(to_read)
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())?;
        Ok(out)
    }

    fn size(&self, hash: &str) -> Result<usize, String> {
        let path = self.safe_path(hash)?;
        usize::try_from(std::fs::metadata(path).map_err(|e| e.to_string())?.len())
            .map_err(|_| "artifact is too large for this platform".into())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("db: {0}")]
    Db(String),
}

#[derive(Clone)]
pub struct SqliteLog {
    conn: Arc<Mutex<Connection>>,
    /// Async callers acquire this permit before entering the blocking pool.
    /// This keeps contention on one SQLite connection from consuming an
    /// unbounded number of blocking workers; callers cancelled while queued
    /// never start database work.
    runtime_gate: Arc<tokio::sync::Semaphore>,
    /// Serializes state changes made by independent processes in this
    /// workspace. It is deliberately a different SQLite database from the
    /// event log: the lease must span an arbitrary external side effect, which
    /// would otherwise hold the event database's write transaction open.
    mutation_lock: PathBuf,
    /// Memory commands can address either project or user scope after parsing,
    /// and the CLI intentionally takes one wildcard lease before rebuilding its
    /// projection. All memory keys therefore share this global lane. CLI
    /// construction supplies one path under `$MEDHA_HOME`.
    global_mutation_lock: Option<PathBuf>,
}

impl SqliteLog {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_inner(path.as_ref(), None)
    }

    /// Open an event log whose user-global mutations coordinate through
    /// `global_mutation_lock`. File, shell, and MCP mutations still use a lock
    /// beside this workspace's event database, so a long build in one
    /// repository does not stall an unrelated repository. Memory mutations are
    /// short and all use the global lane so CLI wildcard operations cannot race
    /// kernel-dispatched project memory.
    pub fn open_with_mutation_lock(
        path: impl AsRef<Path>,
        global_mutation_lock: impl AsRef<Path>,
    ) -> Result<Self, StoreError> {
        Self::open_inner(path.as_ref(), Some(global_mutation_lock.as_ref()))
    }

    fn open_inner(path: &Path, global_mutation_lock: Option<&Path>) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let mutation_lock = path.with_extension("mutations.db");
        initialize_mutation_lock(&mutation_lock)?;
        let global_mutation_lock = global_mutation_lock.map(Path::to_path_buf);
        if let Some(path) = &global_mutation_lock {
            initialize_mutation_lock(path)?;
        }
        let mut conn = Connection::open(path).map_err(|e| StoreError::Db(e.to_string()))?;
        // Runtime-facing event operations run on a blocking worker, but they
        // still need a finite upper bound so a cancelled/abandoned request
        // cannot retain the worker and serialization permit indefinitely.
        conn.busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(|error| StoreError::Db(error.to_string()))?;
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
                hash_version INTEGER NOT NULL DEFAULT 2,
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
        ensure_hash_version_column(&mut conn)?;
        migrate_event_chain_v2(&mut conn)?;
        backfill_event_fts(&mut conn)?;

        // The chain head is read from the DB inside each append's transaction
        // (see `append`), not cached — so a second MEDHA process on the same
        // workspace can't append against a stale head and corrupt the chain (K10).
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            runtime_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            mutation_lock,
            global_mutation_lock,
        })
    }

    fn mutation_lock_for(&self, mutation_key: &str) -> PathBuf {
        if mutation_key.starts_with("memory:") {
            self.global_mutation_lock
                .clone()
                .unwrap_or_else(|| self.mutation_lock.clone())
        } else {
            self.mutation_lock.clone()
        }
    }

    async fn run_store_task<T, F>(&self, operation: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(SqliteLog) -> Result<T, StoreError> + Send + 'static,
    {
        let permit = self
            .runtime_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| StoreError::Db(format!("SQLite worker closed: {error}")))?;
        let log = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(log)
        })
        .await
        .map_err(|error| StoreError::Db(format!("SQLite worker failed: {error}")))?
    }

    async fn run_kernel_task<T, F>(&self, operation: F) -> Result<T, KernelError>
    where
        T: Send + 'static,
        F: FnOnce(SqliteLog) -> Result<T, KernelError> + Send + 'static,
    {
        let permit = self
            .runtime_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| KernelError::Log(format!("SQLite worker closed: {error}")))?;
        let log = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(log)
        })
        .await
        .map_err(|error| KernelError::Log(format!("SQLite worker failed: {error}")))?
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
        let rows = load_chain_rows(&conn)?;
        verify_chain_anchor(&conn, &rows)
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
                "SELECT id, session_id, parent_id, kind, payload, trust, provenance,
                        prev_hash, hash_version, ts
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
                    hash_version: row.get(8)?,
                    ts: row.get(9)?,
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
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Db("lock poisoned".into()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| StoreError::Db(error.to_string()))?;
        rebuild_verified_event_fts(&tx)?;
        let hits = {
            let mut stmt = tx
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
            hits
        };
        tx.commit()
            .map_err(|error| StoreError::Db(error.to_string()))?;
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
                "SELECT rowid, id, session_id, parent_id, kind, payload, trust, provenance,
                        prev_hash, hash_version, ts
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
                            hash_version: row.get(9)?,
                            ts: row.get(10)?,
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
                "SELECT rowid, id, session_id, parent_id, kind, payload, trust, provenance,
                        prev_hash, hash_version, ts
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
                                hash_version: row.get(9)?,
                                ts: row.get(10)?,
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

    pub async fn verify_async(&self) -> Result<(), StoreError> {
        self.run_store_task(|log| log.verify()).await
    }

    pub async fn list_sessions_async(&self) -> Result<Vec<SessionMeta>, StoreError> {
        self.run_store_task(|log| log.list_sessions()).await
    }

    pub async fn all_events_async(&self) -> Result<Vec<Event>, StoreError> {
        self.run_store_task(|log| log.all_events()).await
    }

    pub async fn search_async(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>, StoreError> {
        let query = query.to_string();
        self.run_store_task(move |log| log.search(&query, limit))
            .await
    }

    pub async fn window_async(
        &self,
        session_id: Ulid,
        around_event_id: Ulid,
        radius: usize,
    ) -> Result<Vec<Event>, StoreError> {
        self.run_store_task(move |log| log.window(session_id, around_event_id, radius))
            .await
    }

    pub async fn bookends_async(
        &self,
        session_id: Ulid,
        count: usize,
    ) -> Result<Vec<Event>, StoreError> {
        self.run_store_task(move |log| log.bookends(session_id, count))
            .await
    }

    fn append_sync(&self, mut e: Event) -> Result<Event, KernelError> {
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
        let count: u64 = tx
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .map_err(|err| KernelError::Log(err.to_string()))?;
        let mut prev = [0u8; 32];
        if let Some(h) = head {
            if h.len() != 32 {
                return Err(KernelError::Log(
                    "event-chain head has an invalid width".into(),
                ));
            }
            prev.copy_from_slice(&h);
        }
        let (anchored_count, anchored_head) = read_chain_anchor(&tx)
            .map_err(|error| KernelError::Log(error.to_string()))?
            .ok_or_else(|| KernelError::Log("event-chain anchor is missing".into()))?;
        if anchored_count != count || anchored_head != prev {
            return Err(KernelError::Log(
                "event-chain anchor mismatch; refusing to append".into(),
            ));
        }
        e.prev_hash = prev;
        e.hash_version = EVENT_HASH_VERSION;
        let hash = chain_hash(&e.prev_hash, &e);

        tx.execute(
            "INSERT INTO events
                (id, session_id, parent_id, kind, payload, trust, provenance,
                 prev_hash, hash, hash_version, ts)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
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
                e.hash_version,
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
        set_chain_anchor(&tx, count.saturating_add(1), &hash)
            .map_err(|error| KernelError::Log(error.to_string()))?;
        tx.commit()
            .map_err(|err| KernelError::Log(err.to_string()))?;
        Ok(e)
    }

    fn events_sync(&self, session: Ulid) -> Vec<Event> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, session_id, parent_id, kind, payload, trust, provenance,
                    prev_hash, hash_version, ts
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
                hash_version: r.get(8)?,
                ts: r.get(9)?,
            })
        });
        let mut out = Vec::new();
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                if let Some(event) = row.into_event() {
                    out.push(event);
                }
            }
        }
        out
    }
}

#[async_trait]
impl EventLog for SqliteLog {
    async fn append(&self, event: Event) -> Result<Event, KernelError> {
        self.run_kernel_task(move |log| log.append_sync(event))
            .await
    }

    async fn events(&self, session: Ulid) -> Vec<Event> {
        self.run_store_task(move |log| Ok(log.events_sync(session)))
            .await
            .unwrap_or_default()
    }

    async fn acquire_mutation_lease(
        &self,
        mutation_key: &str,
    ) -> Result<MutationLease, KernelError> {
        let path = self.mutation_lock_for(mutation_key);
        tokio::task::spawn_blocking(move || {
            let conn =
                Connection::open(&path).map_err(|error| KernelError::Log(error.to_string()))?;
            conn.busy_timeout(Duration::from_secs(120))
                .map_err(|error| KernelError::Log(error.to_string()))?;
            // `BEGIN IMMEDIATE` takes SQLite's writer reservation now, rather
            // than on a later statement. The owned connection stays in the
            // returned guard, so another process opening the same lock DB
            // cannot enter its mutation until this lease drops.
            conn.execute_batch("BEGIN IMMEDIATE")
                .map_err(|error| KernelError::Log(error.to_string()))?;
            Ok(MutationLease::guarded(SqliteMutationLease { conn }))
        })
        .await
        .map_err(|error| KernelError::Log(format!("mutation lease task failed: {error}")))?
    }

    /// Expose the session list through the trait so any surface (TUI picker,
    /// REPL, CLI) can browse sessions generically. Errors degrade to an empty
    /// list rather than failing the caller.
    async fn sessions(&self) -> Vec<SessionMeta> {
        self.list_sessions_async().await.unwrap_or_default()
    }
}

/// Set up a tiny dedicated database used only as a cross-process mutex.
fn initialize_mutation_lock(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
    }
    let conn = Connection::open(path).map_err(|error| StoreError::Db(error.to_string()))?;
    conn.busy_timeout(Duration::from_secs(120))
        .map_err(|error| StoreError::Db(error.to_string()))?;
    // No schema or application state is needed: BEGIN IMMEDIATE reserves the
    // database itself. Avoiding DDL here also means a second MEDHA process can
    // open its event log while another process currently owns the lease; it
    // blocks only if and when it attempts a mutation.
    Ok(())
}

struct SqliteMutationLease {
    conn: Connection,
}

impl Drop for SqliteMutationLease {
    fn drop(&mut self) {
        // Closing the connection would release the reservation too, but an
        // explicit rollback makes the lifetime boundary immediate and clear.
        let _ = self.conn.execute_batch("ROLLBACK");
    }
}

#[derive(Clone)]
struct Row {
    id: String,
    session_id: String,
    parent_id: Option<String>,
    kind: String,
    payload: String,
    trust: String,
    provenance: String,
    prev_hash: Vec<u8>,
    hash_version: u8,
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
            hash_version: self.hash_version,
            ts: self.ts,
        })
    }
}

fn ensure_hash_version_column(conn: &mut Connection) -> Result<(), StoreError> {
    // Serialize the check/ALTER pair across concurrently starting processes.
    // Without the write reservation, both can observe the legacy schema and
    // one then fails with "duplicate column name".
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Db(error.to_string()))?;
    let mut stmt = tx
        .prepare("PRAGMA table_info(events)")
        .map_err(|error| StoreError::Db(error.to_string()))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| StoreError::Db(error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| StoreError::Db(error.to_string()))?;
    drop(stmt);
    if !columns.iter().any(|column| column == "hash_version") {
        // Existing rows were written with the original partial encoding.
        tx.execute(
            "ALTER TABLE events
             ADD COLUMN hash_version INTEGER NOT NULL DEFAULT 1",
            [],
        )
        .map_err(|error| StoreError::Db(error.to_string()))?;
    }
    tx.commit()
        .map_err(|error| StoreError::Db(error.to_string()))
}

fn load_chain_rows(conn: &Connection) -> Result<Vec<(i64, Row, Vec<u8>)>, StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT rowid, id, session_id, parent_id, kind, payload, trust, provenance,
                    prev_hash, hash, hash_version, ts
             FROM events ORDER BY rowid ASC",
        )
        .map_err(|error| StoreError::Db(error.to_string()))?;
    stmt.query_map([], |row| {
        Ok((
            row.get(0)?,
            Row {
                id: row.get(1)?,
                session_id: row.get(2)?,
                parent_id: row.get(3)?,
                kind: row.get(4)?,
                payload: row.get(5)?,
                trust: row.get(6)?,
                provenance: row.get(7)?,
                prev_hash: row.get(8)?,
                hash_version: row.get(10)?,
                ts: row.get(11)?,
            },
            row.get(9)?,
        ))
    })
    .map_err(|error| StoreError::Db(error.to_string()))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(|error| StoreError::Db(error.to_string()))
}

fn validated_chain(rows: &[(i64, Row, Vec<u8>)]) -> Result<(u64, [u8; 32]), StoreError> {
    let mut prev = [0u8; 32];
    for (index, (_, row, stored_hash)) in rows.iter().enumerate() {
        let event = row
            .clone()
            .into_event()
            .ok_or_else(|| StoreError::Db(format!("corrupt event row at index {index}")))?;
        if !matches!(event.hash_version, 1 | EVENT_HASH_VERSION) {
            return Err(StoreError::Db(format!(
                "unsupported event hash version {} at index {index}",
                event.hash_version
            )));
        }
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
    Ok((rows.len() as u64, prev))
}

/// Upgrade a valid legacy chain in one SQLite transaction. Logical events are
/// untouched; only their link fields and encoding version change. A corrupt
/// legacy chain is rejected rather than "repaired", and an already-v2 chain is
/// never re-anchored (which would legitimize suffix deletion).
fn migrate_event_chain_v2(conn: &mut Connection) -> Result<(), StoreError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| StoreError::Db(error.to_string()))?;
    let rows = load_chain_rows(&tx)?;
    let legacy = rows
        .iter()
        .any(|(_, row, _)| row.hash_version != EVENT_HASH_VERSION);

    if !legacy {
        if rows.is_empty() && read_chain_anchor(&tx)?.is_none() {
            set_chain_anchor(&tx, 0, &[0u8; 32])?;
        }
        tx.commit()
            .map_err(|error| StoreError::Db(error.to_string()))?;
        return Ok(());
    }

    // Authenticate the old representation before changing any link.
    let (old_count, old_head) = validated_chain(&rows)?;
    if let Some((anchored_count, anchored_head)) = read_chain_anchor(&tx)?
        && (anchored_count != old_count || anchored_head != old_head)
    {
        return Err(StoreError::Db(
            "event-chain anchor does not match the legacy chain".into(),
        ));
    }

    let mut prev = [0u8; 32];
    for (rowid, row, _) in rows {
        let mut event = row
            .into_event()
            .ok_or_else(|| StoreError::Db(format!("corrupt event row at rowid {rowid}")))?;
        event.hash_version = EVENT_HASH_VERSION;
        event.prev_hash = prev;
        let hash = chain_hash(&prev, &event);
        tx.execute(
            "UPDATE events
             SET prev_hash = ?1, hash = ?2, hash_version = ?3
             WHERE rowid = ?4",
            rusqlite::params![
                event.prev_hash.to_vec(),
                hash.to_vec(),
                EVENT_HASH_VERSION,
                rowid
            ],
        )
        .map_err(|error| StoreError::Db(error.to_string()))?;
        prev = hash;
    }
    set_chain_anchor(&tx, old_count, &prev)?;
    tx.commit()
        .map_err(|error| StoreError::Db(error.to_string()))
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
    async fn search_repairs_fts_tampering_from_verified_events_before_returning_hits() {
        let dir = std::env::temp_dir().join(format!("medha-fts-integrity-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = SqliteLog::open(&db).unwrap();
        let session = kernel::Session::new();
        let mut event = Event::user_message(&session, "authentic searchable phrase");
        event.provenance.source = "automation".into();
        let event = log.append(event).await.unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "UPDATE events_fts
                 SET text = 'attacker injected snippet', source = 'interactive',
                     kind = 'model.text'
                 WHERE event_id = ?1",
                [event.id.to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events_fts (event_id, session_id, kind, text, source, ts)
                 VALUES (?1, ?2, 'user.message', 'second forged row', 'interactive', 0)",
                rusqlite::params![Ulid::new().to_string(), session.id.to_string()],
            )
            .unwrap();
        }

        assert!(log.search("attacker injected", 10).unwrap().is_empty());
        assert!(log.search("second forged", 10).unwrap().is_empty());
        let hits = log.search("authentic searchable", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, event.id);
        assert_eq!(hits[0].kind, EventKind::UserMessage.as_str());
        assert_eq!(hits[0].source, "automation");
        assert!(!hits[0].snippet.contains("attacker"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn search_refuses_fts_results_when_the_authenticated_log_is_corrupt() {
        let dir = std::env::temp_dir().join(format!("medha-fts-chain-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = SqliteLog::open(&db).unwrap();
        let session = kernel::Session::new();
        log.append(Event::user_message(&session, "original phrase"))
            .await
            .unwrap();
        Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE events SET payload = '{\"text\":\"tampered phrase\"}'",
                [],
            )
            .unwrap();
        let error = log
            .search("original", 10)
            .expect_err("search must authenticate its source rows");
        assert!(error.to_string().contains("hash chain broken"));
        std::fs::remove_dir_all(dir).ok();
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

    #[test]
    fn artifact_put_repairs_a_corrupt_preexisting_hash_and_ignores_partial_temps() {
        let dir = std::env::temp_dir().join(format!("medha-art-repair-{}", Ulid::new()));
        let store = FileArtifactStore::open(&dir).unwrap();
        let bytes = b"authoritative artifact bytes";
        let hash = format!("{:x}", Sha256::digest(bytes));
        std::fs::write(dir.join(&hash), b"partial").unwrap();
        std::fs::write(dir.join(format!(".{hash}.crash.tmp")), b"partial temp").unwrap();

        assert!(
            store.get(&hash, 0, None).is_err(),
            "a corrupt hash-named file must never be returned"
        );
        assert_eq!(store.put(bytes).unwrap(), hash);
        assert_eq!(store.get(&hash, 0, None).unwrap(), bytes);
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), hash);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn artifact_ranges_are_overflow_safe_and_bounded() {
        let dir = std::env::temp_dir().join(format!("medha-art-range-{}", Ulid::new()));
        let store = FileArtifactStore::open(&dir).unwrap();
        let bytes = vec![b'x'; MAX_ARTIFACT_READ_BYTES + 257];
        let hash = store.put(&bytes).unwrap();

        assert!(
            store
                .get(&hash, usize::MAX, Some(usize::MAX))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.get(&hash, 0, Some(usize::MAX)).unwrap().len(),
            MAX_ARTIFACT_READ_BYTES
        );
        assert_eq!(
            store.get(&hash, 0, None).unwrap().len(),
            MAX_ARTIFACT_READ_BYTES,
            "an omitted length still returns one bounded page"
        );
        assert_eq!(
            store
                .get(&hash, MAX_ARTIFACT_READ_BYTES, Some(usize::MAX))
                .unwrap()
                .len(),
            257
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn concurrent_artifact_publish_and_read_never_exposes_wrong_bytes() {
        let dir = std::env::temp_dir().join(format!("medha-art-race-{}", Ulid::new()));
        let store = std::sync::Arc::new(FileArtifactStore::open(&dir).unwrap());
        let bytes = std::sync::Arc::new(b"same content from every writer".repeat(4_096));
        let hash = format!("{:x}", Sha256::digest(bytes.as_slice()));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(10));
        let mut threads = Vec::new();
        for _ in 0..4 {
            let store = std::sync::Arc::clone(&store);
            let bytes = std::sync::Arc::clone(&bytes);
            let barrier = std::sync::Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.put(bytes.as_slice()).unwrap()
            }));
        }
        for _ in 0..6 {
            let store = std::sync::Arc::clone(&store);
            let bytes = std::sync::Arc::clone(&bytes);
            let hash = hash.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..100 {
                    if let Ok(read) = store.get(&hash, 0, Some(bytes.len())) {
                        assert_eq!(read, *bytes);
                    }
                }
                hash
            }));
        }
        for thread in threads {
            assert_eq!(thread.join().unwrap(), hash);
        }
        assert_eq!(store.get(&hash, 0, Some(bytes.len())).unwrap(), *bytes);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn locked_event_database_does_not_block_timers_streams_or_queued_cancellation() {
        let dir = std::env::temp_dir().join(format!("medha-store-async-lock-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = Arc::new(SqliteLog::open(&db).unwrap());
        let blocker = Connection::open(&db).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let session = kernel::Session::new();

        let first = tokio::spawn({
            let log = Arc::clone(&log);
            let session = session.clone();
            async move {
                log.append(Event::model_text(&session, "first blocked append"))
                    .await
            }
        });
        // Give the append a poll so it reaches SQLite's busy wait. On the old
        // direct-SQLite implementation this yield never returned until the
        // busy timeout elapsed on this single runtime thread.
        tokio::task::yield_now().await;
        tokio::time::timeout(
            Duration::from_millis(250),
            tokio::time::sleep(Duration::from_millis(25)),
        )
        .await
        .expect("a locked database must not starve independent runtime timers");
        assert!(
            !first.is_finished(),
            "the external writer lock was ineffective"
        );
        let (delta_tx, mut delta_rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let _ = delta_tx.send("provider-delta").await;
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(250), delta_rx.recv())
                .await
                .expect("a locked database must not stall independent streams"),
            Some("provider-delta")
        );

        let queued = tokio::spawn({
            let log = Arc::clone(&log);
            let session = session.clone();
            async move {
                log.append(Event::model_text(&session, "cancelled before start"))
                    .await
            }
        });
        tokio::task::yield_now().await;
        queued.abort();
        let cancelled = tokio::time::timeout(Duration::from_millis(250), queued)
            .await
            .expect("a request queued on SQLite serialization must cancel promptly");
        assert!(cancelled.unwrap_err().is_cancelled());

        blocker.execute_batch("ROLLBACK").unwrap();
        tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("append should finish after releasing the database")
            .unwrap()
            .unwrap();
        let events = log.events(session.id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["text"], "first blocked append");
        drop(blocker);
        drop(log);
        std::fs::remove_dir_all(dir).ok();
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
            hash_version: kernel::events::EVENT_HASH_VERSION,
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
            trust: None,
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
    async fn compacted_request_snapshot_survives_sqlite_reload() {
        let dir = std::env::temp_dir().join(format!("medha-compaction-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = SqliteLog::open(&db).unwrap();
        let session = kernel::Session::new();
        let legacy = vec![
            kernel::Message::system("SYSTEM"),
            kernel::Message::user("protected instructions"),
            kernel::Message::new(kernel::Role::Assistant, "HANDOFF"),
            kernel::Message::assistant_calls(
                "",
                vec![kernel::ToolIntent {
                    id: "tail-call".into(),
                    tool: "fs.read".into(),
                    args: serde_json::json!({"path": "recent.rs"}),
                }],
            ),
            kernel::Message::tool_result("tail-call", "recent contents"),
        ];
        let mut ordered: Vec<kernel::ModelMessage> =
            legacy.iter().map(kernel::Message::ordered).collect();
        let state = kernel::ProviderState {
            protocol: kernel::Protocol::AnthropicMessages,
            kind: "thinking-signature".into(),
            value: serde_json::json!({"signature": "opaque-compacted-value"}),
        };
        let kernel::ContentPart::ToolCall(call) = &mut ordered[3].parts[0] else {
            panic!("expected canonical tool call");
        };
        call.provider_state.push(state);
        log.append(Event::compaction_snapshot(
            &session,
            10_000,
            1_000,
            Some("HANDOFF"),
            &legacy,
            &ordered,
        ))
        .await
        .unwrap();
        drop(log);

        let reopened = SqliteLog::open(&db).unwrap();
        let events = reopened.events(session.id).await;
        let mut replayed_legacy = vec![legacy[0].clone()];
        replayed_legacy.extend(kernel::project_messages(&events));
        assert_eq!(
            serde_json::to_vec(&replayed_legacy).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
        let mut replayed_ordered = vec![ordered[0].clone()];
        replayed_ordered.extend(kernel::project_ordered_messages(&events));
        assert_eq!(
            serde_json::to_vec(&replayed_ordered).unwrap(),
            serde_json::to_vec(&ordered).unwrap()
        );
        assert!(
            replayed_ordered
                .iter()
                .any(kernel::ModelMessage::has_provider_state)
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
    async fn independent_connections_share_the_workspace_mutation_lease() {
        let dir = std::env::temp_dir().join(format!("medha-mutation-lease-{}", Ulid::new()));
        let db = dir.join("events.db");
        let a = std::sync::Arc::new(SqliteLog::open(&db).unwrap());
        let b = std::sync::Arc::new(SqliteLog::open(&db).unwrap());

        let first = a.acquire_mutation_lease("state:*").await.unwrap();
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let lease = b.acquire_mutation_lease("state:*").await.unwrap();
            let _ = acquired_tx.send(());
            lease
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut acquired_rx)
                .await
                .is_err(),
            "an independent SQLite connection entered while the first lease was live"
        );
        drop(first);
        tokio::time::timeout(Duration::from_secs(2), &mut acquired_rx)
            .await
            .expect("second connection should enter after release")
            .expect("waiter should report acquisition");
        drop(waiter.await.unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn all_memory_keys_use_the_global_lane_but_workspace_state_stays_local() {
        let dir = std::env::temp_dir().join(format!("medha-global-lease-{}", Ulid::new()));
        let global = dir.join("home").join("mutations.db");
        let a = std::sync::Arc::new(
            SqliteLog::open_with_mutation_lock(dir.join("a/events.db"), &global).unwrap(),
        );
        let b = std::sync::Arc::new(
            SqliteLog::open_with_mutation_lock(dir.join("b/events.db"), &global).unwrap(),
        );

        let memory_lease = a.acquire_mutation_lease("memory:*").await.unwrap();
        // Different workspaces have different non-memory writer lanes.
        let workspace_lease =
            tokio::time::timeout(Duration::from_secs(1), b.acquire_mutation_lease("state:*"))
                .await
                .expect("unrelated workspace state must not wait on memory")
                .unwrap();
        drop(workspace_lease);

        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let lease = b
                .acquire_mutation_lease("memory:project:shared")
                .await
                .unwrap();
            let _ = acquired_tx.send(());
            lease
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut acquired_rx)
                .await
                .is_err(),
            "project memory bypassed the CLI-style global wildcard lane"
        );
        drop(memory_lease);
        tokio::time::timeout(Duration::from_secs(2), &mut acquired_rx)
            .await
            .expect("global waiter should enter after release")
            .expect("waiter should report acquisition");
        drop(waiter.await.unwrap());
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

    async fn seeded_chain(tag: &str) -> (PathBuf, PathBuf, SqliteLog) {
        let dir = std::env::temp_dir().join(format!("medha-chain-{tag}-{}", Ulid::new()));
        let db = dir.join("events.db");
        let log = SqliteLog::open(&db).unwrap();
        let session = kernel::Session::new();
        for text in ["one", "two", "three"] {
            log.append(Event::model_text(&session, text)).await.unwrap();
        }
        log.verify().unwrap();
        (dir, db, log)
    }

    #[tokio::test]
    async fn verification_covers_every_stored_field_order_and_terminal_anchor() {
        let mutations = [
            (
                "id",
                "UPDATE events SET id = (SELECT id FROM events WHERE rowid = 2) WHERE rowid = 1",
            ),
            (
                "session",
                "UPDATE events SET session_id = id WHERE rowid = 1",
            ),
            ("parent", "UPDATE events SET parent_id = id WHERE rowid = 1"),
            (
                "kind",
                "UPDATE events SET kind = 'user.message' WHERE rowid = 1",
            ),
            (
                "payload",
                "UPDATE events SET payload = '{\"text\":\"ONE\"}' WHERE rowid = 1",
            ),
            ("trust", "UPDATE events SET trust = 'web' WHERE rowid = 1"),
            (
                "provenance",
                "UPDATE events SET provenance = 'automation' WHERE rowid = 1",
            ),
            (
                "prev-hash",
                "UPDATE events SET prev_hash = randomblob(32) WHERE rowid = 2",
            ),
            (
                "stored-hash",
                "UPDATE events SET hash = randomblob(32) WHERE rowid = 2",
            ),
            (
                "hash-version",
                "UPDATE events SET hash_version = 1 WHERE rowid = 1",
            ),
            (
                "timestamp",
                "UPDATE events SET ts = ts + 0.125 WHERE rowid = 1",
            ),
            ("middle-delete", "DELETE FROM events WHERE rowid = 2"),
            (
                "suffix-truncate",
                "DELETE FROM events WHERE rowid = (SELECT MAX(rowid) FROM events)",
            ),
            (
                "row-reorder",
                "UPDATE events SET rowid = -1 WHERE rowid = 1;
                 UPDATE events SET rowid = 1 WHERE rowid = 2;
                 UPDATE events SET rowid = 2 WHERE rowid = -1",
            ),
        ];

        for (tag, mutation) in mutations {
            let (dir, db, log) = seeded_chain(tag).await;
            Connection::open(&db)
                .unwrap()
                .execute_batch(mutation)
                .unwrap();
            let error = log
                .verify()
                .expect_err("every stored-field/order/truncation mutation must fail");
            let message = error.to_string();
            assert!(
                message.contains("hash chain") || message.contains("event-chain anchor"),
                "{tag} produced an unexpected verification error: {message}"
            );
            drop(log);
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[tokio::test]
    async fn valid_legacy_chain_is_transactionally_upgraded_and_anchored() {
        let dir = std::env::temp_dir().join(format!("medha-chain-v1-{}", Ulid::new()));
        let db = dir.join("events.db");
        std::fs::create_dir_all(&dir).unwrap();
        let session = kernel::Session::new();
        let mut event = Event::model_text(&session, "legacy");
        event.hash_version = 1;
        event.prev_hash = [0u8; 32];
        let legacy_hash = kernel::events::legacy_chain_hash(&event.prev_hash, &event);

        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (
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
                CREATE TABLE store_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events
                 (id, session_id, parent_id, kind, payload, trust, provenance, prev_hash, hash, ts)
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    event.id.to_string(),
                    event.session_id.to_string(),
                    event.kind.as_str(),
                    event.payload.to_string(),
                    event.trust.as_str(),
                    event.provenance.source,
                    event.prev_hash.to_vec(),
                    legacy_hash.to_vec(),
                    event.ts,
                ],
            )
            .unwrap();
        }

        let log = SqliteLog::open(&db).expect("valid v1 database should migrate");
        log.verify().expect("migrated chain should verify");
        let conn = Connection::open(&db).unwrap();
        let version: u8 = conn
            .query_row("SELECT hash_version FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, EVENT_HASH_VERSION);
        assert_eq!(
            meta_value(&conn, CHAIN_COUNT_KEY).unwrap().as_deref(),
            Some("1")
        );
        assert!(meta_value(&conn, CHAIN_HEAD_KEY).unwrap().is_some());
        std::fs::remove_dir_all(dir).ok();
    }
}

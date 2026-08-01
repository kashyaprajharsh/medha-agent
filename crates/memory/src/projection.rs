//! SQLite projection over `EventKind::MemoryWrite` events (D1). The event log
//! is the source of truth; this table + its FTS5 index are a queryable cache
//! that `rebuild` can always reconstruct from scratch — so replay, resume, and
//! fork/rewind all apply to memory for free.

use crate::entry::{MemoryEntry, Scope};
use kernel::{Event, EventKind};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("db: {0}")]
    Db(String),
    #[error("io: {0}")]
    Io(String),
    #[error("lock poisoned")]
    Poisoned,
}

/// The four mutations a memory event can carry. `Forget`/`Pin` address an
/// entry by `(scope, name)` only — they don't need the full claim to act.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum MemoryOp {
    Write {
        entry: MemoryEntry,
    },
    Update {
        entry: MemoryEntry,
    },
    Forget {
        scope: Scope,
        name: String,
    },
    Pin {
        scope: Scope,
        name: String,
        pinned: bool,
    },
}

impl MemoryOp {
    fn scope(&self) -> Scope {
        match self {
            MemoryOp::Write { entry } | MemoryOp::Update { entry } => entry.scope,
            MemoryOp::Forget { scope, .. } | MemoryOp::Pin { scope, .. } => *scope,
        }
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS entries (
            scope       TEXT NOT NULL,
            name        TEXT NOT NULL,
            claim       TEXT NOT NULL,
            description TEXT NOT NULL,
            kind        TEXT NOT NULL,
            trust       TEXT NOT NULL,
            confidence  TEXT NOT NULL,
            provenance  TEXT NOT NULL,
            sessions    TEXT NOT NULL DEFAULT '[]',
            version     INTEGER NOT NULL,
            pinned      INTEGER NOT NULL,
            links       TEXT NOT NULL,
            created     REAL NOT NULL,
            updated     REAL NOT NULL,
            tombstoned  INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (scope, name)
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
            scope UNINDEXED, name, description, claim
         );",
    )
}

/// Row → `MemoryEntry`, decoding the JSON-encoded `provenance`/`links` columns.
fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<MemoryEntry> {
    let scope: String = row.get("scope")?;
    let kind: String = row.get("kind")?;
    let trust: String = row.get("trust")?;
    let confidence: String = row.get("confidence")?;
    let provenance: String = row.get("provenance")?;
    let sessions: String = row.get("sessions")?;
    let links: String = row.get("links")?;
    Ok(MemoryEntry {
        name: row.get("name")?,
        claim: row.get("claim")?,
        description: row.get("description")?,
        kind: crate::entry::MemoryKind::parse(&kind).unwrap_or(crate::entry::MemoryKind::Project),
        scope: Scope::parse(&scope).unwrap_or(Scope::Project),
        trust: kernel::TrustLabel::parse(&trust).unwrap_or(kernel::TrustLabel::Memory),
        confidence: crate::entry::ConfidenceRung::parse(&confidence)
            .unwrap_or(crate::entry::ConfidenceRung::Candidate),
        provenance: serde_json::from_str(&provenance).unwrap_or_default(),
        sessions: serde_json::from_str(&sessions).unwrap_or_default(),
        version: row.get("version")?,
        pinned: row.get::<_, i64>("pinned")? != 0,
        links: serde_json::from_str(&links).unwrap_or_default(),
        created: row.get("created")?,
        updated: row.get("updated")?,
    })
}

fn upsert_into(conn: &Connection, table: &str, entry: &MemoryEntry) -> Result<(), MemoryError> {
    let unqualified_table = table.rsplit('.').next().unwrap_or(table);
    let provenance = serde_json::to_string(&entry.provenance).unwrap_or_default();
    let sessions = serde_json::to_string(&entry.sessions).unwrap_or_default();
    let links = serde_json::to_string(&entry.links).unwrap_or_default();
    conn.execute(
        &format!(
            "INSERT INTO {table}
            (scope, name, claim, description, kind, trust, confidence, provenance,
             sessions, version, pinned, links, created, updated, tombstoned)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14, 0)
         ON CONFLICT(scope, name) DO UPDATE SET
            claim=excluded.claim, description=excluded.description, kind=excluded.kind,
            trust=excluded.trust, confidence=excluded.confidence, provenance=excluded.provenance,
            sessions=excluded.sessions, version=excluded.version, pinned=excluded.pinned,
            links=excluded.links, created={unqualified_table}.created,
            updated=excluded.updated, tombstoned=0"
        ),
        rusqlite::params![
            entry.scope.as_str(),
            entry.name,
            entry.claim,
            entry.description,
            entry.kind.as_str(),
            entry.trust.as_str(),
            entry.confidence.as_str(),
            provenance,
            sessions,
            entry.version,
            entry.pinned as i64,
            links,
            entry.created,
            entry.updated,
        ],
    )
    .map_err(|e| MemoryError::Db(e.to_string()))?;
    Ok(())
}

fn upsert(conn: &Connection, entry: &MemoryEntry) -> Result<(), MemoryError> {
    upsert_into(conn, "entries", entry)?;
    // FTS mirror: delete-then-reinsert is simplest to keep in sync (no
    // external-content triggers needed at this size).
    conn.execute(
        "DELETE FROM entries_fts WHERE scope = ?1 AND name = ?2",
        rusqlite::params![entry.scope.as_str(), entry.name],
    )
    .map_err(|e| MemoryError::Db(e.to_string()))?;
    conn.execute(
        "INSERT INTO entries_fts (scope, name, description, claim) VALUES (?1,?2,?3,?4)",
        rusqlite::params![
            entry.scope.as_str(),
            entry.name,
            entry.description,
            entry.claim
        ],
    )
    .map_err(|e| MemoryError::Db(e.to_string()))?;
    Ok(())
}

/// Turn raw text into a safe FTS5 MATCH expression: each whitespace-split term
/// becomes a quoted phrase (internal `"` doubled), terms AND together. `None`
/// when no terms survive (empty/whitespace query).
fn fts_match_expr(raw: &str) -> Option<String> {
    let terms: Vec<String> = raw
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn forget_in(conn: &Connection, table: &str, scope: Scope, name: &str) -> Result<(), MemoryError> {
    conn.execute(
        &format!("UPDATE {table} SET tombstoned = 1 WHERE scope = ?1 AND name = ?2"),
        rusqlite::params![scope.as_str(), name],
    )
    .map_err(|e| MemoryError::Db(e.to_string()))?;
    Ok(())
}

fn forget(conn: &Connection, scope: Scope, name: &str) -> Result<(), MemoryError> {
    forget_in(conn, "entries", scope, name)?;
    conn.execute(
        "DELETE FROM entries_fts WHERE scope = ?1 AND name = ?2",
        rusqlite::params![scope.as_str(), name],
    )
    .map_err(|e| MemoryError::Db(e.to_string()))?;
    Ok(())
}

fn pin_in(
    conn: &Connection,
    table: &str,
    scope: Scope,
    name: &str,
    pinned: bool,
) -> Result<(), MemoryError> {
    conn.execute(
        &format!("UPDATE {table} SET pinned = ?1 WHERE scope = ?2 AND name = ?3"),
        rusqlite::params![pinned as i64, scope.as_str(), name],
    )
    .map_err(|e| MemoryError::Db(e.to_string()))?;
    Ok(())
}

fn pin(conn: &Connection, scope: Scope, name: &str, pinned: bool) -> Result<(), MemoryError> {
    pin_in(conn, "entries", scope, name, pinned)
}

const ENTRY_COLUMNS: &str = "scope, name, claim, description, kind, trust, confidence, provenance,
     sessions, version, pinned, links, created, updated, tombstoned";

fn replay_into_staging(
    conn: &Connection,
    schema: &str,
    scope: Option<Scope>,
    preserve_other_scope: bool,
    ops: &[MemoryOp],
) -> Result<(), MemoryError> {
    let stage = format!("{schema}entries_rebuild");
    let entries = format!("{schema}entries");
    let fts = format!("{schema}entries_fts");
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {stage} (
            scope       TEXT NOT NULL,
            name        TEXT NOT NULL,
            claim       TEXT NOT NULL,
            description TEXT NOT NULL,
            kind        TEXT NOT NULL,
            trust       TEXT NOT NULL,
            confidence  TEXT NOT NULL,
            provenance  TEXT NOT NULL,
            sessions    TEXT NOT NULL DEFAULT '[]',
            version     INTEGER NOT NULL,
            pinned      INTEGER NOT NULL,
            links       TEXT NOT NULL,
            created     REAL NOT NULL,
            updated     REAL NOT NULL,
            tombstoned  INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (scope, name)
         );
         DELETE FROM {stage};"
    ))
    .map_err(|error| MemoryError::Db(error.to_string()))?;
    if preserve_other_scope {
        let rebuilt_scope = scope.expect("preserving the other scope requires one target scope");
        conn.execute(
            &format!(
                "INSERT INTO {stage} ({ENTRY_COLUMNS})
                 SELECT {ENTRY_COLUMNS} FROM {entries} WHERE scope != ?1"
            ),
            [rebuilt_scope.as_str()],
        )
        .map_err(|error| MemoryError::Db(error.to_string()))?;
    }

    for op in ops
        .iter()
        .filter(|op| scope.is_none_or(|scope| op.scope() == scope))
    {
        match op {
            MemoryOp::Write { entry } | MemoryOp::Update { entry } => {
                upsert_into(conn, &stage, entry)?
            }
            MemoryOp::Forget { scope, name } => forget_in(conn, &stage, *scope, name)?,
            MemoryOp::Pin {
                scope,
                name,
                pinned,
            } => pin_in(conn, &stage, *scope, name, *pinned)?,
        }
    }

    // The live projection changes only after the complete replay exists. Every
    // statement below is in the caller's transaction, including the FTS mirror.
    conn.execute_batch(&format!(
        "DELETE FROM {entries};
         INSERT INTO {entries} ({ENTRY_COLUMNS}) SELECT {ENTRY_COLUMNS} FROM {stage};
         DELETE FROM {fts};
         INSERT INTO {fts} (scope, name, description, claim)
            SELECT scope, name, description, claim FROM {stage} WHERE tombstoned = 0;
         DELETE FROM {stage};"
    ))
    .map_err(|error| MemoryError::Db(error.to_string()))
}

fn memory_ops(events: impl Iterator<Item = Event>) -> Result<Vec<MemoryOp>, MemoryError> {
    events
        .filter(|event| event.kind == EventKind::MemoryWrite)
        .map(|event| {
            serde_json::from_value::<MemoryOp>(event.payload).map_err(|error| {
                MemoryError::Db(format!(
                    "malformed durable memory event {}: {error}",
                    event.id
                ))
            })
        })
        .collect()
}

/// Two SQLite targets (D9): project entries live in the workspace projection,
/// user entries in the user-global store. Recall merges both, project-first.
#[derive(Clone)]
pub struct MemoryProjection {
    project: Arc<Mutex<Connection>>,
    user: Arc<Mutex<Connection>>,
    /// Async callers serialize before entering Tokio's blocking pool. This
    /// prevents one locked SQLite database from occupying many blocking
    /// workers and lets queued requests be cancelled before they start.
    runtime_gate: Arc<tokio::sync::Semaphore>,
    user_path: PathBuf,
    same_database: bool,
}

impl MemoryProjection {
    pub fn open(
        project_path: impl AsRef<Path>,
        user_path: impl AsRef<Path>,
    ) -> Result<Self, MemoryError> {
        let open_one = |path: &Path| -> Result<Connection, MemoryError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| MemoryError::Io(e.to_string()))?;
            }
            let conn = Connection::open(path).map_err(|e| MemoryError::Db(e.to_string()))?;
            conn.busy_timeout(SQLITE_BUSY_TIMEOUT)
                .map_err(|e| MemoryError::Db(e.to_string()))?;
            // DELETE journal mode gives SQLite a super-journal for the
            // project+attached-user transaction used by a full rebuild.
            conn.pragma_update(None, "journal_mode", "DELETE")
                .map_err(|e| MemoryError::Db(e.to_string()))?;
            init_schema(&conn).map_err(|e| MemoryError::Db(e.to_string()))?;
            Ok(conn)
        };
        let project_path = project_path.as_ref().to_path_buf();
        let user_path = user_path.as_ref().to_path_buf();
        let project = open_one(&project_path)?;
        let user = open_one(&user_path)?;
        let same_database = match (
            std::fs::canonicalize(&project_path),
            std::fs::canonicalize(&user_path),
        ) {
            (Ok(project), Ok(user)) => project == user,
            _ => false,
        };
        Ok(Self {
            project: Arc::new(Mutex::new(project)),
            user: Arc::new(Mutex::new(user)),
            runtime_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            user_path,
            same_database,
        })
    }

    fn conn_for(&self, scope: Scope) -> &Mutex<Connection> {
        match scope {
            Scope::Project => self.project.as_ref(),
            Scope::User => self.user.as_ref(),
        }
    }

    pub(crate) async fn run_blocking<T, F>(&self, operation: F) -> Result<T, MemoryError>
    where
        T: Send + 'static,
        F: FnOnce(MemoryProjection) -> Result<T, MemoryError> + Send + 'static,
    {
        let permit = self
            .runtime_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| MemoryError::Db(format!("SQLite worker closed: {error}")))?;
        let projection = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(projection)
        })
        .await
        .map_err(|error| MemoryError::Db(format!("SQLite worker failed: {error}")))?
    }

    pub fn apply(&self, op: &MemoryOp) -> Result<(), MemoryError> {
        let mut conn = self
            .conn_for(op.scope())
            .lock()
            .map_err(|_| MemoryError::Poisoned)?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| MemoryError::Db(error.to_string()))?;
        match op {
            MemoryOp::Write { entry } | MemoryOp::Update { entry } => upsert(&tx, entry)?,
            MemoryOp::Forget { scope, name } => forget(&tx, *scope, name)?,
            MemoryOp::Pin {
                scope,
                name,
                pinned,
            } => pin(&tx, *scope, name, *pinned)?,
        };
        tx.commit()
            .map_err(|error| MemoryError::Db(error.to_string()))
    }

    /// Drop every row in both tables — the starting state `rebuild` replays onto.
    pub fn clear(&self) -> Result<(), MemoryError> {
        self.rebuild(std::iter::empty())
    }

    pub fn clear_project(&self) -> Result<(), MemoryError> {
        self.rebuild_project(std::iter::empty())
    }

    /// Replay into staging tables, then replace primary + FTS projections in one
    /// transaction. Malformed durable events fail before either live database
    /// changes.
    pub fn rebuild(&self, events: impl Iterator<Item = Event>) -> Result<(), MemoryError> {
        let ops = memory_ops(events)?;
        let mut project = self.project.lock().map_err(|_| MemoryError::Poisoned)?;
        let _user = self.user.lock().map_err(|_| MemoryError::Poisoned)?;
        if self.same_database {
            let tx = project
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| MemoryError::Db(error.to_string()))?;
            replay_into_staging(&tx, "", None, false, &ops)?;
            return tx
                .commit()
                .map_err(|error| MemoryError::Db(error.to_string()));
        }

        let user_path = self.user_path.to_string_lossy().into_owned();
        project
            .execute("ATTACH DATABASE ?1 AS rebuild_user", [&user_path])
            .map_err(|error| MemoryError::Db(error.to_string()))?;
        let result = (|| {
            let tx = project
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| MemoryError::Db(error.to_string()))?;
            replay_into_staging(&tx, "", Some(Scope::Project), false, &ops)?;
            replay_into_staging(&tx, "rebuild_user.", Some(Scope::User), false, &ops)?;
            tx.commit()
                .map_err(|error| MemoryError::Db(error.to_string()))
        })();
        let detached = project
            .execute_batch("DETACH DATABASE rebuild_user")
            .map_err(|error| MemoryError::Db(error.to_string()));
        result.and(detached)
    }

    pub fn rebuild_project(&self, events: impl Iterator<Item = Event>) -> Result<(), MemoryError> {
        let ops = memory_ops(events)?;
        let mut project = self.project.lock().map_err(|_| MemoryError::Poisoned)?;
        let tx = project
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| MemoryError::Db(error.to_string()))?;
        replay_into_staging(&tx, "", Some(Scope::Project), self.same_database, &ops)?;
        tx.commit()
            .map_err(|error| MemoryError::Db(error.to_string()))
    }

    pub fn get(&self, scope: Scope, name: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        let conn = self
            .conn_for(scope)
            .lock()
            .map_err(|_| MemoryError::Poisoned)?;
        conn.query_row(
            "SELECT * FROM entries WHERE scope = ?1 AND name = ?2 AND tombstoned = 0",
            rusqlite::params![scope.as_str(), name],
            row_to_entry,
        )
        .map(Some)
        .or_else(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                Ok(None)
            } else {
                Err(e)
            }
        })
        .map_err(|e| MemoryError::Db(e.to_string()))
    }

    fn list_scope(&self, scope: Scope) -> Result<Vec<MemoryEntry>, MemoryError> {
        let conn = self
            .conn_for(scope)
            .lock()
            .map_err(|_| MemoryError::Poisoned)?;
        let mut stmt = conn
            .prepare("SELECT * FROM entries WHERE scope = ?1 AND tombstoned = 0")
            .map_err(|e| MemoryError::Db(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![scope.as_str()], row_to_entry)
            .map_err(|e| MemoryError::Db(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| MemoryError::Db(e.to_string()))
    }

    /// Merged read across both scopes — project wins on name collision (D9).
    pub fn list(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        let mut out = self.list_scope(Scope::Project)?;
        let seen: std::collections::HashSet<String> = out.iter().map(|e| e.name.clone()).collect();
        out.extend(
            self.list_scope(Scope::User)?
                .into_iter()
                .filter(|e| !seen.contains(&e.name)),
        );
        Ok(out)
    }

    fn search_scope(
        &self,
        scope: Scope,
        match_expr: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let conn = self
            .conn_for(scope)
            .lock()
            .map_err(|_| MemoryError::Poisoned)?;
        let mut stmt = conn
            .prepare(
                "SELECT e.* FROM entries e
                 JOIN entries_fts f ON f.scope = e.scope AND f.name = e.name
                 WHERE f.entries_fts MATCH ?1 AND e.tombstoned = 0
                 ORDER BY rank LIMIT ?2",
            )
            .map_err(|e| MemoryError::Db(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![match_expr, limit as i64], row_to_entry)
            .map_err(|e| MemoryError::Db(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| MemoryError::Db(e.to_string()))
    }

    /// FTS5 search merged across both scopes — project wins on name collision.
    /// The raw query is quoted term-by-term first: MATCH treats `-`, `'`, `:`
    /// as operators, so unsanitized model/user text ("co-authored", "don't")
    /// would be a syntax error, not a search.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        let Some(match_expr) = fts_match_expr(query) else {
            return Ok(Vec::new());
        };
        let mut out = self.search_scope(Scope::Project, &match_expr, limit)?;
        let seen: std::collections::HashSet<String> = out.iter().map(|e| e.name.clone()).collect();
        out.extend(
            self.search_scope(Scope::User, &match_expr, limit)?
                .into_iter()
                .filter(|e| !seen.contains(&e.name)),
        );
        out.truncate(limit);
        Ok(out)
    }

    pub async fn apply_async(&self, op: &MemoryOp) -> Result<(), MemoryError> {
        let op = op.clone();
        self.run_blocking(move |projection| projection.apply(&op))
            .await
    }

    pub async fn rebuild_async(&self, events: Vec<Event>) -> Result<(), MemoryError> {
        self.run_blocking(move |projection| projection.rebuild(events.into_iter()))
            .await
    }

    pub async fn rebuild_project_async(&self, events: Vec<Event>) -> Result<(), MemoryError> {
        self.run_blocking(move |projection| projection.rebuild_project(events.into_iter()))
            .await
    }

    pub async fn get_async(
        &self,
        scope: Scope,
        name: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        let name = name.to_string();
        self.run_blocking(move |projection| projection.get(scope, &name))
            .await
    }

    pub async fn list_async(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.run_blocking(|projection| projection.list()).await
    }

    pub async fn search_async(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let query = query.to_string();
        self.run_blocking(move |projection| projection.search(&query, limit))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{ConfidenceRung, MemoryKind};
    use kernel::{EventLog, Session, TrustLabel};
    use ulid::Ulid;

    fn entry(name: &str, scope: Scope, version: u32) -> MemoryEntry {
        MemoryEntry {
            name: name.into(),
            claim: format!("claim body for {name}"),
            description: format!("hook for {name}"),
            kind: MemoryKind::Feedback,
            scope,
            trust: TrustLabel::User,
            confidence: ConfidenceRung::UserStated,
            provenance: vec![Ulid::new()],
            sessions: vec![Ulid::new()],
            version,
            pinned: false,
            links: vec![],
            created: 1000.0,
            updated: 1000.0,
        }
    }

    fn temp_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("medha-memory-{tag}-{}", Ulid::new()));
        (dir.join("project.db"), dir.join("user.db"))
    }

    /// A `memory.write`-shaped event, built directly from `Event`'s public
    /// fields — the kernel doesn't grow a typed constructor for this until M2
    /// wires the write tool; these tests only need the projection's replay path.
    fn memory_event(s: &Session, op: MemoryOp) -> Event {
        Event {
            id: Ulid::new(),
            session_id: s.id,
            parent_id: None,
            kind: EventKind::MemoryWrite,
            payload: serde_json::to_value(&op).unwrap(),
            trust: TrustLabel::Memory,
            provenance: kernel::Provenance {
                source: "test".into(),
            },
            prev_hash: [0u8; 32],
            hash_version: kernel::events::EVENT_HASH_VERSION,
            ts: 0.0,
        }
    }

    #[test]
    fn apply_all_four_ops() {
        let (p, u) = temp_paths("ops");
        let proj = MemoryProjection::open(&p, &u).unwrap();

        proj.apply(&MemoryOp::Write {
            entry: entry("e1", Scope::Project, 1),
        })
        .unwrap();
        assert!(proj.get(Scope::Project, "e1").unwrap().is_some());

        proj.apply(&MemoryOp::Update {
            entry: entry("e1", Scope::Project, 2),
        })
        .unwrap();
        assert_eq!(proj.get(Scope::Project, "e1").unwrap().unwrap().version, 2);

        proj.apply(&MemoryOp::Pin {
            scope: Scope::Project,
            name: "e1".into(),
            pinned: true,
        })
        .unwrap();
        assert!(proj.get(Scope::Project, "e1").unwrap().unwrap().pinned);

        proj.apply(&MemoryOp::Forget {
            scope: Scope::Project,
            name: "e1".into(),
        })
        .unwrap();
        assert!(
            proj.get(Scope::Project, "e1").unwrap().is_none(),
            "forget hides from get"
        );
        assert!(
            !proj.list().unwrap().iter().any(|e| e.name == "e1"),
            "forget hides from list"
        );

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn locked_projection_does_not_block_timers_or_queued_cancellation() {
        use crate::MemoryStore;

        let (p, u) = temp_paths("async-lock");
        let projection = Arc::new(MemoryProjection::open(&p, &u).unwrap());
        let blocker = Connection::open(&p).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let first = tokio::spawn({
            let projection = Arc::clone(&projection);
            async move {
                MemoryStore::write(
                    projection.as_ref(),
                    entry("first-blocked-write", Scope::Project, 1),
                )
                .await
            }
        });
        tokio::task::yield_now().await;
        tokio::time::timeout(
            Duration::from_millis(250),
            tokio::time::sleep(Duration::from_millis(25)),
        )
        .await
        .expect("a locked projection must not starve independent runtime timers");
        assert!(
            !first.is_finished(),
            "the external writer lock was ineffective"
        );

        let queued = tokio::spawn({
            let projection = Arc::clone(&projection);
            async move {
                MemoryStore::write(
                    projection.as_ref(),
                    entry("cancelled-before-start", Scope::Project, 1),
                )
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
            .expect("write should finish after releasing the database")
            .unwrap()
            .unwrap();
        assert!(
            projection
                .get_async(Scope::Project, "first-blocked-write")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            projection
                .get_async(Scope::Project, "cancelled-before-start")
                .await
                .unwrap()
                .is_none()
        );
        drop(blocker);
        drop(projection);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn forget_tombstones_but_event_replay_still_reconstructs_it() {
        let (p, u) = temp_paths("tombstone");
        let s = Session::new();
        let events = vec![
            memory_event(
                &s,
                MemoryOp::Write {
                    entry: entry("e1", Scope::Project, 1),
                },
            ),
            memory_event(
                &s,
                MemoryOp::Forget {
                    scope: Scope::Project,
                    name: "e1".into(),
                },
            ),
        ];
        let proj = MemoryProjection::open(&p, &u).unwrap();
        proj.rebuild(events.into_iter()).unwrap();
        assert!(proj.get(Scope::Project, "e1").unwrap().is_none());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn replay_determinism_rebuild_matches_incremental() {
        let (p1, u1) = temp_paths("incr");
        let (p2, u2) = temp_paths("rebuild");
        let s = Session::new();
        let events = vec![
            memory_event(
                &s,
                MemoryOp::Write {
                    entry: entry("a", Scope::Project, 1),
                },
            ),
            memory_event(
                &s,
                MemoryOp::Write {
                    entry: entry("b", Scope::User, 1),
                },
            ),
            memory_event(
                &s,
                MemoryOp::Update {
                    entry: entry("a", Scope::Project, 2),
                },
            ),
            memory_event(
                &s,
                MemoryOp::Update {
                    entry: entry("b", Scope::User, 2),
                },
            ),
            memory_event(
                &s,
                MemoryOp::Pin {
                    scope: Scope::User,
                    name: "b".into(),
                    pinned: true,
                },
            ),
        ];

        let incremental = MemoryProjection::open(&p1, &u1).unwrap();
        for e in &events {
            let op: MemoryOp = serde_json::from_value(e.payload.clone()).unwrap();
            incremental.apply(&op).unwrap();
        }

        let rebuilt = MemoryProjection::open(&p2, &u2).unwrap();
        rebuilt.rebuild(events.into_iter()).unwrap();

        let mut a = incremental.list().unwrap();
        let mut b = rebuilt.list().unwrap();
        a.sort_by(|x, y| x.name.cmp(&y.name));
        b.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(a, b, "rebuild-from-log must equal incremental apply");

        std::fs::remove_dir_all(p1.parent().unwrap()).ok();
        std::fs::remove_dir_all(p2.parent().unwrap()).ok();
    }

    #[test]
    fn mutation_failure_rolls_back_primary_and_fts_together() {
        let (p, u) = temp_paths("mutation-rollback");
        let proj = MemoryProjection::open(&p, &u).unwrap();
        let original = entry("atomic", Scope::Project, 1);
        proj.apply(&MemoryOp::Write {
            entry: original.clone(),
        })
        .unwrap();
        proj.project
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_memory_update
                 AFTER UPDATE ON entries
                 BEGIN
                   SELECT RAISE(ABORT, 'injected mutation failure');
                 END;",
            )
            .unwrap();

        let mut updated = entry("atomic", Scope::Project, 2);
        updated.claim = "replacement searchable claim".into();
        assert!(
            proj.apply(&MemoryOp::Update {
                entry: updated.clone()
            })
            .is_err()
        );
        assert_eq!(
            proj.get(Scope::Project, "atomic").unwrap().unwrap(),
            original
        );
        assert_eq!(proj.search("replacement", 10).unwrap().len(), 0);
        assert_eq!(proj.search("claim body", 10).unwrap().len(), 1);

        proj.project
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_memory_update")
            .unwrap();
        proj.apply(&MemoryOp::Update { entry: updated }).unwrap();
        assert_eq!(proj.search("replacement", 10).unwrap().len(), 1);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn malformed_rebuild_event_preserves_the_prior_projection() {
        let (p, u) = temp_paths("malformed-rebuild");
        let proj = MemoryProjection::open(&p, &u).unwrap();
        let original = entry("prior-valid", Scope::Project, 1);
        proj.apply(&MemoryOp::Write {
            entry: original.clone(),
        })
        .unwrap();
        let session = Session::new();
        let mut malformed = memory_event(
            &session,
            MemoryOp::Forget {
                scope: Scope::Project,
                name: "prior-valid".into(),
            },
        );
        malformed.payload = serde_json::json!({"op": "write", "entry": "not an entry"});

        let error = proj
            .rebuild(std::iter::once(malformed))
            .expect_err("malformed durable events must fail visibly");
        assert!(error.to_string().contains("malformed durable memory event"));
        assert_eq!(
            proj.get(Scope::Project, "prior-valid").unwrap().unwrap(),
            original
        );
        assert_eq!(proj.search("prior-valid", 10).unwrap().len(), 1);
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn full_rebuild_failure_rolls_back_both_attached_scopes() {
        let (p, u) = temp_paths("cross-db-rebuild");
        let proj = MemoryProjection::open(&p, &u).unwrap();
        let old_project = entry("old-project", Scope::Project, 1);
        let old_user = entry("old-user", Scope::User, 1);
        proj.apply(&MemoryOp::Write {
            entry: old_project.clone(),
        })
        .unwrap();
        proj.apply(&MemoryOp::Write {
            entry: old_user.clone(),
        })
        .unwrap();
        proj.user
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_user_swap
                 BEFORE DELETE ON entries
                 BEGIN
                   SELECT RAISE(ABORT, 'injected user swap failure');
                 END;",
            )
            .unwrap();
        let session = Session::new();
        let events = vec![
            memory_event(
                &session,
                MemoryOp::Write {
                    entry: entry("new-project", Scope::Project, 1),
                },
            ),
            memory_event(
                &session,
                MemoryOp::Write {
                    entry: entry("new-user", Scope::User, 1),
                },
            ),
        ];

        assert!(proj.rebuild(events.clone().into_iter()).is_err());
        assert_eq!(
            proj.get(Scope::Project, "old-project").unwrap(),
            Some(old_project)
        );
        assert_eq!(proj.get(Scope::User, "old-user").unwrap(), Some(old_user));
        assert!(proj.get(Scope::Project, "new-project").unwrap().is_none());
        assert!(proj.get(Scope::User, "new-user").unwrap().is_none());

        proj.user
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_user_swap")
            .unwrap();
        proj.rebuild(events.into_iter()).unwrap();
        assert!(proj.get(Scope::Project, "old-project").unwrap().is_none());
        assert!(proj.get(Scope::User, "old-user").unwrap().is_none());
        assert!(proj.get(Scope::Project, "new-project").unwrap().is_some());
        assert!(proj.get(Scope::User, "new-user").unwrap().is_some());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn project_only_rebuild_preserves_user_memory_and_drops_branch_future() {
        let (p, u) = temp_paths("branch-project");
        let proj = MemoryProjection::open(&p, &u).unwrap();
        let session = Session::new();
        let user = entry("global-pref", Scope::User, 1);
        let before = entry("before-cut", Scope::Project, 1);
        let after = entry("after-cut", Scope::Project, 1);
        proj.apply(&MemoryOp::Write { entry: user }).unwrap();
        proj.apply(&MemoryOp::Write { entry: after }).unwrap();

        proj.rebuild_project(
            vec![memory_event(&session, MemoryOp::Write { entry: before })].into_iter(),
        )
        .unwrap();
        assert!(proj.get(Scope::Project, "before-cut").unwrap().is_some());
        assert!(proj.get(Scope::Project, "after-cut").unwrap().is_none());
        assert!(proj.get(Scope::User, "global-pref").unwrap().is_some());

        proj.clear_project().unwrap();
        assert!(proj.get(Scope::Project, "before-cut").unwrap().is_none());
        assert!(proj.get(Scope::User, "global-pref").unwrap().is_some());
    }

    #[test]
    fn fork_semantics_post_fork_memories_are_absent() {
        use futures::executor::block_on;
        let log = kernel::InMemoryLog::new();
        let s = Session::new();
        let before = block_on(log.append(memory_event(
            &s,
            MemoryOp::Write {
                entry: entry("before-fork", Scope::Project, 1),
            },
        )))
        .unwrap();
        block_on(log.append(memory_event(
            &s,
            MemoryOp::Write {
                entry: entry("after-fork", Scope::Project, 1),
            },
        )))
        .unwrap();

        // Fork *before* "after-fork" was written — the branch should only ever
        // have learned "before-fork" (§18.4 time-travel applies to memory too).
        let cut = block_on(log.events(s.id))
            .into_iter()
            .find(|e| e.id != before.id)
            .unwrap()
            .id;
        let branch_id = block_on(log.fork(s.id, cut)).unwrap();
        let branch_events = block_on(log.events(branch_id));

        let (p, u) = temp_paths("fork");
        let proj = MemoryProjection::open(&p, &u).unwrap();
        proj.rebuild(branch_events.into_iter()).unwrap();

        assert!(proj.get(Scope::Project, "before-fork").unwrap().is_some());
        assert!(
            proj.get(Scope::Project, "after-fork").unwrap().is_none(),
            "fork must not see post-cut memories"
        );

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn search_finds_entries_and_survives_fts5_syntax_characters() {
        let (p, u) = temp_paths("search");
        let proj = MemoryProjection::open(&p, &u).unwrap();
        let mut e = entry("no-coauthored-by", Scope::Project, 1);
        e.claim = "Omit the Co-Authored-By trailer from commits.".into();
        e.description = "commit trailer preference".into();
        proj.apply(&MemoryOp::Write { entry: e }).unwrap();

        // Plain word, ranked JOIN path.
        assert_eq!(proj.search("trailer", 5).unwrap().len(), 1);
        // Multi-word (terms AND together).
        assert_eq!(proj.search("commit trailer", 5).unwrap().len(), 1);
        // Hyphen and apostrophe are FTS5 MATCH syntax — must search, not error.
        assert_eq!(proj.search("co-authored", 5).unwrap().len(), 1);
        assert_eq!(proj.search("don't", 5).unwrap().len(), 0);
        // Empty/whitespace query is a no-op, not a syntax error.
        assert_eq!(proj.search("   ", 5).unwrap().len(), 0);
        // Tombstoned entries never surface.
        proj.apply(&MemoryOp::Forget {
            scope: Scope::Project,
            name: "no-coauthored-by".into(),
        })
        .unwrap();
        assert_eq!(proj.search("trailer", 5).unwrap().len(), 0);

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn scope_routing_and_project_wins_collision() {
        let (p, u) = temp_paths("scope");
        let proj = MemoryProjection::open(&p, &u).unwrap();
        proj.apply(&MemoryOp::Write {
            entry: entry("shared-name", Scope::Project, 1),
        })
        .unwrap();
        proj.apply(&MemoryOp::Write {
            entry: entry("shared-name", Scope::User, 1),
        })
        .unwrap();

        // Each landed in its own DB.
        assert!(proj.get(Scope::Project, "shared-name").unwrap().is_some());
        assert!(proj.get(Scope::User, "shared-name").unwrap().is_some());

        // Merged read resolves project-first.
        let merged = proj.list().unwrap();
        let hit = merged.iter().find(|e| e.name == "shared-name").unwrap();
        assert_eq!(hit.scope, Scope::Project, "project wins on name collision");
        assert_eq!(merged.iter().filter(|e| e.name == "shared-name").count(), 1);

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }
}

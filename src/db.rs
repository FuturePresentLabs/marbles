//! The store: one SQLite database, one writer, every project on the machine.
//!
//! There is exactly one `Connection`, behind a mutex, with `BEGIN IMMEDIATE` on every mutation —
//! single-writer is not an optimization here, it is the design. The API server owns it; CLIs that
//! are not the server talk to the server. Cross-project queries (the merged portfolio view) are
//! just queries, because the projects share a database instead of syncing a lineage.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::time::WorkWeek;
use crate::types::*;

pub mod snapshot;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS project(
  slug        TEXT PRIMARY KEY,
  root        TEXT NOT NULL UNIQUE,
  prefix      TEXT NOT NULL,
  policy_json TEXT NOT NULL DEFAULT '{}',
  created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS folder(
  slug       TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  parent     TEXT REFERENCES folder(slug),
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS project_folder(
  project     TEXT PRIMARY KEY REFERENCES project(slug) ON DELETE CASCADE,
  folder      TEXT NOT NULL REFERENCES folder(slug),
  assigned_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS project_folder_folder ON project_folder(folder);
CREATE TABLE IF NOT EXISTS issue(
  id           TEXT PRIMARY KEY,
  project      TEXT NOT NULL REFERENCES project(slug),
  title        TEXT NOT NULL,
  description  TEXT NOT NULL DEFAULT '',
  status       TEXT NOT NULL CHECK(status IN ('open','in_progress','review','blocked','deferred','closed')),
  issue_type   TEXT NOT NULL DEFAULT 'task',
  priority     INTEGER NOT NULL DEFAULT 2,
  parent       TEXT,
  labels_json  TEXT NOT NULL DEFAULT '[]',
  assignee     TEXT,
  actor_kind   TEXT,
  claimed_at   INTEGER,
  expires_at   INTEGER,
  available_at INTEGER,
  created_by   TEXT NOT NULL DEFAULT '',
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  closed_at    INTEGER,
  close_reason TEXT,
  evidence_json TEXT NOT NULL DEFAULT '[]',
  metadata_json TEXT NOT NULL DEFAULT '{}',
  external_ref TEXT
);
CREATE INDEX IF NOT EXISTS issue_open ON issue(status, project);
CREATE INDEX IF NOT EXISTS issue_expiry ON issue(expires_at);
CREATE TABLE IF NOT EXISTS dep(
  issue_id    TEXT NOT NULL,
  depends_on  TEXT NOT NULL,
  PRIMARY KEY(issue_id, depends_on)
);
-- Cross-store blocker edges. The child is local; the reference names an issue in another
-- store and carries the last verified assertion. This table never creates an `issue` row and is
-- deliberately separate from `dep`, whose `depends_on` must resolve locally.
CREATE TABLE IF NOT EXISTS remote_dep(
  issue_id        TEXT NOT NULL,
  canonical_ref   TEXT NOT NULL,
  store_id        TEXT NOT NULL,
  remote_issue_id TEXT NOT NULL,
  state           TEXT NOT NULL,
  asserted_at     INTEGER NOT NULL,
  observed_at     INTEGER NOT NULL,
  signature       TEXT NOT NULL,
  key_id          TEXT NOT NULL,
  PRIMARY KEY(issue_id, canonical_ref)
);
CREATE TABLE IF NOT EXISTS history(
  seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  issue_id TEXT NOT NULL,
  ts       INTEGER NOT NULL,
  actor    TEXT NOT NULL,
  event    TEXT NOT NULL,
  detail   TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS outbound_event(
  seq             INTEGER PRIMARY KEY,
  store_id        TEXT NOT NULL,
  issue_id        TEXT NOT NULL,
  project         TEXT NOT NULL,
  occurred_at     INTEGER NOT NULL,
  event           TEXT NOT NULL,
  delivered_at    INTEGER,
  attempts        INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL,
  last_error      TEXT
);
CREATE INDEX IF NOT EXISTS outbound_event_pending
  ON outbound_event(delivered_at, next_attempt_at, seq);
CREATE TABLE IF NOT EXISTS store_meta(
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("postgres: {0}")]
    Postgres(#[from] postgres::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no issue {0}")]
    NotFound(String),
    #[error("no project {0}")]
    NoProject(String),
    #[error(
        "invalid status {0} (expected one of: open, in_progress, review, blocked, deferred, closed)"
    )]
    BadStatus(String),
    #[error("issue {0} is already claimed by {1}")]
    AlreadyClaimed(String, String),
    #[error("adding {child} -> {parent} would create a dependency cycle")]
    Cycle { child: String, parent: String },
    #[error("{0}")]
    Bad(String),
    #[error(
        "refusing to close {0}: done means delivered. Attach --pr/--commit evidence, or --ack \"no delivery expected: <reason>\" for non-code outcomes"
    )]
    NoEvidence(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Storage contract used by the hosted API.
///
/// Local operation deliberately remains a concrete [`Db`]. Hosted backends implement this
/// narrow contract so transport/auth code cannot depend on SQLite internals.
pub trait Store: Send + Sync {
    fn ensure_project(&self, slug: &str, root: &str, prefix: &str) -> Result<()>;
    fn projects(&self) -> Result<Vec<(String, String, String)>>;
    fn ensure_folder(&self, folder: &Folder, at: i64) -> Result<()>;
    fn set_project_folder(&self, project: &str, folder: Option<&str>, at: i64) -> Result<()>;
    fn folder_view(&self) -> Result<FolderView>;
    fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<OutboundEvent>>;
    fn mark_event_delivered(&self, seq: i64, at: i64) -> Result<()>;
    fn mark_event_failed(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()>;
    fn create(&self, spec: &NewIssue, at: i64) -> Result<String>;
    fn get(&self, id: &str) -> Result<Issue>;
    fn list(&self, project: Option<&str>, all: bool, only_id: Option<&str>) -> Result<Vec<Issue>>;
    fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>>;
    fn update(&self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue>;
    fn add_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()>;
    fn remove_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()>;
    fn add_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()>;
    fn remove_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()>;
    fn claim(&self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt>;
    fn touch(&self, id: &str, assignee: &str, at: i64) -> Result<i64>;
    fn release(&self, id: &str, actor: &str, at: i64) -> Result<()>;
    fn sweep(&self, at: i64) -> Result<SweepReport>;
    fn close(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue>;
    fn supersede(&self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()>;
    fn history(&self, id: &str) -> Result<Vec<HistoryEvent>>;
    fn stats(&self) -> Result<serde_json::Value>;
    fn prometheus_metrics(&self, at: i64) -> Result<String>;
}

pub trait CompanyStoreRegistry: Send + Sync {
    fn for_company(&self, company_id: &str) -> Result<Arc<dyn Store>>;
    fn sweep_open(&self, now: i64) -> Result<Vec<(String, SweepReport)>>;
    fn open_stores(&self) -> Result<Vec<(String, Arc<dyn Store>)>>;
    /// Read-only readiness check. Backends may probe their shared connection
    /// even before the first authenticated tenant has opened a store.
    fn health_check(&self) -> Result<()> {
        for (_, store) in self.open_stores()? {
            store.projects()?;
        }
        Ok(())
    }
}

pub struct Db {
    conn: Mutex<Connection>,
    policy: Policy,
    week: WorkWeek,
    /// Stable identity of this physical store. Federation event ids are scoped by it so two
    /// independent stores can never mint the same identity for the same local sequence.
    store_id: String,
    /// The on-disk path, when there is one. `None` for in-memory test stores.
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupReport {
    pub path: String,
    pub bytes: u64,
    pub created_at: i64,
    pub projects: i64,
    pub issues: i64,
    pub deps: i64,
    pub history: i64,
    pub integrity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub integrity: String,
    pub quick_check: String,
    pub foreign_key_violations: Vec<String>,
    pub journal_mode: String,
    pub synchronous: String,
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub projects: i64,
    pub issues: i64,
    pub deps: i64,
    pub history: i64,
    pub outbox_pending: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OutboundEvent {
    pub seq: i64,
    pub store_id: String,
    pub issue_id: String,
    pub project: String,
    pub occurred_at: i64,
    pub event: String,
    pub attempts: i64,
}

/// Lazily opened, physically separate company stores for hosted operation.
///
/// The company id comes exclusively from the verified principal. Keeping each
/// tenant in its own SQLite file makes the isolation boundary visible to
/// operators and backups instead of relying on every query to remember an RLS
/// predicate.
pub struct CompanyStores {
    root: PathBuf,
    open: Mutex<BTreeMap<String, Arc<dyn Store>>>,
}

impl CompanyStores {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|e| Error::Bad(format!("creating {}: {e}", root.display())))?;
        let stores = Self {
            root,
            open: Mutex::new(BTreeMap::new()),
        };
        for entry in std::fs::read_dir(&stores.root)
            .map_err(|e| Error::Bad(format!("reading {}: {e}", stores.root.display())))?
        {
            let entry = entry.map_err(|e| Error::Bad(format!("reading company store: {e}")))?;
            if !entry
                .file_type()
                .map_err(|e| Error::Bad(format!("reading company store type: {e}")))?
                .is_dir()
            {
                continue;
            }
            let Some(company) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if entry.path().join("marbles.db").is_file() {
                validate_company_id(&company)?;
                stores.for_company(&company)?;
            }
        }
        Ok(stores)
    }

    pub fn for_company(&self, company_id: &str) -> Result<Arc<dyn Store>> {
        validate_company_id(company_id)?;
        let mut stores = self
            .open
            .lock()
            .map_err(|_| Error::Bad("company store lock poisoned".into()))?;
        if let Some(db) = stores.get(company_id) {
            return Ok(Arc::clone(db));
        }
        let company_root = self.root.join(company_id);
        std::fs::create_dir_all(&company_root).map_err(|e| {
            Error::Bad(format!(
                "creating company store {}: {e}",
                company_root.display()
            ))
        })?;
        let db: Arc<dyn Store> = Arc::new(Db::open(company_root.join("marbles.db"))?);
        stores.insert(company_id.to_owned(), Arc::clone(&db));
        Ok(db)
    }

    pub fn sweep_open(&self, now: i64) -> Result<Vec<(String, SweepReport)>> {
        let stores = self
            .open
            .lock()
            .map_err(|_| Error::Bad("company store lock poisoned".into()))?;
        stores
            .iter()
            .map(|(company, db)| Ok((company.clone(), db.sweep(now)?)))
            .collect()
    }

    pub fn open_stores(&self) -> Result<Vec<(String, Arc<dyn Store>)>> {
        let stores = self
            .open
            .lock()
            .map_err(|_| Error::Bad("company store lock poisoned".into()))?;
        Ok(stores
            .iter()
            .map(|(company, db)| (company.clone(), Arc::clone(db)))
            .collect())
    }
}

impl CompanyStoreRegistry for CompanyStores {
    fn for_company(&self, company_id: &str) -> Result<Arc<dyn Store>> {
        CompanyStores::for_company(self, company_id)
    }

    fn sweep_open(&self, now: i64) -> Result<Vec<(String, SweepReport)>> {
        CompanyStores::sweep_open(self, now)
    }

    fn open_stores(&self) -> Result<Vec<(String, Arc<dyn Store>)>> {
        CompanyStores::open_stores(self)
    }
}

pub(crate) fn validate_company_id(company_id: &str) -> Result<()> {
    if company_id.is_empty()
        || company_id.len() > 128
        || !company_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(Error::Bad(
            "OIDC company id is not a safe store name".into(),
        ));
    }
    Ok(())
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A fresh, opaque store id. Distinct physical stores must never share one, so this is random
/// rather than derived from a path or hostname.
fn new_store_id() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("rng");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("store_{hex}")
}

/// Read the identity persisted with this database, creating it on first open.
fn load_or_create_store_id(conn: &Connection) -> Result<String> {
    if let Some(store_id) = conn
        .query_row(
            "SELECT value FROM store_meta WHERE key='store_id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        && !store_id.is_empty()
    {
        return Ok(store_id);
    }
    let store_id = new_store_id();
    conn.execute(
        "INSERT INTO store_meta(key,value) VALUES('store_id',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![store_id],
    )?;
    Ok(store_id)
}

/// Add the store-scoped identity column to databases created before federation event identity.
fn backfill_outbound_store_id(conn: &Connection, store_id: &str) -> Result<()> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('outbound_event') WHERE name='store_id'",
        [],
        |row| row.get(0),
    )?;
    if present == 0 {
        conn.execute(
            "ALTER TABLE outbound_event ADD COLUMN store_id TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    conn.execute(
        "UPDATE outbound_event SET store_id=?1 WHERE store_id=''",
        params![store_id],
    )?;
    Ok(())
}

/// Beads-style short suffix: 4 chars of a lowercase alphabet, enough for human quoting and
/// collision-light at tracker scale (the primary key check retries a rare miss).
fn short_suffix() -> String {
    const ALPHA: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("rng");
    bytes
        .iter()
        .map(|b| ALPHA[(*b as usize) % ALPHA.len()] as char)
        .collect()
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
}

fn count_where(conn: &Connection, table: &str, predicate: &str) -> Result<i64> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
        [],
        |r| r.get(0),
    )?)
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Bad(format!("creating {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The tracker acknowledges mutations to agents; a power loss must not lose the
        // last claim/close after the client has a receipt. FULL is the durable WAL setting.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL serializes writers correctly but returns SQLITE_BUSY immediately; give contended
        // writes (sweeper vs daemon tick vs CLI on one machine) a few seconds to queue.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        let store_id = load_or_create_store_id(&conn)?;
        backfill_outbound_store_id(&conn, &store_id)?;
        Ok(Self {
            conn: Mutex::new(conn),
            policy: Policy::default(),
            week: WorkWeek::default(),
            store_id,
            path: Some(path.to_path_buf()),
        })
    }

    /// Open a store with an explicit identity. Used when the identity is owned outside the
    /// database (for example a test fixture or a future identity-backed store).
    pub fn open_with_store_id(path: impl AsRef<Path>, store_id: impl Into<String>) -> Result<Self> {
        let mut db = Self::open(path)?;
        db.set_store_id(store_id.into())?;
        Ok(db)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let store_id = load_or_create_store_id(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            policy: Policy::default(),
            week: WorkWeek::default(),
            store_id,
            path: None,
        })
    }

    pub fn in_memory_with_store_id(store_id: impl Into<String>) -> Result<Self> {
        let mut db = Self::in_memory()?;
        db.set_store_id(store_id.into())?;
        Ok(db)
    }

    /// The stable identity of this physical store.
    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    /// Replace the generated identity before any event exists. Refuses once the outbox has rows,
    /// because rewriting already-minted event identities would hide a real collision.
    fn set_store_id(&mut self, store_id: String) -> Result<()> {
        if store_id.is_empty() {
            return Err(Error::Bad("store id must not be empty".into()));
        }
        if self.store_id == store_id {
            return Ok(());
        }
        self.tx(|tx| {
            let existing: i64 =
                tx.query_row("SELECT COUNT(*) FROM outbound_event", [], |r| r.get(0))?;
            if existing > 0 {
                return Err(Error::Bad(
                    "cannot change store id after outbound events were recorded".into(),
                ));
            }
            tx.execute(
                "INSERT INTO store_meta(key,value) VALUES('store_id',?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![store_id],
            )?;
            Ok(())
        })?;
        self.store_id = store_id;
        Ok(())
    }

    pub fn with_policy(mut self, policy: Policy, week: WorkWeek) -> Self {
        self.policy = policy;
        self.week = week;
        self
    }

    fn tx(&self, f: impl FnOnce(&Connection) -> Result<()>) -> Result<()> {
        let conn = &mut *self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        match f(&tx) {
            Ok(()) => {
                tx.commit()?;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn query(&self, f: impl FnOnce(&Connection) -> Result<()>) -> Result<()> {
        f(&self.conn.lock().expect("store lock poisoned"))
    }

    // ---- projects ----

    pub fn ensure_project(&self, slug: &str, root: &str, prefix: &str) -> Result<()> {
        let ts = now();
        self.tx(|tx| {
            tx.execute(
                "INSERT INTO project(slug, root, prefix, created_at) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(slug) DO UPDATE SET root = excluded.root",
                params![slug, root, prefix, ts],
            )?;
            Ok(())
        })
    }

    /// Remove a whole store: its issues, every edge that touched them, and their
    /// history. Used by scratch fixtures and CI to make runs independent of
    /// leftovers. Edges *pointing in* from other stores are dropped too — a
    /// dangling blocker from a store that no longer exists must not gate anyone.
    pub fn drop_project(&self, slug: &str) -> Result<ProjectRemoval> {
        let mut out = ProjectRemoval::default();
        self.tx(|tx| {
            let exists: Option<String> = tx
                .query_row(
                    "SELECT slug FROM project WHERE slug = ?1",
                    params![slug],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(Error::NoProject(slug.to_string()));
            }
            let mine = "(SELECT id FROM issue WHERE project = ?1)";
            let issues: i64 = tx.query_row(
                "SELECT COUNT(*) FROM issue WHERE project = ?1",
                params![slug],
                |r| r.get(0),
            )?;
            tx.execute(
                &format!("DELETE FROM outbound_event WHERE issue_id IN {mine}"),
                params![slug],
            )?;
            tx.execute(
                &format!("DELETE FROM history WHERE issue_id IN {mine}"),
                params![slug],
            )?;
            tx.execute(
                &format!("DELETE FROM dep WHERE issue_id IN {mine} OR depends_on IN {mine}"),
                params![slug],
            )?;
            tx.execute(
                &format!("DELETE FROM remote_dep WHERE issue_id IN {mine}"),
                params![slug],
            )?;
            tx.execute("DELETE FROM issue WHERE project = ?1", params![slug])?;
            tx.execute("DELETE FROM project WHERE slug = ?1", params![slug])?;
            out.issues = issues;
            Ok(())
        })?;
        Ok(out)
    }

    pub fn projects(&self) -> Result<Vec<(String, String, String)>> {
        let mut out = Vec::new();
        self.query(|conn| {
            let mut rows = conn.prepare("SELECT slug, root, prefix FROM project ORDER BY slug")?;
            let mut rows = rows.query([])?;
            while let Some(row) = rows.next()? {
                out.push((row.get(0)?, row.get(1)?, row.get(2)?));
            }
            Ok(())
        })?;
        Ok(out)
    }

    // ---- folders ----

    /// Create or rename a folder. Folders may nest, but the parent chain is checked
    /// before the write so a rename cannot make the tree cyclic.
    pub fn ensure_folder(&self, folder: &Folder, at: i64) -> Result<()> {
        let slug = folder.slug.trim();
        if slug.is_empty() {
            return Err(Error::Bad("folder slug is empty".into()));
        }
        let name = folder.name.trim();
        if name.is_empty() {
            return Err(Error::Bad("folder name is empty".into()));
        }
        let parent = folder
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        self.tx(|tx| {
            if let Some(parent) = parent {
                if parent == slug {
                    return Err(Error::Bad("a folder cannot be its own parent".into()));
                }
                let exists: Option<String> = tx
                    .query_row(
                        "SELECT slug FROM folder WHERE slug = ?1",
                        params![parent],
                        |r| r.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    return Err(Error::Bad(format!("no folder {parent}")));
                }
                // If `slug` is already an ancestor of `parent`, attaching parent under slug
                // would close a cycle. Walk the parent chain and look for slug.
                let cycle: Option<String> = tx
                    .query_row(
                        "WITH RECURSIVE ancestors(slug) AS (
                             SELECT ?1
                             UNION
                             SELECT f.parent FROM folder f
                               JOIN ancestors a ON f.slug = a.slug
                              WHERE f.parent IS NOT NULL
                         )
                         SELECT slug FROM ancestors WHERE slug = ?2 LIMIT 1",
                        params![parent, slug],
                        |r| r.get(0),
                    )
                    .optional()?;
                if cycle.is_some() {
                    return Err(Error::Bad(format!(
                        "folder {slug} would become a descendant of itself"
                    )));
                }
            }
            tx.execute(
                "INSERT INTO folder(slug, name, parent, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(slug) DO UPDATE SET name = excluded.name, parent = excluded.parent",
                params![slug, name, parent, at],
            )?;
            Ok(())
        })
    }

    /// Put a project in a folder, or unfile it with `None`.
    pub fn set_project_folder(&self, project: &str, folder: Option<&str>, at: i64) -> Result<()> {
        self.tx(|tx| {
            let exists: Option<String> = tx
                .query_row(
                    "SELECT slug FROM project WHERE slug = ?1",
                    params![project],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(Error::NoProject(project.to_string()));
            }
            match folder.map(str::trim).filter(|f| !f.is_empty()) {
                Some(folder) => {
                    let exists: Option<String> = tx
                        .query_row(
                            "SELECT slug FROM folder WHERE slug = ?1",
                            params![folder],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if exists.is_none() {
                        return Err(Error::Bad(format!("no folder {folder}")));
                    }
                    tx.execute(
                        "INSERT INTO project_folder(project, folder, assigned_at)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(project) DO UPDATE SET
                           folder = excluded.folder, assigned_at = excluded.assigned_at",
                        params![project, folder, at],
                    )?;
                }
                None => {
                    tx.execute(
                        "DELETE FROM project_folder WHERE project = ?1",
                        params![project],
                    )?;
                }
            }
            Ok(())
        })
    }

    pub fn folder_view(&self) -> Result<FolderView> {
        let mut out = FolderView::default();
        self.query(|conn| {
            let mut stmt = conn
                .prepare("SELECT slug, name, parent, created_at FROM folder ORDER BY name, slug")?;
            out.folders = stmt
                .query_map([], |r| {
                    Ok(Folder {
                        slug: r.get(0)?,
                        name: r.get(1)?,
                        parent: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut stmt =
                conn.prepare("SELECT project, folder FROM project_folder ORDER BY project")?;
            out.projects = stmt
                .query_map([], |r| {
                    Ok(ProjectFolder {
                        project: r.get(0)?,
                        folder: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(())
        })?;
        Ok(out)
    }

    pub fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<OutboundEvent>> {
        let mut out = Vec::new();
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, store_id, issue_id, project, occurred_at, event, attempts
                 FROM outbound_event
                 WHERE delivered_at IS NULL AND next_attempt_at <= ?1
                 ORDER BY seq LIMIT ?2",
            )?;
            out = stmt
                .query_map(params![at, limit as i64], |row| {
                    Ok(OutboundEvent {
                        seq: row.get(0)?,
                        store_id: row.get(1)?,
                        issue_id: row.get(2)?,
                        project: row.get(3)?,
                        occurred_at: row.get(4)?,
                        event: row.get(5)?,
                        attempts: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(())
        })?;
        Ok(out)
    }

    pub fn mark_event_delivered(&self, seq: i64, at: i64) -> Result<()> {
        self.tx(|tx| {
            tx.execute(
                "UPDATE outbound_event SET delivered_at=?2, attempts=attempts+1,
                 last_error=NULL WHERE seq=?1 AND delivered_at IS NULL",
                params![seq, at],
            )?;
            Ok(())
        })
    }

    pub fn mark_event_failed(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()> {
        self.tx(|tx| {
            tx.execute(
                "UPDATE outbound_event SET attempts=attempts+1, next_attempt_at=?2,
                 last_error=?3 WHERE seq=?1 AND delivered_at IS NULL",
                params![seq, next_attempt_at, error],
            )?;
            Ok(())
        })
    }

    fn prefix_for(&self, conn: &rusqlite::Connection, project: &str) -> Result<String> {
        conn.query_row(
            "SELECT prefix FROM project WHERE slug = ?1",
            params![project],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| Error::NoProject(project.to_string()))
    }

    fn project_for_issue(&self, conn: &rusqlite::Connection, id: &str) -> Result<String> {
        conn.query_row(
            "SELECT project FROM issue WHERE id = ?1",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    // ---- durability ----

    /// Create an atomic, compact snapshot with `VACUUM INTO`. The target must not exist; a
    /// backup never overwrites a previous one by accident. The source is checked first so a
    /// corrupted database is not laundered into a second file.
    pub fn backup(&self, target: &Path) -> Result<BackupReport> {
        if target.exists() {
            return Err(Error::Bad(format!(
                "backup target {} already exists; choose a new path",
                target.display()
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Bad(format!("creating {}: {e}", parent.display())))?;
        }
        let created_at = now();
        let target_sql = target.to_string_lossy().replace('\'', "''");
        let conn = self.conn.lock().expect("store lock poisoned");
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if integrity != "ok" {
            return Err(Error::Bad(format!(
                "refusing to back up a database whose integrity_check is {integrity:?}"
            )));
        }
        let projects = count(&conn, "project")?;
        let issues = count(&conn, "issue")?;
        let deps = count(&conn, "dep")?;
        let history = count(&conn, "history")?;
        conn.execute_batch(&format!("VACUUM INTO '{target_sql}'"))?;
        drop(conn);
        let bytes = std::fs::metadata(target).map(|m| m.len()).unwrap_or(0);
        Ok(BackupReport {
            path: target.display().to_string(),
            bytes,
            created_at,
            projects,
            issues,
            deps,
            history,
            integrity,
        })
    }

    /// Report integrity, settings, row counts, and on-disk/WAL size without mutating anything.
    pub fn doctor(&self) -> Result<DoctorReport> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let quick_check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let sync: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        let synchronous = match sync {
            0 => "OFF",
            1 => "NORMAL",
            2 => "FULL",
            3 => "EXTRA",
            _ => "UNKNOWN",
        }
        .to_string();
        let mut foreign_key_violations = Vec::new();
        let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let table: String = row.get(0)?;
            let rowid: Option<i64> = row.get(1)?;
            let parent: String = row.get(2)?;
            foreign_key_violations.push(format!("{table} row {rowid:?} -> {parent}"));
        }
        let outbox_pending = count_where(&conn, "outbound_event", "delivered_at IS NULL")?;
        let (db_bytes, wal_bytes) = match &self.path {
            Some(path) => {
                let db = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                let wal = std::fs::metadata(format!("{}-wal", path.display()))
                    .map(|m| m.len())
                    .unwrap_or(0);
                (db, wal)
            }
            None => (0, 0),
        };
        Ok(DoctorReport {
            integrity,
            quick_check,
            foreign_key_violations,
            journal_mode,
            synchronous,
            db_bytes,
            wal_bytes,
            projects: count(&conn, "project")?,
            issues: count(&conn, "issue")?,
            deps: count(&conn, "dep")?,
            history: count(&conn, "history")?,
            outbox_pending,
        })
    }

    // ---- issues ----

    pub fn create(&self, spec: &NewIssue, at: i64) -> Result<String> {
        let project = spec
            .project
            .clone()
            .ok_or_else(|| Error::Bad("create requires a project".into()))?;
        let grace_available = spec
            .available_at
            .unwrap_or(at + self.policy.creation_grace_seconds);
        let mut created_id = String::new();
        self.tx(|tx| {
            let prefix = self.prefix_for(tx, &project)?;
            if let Some(parent) = spec.parent.as_deref() {
                let parent_project: Option<String> = tx
                    .query_row(
                        "SELECT project FROM issue WHERE id = ?1",
                        params![parent],
                        |r| r.get(0),
                    )
                    .optional()?;
                match parent_project {
                    None => return Err(Error::NotFound(parent.to_string())),
                    Some(p) if p != project => {
                        return Err(Error::Bad(format!(
                            "parent {parent} belongs to project {p}, not {project}; hierarchy is project-local (use a dependency edge for cross-project blockers)"
                        )))
                    }
                    Some(_) => {}
                }
            }
            let id = match &spec.id {
                Some(id) => {
                    let exists: Option<String> = tx
                        .query_row("SELECT id FROM issue WHERE id = ?1", params![id], |r| r.get(0))
                        .optional()?;
                    if exists.is_some() {
                        return Err(Error::Bad(format!("issue id {id} already exists")));
                    }
                    id.clone()
                }
                None => loop {
                    let candidate = format!("{prefix}-{}", short_suffix());
                    let exists: Option<String> = tx
                        .query_row("SELECT id FROM issue WHERE id = ?1", params![candidate], |r| r.get(0))
                        .optional()?;
                    if exists.is_none() {
                        break candidate;
                    }
                },
            };
            let labels = serde_json::to_string(&spec.labels)?;
            let metadata = spec
                .metadata
                .clone()
                .unwrap_or_else(|| serde_json::json!({}));
            let metadata_json = if metadata.is_object() {
                metadata.as_object().unwrap().clone()
            } else {
                return Err(Error::Bad("create metadata must be a JSON object".into()));
            };
            let mut metadata_json = metadata_json;
            if let Some(reference) = &spec.external_ref {
                metadata_json.insert("external_ref".into(), serde_json::json!(reference));
            }
            let priority = if spec.priority == 0 { 2 } else { spec.priority };
            tx.execute(
                "INSERT INTO issue(id, project, title, description, status, issue_type, priority,
                                   parent, labels_json, available_at, created_by, created_at, updated_at,
                                   metadata_json, external_ref)
                 VALUES (?1,?2,?3,?4,'open',?6,?7,?8,?9,?10,?11,?5,?5,?12,?13)",
                params![
                    id, project, spec.title, spec.description, at, spec.issue_type,
                    priority, spec.parent, labels, grace_available,
                    spec.created_by.clone().unwrap_or_default(),
                    serde_json::to_string(&metadata_json)?, spec.external_ref
                ],
            )?;
            log_event(tx, &id, at, spec.created_by.as_deref().unwrap_or("system"), "created", &spec.title)?;
            created_id = id;
            Ok(())
        })?;
        Ok(created_id)
    }

    pub fn get(&self, id: &str) -> Result<Issue> {
        let mut out = None;
        self.query(|conn| {
            out = Some(row_to_issue(conn, id)?.ok_or_else(|| Error::NotFound(id.to_string()))?);
            Ok(())
        })?;
        Ok(out.expect("set in closure"))
    }

    pub fn list(
        &self,
        project: Option<&str>,
        all: bool,
        only_id: Option<&str>,
    ) -> Result<Vec<Issue>> {
        let mut out = Vec::new();
        self.query(|conn| {
            let mut sql = String::from("SELECT id FROM issue WHERE 1=1");
            let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if !all {
                sql.push_str(" AND status != 'closed'");
            }
            if let Some(p) = project {
                sql.push_str(&format!(" AND project = ?{}", args.len() + 1));
                args.push(Box::new(p.to_string()));
            }
            if let Some(id) = only_id {
                sql.push_str(&format!(" AND id = ?{}", args.len() + 1));
                args.push(Box::new(id.to_string()));
            }
            sql.push_str(" ORDER BY priority, created_at, id");
            let ids: Vec<String> = {
                let mut stmt = conn.prepare(&sql)?;
                let refs: Vec<&dyn rusqlite::types::ToSql> = args.iter().map(Box::as_ref).collect();
                let mut rows = stmt.query(refs.as_slice())?;
                let mut ids = Vec::new();
                while let Some(row) = rows.next()? {
                    ids.push(row.get(0)?);
                }
                ids
            };
            for id in ids {
                out.push(row_to_issue(conn, &id)?.expect("just selected"));
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Swarm-eligible: open, unblocked, past its grace, and not held by an unexpired claim.
    pub fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>> {
        let mut ids = Vec::new();
        self.query(|conn| {
            let sql = format!(
                "SELECT i.id FROM issue i
                 WHERE i.status = 'open'
                   AND (i.available_at IS NULL OR i.available_at <= ?1){scope}
                   AND NOT EXISTS (
                       SELECT 1 FROM dep d JOIN issue p ON p.id = d.depends_on
                       WHERE d.issue_id = i.id AND p.status != 'closed')
                   AND (i.assignee IS NULL OR (i.expires_at IS NOT NULL AND i.expires_at <= ?1))
                 ORDER BY i.priority, i.created_at, i.id",
                scope = if project.is_some() {
                    " AND i.project = ?2"
                } else {
                    ""
                }
            );
            let mut stmt = conn.prepare(&sql)?;
            ids = match project {
                Some(p) => stmt
                    .query_map(params![at, p], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<_>>()?,
                None => stmt
                    .query_map(params![at], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<_>>()?,
            };
            Ok(())
        })?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get(&id)?);
        }
        Ok(out)
    }

    pub fn update(&self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue> {
        if let Some(status) = &patch.status
            && !STATUSES.contains(&status.as_str())
        {
            return Err(Error::BadStatus(status.clone()));
        }
        self.tx(|tx| {
            let mut current = row_to_issue(tx, id)?.ok_or_else(|| Error::NotFound(id.to_string()))?;
            if let Some(title) = &patch.title {
                current.title = title.clone();
            }
            if let Some(description) = &patch.description {
                current.description = description.clone();
            }
            if let Some(notes) = &patch.append_notes {
                if !current.description.is_empty() {
                    current.description.push('\n');
                }
                current.description.push_str(notes);
            }
            if let Some(priority) = patch.priority {
                current.priority = priority;
            }
            if let Some(evidence) = &patch.evidence {
                current.evidence = evidence.clone();
            }
            if let Some(merge) = &patch.metadata {
                let mut map = match std::mem::take(&mut current.metadata) {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                match merge {
                    serde_json::Value::Object(incoming) => {
                        for (key, value) in incoming {
                            if value.is_null() {
                                map.remove(key);
                            } else {
                                map.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    _ => return Err(Error::Bad("metadata patch must be a JSON object".into())),
                }
                current.metadata = serde_json::Value::Object(map);
                current.external_ref = current
                    .metadata
                    .get("external_ref")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            let status_changed = patch.status.as_deref().is_some_and(|s| s != current.status);
            if let Some(status) = &patch.status {
                current.status = status.clone();
                // Closing through `update` is reserved for the importer; the normal close path
                // is Db::close, which demands evidence.
                current.closed_at = (status == "closed").then_some(at);
            }
            // Any substantive edit defers swarm visibility: a bead under active speccing is not
            // up for grabs, but it must not stay hidden either.
            let available_at = patch
                .available_at
                .unwrap_or_else(|| (at + self.policy.edit_deferral_seconds).max(current.available_at.unwrap_or(0)));
            tx.execute(
                "UPDATE issue SET title=?2, description=?3, priority=?4, status=?5, closed_at=?6,
                        labels_json=?7, parent=?8, available_at=?9, updated_at=?10, evidence_json=?11,
                        metadata_json=?12, external_ref=?13
                 WHERE id=?1",
                params![
                    id, current.title, current.description, current.priority, current.status,
                    current.closed_at, serde_json::to_string(&current.labels)?, current.parent,
                    available_at, at, serde_json::to_string(&current.evidence)?,
                    serde_json::to_string(&current.metadata)?, current.external_ref
                ],
            )?;
            if status_changed {
                log_event(tx, id, at, actor, "status", &format!("→ {}", current.status))?;
            }
            log_event(tx, id, at, actor, "updated", "")?;
            Ok(())
        })?;
        self.get(id)
    }

    pub fn set_labels(&self, id: &str, labels: &[String], actor: &str, at: i64) -> Result<()> {
        self.tx(|tx| {
            let _ = row_to_issue(tx, id)?.ok_or_else(|| Error::NotFound(id.to_string()))?;
            tx.execute(
                "UPDATE issue SET labels_json=?2, updated_at=?3 WHERE id=?1",
                params![id, serde_json::to_string(labels)?, at],
            )?;
            log_event(tx, id, at, actor, "labels", &labels.join(","))
        })
    }

    pub fn add_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        let mut issue = self.get(id)?;
        if !issue.labels.iter().any(|l| l == label) {
            issue.labels.push(label.to_string());
            issue.labels.sort();
            self.set_labels(id, &issue.labels, actor, at)?;
        }
        Ok(())
    }

    pub fn remove_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        let mut issue = self.get(id)?;
        issue.labels.retain(|l| l != label);
        self.set_labels(id, &issue.labels, actor, at)
    }

    // ---- dependencies ----

    pub fn add_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        if child == parent {
            return Err(Error::Cycle {
                child: child.into(),
                parent: parent.into(),
            });
        }
        self.tx(|tx| {
            let _ = row_to_issue(tx, child)?.ok_or_else(|| Error::NotFound(child.to_string()))?;
            let _ = row_to_issue(tx, parent)?.ok_or_else(|| Error::NotFound(parent.to_string()))?;
            if reachable(tx, parent, child)? {
                return Err(Error::Cycle {
                    child: child.into(),
                    parent: parent.into(),
                });
            }
            tx.execute(
                "INSERT OR IGNORE INTO dep(issue_id, depends_on) VALUES (?1,?2)",
                params![child, parent],
            )?;
            tx.execute(
                "UPDATE issue SET updated_at=?2 WHERE id=?1",
                params![child, at],
            )?;
            log_event(tx, child, at, actor, "depends_on", parent)
        })
    }

    pub fn remove_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        self.tx(|tx| {
            tx.execute(
                "DELETE FROM dep WHERE issue_id=?1 AND depends_on=?2",
                params![child, parent],
            )?;
            log_event(tx, child, at, actor, "unblocked_from", parent)
        })
    }

    /// Store or refresh a remote dependency edge for `child`.
    ///
    /// The caller must have already verified `dependency` against the pinned key; this method
    /// only persists the verified assertion. It never inserts an `issue` row, never inserts a
    /// local `dep` row, and refuses a remote child (a cross-store edge must originate locally).
    pub fn add_remote_dep(
        &self,
        child: &str,
        dependency: &RemoteDependency,
        actor: &str,
        at: i64,
    ) -> Result<()> {
        let parsed = crate::refs::CanonicalRef::parse(&dependency.canonical_ref)
            .map_err(|error| Error::Bad(format!("invalid canonical ref: {error}")))?;
        if !parsed.names(&dependency.store_id, &dependency.remote_issue_id) {
            return Err(Error::Bad(format!(
                "canonical ref {} does not name store {} issue {}",
                dependency.canonical_ref, dependency.store_id, dependency.remote_issue_id
            )));
        }
        self.tx(|tx| {
            let _ = row_to_issue(tx, child)?.ok_or_else(|| Error::NotFound(child.to_string()))?;
            tx.execute(
                "INSERT INTO remote_dep(
                    issue_id, canonical_ref, store_id, remote_issue_id, state, asserted_at,
                    observed_at, signature, key_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(issue_id, canonical_ref) DO UPDATE SET
                    store_id=excluded.store_id,
                    remote_issue_id=excluded.remote_issue_id,
                    state=excluded.state,
                    asserted_at=excluded.asserted_at,
                    observed_at=excluded.observed_at,
                    signature=excluded.signature,
                    key_id=excluded.key_id",
                params![
                    child,
                    dependency.canonical_ref,
                    dependency.store_id,
                    dependency.remote_issue_id,
                    dependency.state,
                    dependency.asserted_at,
                    dependency.observed_at,
                    dependency.signature,
                    dependency.key_id,
                ],
            )?;
            tx.execute(
                "UPDATE issue SET updated_at=?2 WHERE id=?1",
                params![child, at],
            )?;
            log_event(
                tx,
                child,
                at,
                actor,
                "depends_on_remote",
                &dependency.canonical_ref,
            )
        })
    }

    /// Read the verified remote dependency edges attached to `child`, newest assertion per ref.
    ///
    /// The stored rows carry the last assertion. Freshness is deliberately *not* computed here:
    /// the caller re-verifies against the current pin (see `refs::remote_dependency_freshness`).
    pub fn remote_dependencies(&self, child: &str) -> Result<Vec<RemoteDependency>> {
        let mut out = Vec::new();
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT canonical_ref, store_id, remote_issue_id, state, asserted_at, observed_at,
                        signature, key_id
                 FROM remote_dep WHERE issue_id = ?1 ORDER BY canonical_ref",
            )?;
            let mut rows = stmt.query(params![child])?;
            while let Some(row) = rows.next()? {
                out.push(RemoteDependency {
                    canonical_ref: row.get(0)?,
                    store_id: row.get(1)?,
                    remote_issue_id: row.get(2)?,
                    state: row.get(3)?,
                    asserted_at: row.get(4)?,
                    observed_at: row.get(5)?,
                    signature: row.get(6)?,
                    key_id: row.get(7)?,
                    freshness: None,
                });
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Locally-ready work paired with every remote edge attached to it.
    ///
    /// This is the readiness *input*, not the verdict: the SQL rules in [`Self::ready`] decide
    /// local eligibility exactly as before (local deps closed, past grace, unclaimed), and remote
    /// edges are returned un-filtered so the caller can re-verify each one against the current
    /// pin. A remote edge never removes a row from this list, because a failed verification must
    /// become a *named reason*, not a silently vanished issue.
    pub fn ready_with_remote_edges(
        &self,
        project: Option<&str>,
        at: i64,
    ) -> Result<Vec<(Issue, Vec<RemoteDependency>)>> {
        let rows = self.ready(project, at)?;
        let mut out = Vec::with_capacity(rows.len());
        for issue in rows {
            let edges = self.remote_dependencies(&issue.id)?;
            out.push((issue, edges));
        }
        Ok(out)
    }

    /// Remove one remote dependency edge. This changes local state only; the remote is untouched.
    pub fn remove_remote_dep(
        &self,
        child: &str,
        canonical_ref: &str,
        actor: &str,
        at: i64,
    ) -> Result<()> {
        self.tx(|tx| {
            tx.execute(
                "DELETE FROM remote_dep WHERE issue_id=?1 AND canonical_ref=?2",
                params![child, canonical_ref],
            )?;
            log_event(tx, child, at, actor, "unblocked_from_remote", canonical_ref)
        })
    }

    // ---- claims ----

    /// Atomic claim. Succeeds when unclaimed or the holder's lease lapsed; a live foreign claim
    /// is an error naming the holder, never a silent steal.
    pub fn claim(&self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt> {
        let ttl = match req.actor_kind {
            ActorKind::Agent => req
                .ttl_seconds
                .unwrap_or(self.policy.agent_ttl_seconds)
                .min(self.policy.max_ttl_seconds),
            ActorKind::Human => {
                let business = req
                    .ttl_seconds
                    .map(|secs| {
                        let expired = self
                            .week
                            .deadline(&utc(at), secs.div_euclid(60).max(1))
                            .timestamp();
                        expired - at
                    })
                    .unwrap_or(self.week.human_expiry(&utc(at), &self.policy) - at);
                business.min(self.policy.max_ttl_seconds)
            }
        };
        let mut receipt = None;
        self.tx(|tx| {
            let project = self.project_for_issue(tx, &req.id)?;
            let holder: Option<(Option<String>, Option<i64>)> = tx
                .query_row(
                    "SELECT assignee, expires_at FROM issue WHERE id = ?1",
                    params![req.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (assignee, expires) = holder.ok_or_else(|| Error::NotFound(req.id.clone()))?;
            let held = assignee
                .as_ref()
                .map(|_| expires.is_some_and(|e| e > at))
                .unwrap_or(false);
            if held && assignee.as_deref() != Some(req.assignee.as_str()) {
                return Err(Error::AlreadyClaimed(req.id.clone(), assignee.unwrap()));
            }
            let changed = tx.execute(
                "UPDATE issue SET assignee=?2, actor_kind=?3, claimed_at=?4, expires_at=?5, updated_at=?4
                 WHERE id=?1",
                params![req.id, req.assignee, req.actor_kind.as_str(), at, at + ttl],
            )?;
            if changed != 1 {
                return Err(Error::NotFound(req.id.clone()));
            }
            log_event(tx, &req.id, at, &req.assignee, "claimed", req.actor_kind.as_str())?;
            let _ = project;
            receipt = Some(ClaimReceipt {
                id: req.id.clone(),
                assignee: req.assignee.clone(),
                actor_kind: req.actor_kind,
                expires_at: at + ttl,
            });
            Ok(())
        })?;
        Ok(receipt.expect("set on success"))
    }

    /// Renewal: only the holder may heartbeat.
    pub fn touch(&self, id: &str, assignee: &str, at: i64) -> Result<i64> {
        let mut expiry = None;
        self.tx(|tx| {
            let holder: Option<(Option<String>, Option<String>, Option<i64>)> = tx
                .query_row(
                    "SELECT assignee, actor_kind, expires_at FROM issue WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (current, kind, _) = holder.ok_or_else(|| Error::NotFound(id.to_string()))?;
            if current.as_deref() != Some(assignee) {
                return Err(Error::Bad(format!("{id} is not claimed by {assignee}")));
            }
            let ttl = match ActorKind::parse(&kind.unwrap_or_else(|| "agent".into())) {
                Some(ActorKind::Human) => self.week.human_expiry(&utc(at), &self.policy) - at,
                _ => self.policy.agent_ttl_seconds,
            };
            tx.execute(
                "UPDATE issue SET expires_at=?3, updated_at=?2 WHERE id=?1",
                params![id, at, at + ttl],
            )?;
            log_event(tx, id, at, assignee, "heartbeat", "")?;
            expiry = Some(at + ttl);
            Ok(())
        })?;
        Ok(expiry.expect("set on success"))
    }

    pub fn release(&self, id: &str, actor: &str, at: i64) -> Result<()> {
        self.tx(|tx| {
            tx.execute(
                "UPDATE issue SET assignee=NULL, actor_kind=NULL, claimed_at=NULL, expires_at=NULL, updated_at=?2 WHERE id=?1",
                params![id, at],
            )?;
            log_event(tx, id, at, actor, "released", "")
        })
    }

    /// Requeue claims whose lease lapsed. Agent claims are reclaimed silently (the work was
    /// mechanically interrupted); human holds are *escalated*, not taken, so this only ever
    /// clears expired agent claims and reports expired human holds.
    pub fn sweep(&self, at: i64) -> Result<SweepReport> {
        let mut report = SweepReport::default();
        self.tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT id, assignee, actor_kind FROM issue
                 WHERE assignee IS NOT NULL AND expires_at IS NOT NULL AND expires_at <= ?1",
            )?;
            let rows: Vec<(String, String, String)> = stmt
                .query_map(params![at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            for (id, assignee, kind) in rows {
                if kind == ActorKind::Human.as_str() {
                    report.escalated.push(format!("{id} ({assignee})"));
                    log_event(tx, &id, at, "sweeper", "escalated", &assignee)?;
                    continue;
                }
                tx.execute(
                    "UPDATE issue SET assignee=NULL, actor_kind=NULL, claimed_at=NULL, expires_at=NULL WHERE id=?1",
                    params![id],
                )?;
                log_event(tx, &id, at, "sweeper", "claim_expired", &assignee)?;
                report.requeued.push(id);
            }
            Ok(())
        })?;
        Ok(report)
    }

    // ---- terminal operations ----

    /// Close is the only terminal statement the tracker makes, so it is the only one that
    /// requires a receipt: evidence (PR, commit) or an explicit acknowledgement that this work
    /// had no deliverable. `review` exists precisely so agents never have to stretch `closed`
    /// to mean "there is a PR, go look at it".
    pub fn close(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue> {
        if evidence.is_empty() {
            return Err(Error::NoEvidence(id.to_string()));
        }
        self.check_blockers(id)?;
        self.close_unchecked(id, reason, evidence, actor, at)
    }

    /// Refuse to close past a blocker that has not reached a terminal state — the invariant
    /// that keeps the dependency graph honest on the normal path.
    fn check_blockers(&self, id: &str) -> Result<()> {
        let mut out = Ok(());
        self.query(|conn| {
            let mut stmt = conn.prepare("SELECT depends_on FROM dep WHERE issue_id = ?1")?;
            let blockers: Vec<String> = stmt
                .query_map(params![id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            for blocker in blockers {
                let status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM issue WHERE id = ?1",
                        params![blocker],
                        |r| r.get(0),
                    )
                    .optional()?;
                if status.as_deref().is_some_and(|s| !is_terminal(s)) {
                    out = Err(Error::Bad(format!(
                        "{id} still waits on {blocker}; close it first or remove the dependency"
                    )));
                    return Ok(());
                }
            }
            Ok(())
        })?;
        out
    }

    /// Close without the evidence or blocker gates. Only the importer (landing source state it
    /// does not own) and supersession (whose gate is the operator's decision, already made)
    /// may call this.
    pub(crate) fn close_unchecked(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue> {
        self.tx(|tx| {
            // Evidence accumulates: the PR recorded at `review` and the merge receipt added
            // now are both part of the delivery history of this issue.
            let existing: Vec<Evidence> = serde_json::from_str(
                &tx.query_row("SELECT evidence_json FROM issue WHERE id = ?1", params![id], |r| r.get::<_, String>(0))?,
            )?;
            let mut merged = existing;
            for item in evidence {
                if !merged.iter().any(|e| e.kind == item.kind && e.value == item.value) {
                    merged.push(item.clone());
                }
            }
            let changed = tx.execute(
                "UPDATE issue SET status='closed', closed_at=?2, close_reason=?3, evidence_json=?4,
                        assignee=NULL, actor_kind=NULL, claimed_at=NULL, expires_at=NULL, updated_at=?2
                 WHERE id=?1",
                params![id, at, reason, serde_json::to_string(&merged)?],
            )?;
            if changed != 1 {
                return Err(Error::NotFound(id.to_string()));
            }
            log_event(tx, id, at, actor, "closed", &format!("{} | {}", reason.unwrap_or(""), evidence.iter().map(|e| format!("{}:{}", e.kind, e.value)).collect::<Vec<_>>().join(" ")))
        })?;
        self.get(id)
    }

    pub fn supersede(&self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()> {
        let _ = row_to_issue(&self.conn.lock().expect("lock poisoned"), replacement)?
            .ok_or_else(|| Error::NotFound(replacement.to_string()))?;
        // Supersession is a delivery decision by an operator, not an agent claiming a finish:
        // the ack carries that, and the label carries the pointer.
        self.close_unchecked(
            id,
            Some(&format!("superseded by {replacement}")),
            &[Evidence::ack(format!("superseded by {replacement}"))],
            actor,
            at,
        )?;
        self.add_label(id, &format!("supersedes:{replacement}"), actor, at)
    }

    pub fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        let mut out = Vec::new();
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT issue_id, ts, actor, event, detail FROM history WHERE issue_id = ?1 ORDER BY seq",
            )?;
            let mut rows = stmt.query(params![id])?;
            while let Some(row) = rows.next()? {
                out.push(HistoryEvent {
                    issue_id: row.get(0)?,
                    ts: row.get(1)?,
                    actor: row.get(2)?,
                    event: row.get(3)?,
                    detail: row.get(4)?,
                });
            }
            Ok(())
        })?;
        Ok(out)
    }

    pub fn stats(&self) -> Result<serde_json::Value> {
        use std::collections::BTreeMap;
        let mut counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        self.query(|conn| {
            let mut stmt =
                conn.prepare("SELECT project, status, COUNT(*) FROM issue GROUP BY 1,2")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let project: String = row.get(0)?;
                let status: String = row.get(1)?;
                let count: i64 = row.get(2)?;
                *counts
                    .entry(project)
                    .or_default()
                    .entry(status)
                    .or_default() += count;
            }
            Ok(())
        })?;
        Ok(serde_json::json!(
            counts
                .into_iter()
                .map(|(project, statuses)| {
                    let total: i64 = statuses.values().sum();
                    (
                        project,
                        serde_json::json!({"by_status": statuses, "total": total}),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
        ))
    }

    /// Render tracker-owned pipeline measurements from durable issue/history timestamps.
    ///
    /// These are recomputed from the ledger on every scrape: a process restart cannot reset a
    /// throughput total or turn an unknown duration into zero. Issue ids and assignees are never
    /// labels; project/status/event are the bounded operational dimensions.
    pub fn prometheus_metrics(&self, at: i64) -> Result<String> {
        let mut out = String::from(
            "# HELP marbles_issues Current issues by project and status.\n\
             # TYPE marbles_issues gauge\n\
             # HELP marbles_queue_oldest_age_seconds Age of the oldest currently ready issue.\n\
             # TYPE marbles_queue_oldest_age_seconds gauge\n\
             # HELP marbles_claims_active Current unexpired claims by project and actor kind.\n\
             # TYPE marbles_claims_active gauge\n\
             # HELP marbles_events_total Durable tracker transitions by project and event.\n\
             # TYPE marbles_events_total counter\n\
             # HELP marbles_claim_latency_seconds Time from issue creation to first claim.\n\
             # TYPE marbles_claim_latency_seconds summary\n\
             # HELP marbles_cycle_time_seconds Time from issue creation to evidence-backed close.\n\
             # TYPE marbles_cycle_time_seconds summary\n\
             # HELP marbles_review_time_seconds Time from first review state to close.\n\
             # TYPE marbles_review_time_seconds summary\n",
        );
        self.query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT project, status, COUNT(*) FROM issue GROUP BY project, status ORDER BY project, status",
            )?;
            for row in stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })? {
                let (project, status, count) = row?;
                out.push_str(&format!(
                    "marbles_issues{{project=\"{}\",status=\"{}\"}} {}\n",
                    prometheus_label(&project), prometheus_label(&status), count
                ));
            }

            let ready = "i.status='open' AND COALESCE(i.available_at,0) <= ?1 AND NOT EXISTS (SELECT 1 FROM dep d JOIN issue p ON p.id=d.depends_on WHERE d.issue_id=i.id AND p.status != 'closed')";
            let mut stmt = conn.prepare(&format!(
                "SELECT i.project, ?1 - MIN(i.created_at) FROM issue i WHERE {ready} GROUP BY i.project"
            ))?;
            for row in stmt.query_map(params![at], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (project, age) = row?;
                out.push_str(&format!("marbles_queue_oldest_age_seconds{{project=\"{}\"}} {}\n", prometheus_label(&project), age.max(0)));
            }

            let mut stmt = conn.prepare(
                "SELECT project, actor_kind, COUNT(*) FROM issue WHERE assignee IS NOT NULL AND expires_at > ?1 GROUP BY project, actor_kind",
            )?;
            for row in stmt.query_map(params![at], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
                let (project, kind, count) = row?;
                out.push_str(&format!("marbles_claims_active{{project=\"{}\",actor_kind=\"{}\"}} {}\n", prometheus_label(&project), prometheus_label(&kind), count));
            }

            let mut stmt = conn.prepare(
                "SELECT i.project, h.event, COUNT(*) FROM history h JOIN issue i ON i.id=h.issue_id GROUP BY i.project, h.event",
            )?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
                let (project, event, count) = row?;
                out.push_str(&format!("marbles_events_total{{project=\"{}\",event=\"{}\"}} {}\n", prometheus_label(&project), prometheus_label(&event), count));
            }

            append_duration_summary(conn, &mut out, "marbles_claim_latency_seconds", "SELECT i.project, SUM(first_claim - i.created_at), COUNT(*) FROM issue i JOIN (SELECT issue_id, MIN(ts) AS first_claim FROM history WHERE event='claimed' GROUP BY issue_id) h ON h.issue_id=i.id WHERE first_claim >= i.created_at GROUP BY i.project")?;
            append_duration_summary(conn, &mut out, "marbles_cycle_time_seconds", "SELECT project, SUM(closed_at - created_at), COUNT(*) FROM issue WHERE closed_at IS NOT NULL AND closed_at >= created_at GROUP BY project")?;
            append_duration_summary(conn, &mut out, "marbles_review_time_seconds", "SELECT i.project, SUM(i.closed_at - h.first_review), COUNT(*) FROM issue i JOIN (SELECT issue_id, MIN(ts) AS first_review FROM history WHERE event='status' AND detail='→ review' GROUP BY issue_id) h ON h.issue_id=i.id WHERE i.closed_at IS NOT NULL AND i.closed_at >= h.first_review GROUP BY i.project")?;
            Ok(())
        })?;
        Ok(out)
    }
}

impl Store for Db {
    fn ensure_project(&self, slug: &str, root: &str, prefix: &str) -> Result<()> {
        Db::ensure_project(self, slug, root, prefix)
    }
    fn projects(&self) -> Result<Vec<(String, String, String)>> {
        Db::projects(self)
    }
    fn ensure_folder(&self, folder: &Folder, at: i64) -> Result<()> {
        Db::ensure_folder(self, folder, at)
    }
    fn set_project_folder(&self, project: &str, folder: Option<&str>, at: i64) -> Result<()> {
        Db::set_project_folder(self, project, folder, at)
    }
    fn folder_view(&self) -> Result<FolderView> {
        Db::folder_view(self)
    }
    fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<OutboundEvent>> {
        Db::pending_events(self, at, limit)
    }
    fn mark_event_delivered(&self, seq: i64, at: i64) -> Result<()> {
        Db::mark_event_delivered(self, seq, at)
    }
    fn mark_event_failed(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()> {
        Db::mark_event_failed(self, seq, next_attempt_at, error)
    }
    fn create(&self, spec: &NewIssue, at: i64) -> Result<String> {
        Db::create(self, spec, at)
    }
    fn get(&self, id: &str) -> Result<Issue> {
        Db::get(self, id)
    }
    fn list(&self, project: Option<&str>, all: bool, only_id: Option<&str>) -> Result<Vec<Issue>> {
        Db::list(self, project, all, only_id)
    }
    fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>> {
        Db::ready(self, project, at)
    }
    fn update(&self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue> {
        Db::update(self, id, patch, actor, at)
    }
    fn add_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        Db::add_label(self, id, label, actor, at)
    }
    fn remove_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        Db::remove_label(self, id, label, actor, at)
    }
    fn add_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        Db::add_dep(self, child, parent, actor, at)
    }
    fn remove_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        Db::remove_dep(self, child, parent, actor, at)
    }
    fn claim(&self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt> {
        Db::claim(self, req, at)
    }
    fn touch(&self, id: &str, assignee: &str, at: i64) -> Result<i64> {
        Db::touch(self, id, assignee, at)
    }
    fn release(&self, id: &str, actor: &str, at: i64) -> Result<()> {
        Db::release(self, id, actor, at)
    }
    fn sweep(&self, at: i64) -> Result<SweepReport> {
        Db::sweep(self, at)
    }
    fn close(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue> {
        Db::close(self, id, reason, evidence, actor, at)
    }
    fn supersede(&self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()> {
        Db::supersede(self, id, replacement, actor, at)
    }
    fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        Db::history(self, id)
    }
    fn stats(&self) -> Result<serde_json::Value> {
        Db::stats(self)
    }
    fn prometheus_metrics(&self, at: i64) -> Result<String> {
        Db::prometheus_metrics(self, at)
    }
}

fn append_duration_summary(
    conn: &Connection,
    out: &mut String,
    metric: &str,
    sql: &str,
) -> Result<()> {
    let mut stmt = conn.prepare(sql)?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (project, sum, count) = row?;
        let project = prometheus_label(&project);
        out.push_str(&format!(
            "{metric}_sum{{project=\"{project}\"}} {}\n",
            sum.max(0)
        ));
        out.push_str(&format!(
            "{metric}_count{{project=\"{project}\"}} {}\n",
            count
        ));
    }
    Ok(())
}

fn prometheus_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

#[derive(Debug, Default, serde::Serialize)]
pub struct ProjectRemoval {
    pub issues: i64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct SweepReport {
    pub requeued: Vec<String>,
    pub escalated: Vec<String>,
}

fn utc(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().expect("valid epoch")
}

fn log_event(
    tx: &rusqlite::Connection,
    id: &str,
    ts: i64,
    actor: &str,
    event: &str,
    detail: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO history(issue_id, ts, actor, event, detail) VALUES (?1,?2,?3,?4,?5)",
        params![id, ts, actor, event, detail],
    )?;
    let seq = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO outbound_event(seq, store_id, issue_id, project, occurred_at, event, next_attempt_at)
         SELECT ?1, (SELECT value FROM store_meta WHERE key='store_id'), i.id, i.project, ?2, ?3, ?2
         FROM issue i WHERE i.id=?4",
        params![seq, ts, event, id],
    )?;
    Ok(())
}

/// True when `target` is reachable from `from` over dependency edges (from → depends_on → …).
fn reachable(conn: &rusqlite::Connection, from: &str, target: &str) -> Result<bool> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE walk(id) AS (
        SELECT ?1
        UNION
        SELECT d.depends_on FROM dep d JOIN walk w ON d.issue_id = w.id
    ) SELECT 1 FROM walk WHERE id = ?2 LIMIT 1",
    )?;
    let found: Option<i64> = stmt
        .query_row(params![from, target], |r| r.get(0))
        .optional()?;
    Ok(found.is_some())
}

#[allow(clippy::type_complexity)]
fn row_to_issue(conn: &rusqlite::Connection, id: &str) -> Result<Option<Issue>> {
    let base: Option<(
        String,
        String,
        String,
        String,
        String,
        i64,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        String,
        Option<i64>,
        String,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT id, title, description, status, issue_type, priority, parent, labels_json,
                    assignee, actor_kind, expires_at, available_at, created_at, updated_at, project,
                    closed_at, metadata_json, external_ref
             FROM issue WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get(12)?,
                    r.get(13)?,
                    r.get(14)?,
                    r.get(15)?,
                    r.get(16)?,
                    r.get(17)?,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        title,
        description,
        status,
        issue_type,
        priority,
        parent,
        labels_json,
        assignee,
        actor_kind,
        expires_at,
        available_at,
        created_at,
        updated_at,
        project,
        closed_at,
        metadata_json,
        external_ref,
    )) = base
    else {
        return Ok(None);
    };
    let evidence: Vec<Evidence> = serde_json::from_str(&conn.query_row::<String, _, _>(
        "SELECT evidence_json FROM issue WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?)?;
    let mut dependencies = Vec::new();
    let mut stmt =
        conn.prepare("SELECT depends_on FROM dep WHERE issue_id = ?1 ORDER BY depends_on")?;
    let mut rows = stmt.query(params![id])?;
    while let Some(row) = rows.next()? {
        dependencies.push(Dependency {
            depends_on_id: Some(row.get(0)?),
        });
    }
    let dependency_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM dep d JOIN issue p ON p.id = d.depends_on
         WHERE d.issue_id = ?1 AND p.status != 'closed'",
        params![id],
        |r| r.get(0),
    )?;
    let dependent_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM dep d JOIN issue c ON c.id = d.issue_id
         WHERE d.depends_on = ?1 AND c.status != 'closed'",
        params![id],
        |r| r.get(0),
    )?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata_json)?;
    Ok(Some(Issue {
        id,
        metadata,
        external_ref,
        title,
        description,
        status,
        priority,
        issue_type,
        labels: serde_json::from_str(&labels_json)?,
        dependency_count,
        dependent_count,
        dependencies,
        evidence,
        parent,
        assignee,
        actor_kind,
        expires_at,
        available_at,
        created_at,
        updated_at,
        closed_at,
        project,
    }))
}

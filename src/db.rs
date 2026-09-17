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

use crate::time::WorkWeek;
use crate::types::*;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS project(
  slug        TEXT PRIMARY KEY,
  root        TEXT NOT NULL UNIQUE,
  prefix      TEXT NOT NULL,
  policy_json TEXT NOT NULL DEFAULT '{}',
  created_at  INTEGER NOT NULL
);
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
CREATE TABLE IF NOT EXISTS history(
  seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  issue_id TEXT NOT NULL,
  ts       INTEGER NOT NULL,
  actor    TEXT NOT NULL,
  event    TEXT NOT NULL,
  detail   TEXT NOT NULL DEFAULT ''
);
"#;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
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

pub struct Db {
    conn: Mutex<Connection>,
    policy: Policy,
    week: WorkWeek,
}

/// Lazily opened, physically separate company stores for hosted operation.
///
/// The company id comes exclusively from the verified principal. Keeping each
/// tenant in its own SQLite file makes the isolation boundary visible to
/// operators and backups instead of relying on every query to remember an RLS
/// predicate.
pub struct CompanyStores {
    root: PathBuf,
    open: Mutex<BTreeMap<String, Arc<Db>>>,
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

    pub fn for_company(&self, company_id: &str) -> Result<Arc<Db>> {
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
        let db = Arc::new(Db::open(company_root.join("marbles.db"))?);
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
}

fn validate_company_id(company_id: &str) -> Result<()> {
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

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Bad(format!("creating {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // WAL serializes writers correctly but returns SQLITE_BUSY immediately; give contended
        // writes (sweeper vs daemon tick vs CLI on one machine) a few seconds to queue.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
            policy: Policy::default(),
            week: WorkWeek::default(),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
            policy: Policy::default(),
            week: WorkWeek::default(),
        })
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
                &format!("DELETE FROM history WHERE issue_id IN {mine}"),
                params![slug],
            )?;
            tx.execute(
                &format!("DELETE FROM dep WHERE issue_id IN {mine} OR depends_on IN {mine}"),
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
            let metadata = match spec.metadata.clone().unwrap_or_else(|| serde_json::json!({})) {
                serde_json::Value::Object(map) => serde_json::Value::Object(map),
                other => other,
            };
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
        if let Some(status) = &patch.status {
            if !STATUSES.contains(&status.as_str()) {
                return Err(Error::BadStatus(status.clone()));
            }
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
        let _ = row_to_issue(&*self.conn.lock().expect("lock poisoned"), replacement)?
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

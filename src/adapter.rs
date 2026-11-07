//! A [`Store`] implemented on top of the [`crate::records`] append-only model.
//!
//! This is the spike's central claim: the Marbles `Store` contract — a single-writer,
//! append-only, tombstone-bearing ledger — is expressible *directly* in the `soil:records`
//! model without inventing anything the model does not already provide. Every `Db` method has a
//! faithful counterpart here; the two stores share the same invariants:
//!
//! * **One writer at a time.** `claim` is a compare-and-set on `(assignee, expires_at)`: a live
//!   foreign claim is `AlreadyClaimed`, never a silent steal — exactly as in [`crate::db`].
//! * **Append-only history.** Every mutation appends a revision; nothing is rewritten. The
//!   `marbles_event` audit trail is the same sequence that `Db::history` reads.
//! * **Tombstones, not deletes.** Removing a `dependency` or `project_folder` appends a deleted
//!   revision; the key leaves the live index but the trail is preserved.
//! * **Done means delivered.** `close` still requires evidence (or an explicit ack), and still
//!   refuses to close past an open dependency.
//!
//! The adapter is deliberately *not* wired into `Db` or the CLI: it is a parallel `Store`
//! implementation, exercised by [`crate::adapter`] contract tests, so production SQLite behavior
//! is untouched.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::records::{Engine, Key, RecordSchema, Version};
use crate::store::{Error, Result};
use crate::types::*;

const ISSUE: &str = "issue";
const PROJECT: &str = "project";
const PROJECT_INDEX: &str = "project_index";
const PROJECT_INDEX_KEY: &str = "projects";
const DEPENDENCY_INDEX: &str = "dependency_index";
const DEPENDENCY_INDEX_KEY: &str = "edges";
const FOLDER: &str = "folder";
const PROJECT_FOLDER: &str = "project_folder";
const DEPENDENCY: &str = "dependency";
const REMOTE_DEP: &str = "remote_dep";
const OUTBOUND_EVENT: &str = "outbound_event";
const MARBLES_EVENT: &str = "marbles_event";
const STORE_META: &str = "store_meta";

/// Build the schema declarations the engine needs. Idempotent: the same set is applied on every
/// `open`, mirroring `Db::open` running `CREATE TABLE IF NOT EXISTS`.
pub fn schemas() -> Vec<RecordSchema> {
    let cols = |data: &str, key: &[&str], extra: &[(&str, crate::records::ColumnType)]| {
        let mut cols = vec![
            crate::records::Column {
                name: "id".into(),
                ty: crate::records::ColumnType::Text,
            },
            crate::records::Column {
                name: "seq".into(),
                ty: crate::records::ColumnType::Int,
            },
            crate::records::Column {
                name: "deleted".into(),
                ty: crate::records::ColumnType::Bool,
            },
            crate::records::Column {
                name: "data".into(),
                ty: crate::records::ColumnType::Json,
            },
        ];
        cols.extend(extra.iter().map(|(n, t)| crate::records::Column {
            name: n.to_string(),
            ty: t.clone(),
        }));
        RecordSchema {
            name: data.to_string(),
            key: key.iter().map(|s| s.to_string()).collect(),
            columns: cols,
            data_column: "data".into(),
        }
    };
    vec![
        cols(ISSUE, &["id"], &[]),
        cols(PROJECT, &["slug"], &[]),
        cols(PROJECT_INDEX, &["name"], &[]),
        cols(DEPENDENCY_INDEX, &["name"], &[]),
        cols(FOLDER, &["slug"], &[]),
        cols(PROJECT_FOLDER, &["project"], &[]),
        cols(DEPENDENCY, &["child", "parent"], &[]),
        cols(REMOTE_DEP, &["child", "canonical_ref"], &[]),
        cols(OUTBOUND_EVENT, &["seq"], &[]),
        cols(MARBLES_EVENT, &["seq"], &[]),
        cols(STORE_META, &["key"], &[]),
    ]
}

/// The mutable state behind an [`Adapter`]: the append-only engine, the shared audit-sequence
/// counter, the claim policy, the store id, and the business-hours calendar. It lives behind a
/// `Mutex` so the `&self` methods of the [`Store`] trait can mutate it, exactly as `Db` guards
/// its single `Connection` — the store is single-writer, and the lock is what enforces that.
struct Inner {
    engine: Engine,
    /// One counter feeds both `marbles_event` and `outbound_event`, mirroring SQLite's
    /// `last_insert_rowid()` feeding both inserts in `log_event`.
    seq: i64,
    policy: Policy,
    store_id: String,
    week: crate::time::WorkWeek,
}

/// A `Store` backed by the in-memory [`Engine`].
pub struct Adapter {
    inner: Mutex<Inner>,
}

impl Adapter {
    /// Open a fresh, in-memory adapter with a generated store id.
    pub fn in_memory() -> Result<Self> {
        Self::open("store".to_string())
    }

    /// Open a fresh adapter with a fixed store id (in-memory storage).
    pub fn open(store_id: String) -> Result<Self> {
        Self::with_engine(Inner {
            engine: Engine::new(),
            seq: 0,
            policy: Policy::default(),
            store_id,
            week: crate::time::WorkWeek::default(),
        })
    }

    /// Open a durable adapter rooted at `path`. The append-only trail is replayed on open, so a
    /// store survives a drop/reopen exactly as a records host would persist it. This is the
    /// records-model analogue of `CompanyStores` reopening a company's DB file. The parent
    /// directory is created if needed; a missing trail means a fresh store.
    pub fn open_file(path: std::path::PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Bad(e.to_string()))?;
        }
        let store_id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("records")
            .to_string();
        let engine = Engine::load(&path).map_err(Error::Bad)?;
        Self::with_engine(Inner {
            engine,
            seq: 0,
            policy: Policy::default(),
            store_id,
            week: crate::time::WorkWeek::default(),
        })
    }

    /// Define the record types over an engine, seed the store id, and wrap it. Shared by the
    /// in-memory and durable constructors so both expose the identical schema.
    fn with_engine(mut inner: Inner) -> Result<Self> {
        for schema in schemas() {
            inner.engine.define(schema).map_err(Error::Bad)?;
        }
        // The durable engine's `seq` was restored from the trail; keep the audit counter aligned
        // with it so a reopened store does not reuse sequence numbers.
        inner.seq = inner.engine.seq();
        inner.seed_meta();
        Ok(Adapter {
            inner: Mutex::new(inner),
        })
    }

    /// Replace the policy (used by tests that need non-default TTLs, mirroring `Db`'s policy).
    pub fn with_policy(self, policy: Policy) -> Self {
        self.inner.lock().expect("adapter lock poisoned").policy = policy;
        self
    }

    /// Replace the business-hours calendar (used by human-lease tests).
    pub fn with_week(self, week: crate::time::WorkWeek) -> Self {
        self.inner.lock().unwrap().week = week;
        self
    }

    pub fn store_id(&self) -> String {
        let g = self.inner.lock().expect("adapter lock poisoned");
        g.store_id.clone()
    }

    /// Rebuild the single records-backed project index from the project records.
    pub fn reindex_projects(&self) -> Result<usize> {
        let mut g = self.lock();
        g.rebuild_project_index()
    }

    /// Rebuild all derived indexes in one pass over the records book.
    pub fn reindex(&self) -> Result<(usize, usize)> {
        let mut g = self.lock();
        let projects = g.rebuild_project_index()?;
        let dependencies = g.rebuild_dependency_index()?;
        Ok((projects, dependencies))
    }

    /// Lock the store for the duration of one operation. Every mutation and read goes through
    /// here, so calls are serialized the way `Db.tx` / `Db.query` serialize against the one
    /// SQLite connection.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("adapter lock poisoned")
    }

    /// Remove a project and cascade-tombstone every row that touches it. Mirrors `Db::drop_project`;
    /// not part of the `Store` trait, so it is exposed as an inherent method.
    pub fn drop_project(&self, slug: &str) -> Result<crate::store::ProjectRemoval> {
        let mut g = self.lock();
        Inner::drop_project(&mut g, slug)
    }
}

impl Inner {
    fn seed_meta(&mut self) {
        self.append(
            STORE_META,
            &Key::new([self.store_id.clone()]),
            json!({"value": self.store_id}),
        );
    }

    // ---- append helpers ----

    fn append(&mut self, name: &str, key: &Key, data: Value) -> i64 {
        self.engine.append(name, key, data, false, 0)
    }

    fn tombstone(&mut self, name: &str, key: &Key) -> i64 {
        self.engine.append(name, key, Value::Null, true, 0)
    }

    fn current(&self, name: &str, key: &Key) -> Option<Value> {
        self.engine.current(name, key)
    }

    fn keys(&self, name: &str) -> Vec<Key> {
        self.engine.keys(name)
    }

    /// The next audit sequence number, shared by the history event and its outbound event.
    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    /// Business-hours TTL for a human claim, in seconds — the deadline minus `at`. Mirrors the
    /// `WorkWeek::human_expiry` default used by the SQLite store.
    fn human_ttl(&self, at: i64) -> i64 {
        use chrono::DateTime;
        let at_dt = DateTime::from_timestamp(at, 0).expect("valid epoch");
        self.week.human_expiry(&at_dt, &self.policy) - at
    }

    // ---- issue read/write ----

    fn issue_key(id: &str) -> Key {
        Key::new([id.to_string()])
    }

    fn load_issue(&self, id: &str) -> Option<Value> {
        self.current(ISSUE, &Self::issue_key(id))
    }

    /// Read the `Issue` view of a stored record, or `None` if absent.
    fn row_to_issue(&self, id: &str) -> Option<Issue> {
        let Some(data) = self.load_issue(id) else {
            return None;
        };
        let project = data["project"].as_str().unwrap_or_default().to_string();
        let labels: Vec<String> =
            serde_json::from_value(data["labels"].clone()).unwrap_or_default();
        let metadata = data["metadata"].clone();
        let evidence: Vec<Evidence> =
            serde_json::from_value(data["evidence"].clone()).unwrap_or_default();

        let edges = self.dependency_edges();

        // Dependencies: edges whose child == id, ordered by parent.
        let mut dependencies = Vec::new();
        let mut open_parents = 0i64;
        for (child, parent) in &edges {
            if child == id {
                dependencies.push(Dependency {
                    depends_on_id: Some(parent.clone()),
                });
                let status = self
                    .load_issue(parent)
                    .and_then(|d| d["status"].as_str().map(str::to_string));
                if status.as_deref().is_some_and(|s| !is_terminal(s)) {
                    open_parents += 1;
                }
            }
        }
        dependencies.sort_by(|a, b| a.depends_on_id.cmp(&b.depends_on_id));
        let dependency_count = open_parents;

        // Dependents: edges whose parent == id.
        let mut dependent_count = 0i64;
        for (child, parent) in &edges {
            if parent == id {
                let status = self
                    .load_issue(child)
                    .and_then(|d| d["status"].as_str().map(str::to_string));
                if status.as_deref().is_some_and(|s| !is_terminal(s)) {
                    dependent_count += 1;
                }
            }
        }

        let get_str = |k: &str| data[k].as_str().map(str::to_string);
        let get_opt_i64 = |k: &str| match &data[k] {
            Value::Null => None,
            Value::Number(n) => n.as_i64(),
            _ => None,
        };
        let get_i64 = |k: &str| get_opt_i64(k).unwrap_or(0);

        Some(Issue {
            id: id.to_string(),
            metadata,
            external_ref: get_str("external_ref"),
            title: get_str("title").unwrap_or_default(),
            description: get_str("description").unwrap_or_default(),
            status: get_str("status").unwrap_or_default(),
            priority: get_i64("priority"),
            issue_type: get_str("issue_type").unwrap_or_default(),
            labels,
            dependency_count,
            dependent_count,
            dependencies,
            evidence,
            parent: get_str("parent"),
            assignee: get_str("assignee"),
            actor_kind: get_str("actor_kind"),
            expires_at: get_opt_i64("expires_at"),
            available_at: get_opt_i64("available_at"),
            created_at: get_i64("created_at"),
            updated_at: get_i64("updated_at"),
            closed_at: get_opt_i64("closed_at"),
            project,
        })
    }

    /// Append a new revision of an existing issue from a mutated `Issue` view.
    fn save_issue(&mut self, issue: &Issue) -> Result<()> {
        let data = issue_to_json(issue);
        self.append(ISSUE, &Self::issue_key(&issue.id), data);
        Ok(())
    }

    /// Current issue revision for a conditional write.
    fn issue_version(&self, id: &str) -> Result<Version> {
        self.engine
            .version(ISSUE, &Self::issue_key(id))
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    /// Save an issue only if the caller still holds the revision it read.
    fn save_issue_cas(&mut self, issue: &Issue, expected: Version) -> Result<bool> {
        let result = self
            .engine
            .compare_and_swap(
                ISSUE,
                &Self::issue_key(&issue.id),
                Some(expected),
                issue_to_json(issue),
                false,
            )
            .map_err(Error::Bad)?;
        Ok(result.applied)
    }

    /// Append one audit event and its outbound event, mirroring `Db::log_event`.
    fn log_event(
        &mut self,
        id: &str,
        ts: i64,
        actor: &str,
        event: &str,
        detail: &str,
    ) -> Result<()> {
        let seq = self.next_seq();
        let event_key = Key::new([seq.to_string()]);
        self.append(
            MARBLES_EVENT,
            &event_key,
            json!({
                "issue_id": id,
                "ts": ts,
                "actor": actor,
                "event": event,
                "detail": detail,
            }),
        );
        // The outbound event only fires when the issue exists; the SQLite version selects on it.
        let project = self
            .load_issue(id)
            .map(|d| d["project"].as_str().unwrap_or_default().to_string());
        if let Some(project) = project {
            let outbound_key = Key::new([seq.to_string()]);
            self.append(
                OUTBOUND_EVENT,
                &outbound_key,
                json!({
                    "seq": seq,
                    "store_id": self.store_id,
                    "issue_id": id,
                    "project": project,
                    "occurred_at": ts,
                    "event": event,
                    "next_attempt_at": ts,
                    "delivered_at": null,
                    "attempts": 0,
                    "last_error": null,
                }),
            );
        }
        Ok(())
    }

    /// Generate a short, collision-checked suffix, mirroring `Db::short_suffix`.
    fn short_suffix() -> String {
        const ALPHA: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).expect("rng");
        bytes
            .iter()
            .map(|b| ALPHA[(*b as usize) % ALPHA.len()] as char)
            .collect()
    }

    /// Reachability over dependency edges (from → depends_on → …), mirroring `Db::reachable`.
    fn reachable(&self, from: &str, target: &str) -> Result<bool> {
        if from == target {
            return Ok(true);
        }
        let mut frontier = vec![from.to_string()];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(node) = frontier.pop() {
            if seen.insert(node.clone()) {
                for key in self.keys(DEPENDENCY) {
                    let comps = key.as_vec();
                    if comps.get(0).map(String::as_str) == Some(node.as_str()) {
                        if let Some(next) = comps.get(1) {
                            if next == target {
                                return Ok(true);
                            }
                            frontier.push(next.clone());
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    fn prefix_for(&self, project: &str) -> Result<String> {
        self.load_project(project)
            .map(|d| d["prefix"].as_str().unwrap_or("").to_string())
            .ok_or_else(|| Error::NoProject(project.to_string()))
    }

    fn project_of(&self, id: &str) -> Result<String> {
        self.load_issue(id)
            .map(|d| d["project"].as_str().unwrap_or_default().to_string())
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    fn load_project(&self, slug: &str) -> Option<Value> {
        self.current(PROJECT, &Key::new([slug.to_string()]))
    }

    fn load_folder(&self, slug: &str) -> Option<Value> {
        self.current(FOLDER, &Key::new([slug.to_string()]))
    }
}

fn issue_to_json(issue: &Issue) -> Value {
    json!({
        "id": issue.id,
        "project": issue.project,
        "title": issue.title,
        "description": issue.description,
        "status": issue.status,
        "priority": issue.priority,
        "issue_type": issue.issue_type,
        "labels": issue.labels,
        "parent": issue.parent,
        "assignee": issue.assignee,
        "actor_kind": issue.actor_kind,
        "expires_at": issue.expires_at,
        "available_at": issue.available_at,
        "created_at": issue.created_at,
        "updated_at": issue.updated_at,
        "closed_at": issue.closed_at,
        "metadata": issue.metadata,
        "external_ref": issue.external_ref,
        "evidence": issue.evidence,
    })
}

// ---------------------------------------------------------------------------
// Project & folder management
// ---------------------------------------------------------------------------

impl Inner {
    fn ensure_project(&mut self, slug: &str, root: &str, prefix: &str) -> Result<()> {
        let ts = crate::store::now();
        let mut data = self
            .load_project(slug)
            .unwrap_or_else(|| json!({"slug": slug, "prefix": prefix, "created_at": ts}));
        data["slug"] = json!(slug);
        data["root"] = json!(root);
        // created_at and prefix are set once, then preserved (matching ON CONFLICT DO UPDATE SET root only).
        if data.get("prefix").is_none() {
            data["prefix"] = json!(prefix);
        }
        if data.get("created_at").is_none() {
            data["created_at"] = json!(ts);
        }
        self.append(PROJECT, &Key::new([slug.to_string()]), data);
        self.refresh_project_index(slug)?;
        Ok(())
    }

    fn project_index_key() -> Key {
        Key::new([PROJECT_INDEX_KEY.to_string()])
    }

    fn project_row(&self, slug: &str) -> Option<(String, String, String)> {
        self.load_project(slug).map(|data| {
            (
                slug.to_string(),
                data["root"].as_str().unwrap_or_default().to_string(),
                data["prefix"].as_str().unwrap_or_default().to_string(),
            )
        })
    }

    fn project_rows_from_scan(&self) -> Vec<(String, String, String)> {
        let mut rows = self
            .engine
            .keys_all(PROJECT)
            .iter()
            .filter_map(|key| {
                let slug = key.as_vec().first()?;
                self.project_row(slug)
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    }

    fn load_project_index(&self) -> Option<Vec<(String, String, String)>> {
        let data = self.current(PROJECT_INDEX, &Self::project_index_key())?;
        let projects = data.get("projects")?.as_array()?;
        let mut rows = projects
            .iter()
            .filter_map(|entry| {
                let parts = entry.as_array()?;
                Some((
                    parts.first()?.as_str()?.to_string(),
                    parts.get(1)?.as_str()?.to_string(),
                    parts.get(2)?.as_str()?.to_string(),
                ))
            })
            .collect::<Vec<_>>();
        rows.sort();
        Some(rows)
    }

    fn store_project_index(&mut self, rows: &[(String, String, String)]) {
        let projects = rows
            .iter()
            .map(|(slug, root, prefix)| json!([slug, root, prefix]))
            .collect::<Vec<_>>();
        self.append(
            PROJECT_INDEX,
            &Self::project_index_key(),
            json!({"projects": projects}),
        );
    }

    fn rebuild_project_index(&mut self) -> Result<usize> {
        let rows = self.project_rows_from_scan();
        let count = rows.len();
        self.store_project_index(&rows);
        Ok(count)
    }

    fn refresh_project_index(&mut self, slug: &str) -> Result<()> {
        let mut rows = match self.load_project_index() {
            Some(rows) => rows,
            None => {
                self.rebuild_project_index()?;
                return Ok(());
            }
        };
        rows.retain(|(existing, _, _)| existing != slug);
        if let Some(row) = self.project_row(slug) {
            rows.push(row);
        }
        rows.sort();
        self.store_project_index(&rows);
        Ok(())
    }

    fn projects(&mut self) -> Result<Vec<(String, String, String)>> {
        if let Some(rows) = self.load_project_index() {
            return Ok(rows);
        }
        self.rebuild_project_index()?;
        Ok(self.load_project_index().unwrap_or_default())
    }

    fn dependency_index_key() -> Key {
        Key::new([DEPENDENCY_INDEX_KEY.to_string()])
    }

    fn dependency_rows_from_scan(&self) -> Vec<(String, String)> {
        let mut edges = self
            .engine
            .keys_all(DEPENDENCY)
            .iter()
            .filter_map(|key| {
                let parts = key.as_vec();
                if let (Some(child), Some(parent)) = (parts.first(), parts.get(1)) {
                    return Some((child.clone(), parent.clone()));
                }
                // Older import wrote the pair as one unit-separated component.
                let (child, parent) = parts.first()?.split_once('\u{1f}')?;
                Some((child.to_string(), parent.to_string()))
            })
            .collect::<Vec<_>>();
        edges.sort();
        edges
    }

    fn load_dependency_index(&self) -> Option<Vec<(String, String)>> {
        let data = self.current(DEPENDENCY_INDEX, &Self::dependency_index_key())?;
        let edges = data.get("edges")?.as_array()?;
        let mut rows = edges
            .iter()
            .filter_map(|entry| {
                let parts = entry.as_array()?;
                Some((
                    parts.first()?.as_str()?.to_string(),
                    parts.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect::<Vec<_>>();
        rows.sort();
        Some(rows)
    }

    fn store_dependency_index(&mut self, edges: &[(String, String)]) {
        let edges = edges
            .iter()
            .map(|(child, parent)| json!([child, parent]))
            .collect::<Vec<_>>();
        self.append(
            DEPENDENCY_INDEX,
            &Self::dependency_index_key(),
            json!({"edges": edges}),
        );
    }

    fn rebuild_dependency_index(&mut self) -> Result<usize> {
        let edges = self.dependency_rows_from_scan();
        let count = edges.len();
        self.store_dependency_index(&edges);
        Ok(count)
    }

    fn update_dependency_index(&mut self, child: &str, parent: &str, add: bool) -> Result<()> {
        let mut edges = match self.load_dependency_index() {
            Some(edges) => edges,
            None => {
                self.rebuild_dependency_index()?;
                return Ok(());
            }
        };
        if add {
            if !edges.iter().any(|(c, p)| c == child && p == parent) {
                edges.push((child.to_string(), parent.to_string()));
            }
        } else {
            edges.retain(|(c, p)| !(c == child && p == parent));
        }
        edges.sort();
        self.store_dependency_index(&edges);
        Ok(())
    }

    fn dependency_edges(&self) -> Vec<(String, String)> {
        self.load_dependency_index()
            .unwrap_or_else(|| self.dependency_rows_from_scan())
    }

    fn ensure_folder(&mut self, folder: &Folder, at: i64) -> Result<()> {
        let slug = folder.slug.trim().to_string();
        if slug.is_empty() {
            return Err(Error::Bad("folder slug is empty".into()));
        }
        let name = folder.name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Bad("folder name is empty".into()));
        }
        let parent = folder
            .parent
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        if let Some(parent) = &parent {
            if parent == &slug {
                return Err(Error::Bad("a folder cannot be its own parent".into()));
            }
            if self.load_folder(parent).is_none() {
                return Err(Error::Bad(format!("no folder {parent}")));
            }
            // Walk the parent chain: attaching parent under slug would make slug an ancestor of
            // its own child, a cycle.
            let mut node = self.parent_of(parent);
            while let Some(n) = node {
                if n == slug {
                    return Err(Error::Bad(format!(
                        "folder {slug} would become a descendant of itself"
                    )));
                }
                node = self.parent_of(&n);
            }
        }
        let mut data = self.load_folder(&slug).unwrap_or_else(
            || json!({"slug": &slug, "name": &name, "parent": null, "created_at": at}),
        );
        data["slug"] = json!(slug);
        data["name"] = json!(name);
        data["parent"] = match parent {
            Some(p) => json!(p),
            None => Value::Null,
        };
        if data.get("created_at").is_none() {
            data["created_at"] = json!(at);
        }
        self.append(FOLDER, &Key::new([slug]), data);
        Ok(())
    }

    fn parent_of(&self, slug: &str) -> Option<String> {
        self.load_folder(slug)?["parent"]
            .as_str()
            .map(str::to_string)
    }

    fn set_project_folder(&mut self, project: &str, folder: Option<&str>, at: i64) -> Result<()> {
        if self.load_project(project).is_none() {
            return Err(Error::NoProject(project.to_string()));
        }
        let key = Key::new([project.to_string()]);
        match folder.map(str::trim).filter(|f| !f.is_empty()) {
            Some(folder) => {
                if self.load_folder(folder).is_none() {
                    return Err(Error::Bad(format!("no folder {folder}")));
                }
                let mut data = self
                    .current(PROJECT_FOLDER, &key)
                    .unwrap_or_else(|| json!({"project": project, "assigned_at": at}));
                data["project"] = json!(project);
                data["folder"] = json!(folder);
                data["assigned_at"] = json!(at);
                self.append(PROJECT_FOLDER, &key, data);
            }
            None => {
                // Unfile: tombstone the assignment so the key leaves the live index.
                self.tombstone(PROJECT_FOLDER, &key);
            }
        }
        Ok(())
    }

    fn folder_view(&self) -> Result<FolderView> {
        let mut folders: Vec<Folder> = self
            .keys(FOLDER)
            .iter()
            .filter_map(|k| {
                let slug = k.as_vec().first()?.clone();
                self.load_folder(&slug).map(|d| Folder {
                    slug: d["slug"].as_str().unwrap_or_default().to_string(),
                    name: d["name"].as_str().unwrap_or_default().to_string(),
                    parent: d["parent"].as_str().map(str::to_string),
                    created_at: d["created_at"].as_i64().unwrap_or(0),
                })
            })
            .collect();
        folders.sort_by(|a, b| {
            (a.name.clone(), a.slug.clone()).cmp(&(b.name.clone(), b.slug.clone()))
        });
        let projects: Vec<ProjectFolder> = self
            .keys(PROJECT_FOLDER)
            .iter()
            .filter_map(|k| {
                let _project = k.as_vec().first()?.clone();
                self.current(PROJECT_FOLDER, k).map(|d| ProjectFolder {
                    project: d["project"].as_str().unwrap_or_default().to_string(),
                    folder: d["folder"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect();
        Ok(FolderView { folders, projects })
    }

    fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<crate::store::OutboundEvent>> {
        let mut events: Vec<(i64, crate::store::OutboundEvent)> = self
            .keys(OUTBOUND_EVENT)
            .iter()
            .filter_map(|k| {
                let seq = k.as_vec().first()?.parse::<i64>().ok()?;
                let data = self.current(OUTBOUND_EVENT, k)?;
                if data["delivered_at"].is_number() {
                    return None; // delivered
                }
                let next = data["next_attempt_at"].as_i64().unwrap_or(0);
                if next > at {
                    return None;
                }
                Some((
                    seq,
                    crate::store::OutboundEvent {
                        seq,
                        store_id: data["store_id"].as_str().unwrap_or_default().to_string(),
                        issue_id: data["issue_id"].as_str().unwrap_or_default().to_string(),
                        project: data["project"].as_str().unwrap_or_default().to_string(),
                        occurred_at: data["occurred_at"].as_i64().unwrap_or(0),
                        event: data["event"].as_str().unwrap_or_default().to_string(),
                        attempts: data["attempts"].as_i64().unwrap_or(0),
                    },
                ))
            })
            .collect();
        events.sort_by_key(|(seq, _)| *seq);
        events.truncate(limit);
        Ok(events.into_iter().map(|(_, e)| e).collect())
    }

    fn mark_event_delivered(&mut self, seq: i64, at: i64) -> Result<()> {
        let key = Key::new([seq.to_string()]);
        let Some(mut data) = self.current(OUTBOUND_EVENT, &key) else {
            return Ok(());
        };
        if data["delivered_at"].is_number() {
            return Ok(());
        }
        data["delivered_at"] = json!(at);
        data["attempts"] = json!(data["attempts"].as_i64().unwrap_or(0) + 1);
        data["last_error"] = Value::Null;
        self.append(OUTBOUND_EVENT, &key, data);
        Ok(())
    }

    fn mark_event_failed(&mut self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()> {
        let key = Key::new([seq.to_string()]);
        let Some(mut data) = self.current(OUTBOUND_EVENT, &key) else {
            return Ok(());
        };
        if data["delivered_at"].is_number() {
            return Ok(());
        }
        data["attempts"] = json!(data["attempts"].as_i64().unwrap_or(0) + 1);
        data["next_attempt_at"] = json!(next_attempt_at);
        data["last_error"] = json!(error);
        self.append(OUTBOUND_EVENT, &key, data);
        Ok(())
    }

    fn drop_project(&mut self, slug: &str) -> Result<crate::store::ProjectRemoval> {
        let mut out = crate::store::ProjectRemoval::default();
        if self.load_project(slug).is_none() {
            return Err(Error::NoProject(slug.to_string()));
        }
        let mine: Vec<String> = self
            .keys(ISSUE)
            .iter()
            .filter_map(|k| {
                let id = k.as_vec().first()?.clone();
                self.load_issue(&id)
                    .map(|d| (id, d["project"].as_str().unwrap_or_default().to_string()))
            })
            .filter(|(_, p)| p == slug)
            .map(|(id, _)| id)
            .collect();
        out.issues = mine.len() as i64;
        // Tombstone every outbound event and history row for the removed issues.
        for seq_key in self.keys(OUTBOUND_EVENT) {
            let data = self.current(OUTBOUND_EVENT, &seq_key);
            if let Some(d) = data {
                if d["issue_id"]
                    .as_str()
                    .map(|s| mine.iter().any(|x| x.as_str() == s))
                    .unwrap_or(false)
                {
                    self.tombstone(OUTBOUND_EVENT, &seq_key);
                }
            }
        }
        for seq_key in self.keys(MARBLES_EVENT) {
            let data = self.current(MARBLES_EVENT, &seq_key);
            if let Some(d) = data {
                if d["issue_id"]
                    .as_str()
                    .map(|s| mine.iter().any(|x| x.as_str() == s))
                    .unwrap_or(false)
                {
                    self.tombstone(MARBLES_EVENT, &seq_key);
                }
            }
        }
        // Tombstone dependency edges touching the removed issues.
        for key in self.keys(DEPENDENCY) {
            let comps = key.as_vec();
            let child = comps.get(0).map(String::as_str);
            let parent = comps.get(1).map(String::as_str);
            if child
                .map(|s| mine.iter().any(|x| x.as_str() == s))
                .unwrap_or(false)
                || parent
                    .map(|s| mine.iter().any(|x| x.as_str() == s))
                    .unwrap_or(false)
            {
                self.tombstone(DEPENDENCY, &key);
            }
        }
        for key in self.keys(REMOTE_DEP) {
            let comps = key.as_vec();
            if comps
                .get(0)
                .map(String::as_str)
                .map(|s| mine.iter().any(|x| x.as_str() == s))
                .unwrap_or(false)
            {
                self.tombstone(REMOTE_DEP, &key);
            }
        }
        self.rebuild_dependency_index()?;
        // Tombstone the issues and the project itself.
        for id in &mine {
            self.tombstone(ISSUE, &Self::issue_key(id));
        }
        self.tombstone(PROJECT, &Key::new([slug.to_string()]));
        self.refresh_project_index(slug)?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Issue lifecycle
// ---------------------------------------------------------------------------

impl Inner {
    fn create(&mut self, spec: &NewIssue, at: i64) -> Result<String> {
        let project = spec
            .project
            .clone()
            .ok_or_else(|| Error::Bad("create requires a project".into()))?;
        let grace_available = spec
            .available_at
            .unwrap_or(at + self.policy.creation_grace_seconds);
        let prefix = self.prefix_for(&project)?;
        if let Some(parent) = spec.parent.as_deref() {
            match self.load_issue(parent) {
                None => return Err(Error::NotFound(parent.to_string())),
                Some(d) => {
                    let p = d["project"].as_str().unwrap_or_default();
                    if p != project {
                        return Err(Error::Bad(format!(
                            "parent {parent} belongs to project {p}, not {project}; hierarchy is project-local (use a dependency edge for cross-project blockers)"
                        )));
                    }
                }
            }
        }
        let id = match &spec.id {
            Some(id) => {
                if self.load_issue(id).is_some() {
                    return Err(Error::Bad(format!("issue id {id} already exists")));
                }
                id.clone()
            }
            None => loop {
                let candidate = format!("{prefix}-{}", Self::short_suffix());
                if self.load_issue(&candidate).is_none() {
                    break candidate;
                }
            },
        };
        let mut metadata = spec.metadata.clone().unwrap_or_else(|| json!({}));
        if !metadata.is_object() {
            return Err(Error::Bad("create metadata must be a JSON object".into()));
        }
        if let Some(reference) = &spec.external_ref {
            metadata["external_ref"] = json!(reference);
        }
        let priority = if spec.priority == 0 { 2 } else { spec.priority };
        let issue = Issue {
            id: id.clone(),
            project: project.clone(),
            title: spec.title.clone(),
            description: spec.description.clone(),
            status: "open".into(),
            priority,
            issue_type: spec.issue_type.clone(),
            labels: spec.labels.clone(),
            parent: spec.parent.clone(),
            assignee: None,
            actor_kind: None,
            expires_at: None,
            available_at: Some(grace_available),
            created_at: at,
            updated_at: at,
            closed_at: None,
            metadata,
            external_ref: spec.external_ref.clone(),
            evidence: vec![],
            dependency_count: 0,
            dependent_count: 0,
            dependencies: vec![],
        };
        self.append(ISSUE, &Self::issue_key(&id), issue_to_json(&issue));
        self.log_event(
            &id,
            at,
            spec.created_by.as_deref().unwrap_or("system"),
            "created",
            &spec.title,
        )?;
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Issue> {
        self.row_to_issue(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    fn list(&self, project: Option<&str>, all: bool, only_id: Option<&str>) -> Result<Vec<Issue>> {
        let mut issues: Vec<Issue> = self
            .keys(ISSUE)
            .iter()
            .filter_map(|k| {
                let id = k.as_vec().first()?.clone();
                self.row_to_issue(&id)
            })
            .filter(|issue| {
                if !all && issue.status == "closed" {
                    return false;
                }
                if let Some(p) = project {
                    if issue.project != p {
                        return false;
                    }
                }
                if let Some(id) = only_id {
                    if issue.id != id {
                        return false;
                    }
                }
                true
            })
            .collect();
        issues.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.id.cmp(&b.id))
        });
        Ok(issues)
    }

    fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>> {
        let mut issues: Vec<Issue> = self
            .keys(ISSUE)
            .iter()
            .filter_map(|k| {
                let id = k.as_vec().first()?.clone();
                self.row_to_issue(&id)
            })
            .filter(|issue| {
                if issue.status != "open" {
                    return false;
                }
                if let Some(p) = project {
                    if issue.project != p {
                        return false;
                    }
                }
                // Past the availability gate.
                if let Some(avail) = issue.available_at {
                    if avail > at {
                        return false;
                    }
                }
                // No open dependency: every parent must be terminal.
                for key in self.keys(DEPENDENCY) {
                    let comps = key.as_vec();
                    if comps.get(0).map(String::as_str) == Some(issue.id.as_str()) {
                        if let Some(parent) = comps.get(1) {
                            let status = self
                                .load_issue(parent)
                                .and_then(|d| d["status"].as_str().map(str::to_string));
                            if status.as_deref().is_none_or(|s| !is_terminal(s)) {
                                return false;
                            }
                        }
                    }
                }
                // Unclaimed, or its own expired lease is still up for grabs.
                match (&issue.assignee, issue.expires_at) {
                    (Some(_), Some(exp)) if exp > at => false,
                    _ => true,
                }
            })
            .collect();
        issues.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.id.cmp(&b.id))
        });
        Ok(issues)
    }

    fn update(&mut self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue> {
        if let Some(status) = &patch.status
            && !STATUSES.contains(&status.as_str())
        {
            return Err(Error::BadStatus(status.clone()));
        }
        let mut current = self.get(id)?;
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
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            match merge {
                Value::Object(incoming) => {
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
            current.metadata = Value::Object(map);
            current.external_ref = current
                .metadata
                .get("external_ref")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        let status_changed = patch.status.as_deref().is_some_and(|s| s != current.status);
        if let Some(status) = &patch.status {
            current.status = status.clone();
            current.closed_at = (status == "closed").then_some(at);
        }
        let available_at = patch.available_at.unwrap_or_else(|| {
            (at + self.policy.edit_deferral_seconds).max(current.available_at.unwrap_or(0))
        });
        current.updated_at = at;
        // SQLite wrote the recomputed expression into the `available_at` column, so persist it
        // too: a substantive edit defers swarm visibility by `edit_deferral_seconds`.
        current.available_at = Some(available_at);
        self.save_issue(&current)?;
        if status_changed {
            self.log_event(id, at, actor, "status", &format!("→ {}", current.status))?;
        }
        self.log_event(id, at, actor, "updated", "")?;
        self.get(id)
    }

    #[allow(dead_code)]
    fn set_labels(&mut self, id: &str, labels: &[String], actor: &str, at: i64) -> Result<()> {
        let mut issue = self.get(id)?;
        issue.labels = labels.to_vec();
        issue.labels.sort();
        self.save_issue(&issue)?;
        self.log_event(id, at, actor, "labels", &labels.join(","))
    }

    fn add_label(&mut self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        let mut issue = self.get(id)?;
        if !issue.labels.iter().any(|l| l == label) {
            issue.labels.push(label.to_string());
            issue.labels.sort();
            self.save_issue(&issue)?;
            self.log_event(id, at, actor, "labels", &issue.labels.join(","))?;
        }
        Ok(())
    }

    fn remove_label(&mut self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        let mut issue = self.get(id)?;
        issue.labels.retain(|l| l != label);
        self.save_issue(&issue)?;
        self.log_event(id, at, actor, "labels", &issue.labels.join(","))
    }

    fn add_dep(&mut self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        if child == parent {
            return Err(Error::Cycle {
                child: child.into(),
                parent: parent.into(),
            });
        }
        if self.load_issue(child).is_none() {
            return Err(Error::NotFound(child.to_string()));
        }
        if self.load_issue(parent).is_none() {
            return Err(Error::NotFound(parent.to_string()));
        }
        if self.reachable(parent, child)? {
            return Err(Error::Cycle {
                child: child.into(),
                parent: parent.into(),
            });
        }
        let key = Key::new([child.to_string(), parent.to_string()]);
        // INSERT OR IGNORE: a duplicate edge appends nothing.
        if self.current(DEPENDENCY, &key).is_none() {
            self.append(DEPENDENCY, &key, json!({"child": child, "parent": parent}));
            self.update_dependency_index(child, parent, true)?;
            let mut issue = self.get(child)?;
            issue.updated_at = at;
            self.save_issue(&issue)?;
            self.log_event(child, at, actor, "depends_on", parent)?;
        }
        Ok(())
    }

    fn remove_dep(&mut self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        let key = Key::new([child.to_string(), parent.to_string()]);
        if self.current(DEPENDENCY, &key).is_some() {
            self.tombstone(DEPENDENCY, &key);
            self.update_dependency_index(child, parent, false)?;
            self.log_event(child, at, actor, "unblocked_from", parent)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn add_remote_dep(
        &mut self,
        child: &str,
        dependency: &RemoteDependency,
        actor: &str,
        at: i64,
    ) -> Result<()> {
        if self.load_issue(child).is_none() {
            return Err(Error::NotFound(child.to_string()));
        }
        let key = Key::new([child.to_string(), dependency.canonical_ref.clone()]);
        let mut data = self
            .current(REMOTE_DEP, &key)
            .unwrap_or_else(|| json!({"child": child}));
        data["child"] = json!(child);
        data["canonical_ref"] = json!(dependency.canonical_ref);
        data["store_id"] = json!(dependency.store_id);
        data["remote_issue_id"] = json!(dependency.remote_issue_id);
        data["state"] = json!(dependency.state);
        data["asserted_at"] = json!(dependency.asserted_at);
        data["observed_at"] = json!(dependency.observed_at);
        data["signature"] = json!(dependency.signature);
        data["key_id"] = json!(dependency.key_id);
        self.append(REMOTE_DEP, &key, data);
        let mut issue = self.get(child)?;
        issue.updated_at = at;
        self.save_issue(&issue)?;
        self.log_event(
            child,
            at,
            actor,
            "depends_on_remote",
            &dependency.canonical_ref,
        )
    }

    #[allow(dead_code)]
    fn remote_dependencies(&self, child: &str) -> Result<Vec<RemoteDependency>> {
        let mut out: Vec<(String, RemoteDependency)> = self
            .keys(REMOTE_DEP)
            .iter()
            .filter_map(|k| {
                let comps = k.as_vec();
                if comps.get(0).map(String::as_str) != Some(child) {
                    return None;
                }
                let data = self.current(REMOTE_DEP, k)?;
                let ref_ = data["canonical_ref"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Some((
                    ref_.clone(),
                    RemoteDependency {
                        canonical_ref: ref_,
                        store_id: data["store_id"].as_str().unwrap_or_default().to_string(),
                        remote_issue_id: data["remote_issue_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        state: data["state"].as_str().unwrap_or_default().to_string(),
                        asserted_at: data["asserted_at"].as_i64().unwrap_or(0),
                        observed_at: data["observed_at"].as_i64().unwrap_or(0),
                        signature: data["signature"].as_str().unwrap_or_default().to_string(),
                        key_id: data["key_id"].as_str().unwrap_or_default().to_string(),
                        freshness: None,
                    },
                ))
            })
            .collect();
        out.sort_by_key(|(r, _)| r.clone());
        Ok(out.into_iter().map(|(_, d)| d).collect())
    }

    #[allow(dead_code)]
    fn ready_with_remote_edges(
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

    fn claim(&mut self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt> {
        let ttl = match req.actor_kind {
            ActorKind::Agent => req
                .ttl_seconds
                .unwrap_or(self.policy.agent_ttl_seconds)
                .min(self.policy.max_ttl_seconds),
            ActorKind::Human => {
                let at_dt = chrono::DateTime::from_timestamp(at, 0).expect("valid epoch");
                let business = req.ttl_seconds.map(|secs| {
                    let expired = self
                        .week
                        .deadline(&at_dt, secs.div_euclid(60).max(1))
                        .timestamp();
                    expired - at
                });
                let default_ttl = self.human_ttl(at);
                business
                    .unwrap_or(default_ttl)
                    .min(self.policy.max_ttl_seconds)
            }
        };
        let project = self.project_of(&req.id)?;
        let current = self.get(&req.id)?;
        let expected = self.issue_version(&req.id)?;
        let held = current.assignee.is_some() && current.expires_at.is_some_and(|e| e > at);
        if held && current.assignee.as_deref() != Some(req.assignee.as_str()) {
            return Err(Error::AlreadyClaimed(
                req.id.clone(),
                current.assignee.unwrap(),
            ));
        }
        let mut issue = current;
        issue.assignee = Some(req.assignee.clone());
        issue.actor_kind = Some(req.actor_kind.as_str().to_string());
        issue.expires_at = Some(at + ttl);
        issue.updated_at = at;
        if !self.save_issue_cas(&issue, expected)? {
            let latest = self.get(&req.id)?;
            if latest.assignee.as_deref() != Some(req.assignee.as_str())
                && let Some(holder) = latest.assignee
            {
                return Err(Error::AlreadyClaimed(req.id.clone(), holder));
            }
            return Err(Error::Bad(format!(
                "claim for {} lost a race; retry",
                req.id
            )));
        }
        self.log_event(
            &req.id,
            at,
            &req.assignee,
            "claimed",
            req.actor_kind.as_str(),
        )?;
        let _ = project;
        Ok(ClaimReceipt {
            id: req.id.clone(),
            assignee: req.assignee.clone(),
            actor_kind: req.actor_kind,
            expires_at: at + ttl,
        })
    }

    fn touch(&mut self, id: &str, assignee: &str, at: i64) -> Result<i64> {
        let current = self.get(id)?;
        if current.assignee.as_deref() != Some(assignee) {
            return Err(Error::Bad(format!("{id} is not claimed by {assignee}")));
        }
        let ttl = match current.actor_kind.as_deref().and_then(ActorKind::parse) {
            Some(ActorKind::Human) => self.human_ttl(at),
            _ => self.policy.agent_ttl_seconds,
        };
        let mut issue = current;
        issue.expires_at = Some(at + ttl);
        issue.updated_at = at;
        self.save_issue(&issue)?;
        self.log_event(id, at, assignee, "heartbeat", "")?;
        Ok(at + ttl)
    }

    fn release(&mut self, id: &str, actor: &str, at: i64) -> Result<()> {
        let mut issue = self.get(id)?;
        issue.assignee = None;
        issue.actor_kind = None;
        issue.expires_at = None;
        issue.updated_at = at;
        self.save_issue(&issue)?;
        self.log_event(id, at, actor, "released", "")
    }

    fn sweep(&mut self, at: i64) -> Result<crate::store::SweepReport> {
        let mut report = crate::store::SweepReport::default();
        // Snapshot the eligible ids first so the append-only writes during the loop do not shift
        // the iteration (SQLite iterated a prepared statement over a fixed snapshot).
        let mut eligible: Vec<(String, String, String)> = self
            .keys(ISSUE)
            .iter()
            .filter_map(|k| {
                let id = k.as_vec().first()?.clone();
                let d = self.load_issue(&id)?;
                let assignee = d["assignee"].as_str()?;
                let kind = d["actor_kind"].as_str().unwrap_or("agent");
                let expires = d["expires_at"].as_i64()?;
                if expires <= at {
                    Some((id, assignee.to_string(), kind.to_string()))
                } else {
                    None
                }
            })
            .collect();
        eligible.sort();
        for (id, assignee, kind) in eligible {
            if kind == ActorKind::Human.as_str() {
                report.escalated.push(format!("{id} ({assignee})"));
                self.log_event(&id, at, "sweeper", "escalated", &assignee)?;
                continue;
            }
            let mut issue = self.get(&id)?;
            issue.assignee = None;
            issue.actor_kind = None;
            issue.expires_at = None;
            issue.updated_at = at;
            self.save_issue(&issue)?;
            self.log_event(&id, at, "sweeper", "claim_expired", &assignee)?;
            report.requeued.push(id);
        }
        Ok(report)
    }

    fn check_blockers(&self, id: &str) -> Result<()> {
        for key in self.keys(DEPENDENCY) {
            let comps = key.as_vec();
            if comps.get(0).map(String::as_str) == Some(id) {
                if let Some(blocker) = comps.get(1) {
                    let status = self
                        .load_issue(blocker)
                        .and_then(|d| d["status"].as_str().map(str::to_string));
                    if status.as_deref().is_some_and(|s| !is_terminal(s)) {
                        return Err(Error::Bad(format!(
                            "{id} still waits on {blocker}; close it first or remove the dependency"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn close_unchecked(
        &mut self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue> {
        let mut issue = self.get(id)?;
        // Evidence accumulates, deduped by (kind, value).
        for item in evidence {
            if !issue
                .evidence
                .iter()
                .any(|e| e.kind == item.kind && e.value == item.value)
            {
                issue.evidence.push(item.clone());
            }
        }
        issue.status = "closed".into();
        issue.closed_at = Some(at);
        issue.assignee = None;
        issue.actor_kind = None;
        issue.expires_at = None;
        issue.updated_at = at;
        self.save_issue(&issue)?;
        let detail = format!(
            "{} | {}",
            reason.unwrap_or(""),
            evidence
                .iter()
                .map(|e| format!("{}:{}", e.kind, e.value))
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.log_event(id, at, actor, "closed", &detail)?;
        self.get(id)
    }

    fn close(
        &mut self,
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

    fn supersede(&mut self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()> {
        self.get(replacement)?;
        self.close_unchecked(
            id,
            Some(&format!("superseded by {replacement}")),
            &[Evidence::ack(format!("superseded by {replacement}"))],
            actor,
            at,
        )?;
        self.add_label(id, &format!("supersedes:{replacement}"), actor, at)
    }

    fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        let mut out: Vec<(i64, HistoryEvent)> = self
            .keys(MARBLES_EVENT)
            .iter()
            .filter_map(|k| {
                let seq = k.as_vec().first()?.parse::<i64>().ok()?;
                let data = self.current(MARBLES_EVENT, k)?;
                if data["issue_id"].as_str() != Some(id) {
                    return None;
                }
                Some((
                    seq,
                    HistoryEvent {
                        issue_id: id.to_string(),
                        ts: data["ts"].as_i64().unwrap_or(0),
                        actor: data["actor"].as_str().unwrap_or_default().to_string(),
                        event: data["event"].as_str().unwrap_or_default().to_string(),
                        detail: data["detail"].as_str().unwrap_or_default().to_string(),
                    },
                ))
            })
            .collect();
        out.sort_by_key(|(seq, _)| *seq);
        Ok(out.into_iter().map(|(_, e)| e).collect())
    }

    fn stats(&self) -> Result<Value> {
        let mut counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        for k in self.keys(ISSUE) {
            let id = match k.as_vec().first() {
                Some(s) => s,
                None => continue,
            };
            let Some(d) = self.load_issue(id) else {
                continue;
            };
            let project = d["project"].as_str().unwrap_or_default();
            let status = d["status"].as_str().unwrap_or_default();
            *counts
                .entry(project.to_string())
                .or_default()
                .entry(status.to_string())
                .or_default() += 1;
        }
        let mut map = serde_json::Map::new();
        for (project, statuses) in counts {
            let total: i64 = statuses.values().sum();
            let mut inner = serde_json::Map::new();
            inner.insert(
                "by_status".into(),
                serde_json::to_value(&statuses).unwrap_or(Value::Null),
            );
            inner.insert("total".into(), json!(total));
            map.insert(project, Value::Object(inner));
        }
        Ok(Value::Object(map))
    }

    fn prometheus_metrics(&self, at: i64) -> Result<String> {
        use std::fmt::Write as _;
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
        // issues by project,status
        let mut issues: BTreeMap<(String, String), i64> = BTreeMap::new();
        for k in self.keys(ISSUE) {
            let id = match k.as_vec().first() {
                Some(s) => s,
                None => continue,
            };
            let Some(d) = self.load_issue(id) else {
                continue;
            };
            let key = (
                d["project"].as_str().unwrap_or_default().to_string(),
                d["status"].as_str().unwrap_or_default().to_string(),
            );
            *issues.entry(key).or_default() += 1;
        }
        for ((project, status), count) in issues {
            writeln!(
                out,
                "marbles_issues{{project=\"{}\",status=\"{}\"}} {}",
                prometheus_label(&project),
                prometheus_label(&status),
                count
            )
            .unwrap();
        }
        // queue oldest age
        let mut queue: BTreeMap<String, i64> = BTreeMap::new();
        for issue in self.ready(None, at)? {
            let age = at - issue.created_at;
            let entry = queue.entry(issue.project.clone()).or_insert(i64::MIN);
            *entry = (*entry).max(age);
        }
        for (project, age) in queue {
            writeln!(
                out,
                "marbles_queue_oldest_age_seconds{{project=\"{}\"}} {}",
                prometheus_label(&project),
                age.max(0)
            )
            .unwrap();
        }
        // claims active
        let mut claims: BTreeMap<(String, String), i64> = BTreeMap::new();
        for k in self.keys(ISSUE) {
            let id = match k.as_vec().first() {
                Some(s) => s,
                None => continue,
            };
            let Some(d) = self.load_issue(id) else {
                continue;
            };
            if d["assignee"].is_null() {
                continue;
            }
            let expires = d["expires_at"].as_i64().unwrap_or(0);
            if expires <= at {
                continue;
            }
            let kind = d["actor_kind"].as_str().unwrap_or_default();
            *claims
                .entry((
                    d["project"].as_str().unwrap_or_default().to_string(),
                    kind.to_string(),
                ))
                .or_default() += 1;
        }
        for ((project, kind), count) in claims {
            writeln!(
                out,
                "marbles_claims_active{{project=\"{}\",actor_kind=\"{}\"}} {}",
                prometheus_label(&project),
                prometheus_label(&kind),
                count
            )
            .unwrap();
        }
        // events total
        let mut events: BTreeMap<(String, String), i64> = BTreeMap::new();
        for k in self.keys(MARBLES_EVENT) {
            let data = self.current(MARBLES_EVENT, &k);
            let Some(d) = data else { continue };
            let issue_id = match d["issue_id"].as_str() {
                Some(s) => s,
                None => continue,
            };
            let project = match self.load_issue(issue_id) {
                Some(p) => p["project"].as_str().unwrap_or_default().to_string(),
                None => continue,
            };
            *events
                .entry((project, d["event"].as_str().unwrap_or_default().to_string()))
                .or_default() += 1;
        }
        for ((project, event), count) in events {
            writeln!(
                out,
                "marbles_events_total{{project=\"{}\",event=\"{}\"}} {}",
                prometheus_label(&project),
                prometheus_label(&event),
                count
            )
            .unwrap();
        }
        // claim latency: sum(closed? no) -> first_claim - created_at for issues that were claimed
        let mut claim_lat: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for k in self.keys(ISSUE) {
            let id = match k.as_vec().first() {
                Some(s) => s,
                None => continue,
            };
            let Some(d) = self.load_issue(id) else {
                continue;
            };
            // find first claimed event
            let mut first_claim: Option<i64> = None;
            for ek in self.keys(MARBLES_EVENT) {
                let ed = self.current(MARBLES_EVENT, &ek);
                if let Some(e) = ed {
                    if e["issue_id"].as_str() == Some(id) && e["event"].as_str() == Some("claimed")
                    {
                        let ts = e["ts"].as_i64().unwrap_or(0);
                        first_claim = Some(first_claim.map_or(ts, |c: i64| c.min(ts)));
                    }
                }
            }
            if let Some(fc) = first_claim {
                let created = d["created_at"].as_i64().unwrap_or(0);
                if fc >= created {
                    let e = claim_lat
                        .entry(d["project"].as_str().unwrap_or_default().to_string())
                        .or_insert((0, 0));
                    e.0 += fc - created;
                    e.1 += 1;
                }
            }
        }
        for (project, (sum, count)) in claim_lat {
            writeln!(
                out,
                "marbles_claim_latency_seconds_sum{{project=\"{}\"}} {}",
                prometheus_label(&project),
                sum
            )
            .unwrap();
            writeln!(
                out,
                "marbles_claim_latency_seconds_count{{project=\"{}\"}} {}",
                prometheus_label(&project),
                count
            )
            .unwrap();
        }
        // cycle time
        let mut cycle: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for k in self.keys(ISSUE) {
            let id = match k.as_vec().first() {
                Some(s) => s,
                None => continue,
            };
            let Some(d) = self.load_issue(id) else {
                continue;
            };
            if let (Some(closed), Some(created)) =
                (d["closed_at"].as_i64(), d["created_at"].as_i64())
            {
                if closed >= created {
                    let e = cycle
                        .entry(d["project"].as_str().unwrap_or_default().to_string())
                        .or_insert((0, 0));
                    e.0 += closed - created;
                    e.1 += 1;
                }
            }
        }
        for (project, (sum, count)) in cycle {
            writeln!(
                out,
                "marbles_cycle_time_seconds_sum{{project=\"{}\"}} {}",
                prometheus_label(&project),
                sum
            )
            .unwrap();
            writeln!(
                out,
                "marbles_cycle_time_seconds_count{{project=\"{}\"}} {}",
                prometheus_label(&project),
                count
            )
            .unwrap();
        }
        // review time
        let mut review: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for k in self.keys(ISSUE) {
            let id = match k.as_vec().first() {
                Some(s) => s,
                None => continue,
            };
            let Some(d) = self.load_issue(id) else {
                continue;
            };
            let closed = d["closed_at"].as_i64();
            if closed.is_none() {
                continue;
            }
            let mut first_review: Option<i64> = None;
            for ek in self.keys(MARBLES_EVENT) {
                let ed = self.current(MARBLES_EVENT, &ek);
                if let Some(e) = ed {
                    if e["issue_id"].as_str() == Some(id)
                        && e["event"].as_str() == Some("status")
                        && e["detail"].as_str() == Some("→ review")
                    {
                        let ts = e["ts"].as_i64().unwrap_or(0);
                        first_review = Some(first_review.map_or(ts, |c: i64| c.min(ts)));
                    }
                }
            }
            if let (Some(fr), Some(closed)) = (first_review, closed) {
                if closed >= fr {
                    let e = review
                        .entry(d["project"].as_str().unwrap_or_default().to_string())
                        .or_insert((0, 0));
                    e.0 += closed - fr;
                    e.1 += 1;
                }
            }
        }
        for (project, (sum, count)) in review {
            writeln!(
                out,
                "marbles_review_time_seconds_sum{{project=\"{}\"}} {}",
                prometheus_label(&project),
                sum
            )
            .unwrap();
            writeln!(
                out,
                "marbles_review_time_seconds_count{{project=\"{}\"}} {}",
                prometheus_label(&project),
                count
            )
            .unwrap();
        }
        Ok(out)
    }
}

fn prometheus_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// The Store trait
// ---------------------------------------------------------------------------

impl crate::store::Store for Adapter {
    // Mutations lock mutably; reads lock immutably. Both serialize against the one store.
    fn ensure_project(&self, slug: &str, root: &str, prefix: &str) -> Result<()> {
        let mut g = self.lock();
        Inner::ensure_project(&mut g, slug, root, prefix)
    }
    fn projects(&self) -> Result<Vec<(String, String, String)>> {
        let mut g = self.lock();
        Inner::projects(&mut g)
    }
    fn ensure_folder(&self, folder: &Folder, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::ensure_folder(&mut g, folder, at)
    }
    fn set_project_folder(&self, project: &str, folder: Option<&str>, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::set_project_folder(&mut g, project, folder, at)
    }
    fn folder_view(&self) -> Result<crate::types::FolderView> {
        let mut g = self.lock();
        Inner::folder_view(&mut g)
    }
    fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<crate::store::OutboundEvent>> {
        let g = self.lock();
        Inner::pending_events(&g, at, limit)
    }
    fn mark_event_delivered(&self, seq: i64, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::mark_event_delivered(&mut g, seq, at)
    }
    fn mark_event_failed(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()> {
        let mut g = self.lock();
        Inner::mark_event_failed(&mut g, seq, next_attempt_at, error)
    }
    fn create(&self, spec: &NewIssue, at: i64) -> Result<String> {
        let mut g = self.lock();
        Inner::create(&mut g, spec, at)
    }
    fn get(&self, id: &str) -> Result<Issue> {
        let g = self.lock();
        Inner::get(&g, id)
    }
    fn list(&self, project: Option<&str>, all: bool, only_id: Option<&str>) -> Result<Vec<Issue>> {
        let g = self.lock();
        Inner::list(&g, project, all, only_id)
    }
    fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>> {
        let g = self.lock();
        Inner::ready(&g, project, at)
    }
    fn update(&self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue> {
        let mut g = self.lock();
        Inner::update(&mut g, id, patch, actor, at)
    }
    fn add_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::add_label(&mut g, id, label, actor, at)
    }
    fn remove_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::remove_label(&mut g, id, label, actor, at)
    }
    fn add_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::add_dep(&mut g, child, parent, actor, at)
    }
    fn remove_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::remove_dep(&mut g, child, parent, actor, at)
    }
    fn claim(&self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt> {
        let mut g = self.lock();
        Inner::claim(&mut g, req, at)
    }
    fn touch(&self, id: &str, assignee: &str, at: i64) -> Result<i64> {
        let mut g = self.lock();
        Inner::touch(&mut g, id, assignee, at)
    }
    fn release(&self, id: &str, actor: &str, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::release(&mut g, id, actor, at)
    }
    fn sweep(&self, at: i64) -> Result<crate::store::SweepReport> {
        let mut g = self.lock();
        Inner::sweep(&mut g, at)
    }
    fn close(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue> {
        let mut g = self.lock();
        Inner::close(&mut g, id, reason, evidence, actor, at)
    }
    fn supersede(&self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()> {
        let mut g = self.lock();
        Inner::supersede(&mut g, id, replacement, actor, at)
    }
    fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        let g = self.lock();
        Inner::history(&g, id)
    }
    fn stats(&self) -> Result<serde_json::Value> {
        let g = self.lock();
        Inner::stats(&g)
    }
    fn prometheus_metrics(&self, at: i64) -> Result<String> {
        let g = self.lock();
        Inner::prometheus_metrics(&g, at)
    }
}

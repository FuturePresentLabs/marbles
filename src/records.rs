//! A faithful, in-memory implementation of the `soil:records` append-only model.
//!
//! This engine is a stand-alone, offline, deterministic implementation of the records
//! semantics that a `fungos-records` / `soil:records` host would provide. It is used to
//! prove that the records model can express the Marbles `Store` contract (see
//! [`crate::adapter`]) without inventing records semantics the target crate does not define.
//!
//! ## The model
//!
//! * A **record type** (schema) names its columns and its *natural key* — the ordered list of
//!   fields that together identify a record. Marbles uses: `project`, `folder`,
//!   `project_folder`, `issue`, `dependency`, `remote_dependency`, `outbound_event`,
//!   `store_meta`, and a global `marbles_event` audit trail.
//! * A **record** is identified by its **key** (the natural key). Cross-references (a `dep`'s
//!   `depends_on`) are themselves key components, not stored copies — that is the one place the
//!   model points at another record.
//! * State is an **append-only sequence of `Revision`s**, each `{ seq, data, deleted }`. The
//!   **current state** is the `data` of the last non-deleted revision.
//! * A **tombstone** is a deleted revision. It is retained in the append-only trail (so history
//!   and the audit log are never rewritten) but is invisible to the live-state index, so a key
//!   that has been deleted no longer appears in `keys()`.
//! * `seq` is a per-record-type, monotonically increasing, globally assigned counter.
//!
//! This is append-only, so reads are `O(log n)` over the trail rather than a random-access
//! table. That is the price of the model, and it is exactly what makes a tombstone, a history
//! event, and the current state the same three words.

#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
use std::io::{self, BufRead, Write};
#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
use std::path::PathBuf;

#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
use serde::{Deserialize, Serialize};

/// The declared type of a column. Mirrors the typed columns a host would enforce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    Text,
    Int,
    Bool,
    Json,
}

/// One declared column of a record type.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
}

/// The schema of a record type: its columns and the key that identifies an instance.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub name: String,
    pub key: Vec<String>,
    pub columns: Vec<Column>,
    /// The column that carries the JSON-encoded state. Must be a declared column.
    pub data_column: String,
}

/// A single point in a record's append-only trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub seq: i64,
    pub key: Vec<String>,
    pub data: serde_json::Value,
    pub deleted: bool,
}

/// A composite natural key. Comparison is lexicographic over the ordered components, with the
/// longer key winning on a shared prefix (so `["demo"]` sorts before `["demo", "f"]`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub Vec<String>);

impl Key {
    pub fn new(v: impl IntoIterator<Item = String>) -> Self {
        Key(v.into_iter().collect())
    }
    pub fn as_vec(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// A revision as it is written to the durable trail file. The record-type name is carried here
/// because the trail is a single append-only log shared by every record type.
#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
#[derive(Debug, Serialize, Deserialize)]
struct PersistedRevision {
    r#type: String,
    seq: i64,
    key: Vec<String>,
    data: serde_json::Value,
    deleted: bool,
}

/// The exact revision/origin tuple used by a conditional write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub revision: u64,
    pub origin: String,
}

/// The result of a conditional write. On conflict the current live state is
/// returned so the caller can re-read or report the holder.
#[derive(Debug, Clone)]
pub struct CasResult {
    pub applied: bool,
    pub version: Option<Version>,
    pub data: Option<serde_json::Value>,
    pub deleted: bool,
}

#[cfg(all(feature = "soil", target_arch = "wasm32"))]
pub use crate::soil_engine::SoilEngine as Engine;

/// An append-only, append-only-backed, single-writer record store.
///
/// Synchronous per call, like the Marbles `Store` protocol: a read observes every write that
/// preceded it. There is no concurrency model to reason about — the single-writer guarantee of
/// `claim` comes from the `Store` protocol serializing calls, not from row locks.
#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
pub struct Engine {
    schemas: BTreeMap<String, RecordSchema>,
    history: BTreeMap<String, VecDeque<Revision>>,
    live: BTreeMap<String, BTreeSet<Key>>,
    seq: i64,
    /// When set, every appended revision is also written (as one JSON line) to this trail file,
    /// and `load` replays the same file. This is the local stand-in for a records host's durable,
    /// append-only store: nothing is ever rewritten, only appended.
    path: Option<PathBuf>,
}

#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
impl Default for Engine {
    fn default() -> Self {
        Engine {
            schemas: BTreeMap::new(),
            history: BTreeMap::new(),
            live: BTreeMap::new(),
            seq: 0,
            path: None,
        }
    }
}

#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
fn current_of(deque: &VecDeque<Revision>, key: &Key) -> Option<serde_json::Value> {
    // The current state of *this* key: its most recent revision. A tombstone as the latest
    // revision means the row is gone (the older revisions are retained history, not live state).
    deque
        .iter()
        .rev()
        .find(|r| &r.key == &key.0)
        .and_then(|r| (!r.deleted).then(|| r.data.clone()))
}

#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
fn history_for(deque: &VecDeque<Revision>, key: &Key) -> Vec<Revision> {
    deque.iter().filter(|r| r.key == key.0).cloned().collect()
}

#[cfg(not(all(feature = "soil", target_arch = "wasm32")))]
impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a record type. Idempotent: re-defining an existing type is a no-op, so a migration
    /// or a second `open` can safely re-assert the schema.
    pub fn define(&mut self, schema: RecordSchema) -> Result<(), String> {
        if schema.name.is_empty() {
            return Err("record type name is empty".into());
        }
        if !self.schemas.contains_key(&schema.name) {
            if !schema.columns.iter().any(|c| c.name == schema.data_column) {
                return Err(format!(
                    "data column '{}' is not declared as a column of '{}'",
                    schema.data_column, schema.name
                ));
            }
        }
        self.schemas.insert(schema.name.clone(), schema);
        Ok(())
    }

    pub fn has(&self, name: &str) -> bool {
        self.schemas.contains_key(name)
    }

    pub fn schema(&self, name: &str) -> Option<&RecordSchema> {
        self.schemas.get(name)
    }

    /// Append a revision. Returns the global sequence number assigned to this revision.
    pub fn append(
        &mut self,
        name: &str,
        key: &Key,
        data: serde_json::Value,
        deleted: bool,
        _at: i64,
    ) -> i64 {
        let seq = self.seq + 1;
        self.seq = seq;
        let rev = Revision {
            seq,
            key: key.0.clone(),
            data,
            deleted,
        };
        self.history
            .entry(name.to_string())
            .or_default()
            .push_back(rev.clone());
        if deleted {
            self.live
                .entry(name.to_string())
                .or_default()
                .remove(&Key(key.0.clone()));
        } else {
            self.live
                .entry(name.to_string())
                .or_default()
                .insert(Key(key.0.clone()));
        }
        // Write-through: append the revision to the durable trail so a reopen replays it. The
        // file is only ever appended to, never rewritten — the on-disk shape is the model.
        if let Some(path) = &self.path {
            let line = serde_json::to_string(&PersistedRevision {
                r#type: name.to_string(),
                seq,
                key: rev.key.clone(),
                data: rev.data.clone(),
                deleted: rev.deleted,
            })
            .expect("revision is serializable");
            let mut file = io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .expect("trail open"),
            );
            file.write_all(line.as_bytes()).expect("trail write");
            file.write_all(b"\n").expect("trail write");
            file.flush().expect("trail flush");
        }
        seq
    }

    /// The current (last non-deleted) state of a key, if the record type exists and the key is live.
    pub fn current(&self, name: &str, key: &Key) -> Option<serde_json::Value> {
        current_of(self.history.get(name)?, key)
    }

    /// The full append-only trail for a key (including tombstones), in sequence order.
    pub fn history_of(&self, name: &str, key: &Key) -> Vec<Revision> {
        self.history
            .get(name)
            .map(|dq| history_for(dq, key))
            .unwrap_or_default()
    }

    /// All live keys for a record type, in key order. Deleted keys are not returned.
    pub fn keys(&self, name: &str) -> Vec<Key> {
        self.live
            .get(name)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Unbounded key scan. The in-memory engine has no cap, so this is the
    /// same as `keys`; the wasm records engine uses it for index rebuilds.
    pub fn keys_all(&self, name: &str) -> Vec<Key> {
        self.keys(name)
    }

    /// The current key, if live.
    pub fn key_of(&self, name: &str, key: &Key) -> Option<Key> {
        self.live
            .get(name)
            .filter(|s| s.contains(key))
            .map(|s| s.get(key).cloned().unwrap())
    }

    /// The latest revision for a key, including tombstones.
    pub fn version(&self, name: &str, key: &Key) -> Option<Version> {
        let latest = self
            .history
            .get(name)?
            .iter()
            .rev()
            .find(|revision| revision.key == key.0)?;
        Some(Version {
            revision: latest.seq as u64,
            origin: "memory".into(),
        })
    }

    /// Conditional create/replace/delete against the latest revision.
    pub fn compare_and_swap(
        &mut self,
        name: &str,
        key: &Key,
        expected: Option<Version>,
        data: serde_json::Value,
        deleted: bool,
    ) -> Result<CasResult, String> {
        let latest = self
            .history
            .get(name)
            .and_then(|history| history.iter().rev().find(|revision| revision.key == key.0));
        let matches = match (&expected, latest) {
            (None, None) => true,
            (None, Some(revision)) => revision.deleted,
            (Some(_), None) => return Err("not-found".into()),
            (Some(version), Some(revision)) => {
                revision.seq as u64 == version.revision && version.origin == "memory"
            }
        };
        let current = latest.map(|revision| CasResult {
            applied: false,
            version: Some(Version {
                revision: revision.seq as u64,
                origin: "memory".into(),
            }),
            data: (!revision.deleted).then(|| revision.data.clone()),
            deleted: revision.deleted,
        });
        if !matches {
            return Ok(current.expect("conflict has a current revision"));
        }
        let seq = self.append(name, key, data.clone(), deleted, 0);
        Ok(CasResult {
            applied: true,
            version: Some(Version {
                revision: seq as u64,
                origin: "memory".into(),
            }),
            data: (!deleted).then_some(data),
            deleted,
        })
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }

    /// A durable, append-only engine rooted at `path`. Revisions are written through to
    /// `path` on every append; schemas are re-asserted by the caller after loading (see
    /// [`Adapter::open_file`](crate::adapter::Adapter::open_file)).
    pub fn durable(path: PathBuf) -> Self {
        Engine {
            path: Some(path),
            ..Default::default()
        }
    }

    /// Reopen a trail previously written by a durable engine. Missing file means an empty store.
    /// Replays the append-only log in order, rebuilding history, the live index, and `seq`.
    pub fn load(path: &PathBuf) -> Result<Self, String> {
        let mut engine = Engine::durable(path.clone());
        if !path.exists() {
            return Ok(engine);
        }
        let file = std::fs::File::open(path).map_err(|e| format!("open trail: {e}"))?;
        for line in io::BufReader::new(file).lines() {
            let line = line.map_err(|e| format!("read trail: {e}"))?;
            if line.trim().is_empty() {
                continue;
            }
            let rev: PersistedRevision =
                serde_json::from_str(&line).map_err(|e| format!("parse trail line: {e}"))?;
            engine.seq = engine.seq.max(rev.seq);
            let revision = Revision {
                seq: rev.seq,
                key: rev.key.clone(),
                data: rev.data,
                deleted: rev.deleted,
            };
            engine
                .history
                .entry(rev.r#type.clone())
                .or_default()
                .push_back(revision);
            if rev.deleted {
                engine
                    .live
                    .entry(rev.r#type)
                    .or_default()
                    .remove(&Key(rev.key.clone()));
            } else {
                engine
                    .live
                    .entry(rev.r#type)
                    .or_default()
                    .insert(Key(rev.key));
            }
        }
        Ok(engine)
    }
}

/// A convenience alias used throughout the adapter for record-type names.
pub type Name = String;

#[cfg(all(test, not(all(feature = "soil", target_arch = "wasm32"))))]
mod tests {
    use super::*;

    #[test]
    fn memory_compare_and_swap_rejects_stale_versions() {
        let mut engine = Engine::new();
        let key = Key::new(["issue-1".to_string()]);
        let create = engine
            .compare_and_swap("issue", &key, None, serde_json::json!({"v": 1}), false)
            .unwrap();
        assert!(create.applied);
        let version = create.version.unwrap();

        let duplicate = engine
            .compare_and_swap("issue", &key, None, serde_json::json!({"v": 2}), false)
            .unwrap();
        assert!(!duplicate.applied);

        let stale = engine
            .compare_and_swap(
                "issue",
                &key,
                Some(Version {
                    revision: version.revision.saturating_sub(1),
                    origin: version.origin.clone(),
                }),
                serde_json::json!({"v": 3}),
                false,
            )
            .unwrap();
        assert!(!stale.applied);

        let update = engine
            .compare_and_swap(
                "issue",
                &key,
                Some(version),
                serde_json::json!({"v": 4}),
                false,
            )
            .unwrap();
        assert!(update.applied);
        assert_eq!(engine.current("issue", &key).unwrap()["v"], 4);
    }
}

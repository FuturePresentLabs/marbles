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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

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

/// An append-only, append-only-backed, single-writer record store.
///
/// Synchronous per call, like the Marbles `Store` protocol: a read observes every write that
/// preceded it. There is no concurrency model to reason about — the single-writer guarantee of
/// `claim` comes from the `Store` protocol serializing calls, not from row locks.
pub struct Engine {
    schemas: BTreeMap<String, RecordSchema>,
    history: BTreeMap<String, VecDeque<Revision>>,
    live: BTreeMap<String, BTreeSet<Key>>,
    seq: i64,
}

impl Default for Engine {
    fn default() -> Self {
        Engine {
            schemas: BTreeMap::new(),
            history: BTreeMap::new(),
            live: BTreeMap::new(),
            seq: 0,
        }
    }
}

fn current_of(deque: &VecDeque<Revision>, key: &Key) -> Option<serde_json::Value> {
    // The current state of *this* key: its most recent revision. A tombstone as the latest
    // revision means the row is gone (the older revisions are retained history, not live state).
    deque
        .iter()
        .rev()
        .find(|r| &r.key == &key.0)
        .and_then(|r| (!r.deleted).then(|| r.data.clone()))
}

fn history_for(deque: &VecDeque<Revision>, key: &Key) -> Vec<Revision> {
    deque.iter().filter(|r| r.key == key.0).cloned().collect()
}

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
            .push_back(rev);
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

    /// The current key, if live.
    pub fn key_of(&self, name: &str, key: &Key) -> Option<Key> {
        self.live
            .get(name)
            .filter(|s| s.contains(key))
            .map(|s| s.get(key).cloned().unwrap())
    }

    pub fn seq(&self) -> i64 {
        self.seq
    }
}

/// A convenience alias used throughout the adapter for record-type names.
pub type Name = String;

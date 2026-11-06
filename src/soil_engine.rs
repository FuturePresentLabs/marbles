//! A Marbles record engine backed directly by `soil:records`.
//!
//! This module is compiled only for wasm with the `soil` feature. It keeps
//! the `records::Engine` surface used by `adapter::Adapter`, but every append
//! and read is a guest call. Claims use `compare-and-swap`, so two invocations
//! cannot both win a claim through last-writer-wins.
//!
//! One Marbles record type maps to one `soil:records` collection
//! (`issue` -> `marbles.issue`). The composite key is the record id as a JSON
//! array and the record body is the existing Marbles JSON state.

use std::cell::Cell;

use serde_json::Value;
use soil_sdk_records::{Book, CasResult as SoilCasResult, Version as SoilVersion};

use crate::records::{CasResult, Key, Name, RecordSchema, Revision, Version};

const MAX_PAGE: u32 = 512;
const MAX_KEYS: usize = 100_000;

pub struct SoilEngine {
    schemas: std::collections::BTreeMap<Name, RecordSchema>,
    seq: Cell<i64>,
}

impl SoilEngine {
    pub fn new() -> Self {
        Self {
            schemas: Default::default(),
            seq: Cell::new(Self::max_event_seq()),
        }
    }

    /// Event ids are the shared Marbles audit sequence. Re-seed from the
    /// `marbles_event` collection so a new wasm invocation cannot reuse an id.
    fn max_event_seq() -> i64 {
        let book = Self::book("marbles_event");
        let mut cursor = None;
        let mut max = 0;
        loop {
            let page = book
                .scan(cursor.clone(), MAX_PAGE)
                .expect("soil:records scan");
            for entry in page.entries {
                if let Some(Key(parts)) = Self::key(&entry.key.id)
                    && let Some(first) = parts.first()
                    && let Ok(seq) = first.parse::<i64>()
                {
                    max = max.max(seq);
                }
            }
            match page.next {
                Some(next) if Some(next.as_str()) != cursor.as_deref() => cursor = Some(next),
                _ => break,
            }
        }
        max
    }

    fn collection(name: &str) -> String {
        format!("marbles.{name}")
    }

    fn schema_id(name: &str) -> String {
        format!("marbles.{name}@1")
    }

    fn id(key: &Key) -> String {
        serde_json::to_string(&key.0).expect("key serializes")
    }

    fn key(id: &str) -> Option<Key> {
        serde_json::from_str::<Vec<String>>(id).ok().map(Key::new)
    }

    fn book(name: &str) -> Book {
        Book::new(Self::collection(name))
    }

    fn entry_current(name: &str, key: &Key) -> Option<soil_sdk_records::Entry> {
        Self::book(name).get(Self::id(key)).ok().flatten()
    }

    pub fn define(&mut self, schema: RecordSchema) -> Result<(), String> {
        if schema.name.is_empty() {
            return Err("record type name is empty".into());
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

    /// Append a revision. The soil backend keeps the current state and the
    /// explicit `marbles_event` records; it does not retain per-key history.
    pub fn append(&mut self, name: &str, key: &Key, data: Value, deleted: bool, _at: i64) -> i64 {
        let book = Self::book(name);
        let id = Self::id(key);
        if deleted {
            book.delete(id).expect("soil:records delete");
        } else {
            let body = serde_json::to_vec(&data).expect("record body serializes");
            book.upsert(id, Self::schema_id(name), body)
                .expect("soil:records upsert");
        }
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        seq
    }

    pub fn current(&self, name: &str, key: &Key) -> Option<Value> {
        let entry = Self::entry_current(name, key)?;
        if entry.deleted {
            return None;
        }
        serde_json::from_slice(&entry.body).ok()
    }

    pub fn history_of(&self, name: &str, key: &Key) -> Vec<Revision> {
        let Some(entry) = Self::entry_current(name, key) else {
            return Vec::new();
        };
        let data = serde_json::from_slice(&entry.body).unwrap_or(Value::Null);
        vec![Revision {
            seq: entry.revision as i64,
            key: key.0.clone(),
            data,
            deleted: entry.deleted,
        }]
    }

    pub fn keys(&self, name: &str) -> Vec<Key> {
        let book = Self::book(name);
        let mut cursor = None;
        let mut keys = Vec::new();
        loop {
            let page = book
                .scan(cursor.clone(), MAX_PAGE)
                .expect("soil:records scan");
            for entry in page.entries {
                if entry.deleted {
                    continue;
                }
                if let Some(key) = Self::key(&entry.key.id) {
                    keys.push(key);
                }
                if keys.len() >= MAX_KEYS {
                    keys.sort();
                    return keys;
                }
            }
            match page.next {
                Some(next) if Some(next.as_str()) != cursor.as_deref() => cursor = Some(next),
                _ => break,
            }
        }
        keys.sort();
        keys
    }

    pub fn key_of(&self, name: &str, key: &Key) -> Option<Key> {
        self.current(name, key).map(|_| key.clone())
    }

    /// The latest soil record version for a key, including a tombstone.
    pub fn version(&self, name: &str, key: &Key) -> Option<Version> {
        let entry = Self::entry_current(name, key)?;
        Some(Version {
            revision: entry.revision,
            origin: entry.origin,
        })
    }

    /// Conditional create/replace/delete. This is the claim authority.
    pub fn compare_and_swap(
        &mut self,
        name: &str,
        key: &Key,
        expected: Option<Version>,
        data: Value,
        deleted: bool,
    ) -> Result<CasResult, String> {
        let book = Self::book(name);
        let body = if deleted {
            Vec::new()
        } else {
            serde_json::to_vec(&data).map_err(|error| error.to_string())?
        };
        let expected = expected.map(|version| SoilVersion {
            revision: version.revision,
            origin: version.origin,
        });
        let result: SoilCasResult = book
            .compare_and_swap(
                Self::id(key),
                expected,
                Self::schema_id(name),
                body,
                deleted,
            )
            .map_err(|error| error.to_string())?;
        let current = if result.entry.deleted {
            None
        } else {
            serde_json::from_slice(&result.entry.body).ok()
        };
        Ok(CasResult {
            applied: result.applied,
            version: Some(Version {
                revision: result.entry.revision,
                origin: result.entry.origin,
            }),
            data: current,
            deleted: result.entry.deleted,
        })
    }

    pub fn seq(&self) -> i64 {
        self.seq.get()
    }

    pub fn durable(_path: std::path::PathBuf) -> Self {
        Self::new()
    }

    pub fn load(_path: &std::path::PathBuf) -> Result<Self, String> {
        Ok(Self::new())
    }
}

impl Default for SoilEngine {
    fn default() -> Self {
        Self::new()
    }
}

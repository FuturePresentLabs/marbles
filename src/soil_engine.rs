//! A Marbles record engine backed directly by `soil:records`.
//!
//! This module is compiled only for wasm with the `soil` feature. It keeps
//! the `records::Engine` surface used by `adapter::Adapter`, but every append
//! and read is a guest call. Claims use `compare-and-swap`, so two invocations
//! cannot both win a claim through last-writer-wins.
//!
//! All Marbles record types live in one `soil:records` collection named
//! `marbles`. The entity id is `<type>/<json-key-array>`; the record body is
//! the existing Marbles JSON state. One collection means one records book can
//! hold the whole task store.
//!
//! # One scan, then answer from it
//!
//! The host's `scan` returns whole entries — key *and* body — so a full pass is
//! already a read of the entire store. `keys()` used to keep only the keys and
//! drop the bodies, which made every projection go back over the boundary once
//! per record: listing a project of one issue from a 6,000-issue book cost
//! 6,000 guest calls and took 1.6s. The scan result is now kept whole and
//! `current()` answers from it.
//!
//! The cache lives for one invocation and is dropped on the first write, so a
//! command reads a consistent snapshot and never sees its own half-applied edit.

use std::cell::{Cell, RefCell};

use serde_json::Value;
use soil_sdk_records::{Book, CasResult as SoilCasResult, Version as SoilVersion};

use crate::records::{CasResult, Key, Name, RecordSchema, Revision, Version};

const COLLECTION: &str = "marbles";
const MAX_PAGE: u32 = 512;
const MAX_KEYS: usize = 100_000;

pub struct SoilEngine {
    schemas: std::collections::BTreeMap<Name, RecordSchema>,
    /// The highest audit id already in the book. Zero means "not looked up":
    /// real event ids start at 1, and a read-only invocation should not scan the
    /// collection to learn a number it will never use.
    seq: Cell<i64>,
    /// Record types read in this invocation, each sorted by key. Entity ids are
    /// `<type>/<json>`, so a type occupies one contiguous range of the
    /// collection: reading it is a bounded scan, not a read of the whole store.
    /// `show` touches dependencies and nothing else; it should not pay for
    /// 5,959 issue bodies.
    types: RefCell<std::collections::BTreeMap<Name, Vec<(Key, Value)>>>,
}

impl SoilEngine {
    pub fn new() -> Self {
        Self {
            schemas: Default::default(),
            seq: Cell::new(0),
            types: RefCell::new(Default::default()),
        }
    }

    /// Event ids are the shared Marbles audit sequence. Re-seed from the
    /// `marbles_event` records so a new wasm invocation cannot reuse an id.
    ///
    /// Called from the first write, never from a read.
    fn max_event_seq() -> i64 {
        let book = Self::book();
        let mut cursor = None;
        let mut max = 0;
        loop {
            let page = book
                .scan(cursor.clone(), MAX_PAGE)
                .unwrap_or_else(|error| panic!("soil:records event scan: {error}"));
            for entry in page.entries {
                if let Some((name, Key(parts))) = Self::parse_id(&entry.key.id)
                    && name == "marbles_event"
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

    fn schema_id(name: &str) -> String {
        format!("marbles.{name}@1")
    }

    fn id(name: &str, key: &Key) -> String {
        format!(
            "{name}/{}",
            serde_json::to_string(&key.0).expect("key serializes")
        )
    }

    fn parse_id(id: &str) -> Option<(&str, Key)> {
        let (name, encoded) = id.split_once('/')?;
        let key = serde_json::from_str::<Vec<String>>(encoded).ok()?;
        Some((name, Key::new(key)))
    }

    fn book() -> Book {
        Book::new(COLLECTION)
    }

    fn entry_current(name: &str, key: &Key) -> Option<soil_sdk_records::Entry> {
        Self::book().get(Self::id(name, key)).ok().flatten()
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
        let book = Self::book();
        let id = Self::id(name, key);
        // Name the record and the store's own reason. These were bare
        // `.expect`s, so a contended store aborted the guest with a wasm
        // backtrace and no indication of which write had failed.
        if deleted {
            book.delete(id.clone())
                .unwrap_or_else(|error| panic!("soil:records delete {id}: {error}"));
        } else {
            let body = serde_json::to_vec(&data).expect("record body serializes");
            book.upsert(id.clone(), Self::schema_id(name), body)
                .unwrap_or_else(|error| panic!("soil:records upsert {id}: {error}"));
        }
        let seq = self.next_seq();
        self.invalidate();
        seq
    }

    /// The invocation's audit sequence, resolved from the book on first use.
    fn next_seq(&self) -> i64 {
        let current = match self.seq.get() {
            0 => Self::max_event_seq(),
            known => known,
        };
        let seq = current + 1;
        self.seq.set(seq);
        seq
    }

    fn invalidate(&self) {
        self.types.borrow_mut().clear();
    }

    /// Read one record type, once per invocation.
    fn ensure_loaded(&self, name: &str) {
        if self.types.borrow().contains_key(name) {
            return;
        }
        let book = Self::book();
        let prefix = format!("{name}/");
        let mut cursor = Some(prefix.clone());
        let mut records: Vec<(Key, Value)> = Vec::new();
        while records.len() < MAX_KEYS {
            let page = book
                .scan(cursor.clone(), MAX_PAGE)
                .unwrap_or_else(|error| panic!("soil:records scan {name}: {error}"));
            let mut reached_end = false;
            for entry in page.entries {
                if !entry.key.id.starts_with(&prefix) {
                    // The next type's range began: this type is fully read.
                    reached_end = true;
                    break;
                }
                if entry.deleted {
                    continue;
                }
                if let Some((_, key)) = Self::parse_id(&entry.key.id)
                    && let Ok(value) = serde_json::from_slice::<Value>(&entry.body)
                {
                    records.push((key, value));
                }
            }
            if reached_end {
                break;
            }
            match page.next {
                Some(next) if Some(next.as_str()) != cursor.as_deref() => cursor = Some(next),
                _ => break,
            }
        }
        records.sort_by(|left, right| left.0.cmp(&right.0));
        self.types.borrow_mut().insert(name.to_string(), records);
    }

    pub fn current(&self, name: &str, key: &Key) -> Option<Value> {
        if !self.types.borrow().contains_key(name) {
            // This type has not been read yet. One host call answers one record;
            // reading the type's whole range to serve it is what `keys` is for.
            let entry = Self::entry_current(name, key)?;
            if entry.deleted {
                return None;
            }
            return serde_json::from_slice(&entry.body).ok();
        }
        let types = self.types.borrow();
        let records = types.get(name)?;
        let index = records
            .binary_search_by(|(candidate, _)| candidate.cmp(key))
            .ok()?;
        Some(records[index].1.clone())
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
        self.ensure_loaded(name);
        let types = self.types.borrow();
        types
            .get(name)
            .map(|records| records.iter().map(|(key, _)| key.clone()).collect())
            .unwrap_or_default()
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
        let book = Self::book();
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
                Self::id(name, key),
                expected,
                Self::schema_id(name),
                body,
                deleted,
            )
            .map_err(|error| error.to_string())?;
        if result.applied {
            self.invalidate();
        }
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
        match self.seq.get() {
            0 => Self::max_event_seq(),
            known => known,
        }
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

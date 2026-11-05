//! Durability of the records backend: a store survives a drop/reopen, exactly as the SQLite
//! `CompanyStores` reopen test proves for a company DB file. This is the records-model analogue
//! of that guarantee — the append-only trail on disk is the source of truth.

use marbles::adapter::Adapter;
use marbles::db::Store;
use marbles::types::*;
use std::path::PathBuf;

fn new(title: &str) -> NewIssue {
    NewIssue {
        title: title.into(),
        description: String::new(),
        issue_type: "task".into(),
        priority: 2,
        labels: vec![],
        parent: None,
        id: None,
        project: Some("demo".into()),
        available_at: None,
        created_by: Some("tester".into()),
        metadata: None,
        external_ref: None,
    }
}

#[test]
fn a_records_store_survives_a_drop_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let trail = PathBuf::from(dir.path()).join("records.trail.jsonl");

    let store_id;
    {
        let a = Adapter::open_file(trail.clone()).unwrap();
        store_id = a.store_id();
        a.ensure_project("demo", "/tmp/demo", "demo").unwrap();
        let id = a.create(&new("persisted"), 100).unwrap();
        a.add_label(&id, "alfalfa", "t", 110).unwrap();
        // The append-only trail file exists on disk after writes.
        assert!(trail.is_file(), "revisions must be written through to disk");
        drop(a);
    }

    // Reopen from the same trail: every mutation is still there.
    let reopened = Adapter::open_file(trail.clone()).unwrap();
    assert_eq!(
        reopened.store_id(),
        store_id,
        "store id is stable across reopen"
    );
    let ids: Vec<String> = reopened
        .list(Some("demo"), true, None)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids.len(), 1);
    let issue = reopened.get(&ids[0]).unwrap();
    assert_eq!(issue.title, "persisted");
    assert!(
        issue.labels.iter().any(|l| l == "alfalfa"),
        "labels round-trip"
    );

    // New writes after reopen append to the same trail without disturbing the old ones.
    let more = reopened.create(&new("second"), 200).unwrap();
    assert_eq!(reopened.get(&more).unwrap().title, "second");
    assert_eq!(reopened.list(Some("demo"), true, None).unwrap().len(), 2);
}

#[test]
fn history_and_outbox_events_are_persisted_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let trail = PathBuf::from(dir.path()).join("records.trail.jsonl");

    let id;
    {
        let a = Adapter::open_file(trail.clone()).unwrap();
        a.ensure_project("demo", "/tmp/demo", "demo").unwrap();
        id = a.create(&new("audit me"), 100).unwrap();
        drop(a);
    }

    let reopened = Adapter::open_file(trail.clone()).unwrap();
    let events: Vec<String> = reopened
        .history(&id)
        .unwrap()
        .into_iter()
        .map(|e| e.event)
        .collect();
    assert_eq!(events, vec!["created"], "audit trail survives reopen");
    let pending = reopened.pending_events(1_000, 10).unwrap();
    assert_eq!(pending.len(), 1, "outbox survives reopen");
    assert_eq!(pending[0].event, "created");
}

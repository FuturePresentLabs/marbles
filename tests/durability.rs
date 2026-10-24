//! The local store's durability contract: an atomic snapshot, an integrity report, and the
//! settings that make an acknowledged write survive a crash.

use marbles::db::{Db, now};
use marbles::types::*;

fn seeded(path: &std::path::Path) -> Db {
    let db = Db::open(path).unwrap();
    db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    let id = db
        .create(
            &NewIssue {
                title: "durable".into(),
                project: Some("demo".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
    db.close(
        &id,
        Some("delivered"),
        &[Evidence::commit("deadbeef")],
        "tester",
        now() + 1,
    )
    .unwrap();
    db
}

#[test]
fn a_backup_is_an_openable_snapshot_with_the_same_rows() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("marbles.db");
    let db = seeded(&source);
    let target = dir.path().join("snapshots").join("marbles-1.db");
    let report = db.backup(&target).unwrap();
    assert_eq!(report.integrity, "ok");
    assert!(report.bytes > 0);
    assert_eq!(report.projects, 1);
    assert_eq!(report.issues, 1);
    assert!(target.exists());

    let restored = Db::open(&target).unwrap();
    let issues = restored.list(Some("demo"), true, None).unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].status, "closed");
    assert_eq!(issues[0].title, "durable");
}

#[test]
fn backup_refuses_to_overwrite_a_previous_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded(&dir.path().join("marbles.db"));
    let target = dir.path().join("snapshot.db");
    db.backup(&target).unwrap();
    let err = db.backup(&target).unwrap_err();
    assert!(matches!(err, marbles::db::Error::Bad(_)), "{err}");
}

#[test]
fn doctor_reports_integrity_wal_and_durable_settings() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded(&dir.path().join("marbles.db"));
    let report = db.doctor().unwrap();
    assert_eq!(report.integrity, "ok");
    assert_eq!(report.quick_check, "ok");
    assert_eq!(report.journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(report.synchronous, "FULL");
    assert!(report.db_bytes > 0);
    assert_eq!(report.projects, 1);
    assert_eq!(report.issues, 1);
    assert!(report.foreign_key_violations.is_empty());
    assert_eq!(
        report.outbox_pending, 2,
        "created + closed events are queued"
    );
}

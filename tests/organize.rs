//! Folders, project assignment, and the goal → epic → task hierarchy.
//!
//! Dependencies are allowed to cross projects (blockers are global); hierarchy is deliberately
//! project-local so a goal cannot silently adopt work from another tracker.

use marbles::db::{Db, now};
use marbles::types::*;

fn db() -> Db {
    let db = Db::in_memory().unwrap();
    db.ensure_project("personal", "/tmp/personal", "personal")
        .unwrap();
    db.ensure_project("acme", "/tmp/acme", "acme").unwrap();
    db
}

fn folder(slug: &str, parent: Option<&str>) -> Folder {
    Folder {
        slug: slug.into(),
        name: slug.into(),
        parent: parent.map(str::to_string),
        created_at: 0,
    }
}

#[test]
fn projects_move_between_nested_folders_and_can_be_unfiled() {
    let db = db();
    db.ensure_folder(&folder("personal", None), 1).unwrap();
    db.ensure_folder(&folder("business", None), 1).unwrap();
    db.ensure_folder(&folder("acme", Some("business")), 1)
        .unwrap();
    db.set_project_folder("acme", Some("acme"), 2).unwrap();
    db.set_project_folder("personal", Some("personal"), 2)
        .unwrap();

    let view = db.folder_view().unwrap();
    assert_eq!(view.folders.len(), 3);
    let acme = view.projects.iter().find(|p| p.project == "acme").unwrap();
    assert_eq!(acme.folder, "acme");
    assert_eq!(
        view.folders
            .iter()
            .find(|f| f.slug == "acme")
            .unwrap()
            .parent
            .as_deref(),
        Some("business")
    );

    db.set_project_folder("acme", None, 3).unwrap();
    let view = db.folder_view().unwrap();
    assert!(!view.projects.iter().any(|p| p.project == "acme"));
}

#[test]
fn a_folder_tree_cannot_be_made_cyclic() {
    let db = db();
    db.ensure_folder(&folder("a", None), 1).unwrap();
    db.ensure_folder(&folder("b", Some("a")), 1).unwrap();
    let err = db.ensure_folder(&folder("a", Some("b")), 2).unwrap_err();
    assert!(matches!(err, marbles::db::Error::Bad(_)), "{err}");
    let err = db.ensure_folder(&folder("a", Some("a")), 2).unwrap_err();
    assert!(matches!(err, marbles::db::Error::Bad(_)), "{err}");
}

#[test]
fn goals_epics_and_tasks_are_a_project_local_hierarchy() {
    let db = db();
    let goal = db
        .create(
            &NewIssue {
                title: "ship v1".into(),
                project: Some("acme".into()),
                issue_type: "goal".into(),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
    let epic = db
        .create(
            &NewIssue {
                title: "storage".into(),
                project: Some("acme".into()),
                issue_type: "epic".into(),
                parent: Some(goal.clone()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
    let task = db
        .create(
            &NewIssue {
                title: "write WAL".into(),
                project: Some("acme".into()),
                issue_type: "task".into(),
                parent: Some(epic.clone()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();

    let issues = db.list(Some("acme"), true, None).unwrap();
    let progress = marbles::report::progress_for(&issues, &goal);
    assert_eq!(progress.total, 2);
    assert_eq!(progress.epics, 1);
    assert_eq!(progress.tasks, 1);
    let lines = marbles::report::tree_lines(&issues, &goal);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains(&goal));
    assert!(lines[2].contains(&task));
}

#[test]
fn hierarchy_is_project_local_but_dependencies_may_cross_projects() {
    let db = db();
    let parent = db
        .create(
            &NewIssue {
                title: "personal goal".into(),
                project: Some("personal".into()),
                issue_type: "goal".into(),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
    let err = db
        .create(
            &NewIssue {
                title: "wrong child".into(),
                project: Some("acme".into()),
                parent: Some(parent.clone()),
                ..Default::default()
            },
            now(),
        )
        .unwrap_err();
    assert!(matches!(err, marbles::db::Error::Bad(_)), "{err}");

    let blocker = db
        .create(
            &NewIssue {
                title: "blocker in personal".into(),
                project: Some("personal".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
    let blocked = db
        .create(
            &NewIssue {
                title: "work in acme".into(),
                project: Some("acme".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
    db.add_dep(&blocked, &blocker, "tester", now()).unwrap();
    let issue = db.get(&blocked).unwrap();
    assert!(
        issue
            .dependencies
            .iter()
            .any(|d| d.depends_on_id.as_deref() == Some(blocker.as_str()))
    );
}

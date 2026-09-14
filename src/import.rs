//! One-shot import of a Beads export. The shape mirrors `bd list --all --json` because that is
//! what the migration source actually emits (see README, "Migrating from Beads").

use std::io::Read;

use serde::Deserialize;

use crate::db::Db;
use crate::types::{Issue, NewIssue};

/// Accepts either a bare array of beads or `{"issues": [...]}` — both are shapes `bd` has used.
pub fn read_export(path: &str) -> Result<Vec<Issue>, String> {
    let mut text = String::new();
    if path == "-" {
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
    } else {
        text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    }
    parse_rows(&text).map_err(|e| format!("{path}: {e}"))
}

pub fn parse_rows(text: &str) -> Result<Vec<Issue>, String> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Export {
        Array(Vec<Issue>),
        Wrapped { issues: Vec<Issue> },
    }
    let export: Export = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(match export {
        Export::Array(rows) => rows,
        Export::Wrapped { issues } => issues,
    })
}

/// Insert issues with their original ids and statuses, then the dependency edges. Ids are kept
/// verbatim so every existing reference — commits, run ledgers, docs, muscle memory — stays
/// valid across the move.
pub fn import(db: &Db, project: &str, rows: &[Issue]) -> Result<Report, String> {
    let mut report = Report::default();
    for row in rows {
        let spec = NewIssue {
            title: row.title.clone(),
            description: row.description.clone(),
            issue_type: if row.issue_type.is_empty() {
                "task".into()
            } else {
                row.issue_type.clone()
            },
            priority: row.priority,
            labels: row.labels.clone(),
            parent: row.parent.clone(),
            id: Some(row.id.clone()),
            project: Some(project.to_string()),
            // Imported work predates the swarm policy; it does not get a grace period retroactively.
            available_at: Some(0),
            created_by: Some("marbles-import".to_string()),
            metadata: None,
            external_ref: None,
        };
        let created_at = if row.created_at > 0 {
            row.created_at
        } else {
            crate::db::now()
        };
        let id = db.create(&spec, created_at).map_err(|e| e.to_string())?;
        if id != row.id {
            return Err(format!("import promised id {} but got {id}", row.id));
        }
        for dep in &row.dependencies {
            if let Some(parent) = &dep.depends_on_id {
                // Edges to beads outside the export are skipped rather than failing the import:
                // a partial export is normal, a silent wrong-graph is not acceptable later, so
                // the count lands in the report.
                match db.add_dep(&row.id, parent, "marbles-import", created_at) {
                    Ok(()) => report.deps += 1,
                    Err(crate::db::Error::NotFound(_)) => report.missing_deps += 1,
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
        if row.status != "open" {
            db.update(
                &row.id,
                &crate::types::IssuePatch {
                    status: Some(row.status.clone()),
                    ..Default::default()
                },
                "marbles-import",
                crate::db::now(),
            )
            .map_err(|e| e.to_string())?;
            report.statused += 1;
        }
        report.issues += 1;
    }
    Ok(report)
}

#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub issues: i64,
    pub deps: i64,
    pub statused: i64,
    pub missing_deps: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: &str, deps: &[&str]) -> Issue {
        Issue {
            id: id.into(),
            title: format!("{id} title"),
            description: "text".into(),
            status: status.into(),
            priority: 2,
            issue_type: "task".into(),
            labels: vec!["alfalfa".into()],
            dependency_count: deps.len() as i64,
            dependent_count: 0,
            dependencies: deps
                .iter()
                .map(|d| crate::types::Dependency {
                    depends_on_id: Some(d.to_string()),
                })
                .collect(),
            parent: None,
            evidence: Vec::new(),
            metadata: serde_json::json!({}),
            external_ref: None,
            assignee: None,
            actor_kind: None,
            expires_at: None,
            available_at: None,
            closed_at: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            project: "demo".into(),
        }
    }

    #[test]
    fn ids_and_statuses_survive_the_move() {
        let db = Db::in_memory().unwrap();
        db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
        let report = import(
            &db,
            "demo",
            &[
                row("demo-one", "closed", &[]),
                row("demo-two", "open", &["demo-one"]),
                row("demo-three", "blocked", &["demo-missing"]),
            ],
        )
        .unwrap();
        assert_eq!(report.issues, 3);
        assert_eq!(report.deps, 1);
        assert_eq!(report.missing_deps, 1);
        assert_eq!(db.get("demo-one").unwrap().status, "closed");
        assert_eq!(db.get("demo-three").unwrap().status, "blocked");
        assert_eq!(
            db.get("demo-two").unwrap().dependencies[0]
                .depends_on_id
                .as_deref(),
            Some("demo-one")
        );
    }

    #[test]
    fn imported_closed_beads_do_not_appear_in_ready() {
        let db = Db::in_memory().unwrap();
        db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
        import(
            &db,
            "demo",
            &[row("demo-a", "closed", &[]), row("demo-b", "open", &[])],
        )
        .unwrap();
        let ready: Vec<String> = db
            .ready(Some("demo"), 1_800_000_000)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ready, vec!["demo-b"]);
    }
}

#[cfg(test)]
mod bd_shape_tests {
    use super::*;

    #[test]
    fn real_bd_timestamps_parse() {
        let json = r#"[{"id":"x-1","title":"t","status":"closed","priority":2,
            "issue_type":"task","created_at":"2026-03-14T20:22:24.512074Z",
            "updated_at":"2026-03-15T01:02:03Z","closed_at":"2026-03-15T01:02:03Z",
            "dependency_count":0,"dependent_count":0,"owner":"o","comment_count":0,
            "close_reason":"done","started_at":null,"assignee":null,"description":""}]"#;
        let rows = parse_rows(json).expect("bd export must parse");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].created_at > 1_700_000_000);
        assert!(rows[0].closed_at.unwrap() > 0);
        // null started_at etc. must not break anything:
        let _ = rows[0].available_at;
    }
}

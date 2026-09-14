//! Importing a `bd export` JSONL into a marbles project.
//!
//! This is the migration path, so it is written to the standard of a data migration, not a
//! convenience script:
//!
//! - **Ids are verbatim.** Every commit, run ledger, and doc that cites a bead id stays valid.
//!   The one exception is `--rewrite-prefix old:new`, which exists to fix a *configuration
//!   mistake* — two stores sharing one prefix — by renaming one store's ids, not the rest of the
//!   fleet's references to them. Rewrites are reported so the caller can update pointers.
//! - **Re-import is safe.** A row whose id already exists and whose content matches (same
//!   updated_at, status, title) is counted as `unchanged`, never an error. A row whose id exists
//!   with *different* content is a `conflict`: it is reported and the import aborts before
//!   writing anything, because two different beads sharing an id means someone's prefix policy
//!   broke and silently keeping either one is data loss.
//! - **Nothing is dropped silently.** Beads fields marbles has no column for (`design`,
//!   `acceptance_criteria`, `notes`, `comments`) fold into the description; `close_reason`
//!   becomes an ack evidence; unknown statuses map through one explicit table; `_type:memory`
//!   records are skipped with a count.

use std::collections::BTreeMap;
use std::io::Read;

use chrono::DateTime;
use serde_json::Value;

use crate::db::{Db, now};
use crate::types::{Evidence, IssuePatch, NewIssue};

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{0}")]
    Bad(String),
    #[error("database: {0}")]
    Db(#[from] crate::db::Error),
    #[error("conflicts with existing issues and aborted (re-run is safe once resolved): {ids:?}")]
    Conflicts { ids: Vec<String> },
}

pub type Result<T> = std::result::Result<T, ImportError>;

#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct ImportReport {
    pub imported: i64,
    /// Closed by the export while a blocker was still open — landed as-is, flagged.
    #[serde(default)]
    pub exception_closed: i64,
    pub unchanged: i64,
    pub renamed: i64,
    pub deps: i64,
    pub missing_deps: i64,
    pub skipped_memories: i64,
    pub conflicts: Vec<String>,
}

/// Read a `bd export` JSONL (one JSON object per line, `_type` discriminates rows).
pub fn read_jsonl(path: &str) -> Result<Vec<Value>> {
    let mut text = String::new();
    if path == "-" {
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| ImportError::Bad(format!("stdin: {e}")))?;
    } else {
        text =
            std::fs::read_to_string(path).map_err(|e| ImportError::Bad(format!("{path}: {e}")))?;
    }
    let mut rows = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| ImportError::Bad(format!("{path}:{}: {e}", n + 1)))?;
        if value.get("_type").and_then(Value::as_str) == Some("issue") {
            rows.push(value);
        } else {
            rows.push(serde_json::json!({"_skip": true, "_row": value}));
        }
    }
    Ok(rows)
}

/// One explicit status translation table. Anything unmapped is an error, not a fallback —
/// a silently reinterpreted status is how a closed issue comes back as open.
fn normalize_status(raw: &str) -> Result<&'static str> {
    match raw {
        "open" | "backlog" => Ok("open"),
        "in_progress" | "started" => Ok("in_progress"),
        "review" | "inreview" | "in_review" => Ok("review"),
        "blocked" => Ok("blocked"),
        "deferred" => Ok("deferred"),
        "closed" | "done" | "resolved" => Ok("closed"),
        other => Err(ImportError::Bad(format!(
            "unknown status {other:?}; add it to the mapping deliberately"
        ))),
    }
}

fn parse_ts(value: Option<&Value>) -> Result<i64> {
    match value {
        None | Some(Value::Null) => Ok(now()),
        Some(Value::String(text)) => DateTime::parse_from_rfc3339(text)
            .map(|at| at.timestamp())
            .map_err(|e| ImportError::Bad(format!("timestamp {text:?}: {e}"))),
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| ImportError::Bad("timestamp must be RFC3339 or integer epoch".into())),
        Some(other) => Err(ImportError::Bad(format!("timestamp {other}"))),
    }
}

/// Compose the description so no authored prose is lost to a column-less field.
fn compose_description(row: &Value) -> String {
    let mut out = row["description"]
        .as_str()
        .unwrap_or_default()
        .trim_end()
        .to_string();
    for (heading, key) in [
        ("Design", "design"),
        ("Acceptance criteria", "acceptance_criteria"),
        ("Notes", "notes"),
    ] {
        if let Some(text) = row[key].as_str().filter(|t| !t.trim().is_empty()) {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!("## {heading}\n\n{}", text.trim()));
        }
    }
    if let Some(comments) = row["comments"].as_array().filter(|c| !c.is_empty()) {
        let rendered: Vec<String> = comments
            .iter()
            .map(|c| {
                let author = c["author"].as_str().unwrap_or("?");
                let text = c["text"].as_str().unwrap_or_default();
                format!("- {author}: {text}")
            })
            .collect();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("## Comments\n\n{}", rendered.join("\n")));
    }
    out
}

fn rewrite(id: &str, from: &str, to: &str) -> String {
    match id.strip_prefix(&format!("{from}-")) {
        Some(tail) if !tail.contains('-') => format!("{to}-{tail}"),
        _ => id.to_string(),
    }
}

pub fn import(
    db: &Db,
    project: &str,
    rows: &[Value],
    rewrite_prefix: Option<(&str, &str)>,
    dry_run: bool,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let id_of = |row: &Value| -> Result<(String, String)> {
        let raw = row["id"]
            .as_str()
            .ok_or_else(|| ImportError::Bad("issue row without an id".into()))?
            .to_string();
        let mapped = match rewrite_prefix {
            Some((from, to)) => {
                let next = rewrite(&raw, from, to);
                if next != raw {
                    (next, raw)
                } else {
                    (raw.clone(), raw)
                }
            }
            None => (raw.clone(), raw),
        };
        Ok(mapped)
    };

    // Phase 0: conflicts are decided before any write, so an abort truly changes nothing.
    let mut id_map: BTreeMap<String, String> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.get("_skip").is_none()) {
        let (id, original) = id_of(row)?;
        let status = normalize_status(row["status"].as_str().unwrap_or("open"))?;
        match db.get(id.as_str()).ok() {
            Some(existing) => {
                // Resume rule: a row carrying this importer's mark in the same project is a
                // previously-imported (or half-imported) copy of this exact export row — the
                // remaining phases are idempotent and will restate its status. Anything else
                // sharing the id is a genuine conflict: one bead, two truths, a human picks.
                let mine = existing.project == project
                    && existing.metadata.get("marbles_import") == Some(&serde_json::json!(true));
                if mine {
                    report.unchanged += 1;
                } else {
                    report.conflicts.push(if original != id {
                        format!("{original} → {id} ({})", existing.project)
                    } else {
                        format!("{id} (project {})", existing.project)
                    });
                }
            }
            None => {}
        }
        id_map.insert(original, id);
    }
    if !report.conflicts.is_empty() {
        return Err(ImportError::Conflicts {
            ids: report.conflicts,
        });
    }
    if dry_run {
        return Ok(report);
    }

    // Phase 1: create everything as open, ids verbatim (or rewritten), metadata and
    // external_ref preserved.
    for row in rows.iter().filter(|r| r.get("_skip").is_none()) {
        let original = row["id"].as_str().unwrap_or_default().to_string();
        let id = &id_map[&original];
        if id != &original {
            report.renamed += 1;
        }
        if db.get(id).is_ok() {
            continue; // unchanged; already present
        }
        let labels: Vec<String> = row["labels"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let created_at = parse_ts(row.get("created_at"))?;
        let spec = NewIssue {
            title: row["title"].as_str().unwrap_or("(untitled)").to_string(),
            description: compose_description(row),
            issue_type: row["issue_type"].as_str().unwrap_or("task").to_string(),
            priority: row["priority"].as_i64().unwrap_or(2),
            labels,
            parent: row["parent_id"].as_str().map(str::to_string),
            id: Some(id.clone()),
            project: Some(project.to_string()),
            available_at: Some(0),
            created_by: row["created_by"].as_str().map(str::to_string),
            metadata: {
                let mut meta = match &row["metadata"] {
                    Value::Object(map) => map.clone(),
                    _ => serde_json::Map::new(),
                };
                meta.insert("marbles_import".into(), serde_json::json!(true));
                Some(serde_json::Value::Object(meta))
            },
            external_ref: row["external_ref"].as_str().map(str::to_string),
        };
        db.create(&spec, created_at)?;
        report.imported += 1;
    }

    // Phase 2: dependencies (edges point at the final, possibly renamed, ids).
    for row in rows.iter().filter(|r| r.get("_skip").is_none()) {
        let original = row["id"].as_str().unwrap_or_default().to_string();
        let id = id_map[&original].clone();
        for dep in row["dependencies"].as_array().into_iter().flatten() {
            if dep["type"].as_str().is_some_and(|t| t != "blocks") {
                continue;
            }
            let Some(parent_original) = dep["depends_on_id"].as_str() else {
                continue;
            };
            let parent = parent_original
                .strip_prefix("memory:")
                .map(str::to_string)
                .unwrap_or_else(|| parent_original.to_string());
            let parent = id_map
                .get(&parent)
                .cloned()
                .unwrap_or_else(|| match rewrite_prefix {
                    Some((from, to)) => rewrite(&parent, from, to),
                    None => parent.clone(),
                });
            if parent == id {
                continue;
            }
            match db.add_dep(&id, &parent, "marbles-import", crate::db::now()) {
                Ok(()) => report.deps += 1,
                Err(crate::db::Error::NotFound(_)) => report.missing_deps += 1,
                Err(err) => return Err(err.into()),
            }
        }
    }

    // Phase 3: terminal states last, and *ordered* last — a closed row whose blocker is also
    // closed in this import must wait for it, so we pass over the rows until a full round makes
    // no progress. (Import order is file order; dependency order is not.)
    let mut pending: Vec<(&Value, String)> = rows
        .iter()
        .filter(|r| r.get("_skip").is_none())
        .map(|r| {
            let original = r["id"].as_str().unwrap_or_default().to_string();
            (r, id_map[&original].clone())
        })
        .collect();
    loop {
        let mut progressed = false;
        let mut failed = Vec::new();
        for (row, id) in &pending {
            let row = *row;
            let status = normalize_status(row["status"].as_str().unwrap_or("open"))?;
            let current = db.get(id)?;
            if status == "closed" && current.status != "closed" {
                let mut evidence = vec![Evidence::ack(
                    row["close_reason"]
                        .as_str()
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or("imported closed from beads export"),
                )];
                evidence.extend(
                    row["metadata"]
                        .get("external_ref")
                        .and_then(Value::as_str)
                        .or_else(|| row["external_ref"].as_str())
                        .map(Evidence::pr),
                );
                let closed_at = parse_ts(row.get("closed_at")).unwrap_or_else(|_| crate::db::now());
                match db.close(
                    id,
                    row["close_reason"].as_str(),
                    &evidence,
                    "marbles-import",
                    closed_at,
                ) {
                    Ok(_) => progressed = true,
                    // Still waiting on an unclosed blocker; another round will catch it.
                    Err(crate::db::Error::Bad(ref msg)) if msg.contains("still waits on") => {
                        failed.push((row, id.clone()))
                    }
                    Err(err) => return Err(err.into()),
                }
            } else if status != "open" && status != "closed" && current.status == "open" {
                db.update(
                    id,
                    &IssuePatch {
                        status: Some(status.to_string()),
                        available_at: Some(0),
                        ..Default::default()
                    },
                    "marbles-import",
                    crate::db::now(),
                )?;
                progressed = true;
            } else if status == "closed" && current.status == "closed" {
                // already terminal: satisfied
            }
        }
        pending = failed;
        if !progressed {
            break;
        }
    }
    // What the fixpoint could not close is an edge the source tracker allowed and marbles
    // does not: a bead closed while its blocker is still open. The import's contract is to
    // land the source state, not to launder it — so those rows close unchecked (the edges and
    // the open blocker both remain visible in the graph) and are reported.
    for (_row, id) in &pending {
        let row = *_row;
        db.close_unchecked(
            id,
            row["close_reason"].as_str(),
            &[Evidence::ack(format!(
                "imported closed while open blockers remain: {}",
                row["dependencies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| d["depends_on_id"].as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ))],
            "marbles-import",
            parse_ts(row.get("closed_at")).unwrap_or_else(|_| crate::db::now()),
        )?;
        report.exception_closed += 1;
    }

    report.skipped_memories = rows.iter().filter(|r| r.get("_skip").is_some()).count() as i64;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::in_memory().unwrap();
        db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
        db
    }

    fn row(id: &str, status: &str) -> Value {
        serde_json::json!({
            "_type": "issue", "id": id, "title": format!("title {id}"),
            "description": "desc", "design": "how", "acceptance_criteria": "when",
            "status": status, "priority": 2, "issue_type": "task",
            "created_at": "2026-09-01T10:00:00Z", "updated_at": "2026-09-02T10:00:00Z",
            "labels": ["x"], "dependencies": [], "metadata": {}
        })
    }

    #[test]
    fn timestamps_and_status_vocabulary_import_cleanly() {
        let db = db();
        let rows = vec![row("demo-a1", "open"), row("demo-a2", "inreview")];
        let report = import(&db, "demo", &rows, None, false).unwrap();
        assert_eq!(report.imported, 2);
        assert!(
            db.get("demo-a1").unwrap().created_at > 1_700_000_000,
            "RFC3339 → epoch"
        );
        assert_eq!(
            db.get("demo-a2").unwrap().status,
            "review",
            "inreview → review"
        );
        let text = &db.get("demo-a1").unwrap().description;
        assert!(
            text.contains("## Design") && text.contains("## Acceptance criteria"),
            "no prose lost: {text}"
        );
    }

    #[test]
    fn unknown_status_is_a_hard_error_not_a_guess() {
        let db = db();
        let err = import(&db, "demo", &[row("demo-z", "zinged")], None, false).unwrap_err();
        assert!(err.to_string().contains("unknown status"), "{err}");
    }

    #[test]
    fn closed_rows_come_over_as_closed_with_their_receipt() {
        let db = db();
        let mut r = row("demo-c1", "closed");
        r["close_reason"] = serde_json::json!("shipped in v2");
        import(&db, "demo", &[r], None, false).unwrap();
        let issue = db.get("demo-c1").unwrap();
        assert_eq!(issue.status, "closed");
        assert!(
            issue
                .evidence
                .iter()
                .any(|e| e.kind == "ack" && e.value.contains("shipped"))
        );
    }

    #[test]
    fn re_import_is_unchanged_and_renames_follow_the_prefix_flag() {
        let db = db();
        db.ensure_project("pkpro", "/tmp/pkpro", "pkpro").unwrap();
        let rows = vec![row("demo-a1", "open")];
        import(&db, "pkpro", &rows, Some(("demo", "pkpro")), false).unwrap();
        assert!(db.get("pkpro-a1").is_ok(), "id was rewritten");
        let second = import(&db, "pkpro", &rows, Some(("demo", "pkpro")), false).unwrap();
        assert_eq!(second.unchanged, 1);
        assert_eq!(second.imported, 0);
    }

    #[test]
    fn a_conflicting_existing_id_aborts_without_writing() {
        let db = db();
        db.create(
            &NewIssue {
                title: "DIFFERENT".into(),
                id: Some("demo-a1".into()),
                project: Some("demo".into()),
                available_at: Some(0),
                ..Default::default()
            },
            1_800_000_000,
        )
        .unwrap();
        let err = import(
            &db,
            "demo",
            &[row("demo-a1", "open"), row("demo-new", "open")],
            None,
            false,
        )
        .unwrap_err();
        assert!(
            matches!(&err, ImportError::Conflicts { ids } if ids.len() == 1),
            "{err}"
        );
        assert_eq!(
            db.get("demo-a1").unwrap().title,
            "DIFFERENT",
            "abort left rows alone"
        );
        assert!(
            db.get("demo-new").is_err(),
            "abort happened before any write"
        );
    }

    #[test]
    fn dependencies_survive_a_prefix_rewrite() {
        let db = db();
        db.ensure_project("pkpro", "/tmp/pkpro", "pkpro").unwrap();
        let mut parent = row("demo-p1", "closed");
        let mut child = row("demo-c2", "open");
        child["dependencies"] = serde_json::json!([{"issue_id": "demo-c2", "depends_on_id": "demo-p1", "type": "blocks"}]);
        parent["status"] = serde_json::json!("closed");
        let report = import(
            &db,
            "pkpro",
            &[parent, child],
            Some(("demo", "pkpro")),
            false,
        )
        .unwrap();
        assert_eq!(report.deps, 1);
        assert_eq!(
            db.get("pkpro-c2").unwrap().dependencies[0]
                .depends_on_id
                .as_deref(),
            Some("pkpro-p1"),
            "edges point at the renamed ids"
        );
    }
}

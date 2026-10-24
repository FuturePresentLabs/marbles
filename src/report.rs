//! Read-only portfolio reports: goal rollups, goal trees, and burndown.
//!
//! These are deliberately views over the same issue graph, not another store. The CLI fetches
//! one `issues.list` page (local or HTTP) and computes everything here, so a report is identical
//! in local mode and against the hosted API.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{Issue, STATUSES};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub total: i64,
    pub open: i64,
    pub in_progress: i64,
    pub review: i64,
    pub blocked: i64,
    pub deferred: i64,
    pub closed: i64,
    pub epics: i64,
    pub tasks: i64,
    /// Closed / total, 0-100, one decimal.
    pub percent_done: f64,
}

impl Progress {
    pub fn from_issues(issues: &[&Issue]) -> Self {
        let mut out = Progress::default();
        for issue in issues {
            out.total += 1;
            match issue.status.as_str() {
                "open" => out.open += 1,
                "in_progress" => out.in_progress += 1,
                "review" => out.review += 1,
                "blocked" => out.blocked += 1,
                "deferred" => out.deferred += 1,
                "closed" => out.closed += 1,
                _ => {}
            }
            match issue.issue_type.as_str() {
                "epic" => out.epics += 1,
                "goal" => {}
                _ => out.tasks += 1,
            }
        }
        out.percent_done = if out.total == 0 {
            0.0
        } else {
            ((out.closed as f64 / out.total as f64) * 1000.0).round() / 10.0
        };
        out
    }
}

/// The root plus every transitive child through `parent`.
pub fn descendant_ids(issues: &[Issue], root: &str) -> BTreeSet<String> {
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for issue in issues {
        if let Some(parent) = issue.parent.as_deref() {
            children.entry(parent).or_default().push(issue.id.as_str());
        }
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(kids) = children.get(id) {
            stack.extend(kids.iter().copied());
        }
    }
    seen
}

pub fn progress_for(issues: &[Issue], root: &str) -> Progress {
    let ids = descendant_ids(issues, root);
    // The root is the container, not work to be closed; count its descendants only.
    let scoped: Vec<&Issue> = issues
        .iter()
        .filter(|i| i.id != root && ids.contains(&i.id))
        .collect();
    Progress::from_issues(&scoped)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: i64,
    pub project: String,
    pub progress: Progress,
}

/// Every open or closed goal, optionally scoped to one project, with descendant rollups.
pub fn goal_summaries(issues: &[Issue], project: Option<&str>) -> Vec<GoalSummary> {
    let mut out: Vec<GoalSummary> = issues
        .iter()
        .filter(|i| i.issue_type == "goal")
        .filter(|i| project.is_none_or(|p| i.project == p))
        .map(|goal| GoalSummary {
            id: goal.id.clone(),
            title: goal.title.clone(),
            status: goal.status.clone(),
            priority: goal.priority,
            project: goal.project.clone(),
            progress: progress_for(issues, &goal.id),
        })
        .collect();
    out.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
    out
}

/// A depth-first rendering of one goal/epic and its children, with status and progress.
pub fn tree_lines(issues: &[Issue], root: &str) -> Vec<String> {
    let mut children: BTreeMap<&str, Vec<&Issue>> = BTreeMap::new();
    for issue in issues {
        if let Some(parent) = issue.parent.as_deref() {
            children.entry(parent).or_default().push(issue);
        }
    }
    for kids in children.values_mut() {
        kids.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
    }
    let mut out = Vec::new();
    let Some(root_issue) = issues.iter().find(|i| i.id == root) else {
        return out;
    };
    fn walk(
        issue: &Issue,
        depth: usize,
        children: &BTreeMap<&str, Vec<&Issue>>,
        out: &mut Vec<String>,
    ) {
        let indent = "  ".repeat(depth);
        let kind = if issue.issue_type.is_empty() {
            "task"
        } else {
            &issue.issue_type
        };
        out.push(format!(
            "{indent}{} [{}] {} ({kind}, p{})",
            issue.id, issue.status, issue.title, issue.priority
        ));
        if let Some(kids) = children.get(issue.id.as_str()) {
            for kid in kids {
                walk(kid, depth + 1, children, out);
            }
        }
    }
    walk(root_issue, 0, &children, &mut out);
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurndownDay {
    pub date: String,
    pub scope: i64,
    pub done: i64,
    pub remaining: i64,
    pub ideal_remaining: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Burndown {
    pub days: i64,
    pub scope: i64,
    pub done: i64,
    pub remaining: i64,
    /// Closed issues per day over the observed window; absent with no closes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_per_day: Option<f64>,
    /// Remaining / burn rate; absent when the burn rate is zero or unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_days_to_zero: Option<f64>,
    pub points: Vec<BurndownDay>,
}

/// Daily remaining-work series over the last `days` days. `scope` is the work that existed by
/// that day's end; `done` is the subset closed by then.
pub fn burndown(issues: &[Issue], days: i64, now: i64) -> Burndown {
    let days = days.max(1);
    let span = days.saturating_mul(86_400);
    let start = now.saturating_sub(span);
    let total_scope = issues.len() as i64;
    let mut points = Vec::with_capacity(days as usize + 1);
    for d in 0..=days {
        let t = (start + d.saturating_mul(86_400)).min(now);
        let scope = issues.iter().filter(|i| i.created_at <= t).count() as i64;
        let done = issues
            .iter()
            .filter(|i| i.created_at <= t && i.closed_at.is_some_and(|c| c <= t))
            .count() as i64;
        let remaining = (scope - done).max(0);
        // The ideal line burns the total scope evenly across the window.
        let ideal = ((total_scope as f64) * (1.0 - (d as f64 / days as f64))).round() as i64;
        points.push(BurndownDay {
            date: Utc
                .timestamp_opt(t, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            scope,
            done,
            remaining,
            ideal_remaining: ideal.max(0),
        });
    }
    let done_at_start = issues
        .iter()
        .filter(|i| i.closed_at.is_some_and(|c| c <= start))
        .count() as i64;
    let done_now = issues
        .iter()
        .filter(|i| i.closed_at.is_some_and(|c| c <= now))
        .count() as i64;
    let elapsed_days = (now - start) as f64 / 86_400.0;
    let burn_per_day = if elapsed_days > 0.0 && done_now > done_at_start {
        Some((done_now - done_at_start) as f64 / elapsed_days)
    } else {
        None
    };
    let remaining = (total_scope - done_now).max(0);
    let projected_days_to_zero = burn_per_day
        .filter(|rate| *rate > 0.0)
        .map(|rate| remaining as f64 / rate);
    Burndown {
        days,
        scope: total_scope,
        done: done_now,
        remaining,
        burn_per_day,
        projected_days_to_zero,
        points,
    }
}

/// The status vocabulary in display order, useful to clients that want a stable legend.
pub fn status_legend() -> &'static [&'static str; 6] {
    &STATUSES
}

/// A compact one-line progress bar, e.g. `############--------  60%`.
pub fn progress_bar(progress: &Progress, width: usize) -> String {
    let width = width.max(4);
    let done = ((progress.percent_done / 100.0) * width as f64).round() as usize;
    let done = done.min(width);
    format!(
        "{}{} {:>5.1}%",
        "#".repeat(done),
        "-".repeat(width - done),
        progress.percent_done
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, now};
    use crate::types::{Issue, NewIssue};

    fn issue(
        id: &str,
        parent: Option<&str>,
        kind: &str,
        status: &str,
        created: i64,
        closed: Option<i64>,
    ) -> Issue {
        let mut issue = Issue {
            id: id.into(),
            title: id.into(),
            description: String::new(),
            status: status.into(),
            issue_type: kind.into(),
            priority: 2,
            parent: parent.map(str::to_string),
            labels: vec![],
            assignee: None,
            actor_kind: None,
            expires_at: None,
            available_at: None,
            closed_at: closed,
            created_at: created,
            updated_at: created,
            project: "p".into(),
            ..Issue::default()
        };
        issue.status = status.into();
        issue
    }

    #[test]
    fn goal_progress_counts_descendants_and_ignores_siblings() {
        let issues = vec![
            issue("g", None, "goal", "open", 0, None),
            issue("e1", Some("g"), "epic", "open", 0, None),
            issue("t1", Some("e1"), "task", "closed", 0, Some(10)),
            issue("t2", Some("e1"), "task", "in_progress", 0, None),
            issue("other", None, "task", "open", 0, None),
        ];
        let p = progress_for(&issues, "g");
        assert_eq!(p.total, 3);
        assert_eq!(p.closed, 1);
        assert_eq!(p.epics, 1);
        assert_eq!(p.tasks, 2);
        assert_eq!(p.percent_done, 33.3);
    }

    #[test]
    fn goal_summaries_only_returns_goals() {
        let issues = vec![
            issue("g", None, "goal", "open", 0, None),
            issue("t", Some("g"), "task", "closed", 0, Some(10)),
        ];
        let sums = goal_summaries(&issues, None);
        assert_eq!(sums.len(), 1);
        assert_eq!(sums[0].progress.total, 1);
        assert_eq!(sums[0].progress.closed, 1);
    }

    #[test]
    fn burndown_uses_creation_and_close_time() {
        let now = 10 * 86_400;
        let issues = vec![
            issue("a", None, "task", "closed", 0, Some(2 * 86_400)),
            issue("b", None, "task", "open", 3 * 86_400, None),
        ];
        let b = burndown(&issues, 10, now);
        assert_eq!(b.scope, 2);
        assert_eq!(b.done, 1);
        assert_eq!(b.remaining, 1);
        assert_eq!(b.points.last().unwrap().remaining, 1);
        assert_eq!(b.points.last().unwrap().scope, 2);
        assert!(b.burn_per_day.is_some());
    }

    #[test]
    fn parent_creation_is_project_local() {
        let db = Db::in_memory().unwrap();
        db.ensure_project("a", "/a", "a").unwrap();
        db.ensure_project("b", "/b", "b").unwrap();
        let parent = db
            .create(
                &NewIssue {
                    title: "parent".into(),
                    project: Some("a".into()),
                    issue_type: "goal".into(),
                    ..Default::default()
                },
                now(),
            )
            .unwrap();
        let err = db
            .create(
                &NewIssue {
                    title: "child".into(),
                    project: Some("b".into()),
                    parent: Some(parent),
                    ..Default::default()
                },
                now(),
            )
            .unwrap_err();
        assert!(matches!(err, crate::db::Error::Bad(_)));
    }
}

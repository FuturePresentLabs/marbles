//! Read-only portfolio reports: goal rollups, goal trees, and burndown.
//!
//! These are deliberately views over the same issue graph, not another store. The CLI fetches
//! one `issues.list` page (local or HTTP) and computes everything here, so a report is identical
//! in local mode and against the hosted API.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::assertion_cache::{AssertionCache, AssertionFreshness, AssertionView};
use crate::identity::RemoteRegistry;
use crate::types::{Folder, Issue, STATUSES};

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

/// The wire shape of `projects.list`, including its optional folder assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub slug: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub folder: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRollup {
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub prefix: String,
    pub progress: Progress,
    pub ready: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_ready_age_secs: Option<i64>,
    pub stale: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_review_age_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_per_day: Option<f64>,
    pub goals: Vec<GoalSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderRollup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub projects: Vec<ProjectRollup>,
    pub progress: Progress,
    pub ready: i64,
    pub stale: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_per_day: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioTotals {
    pub projects: i64,
    pub progress: Progress,
    pub ready: i64,
    pub stale: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burn_per_day: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    pub generated_at: i64,
    pub days: i64,
    pub stale_days: i64,
    pub folders: Vec<FolderRollup>,
    pub unfiled: Vec<ProjectRollup>,
    pub totals: PortfolioTotals,
}

fn is_ready(issue: &Issue, statuses: &BTreeMap<&str, &str>, now: i64) -> bool {
    if issue.issue_type == "goal" || issue.status != "open" {
        return false;
    }
    if issue.available_at.is_some_and(|available| available > now) {
        return false;
    }
    if issue.assignee.is_some() && issue.expires_at.is_none_or(|expires| expires > now) {
        return false;
    }
    issue.dependencies.iter().all(|dep| {
        dep.depends_on_id
            .as_deref()
            .and_then(|id| statuses.get(id).copied())
            .is_none_or(|status| status == "closed")
    })
}

fn project_rollup(
    project: &ProjectInfo,
    issues: &[&Issue],
    statuses: &BTreeMap<&str, &str>,
    now: i64,
    days: i64,
    stale_days: i64,
) -> ProjectRollup {
    // Goals are containers, not executable work; they are shown separately below.
    let work: Vec<&Issue> = issues
        .iter()
        .copied()
        .filter(|issue| issue.issue_type != "goal")
        .collect();
    let ready: Vec<&Issue> = work
        .iter()
        .copied()
        .filter(|issue| is_ready(issue, statuses, now))
        .collect();
    let oldest_ready_age_secs = ready
        .iter()
        .map(|issue| (now - issue.created_at).max(0))
        .max();
    let stale_cutoff = now.saturating_sub(stale_days.saturating_mul(86_400));
    let stale = work
        .iter()
        .filter(|issue| matches!(issue.status.as_str(), "open" | "in_progress"))
        .filter(|issue| issue.updated_at < stale_cutoff)
        .count() as i64;
    let oldest_review_age_secs = work
        .iter()
        .filter(|issue| issue.status == "review")
        .map(|issue| (now - issue.updated_at).max(0))
        .max();
    let burn_per_day = burndown(
        &work.iter().copied().cloned().collect::<Vec<_>>(),
        days,
        now,
    )
    .burn_per_day;
    let mut goals = goal_summaries(
        &issues.iter().copied().cloned().collect::<Vec<_>>(),
        Some(&project.slug),
    );
    goals.retain(|goal| goal.status != "closed");
    ProjectRollup {
        slug: project.slug.clone(),
        folder: project.folder.clone(),
        prefix: project.prefix.clone(),
        progress: Progress::from_issues(&work),
        ready: ready.len() as i64,
        oldest_ready_age_secs,
        stale,
        oldest_review_age_secs,
        burn_per_day,
        goals,
    }
}

/// Build a folder/project/goal portfolio from one issue page. Ready work follows the same
/// rules as `mb ready`: open, past grace, not under a live claim, and every existing
/// dependency closed. Missing dependency rows are ignored, matching the SQL ready query.
pub fn portfolio(
    projects: &[ProjectInfo],
    folders: &[Folder],
    issues: &[Issue],
    now: i64,
    days: i64,
    stale_days: i64,
) -> Portfolio {
    let statuses: BTreeMap<&str, &str> = issues
        .iter()
        .map(|issue| (issue.id.as_str(), issue.status.as_str()))
        .collect();
    let mut by_project: BTreeMap<&str, Vec<&Issue>> = BTreeMap::new();
    for issue in issues {
        by_project
            .entry(issue.project.as_str())
            .or_default()
            .push(issue);
    }
    let mut rollups: Vec<ProjectRollup> = projects
        .iter()
        .map(|project| {
            let project_issues = by_project
                .get(project.slug.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            project_rollup(project, project_issues, &statuses, now, days, stale_days)
        })
        .collect();
    rollups.sort_by(|a, b| a.slug.cmp(&b.slug));

    let mut folder_rollups = Vec::new();
    for folder in folders {
        let in_folder: Vec<ProjectRollup> = rollups
            .iter()
            .filter(|project| project.folder.as_deref() == Some(folder.slug.as_str()))
            .cloned()
            .collect();
        if in_folder.is_empty() {
            continue;
        }
        let folder_issues: Vec<&Issue> = in_folder
            .iter()
            .flat_map(|project| by_project.get(project.slug.as_str()).into_iter().flatten())
            .copied()
            .filter(|issue| issue.issue_type != "goal")
            .collect();
        folder_rollups.push(FolderRollup {
            slug: Some(folder.slug.clone()),
            name: folder.name.clone(),
            parent: folder.parent.clone(),
            ready: in_folder.iter().map(|project| project.ready).sum(),
            stale: in_folder.iter().map(|project| project.stale).sum(),
            progress: Progress::from_issues(&folder_issues),
            burn_per_day: burndown(
                &folder_issues.iter().copied().cloned().collect::<Vec<_>>(),
                days,
                now,
            )
            .burn_per_day,
            projects: in_folder,
        });
    }
    folder_rollups.sort_by(|a, b| a.name.cmp(&b.name));

    let unfiled: Vec<ProjectRollup> = rollups
        .iter()
        .filter(|project| {
            project.folder.is_none()
                || !folders
                    .iter()
                    .any(|folder| Some(folder.slug.as_str()) == project.folder.as_deref())
        })
        .cloned()
        .collect();
    let all_issues: Vec<&Issue> = issues
        .iter()
        .filter(|issue| issue.issue_type != "goal")
        .collect();
    let totals = PortfolioTotals {
        projects: projects.len() as i64,
        progress: Progress::from_issues(&all_issues),
        ready: rollups.iter().map(|project| project.ready).sum(),
        stale: rollups.iter().map(|project| project.stale).sum(),
        burn_per_day: burndown(
            &issues
                .iter()
                .filter(|issue| issue.issue_type != "goal")
                .cloned()
                .collect::<Vec<_>>(),
            days,
            now,
        )
        .burn_per_day,
    };
    Portfolio {
        generated_at: now,
        days,
        stale_days,
        folders: folder_rollups,
        unfiled,
        totals,
    }
}

/// One pinned remote as it appears in the **Remotes workspace**.
///
/// This is a projection of local client state (`remotes.json` + the assertion cache). It is not a
/// project, issue, or folder row, and nothing in the projection can create one: a remote store is
/// listed here precisely so it never has to look like local work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteWorkspaceEntry {
    pub name: String,
    pub display_name: String,
    pub store_id: String,
    pub key_id: String,
    /// Locator only; trust comes from `store_id`/`key_id`, never from the endpoint.
    pub endpoint: String,
    /// Named freshness for the remote as a whole (see [`remote_workspace_freshness`]).
    pub freshness: AssertionFreshness,
    /// Newest observation we hold for any of this remote's references, when one verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    /// Cached references (remote issues) we hold for this store.
    pub references: usize,
    /// Why the remote is `unverifiable`, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The whole read-only Remotes workspace: pinned stores, their identities, and freshness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotesWorkspace {
    pub registry: String,
    /// False before the first pin; the view still succeeds and says so plainly.
    pub registry_present: bool,
    pub freshness_window_seconds: i64,
    pub remotes: Vec<RemoteWorkspaceEntry>,
}

/// Aggregate a remote's cached references into one named freshness.
///
/// The aggregate is deliberately fail-closed and never reports more health than the weakest
/// reference we hold:
///
/// * any `unverifiable` reference makes the remote `unverifiable`;
/// * any `unknown` reference (an unreachable observation, or no observation ever) makes the
///   remote `unknown` — an outage can never read as healthy;
/// * otherwise any `stale` reference makes it `stale`;
/// * only when every reference verifies inside the window is the remote `fresh`.
///
/// No references means `unknown`, never `fresh`/zero.
pub fn remote_workspace_freshness(views: &[AssertionView]) -> AssertionFreshness {
    let mut stale = false;
    let mut fresh = false;
    for view in views {
        match view.freshness {
            AssertionFreshness::Unverifiable => return AssertionFreshness::Unverifiable,
            AssertionFreshness::Unknown => return AssertionFreshness::Unknown,
            AssertionFreshness::Stale => stale = true,
            AssertionFreshness::Fresh => fresh = true,
        }
    }
    if stale {
        AssertionFreshness::Stale
    } else if fresh {
        AssertionFreshness::Fresh
    } else {
        AssertionFreshness::Unknown
    }
}

fn remote_entry(
    registry: &RemoteRegistry,
    cache: &AssertionCache,
    record: &crate::identity::RemoteRecord,
    now: i64,
) -> RemoteWorkspaceEntry {
    let views = cache.read_all(registry, &record.store_id, now);
    let freshness = remote_workspace_freshness(&views);
    let observed_at = views.iter().filter_map(AssertionView::observed_at).max();
    let problem = views.iter().find_map(|view| view.problem.clone());
    RemoteWorkspaceEntry {
        name: record.name.clone(),
        display_name: record.display_name.clone(),
        store_id: record.store_id.clone(),
        key_id: record.key_id.clone(),
        endpoint: record.endpoint.clone(),
        freshness,
        observed_at,
        references: views.len(),
        problem,
    }
}

/// A row in `mb ready`: the local issue, plus named reasons it cannot be claimed yet.
///
/// Remote issues never appear as rows. A local issue whose remote blocker cannot be satisfied
/// still appears — the edge is local state and removing it would hide real work — with the reason
/// named in [`Self::blocking_reasons`] instead of a silent omission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadinessRow {
    #[serde(flatten)]
    pub issue: Issue,
    /// Named reasons this local issue is not claimable (`remote: stale`, `remote: open`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocking_reasons: Vec<String>,
}

impl ReadinessRow {
    pub fn is_ready(&self) -> bool {
        self.blocking_reasons.is_empty()
    }
}

/// The strict verdict for one remote dependency edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteGate {
    /// A fresh, signature-verified assertion says the remote issue is `closed`.
    Satisfied,
    /// Anything else blocks, and the name says why.
    Blocked { reason: String },
}

/// Gate one re-verified remote assertion under the strict readiness rule.
///
/// Only `fresh` + a verified `closed` assertion satisfies a remote blocker. A `stale` assertion is
/// not good enough even when it says `closed`: an old claim about a remote issue cannot make local
/// work claimable. `unknown` (never observed, or an unreachable remote) and `unverifiable` (bad
/// signature, retired key, wrong store/issue) both block, as does a fresh non-`closed` state.
pub fn remote_gate(view: &AssertionView) -> RemoteGate {
    match view.freshness {
        AssertionFreshness::Fresh => match view.asserted_state() {
            Some("closed") => RemoteGate::Satisfied,
            Some(state) => RemoteGate::Blocked {
                reason: format!("remote: {state}"),
            },
            None => RemoteGate::Blocked {
                reason: "remote: unverifiable".to_string(),
            },
        },
        AssertionFreshness::Stale => RemoteGate::Blocked {
            reason: "remote: stale".to_string(),
        },
        AssertionFreshness::Unknown => RemoteGate::Blocked {
            reason: "remote: unknown".to_string(),
        },
        AssertionFreshness::Unverifiable => RemoteGate::Blocked {
            reason: "remote: unverifiable".to_string(),
        },
    }
}

/// Build the Remotes workspace from the registry and assertion cache.
///
/// A missing registry is an empty workspace (`registry_present = false`), not an error. A corrupt
/// registry is rejected by [`RemoteRegistry::load`] before this function runs, so the CLI fails
/// closed with a named error rather than showing a half-built view.
pub fn remotes_workspace(
    registry: &RemoteRegistry,
    cache: &AssertionCache,
    registry_path: &Path,
    now: i64,
) -> RemotesWorkspace {
    RemotesWorkspace {
        registry: registry_path.display().to_string(),
        registry_present: registry_path.exists(),
        freshness_window_seconds: cache.freshness_window_seconds(),
        remotes: registry
            .list()
            .iter()
            .map(|record| remote_entry(registry, cache, record, now))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertion_cache::{CachedAssertion, MissingReason};
    use crate::db::{Db, now};
    use crate::types::{Dependency, Folder, Issue, NewIssue};

    fn view(freshness: AssertionFreshness, state: Option<&str>) -> AssertionView {
        AssertionView {
            store_id: "store_a".into(),
            remote_issue_id: "issue-1".into(),
            freshness,
            assertion: state.map(|state| CachedAssertion {
                schema: "marbles.issue_assertion/1".into(),
                store_id: "store_a".into(),
                remote_issue_id: "issue-1".into(),
                state: state.into(),
                asserted_at: 1_000,
                signature: "sig".into(),
                key_id: "key-1".into(),
                observed_at: 1_000,
            }),
            missing: (freshness == AssertionFreshness::Unknown)
                .then_some(MissingReason::NeverObserved),
            problem: (freshness == AssertionFreshness::Unverifiable)
                .then(|| "bad signature".to_string()),
        }
    }

    #[test]
    fn remote_gate_accepts_only_fresh_closed() {
        assert_eq!(
            remote_gate(&view(AssertionFreshness::Fresh, Some("closed"))),
            RemoteGate::Satisfied
        );
        for (freshness, state, expected) in [
            (AssertionFreshness::Fresh, Some("open"), "remote: open"),
            (AssertionFreshness::Fresh, Some("blocked"), "remote: blocked"),
            (AssertionFreshness::Stale, Some("closed"), "remote: stale"),
            (AssertionFreshness::Unknown, None, "remote: unknown"),
            (
                AssertionFreshness::Unverifiable,
                None,
                "remote: unverifiable",
            ),
        ] {
            assert_eq!(
                remote_gate(&view(freshness, state)),
                RemoteGate::Blocked {
                    reason: expected.to_string()
                }
            );
        }
    }

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
    fn portfolio_counts_ready_work_and_excludes_goal_containers() {
        let now = 10 * 86_400;
        let mut ready = issue("t1", None, "task", "open", 0, None);
        ready.available_at = Some(0);
        let mut blocked = issue("t2", None, "task", "open", 0, None);
        blocked.available_at = Some(0);
        blocked.dependencies = vec![Dependency {
            depends_on_id: Some("t1".into()),
        }];
        let mut claimed = issue("t3", None, "task", "open", 0, None);
        claimed.available_at = Some(0);
        claimed.assignee = Some("agent".into());
        claimed.expires_at = Some(now + 1_000);
        let mut review = issue("r", None, "task", "review", 0, None);
        review.updated_at = 0;
        let closed = issue("c", None, "task", "closed", 0, Some(2 * 86_400));
        let goal = issue("g", None, "goal", "open", 0, None);
        let issues = vec![goal, ready, blocked, claimed, review, closed];
        let projects = vec![ProjectInfo {
            slug: "p".into(),
            root: "/p".into(),
            prefix: "p".into(),
            folder: Some("business".into()),
        }];
        let folders = vec![Folder {
            slug: "business".into(),
            name: "Business".into(),
            parent: None,
            created_at: 0,
        }];
        let report = portfolio(&projects, &folders, &issues, now, 10, 7);
        assert_eq!(report.folders.len(), 1);
        let project = &report.folders[0].projects[0];
        assert_eq!(project.progress.total, 5, "goal container is not work");
        assert_eq!(project.progress.open, 3);
        assert_eq!(project.progress.review, 1);
        assert_eq!(project.progress.closed, 1);
        assert_eq!(project.ready, 1);
        assert_eq!(project.stale, 3);
        assert_eq!(project.oldest_ready_age_secs, Some(10 * 86_400));
        assert_eq!(project.oldest_review_age_secs, Some(10 * 86_400));
        assert_eq!(project.goals.len(), 1);
        let burn = project.burn_per_day.unwrap();
        assert!((burn - 0.1).abs() < 0.001, "burn={burn}");
        assert_eq!(report.totals.ready, 1);
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

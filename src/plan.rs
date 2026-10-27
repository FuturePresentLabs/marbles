//! Plans as DAGs.
//!
//! A plan is not a new store object. It is a **view over marbles**: nodes are ordinary issues
//! carrying a small machine-readable metadata block, and edges are the ordinary `dep` rows. That
//! keeps one source of truth (the issue table), one claim race, and one company scope — a plan can
//! span projects without a second database, and an executor consumes the same `ready` semantics it
//! already understands.
//!
//! The metadata block is deliberately flat so it is queryable with JSON extraction on every
//! backend (`metadata_json` in SQLite, the `metadata` column in PostgreSQL):
//!
//! ```json
//! {
//!   "plan": "checkout-v2",          // plan membership (the DAG's name)
//!   "plan_key": "schema",           // stable key used by the plan spec's `deps`
//!   "done_means": "cargo test -p checkout schema",   // the COMMAND that proves the node done
//!   "repo": "checkout",             // which checkout the node edits
//!   "conflict_group": "db",         // nodes sharing a group must not run concurrently
//!   "acceptance": "migration applies on an empty database",
//!   "risk": "high",                 // low | medium | high
//!   "estimate": 45                  // minutes, feeds the Gantt and critical path
//! }
//! ```
//!
//! `mb plan show|gantt|ready|create` are thin renderers over this module, and the same functions
//! back `/v1/plans.*` so local and hosted Marbles compute identical answers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::db::{Error, Result, Store};
use crate::types::{Issue, NewIssue};

pub const META_PLAN: &str = "plan";
pub const META_KEY: &str = "plan_key";
pub const META_DONE: &str = "done_means";
pub const META_REPO: &str = "repo";
pub const META_GROUP: &str = "conflict_group";
pub const META_ACCEPTANCE: &str = "acceptance";
pub const META_RISK: &str = "risk";
pub const META_ESTIMATE: &str = "estimate";

/// Default node duration when the spec does not say. Thirty minutes is the agent-lease scale:
/// long enough to be a real unit of work, short enough that a plan of ten nodes is a day.
pub const DEFAULT_ESTIMATE_MINUTES: i64 = 30;

/// Statuses that keep a conflict group busy. `review` is intentionally *not* here: the code work
/// is finished and the PR is open, so a different node in the same group may start; the merge is
/// the reviewer's job, not the group's.
const RUNNING_STATUS: &str = "in_progress";

fn meta_str<'a>(issue: &'a Issue, key: &str) -> Option<&'a str> {
    issue.metadata.get(key).and_then(|v| v.as_str())
}

fn meta_i64(issue: &Issue, key: &str) -> Option<i64> {
    issue.metadata.get(key).and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

pub fn plan_name_of(issue: &Issue) -> Option<&str> {
    meta_str(issue, META_PLAN)
}

/// The estimate in minutes; never zero, so the Gantt and critical path stay meaningful for
/// nodes whose spec omitted one.
pub fn estimate_of(issue: &Issue) -> i64 {
    estimate_of_with_default(issue, DEFAULT_ESTIMATE_MINUTES).0
}

/// The numeric estimate carried in metadata, when it is usable. This is the raw field the scrum
/// card reports; the Gantt applies the scheduler's default when it is absent.
pub fn metadata_estimate(issue: &Issue) -> Option<i64> {
    meta_i64(issue, META_ESTIMATE).filter(|minutes| *minutes > 0)
}

/// Resolve one node's duration and where it came from. `default` is explicit so a view can ask
/// for its own estimate without changing plan semantics.
pub fn estimate_of_with_default(issue: &Issue, default: i64) -> (i64, &'static str) {
    match metadata_estimate(issue) {
        Some(minutes) => (minutes, "metadata"),
        None => (default.max(1), "default"),
    }
}

fn conflict_group(issue: &Issue) -> Option<&str> {
    meta_str(issue, META_GROUP).filter(|g| !g.is_empty())
}

fn is_live_claim(issue: &Issue, now: i64) -> bool {
    issue
        .assignee
        .as_ref()
        .is_some_and(|_| issue.expires_at.is_some_and(|e| e > now))
}

fn is_running(issue: &Issue, now: i64) -> bool {
    issue.status == RUNNING_STATUS && (issue.assignee.is_none() || is_live_claim(issue, now))
}

// ---------------------------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------------------------

/// One node of a plan spec: the reviewable draft, before any issue exists.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeSpec {
    /// Stable local key; `deps` and reports refer to nodes by this, never by issue id (ids do not
    /// exist until the plan is written).
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Project (tracker store) this node lives in; defaults to the plan's project. A plan may
    /// span projects because membership is a metadata predicate, not a table.
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub conflict_group: String,
    /// low | medium | high (critical is accepted as the top of the scale).
    #[serde(default)]
    pub risk: String,
    /// Minutes.
    #[serde(default)]
    pub estimate: Option<i64>,
    /// The command that proves the node done — the executor runs this, not a human reading prose.
    #[serde(default)]
    pub done_means: String,
    #[serde(default)]
    pub acceptance: String,
    /// Keys of nodes this one waits for.
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// A plan spec as authored on disk (TOML or JSON), before project/plan defaults are resolved.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub nodes: Vec<NodeSpec>,
}

/// A spec with its plan name and project resolved from flags, file, or project discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPlan {
    pub plan: String,
    pub project: String,
    pub nodes: Vec<NodeSpec>,
}

/// TOML (the repo's config language) or JSON, detected by the first non-whitespace byte.
pub fn parse_spec(text: &str) -> std::result::Result<PlanSpec, String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        serde_json::from_str(trimmed).map_err(|e| format!("parsing JSON plan spec: {e}"))
    } else {
        toml::from_str(trimmed).map_err(|e| format!("parsing TOML plan spec: {e}"))
    }
}

fn validate_risk(risk: &str) -> bool {
    matches!(risk, "" | "low" | "medium" | "high" | "critical")
}

/// Structural validation, run before the first issue is written. A plan that cannot be written
/// atomically is still worth reviewing, so every problem is reported, not just the first.
pub fn validate(plan: &ResolvedPlan) -> std::result::Result<(), Vec<String>> {
    let mut problems = Vec::new();
    if plan.plan.trim().is_empty() {
        problems.push("plan name is empty".into());
    }
    if plan.project.trim().is_empty() {
        problems.push("project is empty; pass --project or run inside a project".into());
    }
    if plan.nodes.is_empty() {
        problems.push("plan has no nodes".into());
    }
    let mut keys = BTreeSet::new();
    for node in &plan.nodes {
        if node.key.trim().is_empty() {
            problems.push(format!("node {:?} has an empty key", node.title));
        } else if !keys.insert(node.key.clone()) {
            problems.push(format!("duplicate node key {:?}", node.key));
        }
        if node.title.trim().is_empty() {
            problems.push(format!("node {:?} has an empty title", node.key));
        }
        if !validate_risk(&node.risk) {
            problems.push(format!(
                "node {:?} has risk {:?} (expected low, medium, high, or critical)",
                node.key, node.risk
            ));
        }
        if node.estimate.is_some_and(|estimate| estimate <= 0) {
            problems.push(format!("node {:?} has a non-positive estimate", node.key));
        }
    }
    for node in &plan.nodes {
        for dep in &node.deps {
            if dep == &node.key {
                problems.push(format!("node {:?} depends on itself", node.key));
            } else if !keys.contains(dep) {
                problems.push(format!(
                    "node {:?} depends on unknown key {:?}",
                    node.key, dep
                ));
            }
        }
    }
    // Cycle check over the spec keys.
    let by_key: BTreeMap<&str, &NodeSpec> =
        plan.nodes.iter().map(|n| (n.key.as_str(), n)).collect();
    let mut color: HashMap<&str, u8> = HashMap::new();
    fn visit<'a>(
        key: &'a str,
        by_key: &BTreeMap<&'a str, &'a NodeSpec>,
        color: &mut HashMap<&'a str, u8>,
        problems: &mut Vec<String>,
    ) {
        match color.get(key).copied() {
            Some(1) => {
                problems.push(format!("dependency cycle at node {key:?}"));
                return;
            }
            Some(2) => return,
            _ => {}
        }
        color.insert(key, 1);
        if let Some(node) = by_key.get(key) {
            for dep in &node.deps {
                if by_key.contains_key(dep.as_str()) {
                    visit(dep.as_str(), by_key, color, problems);
                }
            }
        }
        color.insert(key, 2);
    }
    for node in &plan.nodes {
        visit(node.key.as_str(), &by_key, &mut color, &mut problems);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn metadata_for(plan: &ResolvedPlan, node: &NodeSpec) -> serde_json::Value {
    let mut meta = serde_json::Map::new();
    meta.insert(META_PLAN.into(), serde_json::json!(plan.plan));
    meta.insert(META_KEY.into(), serde_json::json!(node.key));
    if !node.done_means.is_empty() {
        meta.insert(META_DONE.into(), serde_json::json!(node.done_means));
    }
    if !node.repo.is_empty() {
        meta.insert(META_REPO.into(), serde_json::json!(node.repo));
    }
    if !node.conflict_group.is_empty() {
        meta.insert(META_GROUP.into(), serde_json::json!(node.conflict_group));
    }
    if !node.acceptance.is_empty() {
        meta.insert(META_ACCEPTANCE.into(), serde_json::json!(node.acceptance));
    }
    if !node.risk.is_empty() {
        meta.insert(META_RISK.into(), serde_json::json!(node.risk));
    }
    meta.insert(
        META_ESTIMATE.into(),
        serde_json::json!(node.estimate.unwrap_or(DEFAULT_ESTIMATE_MINUTES).max(1)),
    );
    serde_json::Value::Object(meta)
}

// ---------------------------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------------------------

/// The plan DAG loaded from stored issues. `status_of` covers the whole store (not just the plan)
/// so blockers that point outside the plan still gate the frontier correctly.
pub struct PlanGraph {
    pub plan: String,
    nodes: Vec<Issue>,
    status_of: HashMap<String, String>,
    default_estimate: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub id: String,
    pub key: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    pub estimate: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_means: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<String>,
    /// Dependencies that are themselves plan nodes (ids).
    pub deps: Vec<String>,
    /// Dependencies outside the plan (ids); their status still gates readiness.
    pub external_deps: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub child: String,
    pub parent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanView {
    pub plan: String,
    pub nodes: Vec<NodeView>,
    pub edges: Vec<Edge>,
    /// Distinct non-empty repos, sorted (the plan's blast radius).
    pub repos: Vec<String>,
    /// Distinct non-empty conflict groups, sorted.
    pub conflict_groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedNode {
    pub key: String,
    pub id: String,
    pub title: String,
    pub project: String,
    pub deps: Vec<String>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateReport {
    pub plan: String,
    pub project: String,
    pub nodes: Vec<CreatedNode>,
    pub edges: Vec<Edge>,
}

impl PlanGraph {
    pub fn nodes(&self) -> &[Issue] {
        &self.nodes
    }

    /// Build a graph over an arbitrary issue scope. The plan loader uses this for named plans;
    /// the view API uses it for project/folder scopes with an explicit default estimate.
    pub fn scoped(
        name: impl Into<String>,
        nodes: Vec<Issue>,
        status_of: HashMap<String, String>,
        default_estimate: i64,
    ) -> Result<Self> {
        let graph = Self {
            plan: name.into(),
            nodes,
            status_of,
            default_estimate: default_estimate.max(1),
        };
        // A read must fail closed: a hand-edited cycle never produces a partial chart.
        graph.topo()?;
        Ok(graph)
    }

    fn index(&self) -> HashMap<&str, &Issue> {
        self.nodes.iter().map(|i| (i.id.as_str(), i)).collect()
    }

    fn dep_ids(issue: &Issue) -> Vec<&str> {
        issue
            .dependencies
            .iter()
            .filter_map(|d| d.depends_on_id.as_deref())
            .collect()
    }

    fn internal_deps<'a>(&'a self, issue: &'a Issue) -> Vec<&'a str> {
        let index = self.index();
        Self::dep_ids(issue)
            .into_iter()
            .filter(|id| index.contains_key(id))
            .collect()
    }

    fn external_deps<'a>(&'a self, issue: &'a Issue) -> Vec<&'a str> {
        let index = self.index();
        Self::dep_ids(issue)
            .into_iter()
            .filter(|id| !index.contains_key(id))
            .collect()
    }

    /// Deterministic topological order (dependencies before dependents). Ties fall back to the
    /// store's priority/creation ordering. A cycle is a broken plan, reported rather than looped.
    fn topo(&self) -> Result<Vec<usize>> {
        let mut indegree = vec![0usize; self.nodes.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        let index: HashMap<&str, usize> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        for (i, node) in self.nodes.iter().enumerate() {
            for dep in Self::dep_ids(node) {
                if let Some(&parent) = index.get(dep) {
                    indegree[i] += 1;
                    dependents[parent].push(i);
                }
            }
        }
        // Ready queue as a BTreeSet keeps output stable across runs.
        let mut ready: BTreeSet<usize> = (0..self.nodes.len())
            .filter(|i| indegree[*i] == 0)
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(i) = ready.pop_first() {
            order.push(i);
            for &child in &dependents[i] {
                indegree[child] -= 1;
                if indegree[child] == 0 {
                    ready.insert(child);
                }
            }
        }
        if order.len() != self.nodes.len() {
            return Err(Error::Bad(format!(
                "plan {:?} has a dependency cycle among its nodes",
                self.plan
            )));
        }
        Ok(order)
    }

    fn node_view(&self, issue: &Issue) -> NodeView {
        NodeView {
            id: issue.id.clone(),
            key: meta_str(issue, META_KEY)
                .unwrap_or(issue.id.as_str())
                .to_owned(),
            title: issue.title.clone(),
            status: issue.status.clone(),
            repo: meta_str(issue, META_REPO).map(str::to_owned),
            conflict_group: conflict_group(issue).map(str::to_owned),
            risk: meta_str(issue, META_RISK).map(str::to_owned),
            estimate: estimate_of_with_default(issue, self.default_estimate).0,
            done_means: meta_str(issue, META_DONE).map(str::to_owned),
            acceptance: meta_str(issue, META_ACCEPTANCE).map(str::to_owned),
            deps: self
                .internal_deps(issue)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            external_deps: self
                .external_deps(issue)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            assignee: issue.assignee.clone(),
            priority: Some(issue.priority),
        }
    }

    pub fn view(&self) -> Result<PlanView> {
        let order = self.topo()?;
        let mut nodes = Vec::with_capacity(order.len());
        let mut edges = Vec::new();
        let mut repos = BTreeSet::new();
        let mut groups = BTreeSet::new();
        for &i in &order {
            let issue = &self.nodes[i];
            let view = self.node_view(issue);
            for parent in &view.deps {
                edges.push(Edge {
                    child: issue.id.clone(),
                    parent: parent.clone(),
                });
            }
            if let Some(repo) = &view.repo {
                repos.insert(repo.clone());
            }
            if let Some(group) = &view.conflict_group {
                groups.insert(group.clone());
            }
            nodes.push(view);
        }
        Ok(PlanView {
            plan: self.plan.clone(),
            nodes,
            edges,
            repos: repos.into_iter().collect(),
            conflict_groups: groups.into_iter().collect(),
        })
    }
}

/// Load a plan: every issue in the store carrying `metadata.plan == plan_name`.
///
/// Plan membership is a metadata predicate, not a separate table, so the hosted store's company
/// scoping already applies and the same call works for SQLite and PostgreSQL.
pub fn load(store: &dyn Store, plan_name: &str) -> Result<PlanGraph> {
    let all = store.list(None, true, None)?;
    let mut status_of = HashMap::new();
    let mut nodes = Vec::new();
    for issue in all {
        status_of.insert(issue.id.clone(), issue.status.clone());
        if plan_name_of(&issue) == Some(plan_name) {
            nodes.push(issue);
        }
    }
    if nodes.is_empty() {
        return Err(Error::NotFound(format!("plan {plan_name}")));
    }
    // Stable plan order: priority, then authoring order (created_at), then the spec key. The store
    // orders by id only after these ties, and ids are random, so tie-break on the key here.
    nodes.sort_by_key(|i| {
        (
            i.priority,
            i.created_at,
            meta_str(i, META_KEY).unwrap_or(i.id.as_str()).to_owned(),
            i.id.clone(),
        )
    });
    let graph = PlanGraph {
        plan: plan_name.to_owned(),
        nodes,
        status_of,
        default_estimate: DEFAULT_ESTIMATE_MINUTES,
    };
    // Validate the DAG as loaded, so a hand-edited cycle surfaces on `show` rather than hanging
    // an executor that assumes a topological frontier.
    graph.topo()?;
    Ok(graph)
}

fn status<'a>(graph: &'a PlanGraph, id: &'a str) -> &'a str {
    graph
        .status_of
        .get(id)
        .map(String::as_str)
        .unwrap_or("open")
}

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

/// A reviewable draft: exactly what `create --dry-run` prints and what `create` then writes.
pub fn draft(plan: &ResolvedPlan) -> std::result::Result<CreateReport, Vec<String>> {
    validate(plan)?;
    let ids: BTreeMap<&str, String> = plan
        .nodes
        .iter()
        .map(|n| (n.key.as_str(), format!("<new:{}>", n.key)))
        .collect();
    let mut nodes = Vec::with_capacity(plan.nodes.len());
    let mut edges = Vec::new();
    for node in &plan.nodes {
        for dep in &node.deps {
            edges.push(Edge {
                child: ids[node.key.as_str()].clone(),
                parent: ids[dep.as_str()].clone(),
            });
        }
        nodes.push(CreatedNode {
            key: node.key.clone(),
            id: ids[node.key.as_str()].clone(),
            title: node.title.clone(),
            project: if node.project.is_empty() {
                plan.project.clone()
            } else {
                node.project.clone()
            },
            deps: node.deps.clone(),
            metadata: metadata_for(plan, node),
        });
    }
    Ok(CreateReport {
        plan: plan.plan.clone(),
        project: plan.project.clone(),
        nodes,
        edges,
    })
}

/// Materialize a plan: create one issue per node, then wire the dependency edges.
///
/// Nodes are created immediately swarm-visible (`available_at = at`): authoring a plan *is* the
/// human review step, so the usual creation grace would only delay the frontier the executor
/// consumes. Refusing to extend a plan that already exists keeps `plan create` idempotent-ish in
/// the useful direction: a re-run is an error you can see, not a half-duplicated DAG.
pub fn create(
    store: &dyn Store,
    plan: &ResolvedPlan,
    at: i64,
    actor: &str,
) -> std::result::Result<CreateReport, String> {
    validate(plan).map_err(|problems| problems.join("; "))?;
    let existing = store
        .list(None, true, None)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|i| plan_name_of(i) == Some(plan.plan.as_str()))
        .count();
    if existing > 0 {
        return Err(format!(
            "plan {:?} already has {existing} node(s); choose a new name or close the old plan",
            plan.plan
        ));
    }
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut created = Vec::new();
    for node in &plan.nodes {
        let metadata = metadata_for(plan, node);
        let spec = NewIssue {
            title: node.title.clone(),
            description: node.description.clone(),
            issue_type: "task".into(),
            priority: node.priority.unwrap_or(2),
            labels: node.labels.clone(),
            parent: None,
            id: None,
            project: Some(if node.project.is_empty() {
                plan.project.clone()
            } else {
                node.project.clone()
            }),
            available_at: Some(at),
            created_by: Some(actor.to_owned()),
            metadata: Some(metadata.clone()),
            external_ref: None,
        };
        let id = store.create(&spec, at).map_err(|e| e.to_string())?;
        ids.insert(node.key.clone(), id.clone());
        created.push(CreatedNode {
            key: node.key.clone(),
            id,
            title: node.title.clone(),
            project: if node.project.is_empty() {
                plan.project.clone()
            } else {
                node.project.clone()
            },
            deps: node.deps.clone(),
            metadata,
        });
    }
    let mut edges = Vec::new();
    for node in &plan.nodes {
        let child = ids[&node.key].clone();
        for dep in &node.deps {
            let parent = ids[dep].clone();
            store
                .add_dep(&child, &parent, actor, at)
                .map_err(|e| e.to_string())?;
            edges.push(Edge {
                child: child.clone(),
                parent,
            });
        }
    }
    Ok(CreateReport {
        plan: plan.plan.clone(),
        project: plan.project.clone(),
        nodes: created,
        edges,
    })
}

// ---------------------------------------------------------------------------------------------
// Gantt / critical path
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GanttNode {
    pub id: String,
    pub key: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_group: Option<String>,
    pub estimate: i64,
    /// `metadata` when the issue carried an estimate, `default` when the scheduler supplied one.
    pub estimate_source: String,
    /// Minutes from plan start (`started_at`).
    pub start_min: i64,
    pub finish_min: i64,
    /// Completion slack in minutes; critical nodes have 0.
    pub slack_min: i64,
    pub critical: bool,
    pub start_at: i64,
    pub finish_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gantt {
    pub plan: String,
    pub started_at: i64,
    pub nodes: Vec<GanttNode>,
    /// Node ids on the longest dependency chain.
    pub critical_path: Vec<String>,
    pub critical_path_minutes: i64,
    /// Total makespan (last finish).
    pub total_minutes: i64,
    /// Nodes currently `in_progress` with a live claim (the executor's running set).
    pub current_concurrency: usize,
    /// Peak number of nodes whose scheduled intervals overlap.
    pub peak_concurrency: usize,
}

/// Earliest-start schedule over estimates, then a backward pass for slack. The critical path is
/// the zero-slack chain (equivalently the longest chain): slipping any node on it slips the plan.
pub fn gantt(graph: &PlanGraph, now: i64) -> Result<Gantt> {
    let order = graph.topo()?;
    let index: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();
    let n = graph.nodes.len();
    let estimates: Vec<(i64, &'static str)> = graph
        .nodes
        .iter()
        .map(|issue| estimate_of_with_default(issue, graph.default_estimate))
        .collect();
    let durations: Vec<i64> = estimates.iter().map(|(minutes, _)| *minutes).collect();
    let mut start = vec![0i64; n];
    let mut finish = vec![0i64; n];
    for &i in &order {
        let mut s = 0;
        for dep in PlanGraph::dep_ids(&graph.nodes[i]) {
            if let Some(&p) = index.get(dep) {
                s = s.max(finish[p]);
            }
        }
        start[i] = s;
        finish[i] = s + durations[i];
    }
    let total = finish.iter().copied().max().unwrap_or(0);
    // Backward pass: latest finish before the tightest dependent must start (or the makespan).
    let mut latest_finish = vec![total; n];
    for &i in order.iter().rev() {
        let latest_start = latest_finish[i] - durations[i];
        for dep in PlanGraph::dep_ids(&graph.nodes[i]) {
            if let Some(&p) = index.get(dep) {
                latest_finish[p] = latest_finish[p].min(latest_start);
            }
        }
    }
    let slack: Vec<i64> = (0..n)
        .map(|i| (latest_finish[i] - finish[i]).max(0))
        .collect();
    // Reconstruct the critical chain: from a zero-slack sink, follow the zero-slack dependency
    // with the latest finish.
    let mut critical_path: Vec<String> = Vec::new();
    if let Some(mut cursor) = (0..n).filter(|i| slack[*i] == 0).max_by_key(|i| finish[*i]) {
        let mut chain = vec![cursor];
        loop {
            let parent = PlanGraph::dep_ids(&graph.nodes[cursor])
                .into_iter()
                .filter_map(|d| index.get(d).copied())
                .filter(|p| slack[*p] == 0)
                .max_by_key(|p| finish[*p]);
            match parent {
                Some(p) => {
                    chain.push(p);
                    cursor = p;
                }
                None => break,
            }
        }
        chain.reverse();
        critical_path = chain
            .into_iter()
            .map(|i| graph.nodes[i].id.clone())
            .collect();
    }
    // Peak overlap of [start, finish) intervals.
    let mut events: Vec<(i64, i64)> = Vec::new();
    for i in 0..n {
        events.push((start[i], 1));
        events.push((finish[i], -1));
    }
    events.sort_by_key(|(t, delta)| (*t, *delta));
    let mut running = 0i64;
    let mut peak = 0i64;
    for (_, delta) in events {
        running += delta;
        peak = peak.max(running);
    }
    let nodes = (0..n)
        .map(|i| {
            let issue = &graph.nodes[i];
            GanttNode {
                id: issue.id.clone(),
                key: meta_str(issue, META_KEY)
                    .unwrap_or(issue.id.as_str())
                    .to_owned(),
                title: issue.title.clone(),
                status: issue.status.clone(),
                repo: meta_str(issue, META_REPO).map(str::to_owned),
                conflict_group: conflict_group(issue).map(str::to_owned),
                estimate: durations[i],
                estimate_source: estimates[i].1.to_owned(),
                start_min: start[i],
                finish_min: finish[i],
                slack_min: slack[i],
                critical: slack[i] == 0,
                start_at: now + start[i] * 60,
                finish_at: now + finish[i] * 60,
            }
        })
        .collect();
    let current_concurrency = graph
        .nodes
        .iter()
        .filter(|issue| is_running(issue, now))
        .count();
    Ok(Gantt {
        plan: graph.plan.clone(),
        started_at: now,
        nodes,
        critical_path_minutes: critical_path
            .iter()
            .filter_map(|id| index.get(id.as_str()))
            .map(|i| durations[*i])
            .sum(),
        critical_path,
        total_minutes: total,
        current_concurrency,
        peak_concurrency: peak as usize,
    })
}

/// The scope a generalized Gantt view was requested over. Nulls mean "not filtered".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GanttScope {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
}

/// A Gantt chart over an ordinary issue scope. It flattens [`Gantt`] so existing plan consumers
/// keep the same node/critical-path shape, then adds the scope and estimate policy that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GanttView {
    #[serde(flatten)]
    pub gantt: Gantt,
    pub scope: GanttScope,
    pub default_estimate_min: i64,
}

/// Schedule a plan or an ordinary issue scope through the same scheduler as `plans.gantt`.
///
/// Goals are containers and never become work cards. Closed issues are excluded unless
/// `include_closed` is explicit. `all_issues` supplies dependency statuses outside the selected
/// scope, so an external blocker still gates readiness; a dependency missing from the store is
/// simply absent (the Gantt cannot schedule an edge it has no row for).
pub fn gantt_scoped(
    all_issues: &[Issue],
    selected: Vec<Issue>,
    scope: GanttScope,
    include_closed: bool,
    default_estimate_min: i64,
    now: i64,
) -> Result<GanttView> {
    let status_of: HashMap<String, String> = all_issues
        .iter()
        .map(|issue| (issue.id.clone(), issue.status.clone()))
        .collect();
    let mut nodes: Vec<Issue> = selected
        .into_iter()
        .filter(|issue| issue.issue_type != "goal")
        .filter(|issue| include_closed || issue.status != "closed")
        .collect();
    nodes.sort_by_key(|i| {
        (
            i.priority,
            i.created_at,
            meta_str(i, META_KEY).unwrap_or(i.id.as_str()).to_owned(),
            i.id.clone(),
        )
    });
    let name = scope
        .plan
        .clone()
        .or_else(|| scope.project.clone())
        .or_else(|| scope.folder.clone())
        .unwrap_or_else(|| "all".into());
    let graph = PlanGraph::scoped(name, nodes, status_of, default_estimate_min)?;
    Ok(GanttView {
        gantt: gantt(&graph, now)?,
        scope,
        default_estimate_min: default_estimate_min.max(1),
    })
}

// ---------------------------------------------------------------------------------------------
// Ready frontier
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockedNode {
    pub id: String,
    pub key: String,
    pub title: String,
    /// `deps` | `conflict_group` | `claim` | `not_open` | `grace`
    pub reason: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyView {
    pub plan: String,
    pub ready: Vec<NodeView>,
    /// Eligible on dependencies but waiting for a slot, plus nodes still gated by deps/claims.
    pub blocked: Vec<BlockedNode>,
    /// Conflict groups with a node currently running.
    pub busy_groups: BTreeMap<String, String>,
    pub total_nodes: usize,
}

/// The parallel frontier the executor consumes: deps closed, no live foreign claim, and no node
/// in the same conflict group already running. At most one node per group is offered, so a naive
/// dispatcher that claims everything in this list cannot race two same-group edits.
pub fn ready(graph: &PlanGraph, now: i64) -> Result<ReadyView> {
    let order = graph.topo()?;
    let views: Vec<NodeView> = graph.nodes.iter().map(|i| graph.node_view(i)).collect();
    let mut busy_groups: BTreeMap<String, String> = BTreeMap::new();
    for node in &views {
        if node.status == RUNNING_STATUS
            && let Some(group) = &node.conflict_group
        {
            busy_groups.insert(group.clone(), node.id.clone());
        }
    }
    let mut eligible = Vec::new();
    let mut blocked = Vec::new();
    // Walk in topo order so the frontier is stable; then order candidates by the store's priority.
    for &i in &order {
        let node = &views[i];
        if node.status != "open" {
            blocked.push(BlockedNode {
                id: node.id.clone(),
                key: node.key.clone(),
                title: node.title.clone(),
                reason: "not_open".into(),
                detail: format!("status is {}", node.status),
            });
            continue;
        }
        let issue = &graph.nodes[i];
        if is_live_claim(issue, now) {
            blocked.push(BlockedNode {
                id: node.id.clone(),
                key: node.key.clone(),
                title: node.title.clone(),
                reason: "claim".into(),
                detail: format!("claimed by {}", node.assignee.as_deref().unwrap_or("?")),
            });
            continue;
        }
        if issue.available_at.is_some_and(|at| at > now) {
            blocked.push(BlockedNode {
                id: node.id.clone(),
                key: node.key.clone(),
                title: node.title.clone(),
                reason: "grace".into(),
                detail: format!("available in {}s", issue.available_at.unwrap_or(now) - now),
            });
            continue;
        }
        let open_deps: Vec<&str> = node
            .deps
            .iter()
            .chain(node.external_deps.iter())
            .map(String::as_str)
            .filter(|dep| status(graph, dep) != "closed")
            .collect();
        if !open_deps.is_empty() {
            blocked.push(BlockedNode {
                id: node.id.clone(),
                key: node.key.clone(),
                title: node.title.clone(),
                reason: "deps".into(),
                detail: format!("waits on {}", open_deps.join(", ")),
            });
            continue;
        }
        eligible.push(i);
    }
    eligible.sort_by_key(|i| {
        let issue = &graph.nodes[*i];
        let key = meta_str(issue, META_KEY).unwrap_or(issue.id.as_str());
        // Plan order (key) before the random id keeps a same-second plan creation deterministic.
        (
            issue.priority,
            issue.created_at,
            key.to_owned(),
            issue.id.clone(),
        )
    });
    let mut chosen_groups: BTreeSet<String> = BTreeSet::new();
    let mut ready_nodes = Vec::new();
    for i in eligible {
        let node = &views[i];
        if let Some(group) = &node.conflict_group {
            if let Some(running) = busy_groups.get(group) {
                blocked.push(BlockedNode {
                    id: node.id.clone(),
                    key: node.key.clone(),
                    title: node.title.clone(),
                    reason: "conflict_group".into(),
                    detail: format!("group {group:?} busy with running node {running}"),
                });
                continue;
            }
            if chosen_groups.contains(group) {
                blocked.push(BlockedNode {
                    id: node.id.clone(),
                    key: node.key.clone(),
                    title: node.title.clone(),
                    reason: "conflict_group".into(),
                    detail: format!("group {group:?} already has a node in this frontier"),
                });
                continue;
            }
            chosen_groups.insert(group.clone());
        }
        ready_nodes.push(node.clone());
    }
    Ok(ReadyView {
        plan: graph.plan.clone(),
        ready: ready_nodes,
        blocked,
        busy_groups,
        total_nodes: views.len(),
    })
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn short(id: &str, key: &str) -> String {
    if key != id {
        format!("{key} ({id})")
    } else {
        id.to_owned()
    }
}

fn node_label(id: &str, key: &str) -> String {
    if key != id {
        format!("{id} [{key}]")
    } else {
        id.to_owned()
    }
}

fn minutes(value: i64) -> String {
    if value >= 120 {
        format!("{}h{:02}", value / 60, value % 60)
    } else {
        format!("{value}m")
    }
}

pub fn render_plan(view: &PlanView) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "plan {:?} — {} node(s), {} repo(s), {} conflict group(s)\n",
        view.plan,
        view.nodes.len(),
        view.repos.len(),
        view.conflict_groups.len()
    ));
    if !view.repos.is_empty() {
        out.push_str(&format!("repos: {}\n", view.repos.join(", ")));
    }
    if !view.conflict_groups.is_empty() {
        out.push_str(&format!("groups: {}\n", view.conflict_groups.join(", ")));
    }
    out.push('\n');
    let find = |id: &str| {
        view.nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.key.clone())
            .unwrap_or_else(|| id.to_owned())
    };
    for node in &view.nodes {
        let mut flags = Vec::new();
        if let Some(group) = &node.conflict_group {
            flags.push(format!("group:{group}"));
        }
        if let Some(repo) = &node.repo {
            flags.push(format!("repo:{repo}"));
        }
        if let Some(risk) = &node.risk {
            flags.push(format!("risk:{risk}"));
        }
        flags.push(format!("est:{}", minutes(node.estimate)));
        out.push_str(&format!(
            "{}  [{}]  {}\n",
            short(&node.id, &node.key),
            node.status,
            node.title
        ));
        out.push_str(&format!("    {}\n", flags.join("  ")));
        if !node.deps.is_empty() {
            out.push_str(&format!(
                "    waits on: {}\n",
                node.deps
                    .iter()
                    .map(|d| find(d))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !node.external_deps.is_empty() {
            out.push_str(&format!(
                "    external blockers: {}\n",
                node.external_deps.join(", ")
            ));
        }
        if let Some(done) = &node.done_means {
            out.push_str(&format!("    done-means: {done}\n"));
        }
        if let Some(acceptance) = &node.acceptance {
            out.push_str(&format!("    acceptance: {acceptance}\n"));
        }
    }
    out
}

pub fn render_gantt(gantt: &Gantt) -> String {
    let key_of = |id: &str| {
        gantt
            .nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.key.clone())
            .unwrap_or_else(|| id.to_owned())
    };
    let mut out = String::new();
    out.push_str(&format!(
        "plan {:?} — makespan {}, {} node(s), peak concurrency {}, running now {}\n",
        gantt.plan,
        minutes(gantt.total_minutes),
        gantt.nodes.len(),
        gantt.peak_concurrency,
        gantt.current_concurrency
    ));
    out.push_str(&format!(
        "critical path ({}): {}\n\n",
        minutes(gantt.critical_path_minutes),
        gantt
            .critical_path
            .iter()
            .map(|id| key_of(id))
            .collect::<Vec<_>>()
            .join(" → ")
    ));
    out.push_str(&format!(
        "{:<20} {:<12} {:<10} {:>6} {:>7} {:>7} {:>6} {}\n",
        "NODE", "STATUS", "GROUP", "EST", "START", "FINISH", "SLACK", "CRIT"
    ));
    for node in &gantt.nodes {
        out.push_str(&format!(
            "{:<20} {:<12} {:<10} {:>6} {:>7} {:>7} {:>6} {}\n",
            node.key,
            node.status,
            node.conflict_group.as_deref().unwrap_or("-"),
            minutes(node.estimate),
            minutes(node.start_min),
            minutes(node.finish_min),
            minutes(node.slack_min),
            if node.critical { "yes" } else { "" }
        ));
    }
    out
}

pub fn render_ready(view: &ReadyView) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "plan {:?} — {} ready / {} node(s)\n",
        view.plan,
        view.ready.len(),
        view.total_nodes
    ));
    if !view.busy_groups.is_empty() {
        out.push_str(&format!(
            "busy conflict groups: {}\n",
            view.busy_groups
                .iter()
                .map(|(group, id)| format!("{group} → {id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for node in &view.ready {
        out.push_str(&format!(
            "  {}  {}{}  {}\n",
            node_label(&node.id, &node.key),
            node.conflict_group
                .as_ref()
                .map(|g| format!("[{g}] "))
                .unwrap_or_default(),
            node.title,
            node.done_means
                .as_ref()
                .map(|d| format!("— done-means: {d}"))
                .unwrap_or_default()
        ));
    }
    if view.ready.is_empty() {
        out.push_str("  (nothing eligible)\n");
    }
    let gated: Vec<&BlockedNode> = view
        .blocked
        .iter()
        .filter(|b| b.reason != "not_open")
        .collect();
    if !gated.is_empty() {
        out.push_str("\nwaiting:\n");
        for node in gated {
            out.push_str(&format!(
                "  {}  {} — {}: {}\n",
                node_label(&node.id, &node.key),
                node.title,
                node.reason,
                node.detail
            ));
        }
    }
    out
}

pub fn render_create(report: &CreateReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "plan {:?} — {} node(s) in project {}, {} edge(s)\n",
        report.plan,
        report.nodes.len(),
        report.project,
        report.edges.len()
    ));
    for node in &report.nodes {
        out.push_str(&format!("  {}  {}  {}\n", node.id, node.key, node.title));
        for dep in &node.deps {
            out.push_str(&format!("      waits on {dep}\n"));
        }
    }
    out
}

pub fn render_draft(report: &CreateReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "draft plan {:?} (project {}, {} node(s), {} edge(s)) — not written\n",
        report.plan,
        report.project,
        report.nodes.len(),
        report.edges.len()
    ));
    for node in &report.nodes {
        let meta = &node.metadata;
        out.push_str(&format!("  {}  {}\n", node.key, node.title));
        for key in [
            META_REPO,
            META_GROUP,
            META_RISK,
            META_DONE,
            META_ACCEPTANCE,
            META_ESTIMATE,
        ] {
            if let Some(value) = meta.get(key) {
                let rendered = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                out.push_str(&format!("      {key}: {rendered}\n"));
            }
        }
        if !node.deps.is_empty() {
            out.push_str(&format!("      deps: {}\n", node.deps.join(", ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(key: &str, deps: &[&str]) -> NodeSpec {
        NodeSpec {
            key: key.into(),
            title: format!("node {key}"),
            repo: "marbles".into(),
            conflict_group: "g".into(),
            estimate: Some(10),
            deps: deps.iter().map(|d| (*d).into()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn specs_parse_from_toml_and_json() {
        let toml = r#"
name = "demo"
project = "p"
[[nodes]]
key = "a"
title = "A"
estimate = 15
[[nodes]]
key = "b"
title = "B"
deps = ["a"]
"#;
        let spec = parse_spec(toml).unwrap();
        assert_eq!(spec.name.as_deref(), Some("demo"));
        assert_eq!(spec.nodes.len(), 2);
        let json = r#"{"name":"demo","project":"p","nodes":[
            {"key":"a","title":"A"},{"key":"b","title":"B","deps":["a"]}]}"#;
        let spec = parse_spec(json).unwrap();
        assert_eq!(spec.nodes[1].deps, vec!["a"]);
    }

    #[test]
    fn validation_reports_unknown_deps_duplicates_and_cycles() {
        let cyclic = ResolvedPlan {
            plan: "p".into(),
            project: "demo".into(),
            nodes: vec![node("a", &["b"]), node("b", &["a"])],
        };
        let problems = validate(&cyclic).unwrap_err();
        assert!(problems.iter().any(|p| p.contains("cycle")));
        let duplicate = ResolvedPlan {
            plan: "p".into(),
            project: "demo".into(),
            nodes: vec![node("a", &[]), node("a", &[])],
        };
        let problems = validate(&duplicate).unwrap_err();
        assert!(problems.iter().any(|p| p.contains("duplicate")));
        let dangling = ResolvedPlan {
            plan: "p".into(),
            project: "demo".into(),
            nodes: vec![node("a", &["nope"])],
        };
        assert!(validate(&dangling).unwrap_err()[0].contains("unknown key"));
    }

    #[test]
    fn drafts_are_reviewable_without_touching_the_store() {
        let plan = ResolvedPlan {
            plan: "p".into(),
            project: "demo".into(),
            nodes: vec![node("a", &[]), node("b", &["a"])],
        };
        let draft = draft(&plan).unwrap();
        assert_eq!(draft.edges.len(), 1);
        assert_eq!(draft.nodes[1].metadata[META_PLAN], "p");
    }

    fn scoped_issue(id: &str, deps: &[&str], estimate: Option<i64>) -> Issue {
        Issue {
            id: id.into(),
            title: id.into(),
            status: "open".into(),
            issue_type: "task".into(),
            project: "p".into(),
            dependencies: deps
                .iter()
                .map(|dep| crate::types::Dependency {
                    depends_on_id: Some((*dep).into()),
                })
                .collect(),
            metadata: estimate
                .map(|minutes| serde_json::json!({META_ESTIMATE: minutes}))
                .unwrap_or(serde_json::json!({})),
            ..Default::default()
        }
    }

    #[test]
    fn scoped_gantt_uses_metadata_estimates_and_reports_the_default() {
        let all = vec![
            scoped_issue("a", &[], Some(20)),
            scoped_issue("b", &["a"], None),
        ];
        let view = gantt_scoped(
            &all,
            all.clone(),
            GanttScope {
                project: Some("p".into()),
                ..Default::default()
            },
            false,
            60,
            1_000,
        )
        .unwrap();
        let a = view.gantt.nodes.iter().find(|node| node.id == "a").unwrap();
        let b = view.gantt.nodes.iter().find(|node| node.id == "b").unwrap();
        assert_eq!(a.estimate, 20);
        assert_eq!(a.estimate_source, "metadata");
        assert_eq!(b.estimate, 60);
        assert_eq!(b.estimate_source, "default");
        assert_eq!(a.start_min, 0);
        assert_eq!(b.start_min, 20);
        assert_eq!(
            view.gantt.critical_path,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn scoped_gantt_fails_closed_on_a_cycle() {
        let all = vec![
            scoped_issue("a", &["b"], None),
            scoped_issue("b", &["a"], None),
        ];
        assert!(gantt_scoped(&all, all.clone(), GanttScope::default(), false, 60, 1_000).is_err());
    }
}

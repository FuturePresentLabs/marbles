//! Plans are DAGs over issues: spec → nodes+edges, and the three views an executor consumes.
//! These tests exercise the real store (local mode) and the real HTTP router, because the claim
//! is that both compute the same answer.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use marbles::api::Api;
use marbles::auth::{Auth, AuthConfig};
use marbles::db::{Db, now};
use marbles::plan::{self, CreateReport, Gantt, NodeSpec, PlanView, ReadyView, ResolvedPlan};
use marbles::types::{ActorKind, ClaimRequest, Evidence, IssuePatch};
use tower::ServiceExt;

fn node(
    key: &str,
    title: &str,
    project: &str,
    repo: &str,
    group: &str,
    estimate: i64,
    deps: &[&str],
) -> NodeSpec {
    NodeSpec {
        key: key.into(),
        title: title.into(),
        project: project.into(),
        repo: repo.into(),
        conflict_group: group.into(),
        estimate: Some(estimate),
        risk: "medium".into(),
        done_means: format!("cargo test {key}"),
        acceptance: format!("{key} behaves"),
        deps: deps.iter().map(|d| (*d).into()).collect(),
        ..Default::default()
    }
}

/// Six nodes, two projects (repos), two conflict groups. a and f are dep-free `db` siblings: the
/// frontier must offer at most one of them.
fn demo_plan() -> ResolvedPlan {
    ResolvedPlan {
        plan: "checkout-v2".into(),
        project: "marbles".into(),
        nodes: vec![
            node("a", "plan schema", "marbles", "marbles", "db", 30, &[]),
            node(
                "b",
                "client scaffold",
                "checkout",
                "checkout",
                "api",
                20,
                &[],
            ),
            node("c", "plan API", "marbles", "marbles", "api", 45, &["a"]),
            node(
                "d",
                "client wiring",
                "checkout",
                "checkout",
                "db",
                40,
                &["b"],
            ),
            node("e", "docs", "marbles", "marbles", "docs", 15, &["c"]),
            node("f", "migration", "marbles", "marbles", "db", 25, &[]),
        ],
    }
}

fn seeded() -> (Db, ResolvedPlan, CreateReport) {
    let db = Db::in_memory().unwrap();
    db.ensure_project("marbles", "/tmp/marbles", "mb").unwrap();
    db.ensure_project("checkout", "/tmp/checkout", "co")
        .unwrap();
    let plan = demo_plan();
    let report = plan::create(&db, &plan, now(), "planner").unwrap();
    (db, plan, report)
}

fn ids(report: &CreateReport) -> std::collections::BTreeMap<String, String> {
    report
        .nodes
        .iter()
        .map(|n| (n.key.clone(), n.id.clone()))
        .collect()
}

#[test]
fn create_writes_nodes_edges_and_machine_readable_metadata() {
    let (db, plan_spec, report) = seeded();
    assert_eq!(report.nodes.len(), 6);
    assert_eq!(report.edges.len(), 3);
    let by_key = ids(&report);
    // Metadata is stored per node and is queryable by plan name.
    for key in ["a", "b", "c", "d", "e", "f"] {
        let issue = db.get(&by_key[key]).unwrap();
        assert_eq!(issue.metadata["plan"], "checkout-v2");
        assert_eq!(issue.metadata["plan_key"], key);
        assert!(issue.metadata["done_means"].as_str().unwrap().contains(key));
        assert!(issue.metadata["estimate"].as_i64().unwrap() > 0);
    }
    assert_eq!(db.get(&by_key["b"]).unwrap().metadata["repo"], "checkout");
    assert_eq!(
        db.get(&by_key["f"]).unwrap().metadata["conflict_group"],
        "db"
    );
    // The two projects really are distinct stores in one database.
    assert_eq!(db.get(&by_key["b"]).unwrap().project, "checkout");
    assert_eq!(db.get(&by_key["a"]).unwrap().project, "marbles");

    let view = plan::load(&db, "checkout-v2").unwrap().view().unwrap();
    assert_eq!(view.nodes.len(), 6);
    assert_eq!(view.repos, vec!["checkout", "marbles"]);
    assert_eq!(view.conflict_groups, vec!["api", "db", "docs"]);
    // Topological order: dependencies appear before dependents.
    let order: Vec<&str> = view.nodes.iter().map(|n| n.key.as_str()).collect();
    let pos = |key: &str| order.iter().position(|k| *k == key).unwrap();
    assert!(pos("a") < pos("c"));
    assert!(pos("c") < pos("e"));
    assert!(pos("b") < pos("d"));
    // Idempotency guard: re-creating the same plan is refused, not half-applied.
    let again = plan::create(&db, &plan_spec, now(), "planner");
    assert!(again.unwrap_err().contains("already has 6 node"));
}

#[test]
fn gantt_marks_the_longest_chain_and_peak_concurrency() {
    let (db, _, _) = seeded();
    let graph = plan::load(&db, "checkout-v2").unwrap();
    let gantt: Gantt = plan::gantt(&graph, now()).unwrap();
    let keys: Vec<&str> = gantt
        .nodes
        .iter()
        .filter(|n| n.critical)
        .map(|n| n.key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec!["a", "c", "e"],
        "critical chain is schema→api→docs"
    );
    assert_eq!(gantt.critical_path_minutes, 90);
    assert_eq!(gantt.total_minutes, 90);
    assert_eq!(gantt.peak_concurrency, 3);
    let f = gantt.nodes.iter().find(|n| n.key == "f").unwrap();
    assert!(f.slack_min > 0, "migration can slip without moving the end");
    // Estimates drive the schedule: the dependent starts when its parent finishes.
    let c = gantt.nodes.iter().find(|n| n.key == "c").unwrap();
    assert_eq!(c.start_min, 30);
    assert_eq!(c.finish_min, 75);
}

#[test]
fn ready_respects_dependency_frontier_and_conflict_groups() {
    let (db, _, report) = seeded();
    let by_key = ids(&report);

    let frontier: ReadyView = plan::ready(&plan::load(&db, "checkout-v2").unwrap(), now()).unwrap();
    let offered: Vec<&str> = frontier.ready.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(offered, vec!["a", "b"], "one node per conflict group");
    let f = frontier.blocked.iter().find(|b| b.key == "f").unwrap();
    assert_eq!(f.reason, "conflict_group");
    assert!(f.detail.contains("already has a node in this frontier"));

    // A running node makes its group busy: the sibling stays out even though deps are closed.
    db.update(
        &by_key["a"],
        &IssuePatch {
            status: Some("in_progress".into()),
            ..Default::default()
        },
        "worker",
        now(),
    )
    .unwrap();
    db.claim(
        &ClaimRequest {
            id: by_key["a"].clone(),
            assignee: "worker".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(600),
        },
        now(),
    )
    .unwrap();
    let frontier = plan::ready(&plan::load(&db, "checkout-v2").unwrap(), now()).unwrap();
    assert_eq!(
        frontier
            .ready
            .iter()
            .map(|n| n.key.as_str())
            .collect::<Vec<_>>(),
        vec!["b"]
    );
    assert_eq!(
        frontier.busy_groups.get("db").map(String::as_str),
        Some(by_key["a"].as_str())
    );
    let f = frontier.blocked.iter().find(|b| b.key == "f").unwrap();
    assert_eq!(f.reason, "conflict_group");
    assert!(f.detail.contains("busy"));

    // Closing a node with evidence advances its dependents; the group frees up.
    db.close(
        &by_key["a"],
        Some("schema merged"),
        &[Evidence::commit("deadbeef")],
        "reviewer",
        now(),
    )
    .unwrap();
    let frontier = plan::ready(&plan::load(&db, "checkout-v2").unwrap(), now()).unwrap();
    let mut offered: Vec<&str> = frontier.ready.iter().map(|n| n.key.as_str()).collect();
    offered.sort();
    assert_eq!(
        offered,
        vec!["b", "f"],
        "db group freed, api group still has b"
    );
    let c = frontier.blocked.iter().find(|b| b.key == "c").unwrap();
    assert_eq!(
        c.reason, "conflict_group",
        "c is eligible but api is in the frontier"
    );
}

#[tokio::test]
async fn plan_views_are_identical_over_the_http_api() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::in_memory().unwrap());
    db.ensure_project("marbles", "/tmp/marbles", "mb").unwrap();
    db.ensure_project("checkout", "/tmp/checkout", "co")
        .unwrap();
    let (_, token) =
        marbles::auth::mint_token(&dir.path().join("tokens"), ActorKind::Agent, "planner").unwrap();
    let app = Arc::new(Api {
        db: db.clone(),
        company_stores: None,
        auth: Arc::new(Auth::new(AuthConfig::default(), dir.path().join("tokens"))),
    })
    .router();

    let (status, body) = call(&app, "plans.create", &demo_plan(), &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let report: CreateReport = serde_json::from_str(&body).unwrap();
    assert_eq!(report.nodes.len(), 6);

    let (status, body) = call(
        &app,
        "plans.show",
        &serde_json::json!({"plan": "checkout-v2"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let view: PlanView = serde_json::from_str(&body).unwrap();
    assert_eq!(view.nodes.len(), 6);
    assert_eq!(view.edges.len(), 3);

    let (status, body) = call(
        &app,
        "plans.gantt",
        &serde_json::json!({"plan": "checkout-v2"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let gantt: Gantt = serde_json::from_str(&body).unwrap();
    assert_eq!(gantt.total_minutes, 90);

    let (status, body) = call(
        &app,
        "plans.ready",
        &serde_json::json!({"plan": "checkout-v2"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let frontier: ReadyView = serde_json::from_str(&body).unwrap();
    assert_eq!(frontier.ready.len(), 2);
    assert_eq!(frontier.total_nodes, 6);

    // The same DAG computed in local mode agrees with the wire result.
    let local = plan::ready(&plan::load(&*db, "checkout-v2").unwrap(), now()).unwrap();
    assert_eq!(
        local.ready.iter().map(|n| n.id.clone()).collect::<Vec<_>>(),
        frontier
            .ready
            .iter()
            .map(|n| n.id.clone())
            .collect::<Vec<_>>()
    );
}

async fn call<T: serde::Serialize>(
    app: &axum::Router,
    op: &str,
    body: &T,
    token: &str,
) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/{op}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

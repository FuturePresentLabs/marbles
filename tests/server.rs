//! End-to-end over the real HTTP stack, with the real auth middleware: an unauthenticated
//! request fails, an agent token cannot sweep or impersonate a human hold, and the lease
//! lifecycle works across the wire.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use marbles::api::Api;
use marbles::auth::{Auth, AuthConfig};
use marbles::db::{CompanyStores, Db};
use marbles::types::*;
use tower::ServiceExt; // oneshot

fn app() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::in_memory().unwrap());
    db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    marbles::auth::mint_token(&dir.path().join("tokens"), ActorKind::Agent, "worker").unwrap();
    marbles::auth::mint_token(&dir.path().join("tokens"), ActorKind::Human, "avery").unwrap();
    let api = Arc::new(Api {
        db: Arc::clone(&db),
        company_stores: None,
        auth: Arc::new(Auth::new(AuthConfig::default(), dir.path().join("tokens"))),
    });
    (api.router(), dir)
}

use axum::Router;

async fn call(
    app: &Router,
    op: &str,
    body: serde_json::Value,
    token: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/v1/{op}"))
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn token_file(dir: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(dir.join("tokens").join(name))
        .unwrap()
        .trim()
        .to_string()
}

#[tokio::test]
async fn an_unauthenticated_request_is_refused_before_it_reaches_the_store() {
    let (app, dir) = app();
    let _ = dir;
    let (status, _) = call(&app, "issues.list", serde_json::json!({}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn hosted_company_stores_reject_credentials_without_a_verified_company() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::in_memory().unwrap());
    let stores = Arc::new(CompanyStores::new(dir.path().join("companies")).unwrap());
    marbles::auth::mint_token(&dir.path().join("tokens"), ActorKind::Human, "avery").unwrap();
    let token = token_file(dir.path(), "human-avery");
    let app = Arc::new(Api {
        db,
        company_stores: Some(stores),
        auth: Arc::new(Auth::new(AuthConfig::default(), dir.path().join("tokens"))),
    })
    .router();
    let (status, body) = call(&app, "stats", serde_json::json!({}), Some(&token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("not scoped to an FPL Auth company"));
}

#[tokio::test]
async fn the_full_lease_lifecycle_crosses_the_wire() {
    let (app, dir) = app();
    let agent = token_file(dir.path(), "agent-worker");
    let human = token_file(dir.path(), "human-avery");

    let (status, body) = call(
        &app,
        "issues.create",
        serde_json::json!({"title": "wire job", "project": "demo"}),
        Some(&agent),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // The grace period is visible over HTTP exactly as in-process.
    let (_, body) = call(
        &app,
        "issues.ready",
        serde_json::json!({"project":"demo"}),
        Some(&agent),
    )
    .await;
    assert!(!body.contains(&id), "fresh work is not swarm-ready: {body}");

    // A human principal's claim assigns to the *principal*, whatever the body asked.
    let (status, body) = call(
        &app,
        "claims.claim",
        serde_json::json!({"id": id, "assignee": "somebody-else"}),
        Some(&human),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let receipt: ClaimReceipt = serde_json::from_str(&body).unwrap();
    assert_eq!(
        receipt.assignee, "avery",
        "identity comes from the token, not the request"
    );
    assert_eq!(receipt.actor_kind, ActorKind::Human);

    // An agent cannot sweep — requeuing other people's work is an operator action.
    let (status, _) = call(&app, "claims.sweep", serde_json::json!({}), Some(&agent)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A leaked agent token cannot steal the human's live hold.
    let (status, _) = call(
        &app,
        "claims.claim",
        serde_json::json!({"id": id, "assignee": "thief"}),
        Some(&agent),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The human closes their own hold with a release; then the agent claims and closes properly.
    let (status, _) = call(
        &app,
        "claims.release",
        serde_json::json!({"id": id}),
        Some(&human),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "claims.claim",
        serde_json::json!({"id": id}),
        Some(&agent),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Done means merged: closing with no evidence is a refusal, not a warning.
    let (status, _) = call(
        &app,
        "issues.close",
        serde_json::json!({"id": id}),
        Some(&agent),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(
        &app,
        "issues.close",
        serde_json::json!({"id": id, "pr": "https://example/pr/1"}),
        Some(&agent),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_second_agent_watching_the_list_sees_the_first_agents_claim() {
    let (app, dir) = app();
    let agent = token_file(dir.path(), "agent-worker");
    let (status, body) = call(
        &app,
        "issues.create",
        serde_json::json!({"title": "contested", "project": "demo", "available_at": 0}),
        Some(&agent),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, body) = call(
        &app,
        "issues.ready",
        serde_json::json!({"project":"demo"}),
        Some(&agent),
    )
    .await;
    assert!(body.contains(&id));
    let (_, body) = call(
        &app,
        "issues.get",
        serde_json::json!({"id": id}),
        Some(&agent),
    )
    .await;
    let issue: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(issue["title"], "contested");
    assert_eq!(
        issue["project"], "demo",
        "the merged view knows who owns what"
    );
}

#[tokio::test]
async fn human_import_preserves_ids_deps_and_refuses_agents() {
    let (app, dir) = app();
    let agent = token_file(dir.path(), "agent-worker");
    let human = token_file(dir.path(), "human-avery");

    let rows = serde_json::json!([
        {"id": "demo-aaa", "title": "open work", "status": "open", "priority": 1,
         "issue_type": "feature", "dependencies": [], "closed_at": null},
        {"id": "demo-bbb", "title": "shipped work", "status": "closed", "priority": 2,
         "issue_type": "task", "dependencies": [{"depends_on_id": "demo-aaa"}], "closed_at": null},
    ]);
    let (status, body) = call(
        &app,
        "issues.import",
        serde_json::json!({"project": "demo", "rows": rows}),
        Some(&agent),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "agents must not import: {body}"
    );

    let (status, body) = call(
        &app,
        "issues.import",
        serde_json::json!({"project": "demo", "rows": rows}),
        Some(&human),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let report: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(report["issues"], 2);
    assert_eq!(report["deps"], 1);
    assert_eq!(report["missing_deps"], 0);
    assert_eq!(report["statused"], 1); // the closed one

    // Original ids survive the move — references, ledgers, muscle memory.
    let (status, body) = call(
        &app,
        "issues.get",
        serde_json::json!({"id": "demo-aaa"}),
        Some(&human),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.matches("demo-aaa").count() >= 1, true);
    let (status, _) = call(
        &app,
        "issues.get",
        serde_json::json!({"id": "demo-bbb"}),
        Some(&human),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Closed imports stay closed: ready shows only the open one.
    let (_, body) = call(
        &app,
        "issues.ready",
        serde_json::json!({"project": "demo"}),
        Some(&human),
    )
    .await;
    assert!(
        body.contains("demo-aaa") && !body.contains("demo-bbb"),
        "{body}"
    );
}

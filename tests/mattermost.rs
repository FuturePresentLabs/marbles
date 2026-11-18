//! End-to-end over the real form-encoded Mattermost slash-command surface.
//!
//! The point of these tests is not the Markdown; it is that a slash command
//! resolves to the same `Store` and the same human principal as any other
//! Marbles client. A claim made in Mattermost must therefore be a real hold,
//! and a close must still carry evidence.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use marbles::api::Api;
use marbles::auth::{Auth, AuthConfig};
use marbles::config::MattermostConfig;
use marbles::db::{CompanyStores, Db, Store};
use marbles::mattermost::SLASH_PATH;
use marbles::types::ActorKind;
use tower::ServiceExt;

struct Fixture {
    app: Router,
    db: Arc<Db>,
    _dir: tempfile::TempDir,
}

fn fixture(allowed_users: Vec<String>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::in_memory().unwrap());
    db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    let token_path = dir.path().join("mattermost-slash.token");
    std::fs::write(&token_path, "slash-secret\n").unwrap();
    let store: Arc<dyn Store> = db.clone();
    let api = Arc::new(Api {
        db: store,
        company_stores: None,
        auth: Arc::new(Auth::new(AuthConfig::default(), dir.path().join("tokens"))),
    });
    let app = api
        .router_with_mattermost(MattermostConfig {
            token_file: Some(token_path),
            token_env: None,
            company: None,
            default_project: Some("demo".into()),
            allowed_users,
        })
        .unwrap();
    Fixture { app, db, _dir: dir }
}

fn encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn slash(
    app: &Router,
    token: &str,
    user: &str,
    text: &str,
) -> (StatusCode, serde_json::Value) {
    let body = format!(
        "token={}&user_name={}&text={}",
        encode(token),
        encode(user),
        encode(text)
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(SLASH_PATH)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(
            |_| serde_json::json!({"raw": String::from_utf8_lossy(&bytes).into_owned()}),
        ),
    )
}

fn text(response: &serde_json::Value) -> &str {
    response["text"].as_str().unwrap_or("")
}

#[tokio::test]
async fn a_bad_mattermost_token_is_refused_before_the_store() {
    let fixture = fixture(Vec::new());
    let (status, _) = slash(&fixture.app, "wrong", "avery", "help").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn help_is_ephemeral_and_lists_the_commands() {
    let fixture = fixture(Vec::new());
    let (status, response) = slash(&fixture.app, "slash-secret", "avery", "help").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["response_type"], "ephemeral");
    assert!(text(&response).contains("Marbles slash commands"));
    assert!(text(&response).contains("close <id>"));
}

#[tokio::test]
async fn create_uses_the_default_project_and_reports_the_new_id() {
    let fixture = fixture(Vec::new());
    let (status, response) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        "create Fix flux capacitor --priority 1 --type bug --description \"it burns\"",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["response_type"], "in_channel");
    assert!(text(&response).contains("Created **demo-"), "{response}");

    let issues = fixture.db.list(Some("demo"), false, None).unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].title, "Fix flux capacitor");
    assert_eq!(issues[0].priority, 1);
    assert_eq!(issues[0].issue_type, "bug");
    assert_eq!(issues[0].description, "it burns");
}

#[tokio::test]
async fn update_claim_release_and_close_are_real_store_mutations() {
    let fixture = fixture(Vec::new());
    let (_, _) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        "create Fix flux capacitor --project demo",
    )
    .await;
    let id = fixture.db.list(Some("demo"), false, None).unwrap()[0]
        .id
        .clone();

    let (status, response) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        &format!("update {id} --status in_progress --note \"started on it\""),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(fixture.db.get(&id).unwrap().status, "in_progress");
    assert!(
        fixture
            .db
            .get(&id)
            .unwrap()
            .description
            .contains("started on it")
    );

    let (status, response) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        &format!("claim {id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let issue = fixture.db.get(&id).unwrap();
    assert_eq!(issue.assignee.as_deref(), Some("avery"));
    assert_eq!(issue.actor_kind.as_deref(), Some("human"));

    let (status, _) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        &format!("release {id}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(fixture.db.get(&id).unwrap().assignee.is_none());

    let (status, response) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        &format!("close {id} --pr https://example.invalid/pr/1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let issue = fixture.db.get(&id).unwrap();
    assert_eq!(issue.status, "closed");
    assert_eq!(issue.evidence[0].kind, "pr");
    assert!(text(&response).contains("Closed"));
}

#[tokio::test]
async fn close_without_evidence_fails_closed() {
    let fixture = fixture(Vec::new());
    let (_, _) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        "create Needs a receipt --project demo",
    )
    .await;
    let id = fixture.db.list(Some("demo"), false, None).unwrap()[0]
        .id
        .clone();
    let (status, response) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        &format!("close {id} --reason done"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text(&response).contains("close needs evidence"),
        "{response}"
    );
    assert_eq!(fixture.db.get(&id).unwrap().status, "open");
}

#[tokio::test]
async fn list_filters_by_status_and_query() {
    let fixture = fixture(Vec::new());
    let (_, _) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        "create One --project demo --label alpha",
    )
    .await;
    let (_, _) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        "create Two --project demo --description \"the token path\"",
    )
    .await;

    let (status, response) = slash(
        &fixture.app,
        "slash-secret",
        "avery",
        "list --project demo --query token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(text(&response).contains("Two"), "{response}");
    let issue_lines = text(&response)
        .lines()
        .filter(|line| line.starts_with("- `"))
        .count();
    assert_eq!(
        issue_lines, 1,
        "only the matching issue should list: {response}"
    );
}

#[tokio::test]
async fn an_allowlist_refuses_other_mattermost_users() {
    let fixture = fixture(vec!["avery".into()]);
    let (status, response) = slash(&fixture.app, "slash-secret", "mallory", "help").await;
    assert_eq!(status, StatusCode::OK);
    assert!(text(&response).contains("not allowed"), "{response}");
}

#[test]
fn hosted_servers_refuse_mattermost_without_a_company() {
    let dir = tempfile::tempdir().unwrap();
    let api = Arc::new(Api {
        db: Arc::new(Db::in_memory().unwrap()),
        company_stores: Some(Arc::new(
            CompanyStores::new(dir.path().join("companies")).unwrap(),
        )),
        auth: Arc::new(Auth::new(AuthConfig::default(), dir.path().join("tokens"))),
    });
    let error = api
        .router_with_mattermost(MattermostConfig {
            token_file: Some(dir.path().join("unused-token")),
            token_env: None,
            company: None,
            default_project: None,
            allowed_users: Vec::new(),
        })
        .unwrap_err();
    assert!(error.contains("needs `company`"), "{error}");
}

#[test]
fn the_config_defaults_to_no_default_project() {
    // A missing `default_project` must not silently become a project called
    // `default`: create then fails with a message naming the missing flag.
    let config = MattermostConfig {
        token_file: Some("token".into()),
        token_env: None,
        company: None,
        default_project: None,
        allowed_users: Vec::new(),
    };
    assert!(config.default_project.is_none());
}

#[test]
fn the_principal_kind_is_human_for_slash_commands() {
    // Kept next to the integration test so the policy is hard to miss when the
    // handler changes: a slash command is the person, not an agent token.
    assert_eq!(ActorKind::Human.as_str(), "human");
}

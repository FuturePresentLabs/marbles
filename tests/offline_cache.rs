//! A tracker outage must degrade the CLI, not stop it: reads replay from a local cache with a
//! clear banner, writes fail loudly, and no call can hang. Everything here is hermetic — a real
//! axum server on 127.0.0.1:0, a temporary `MARBLES_HOME`, and sockets that are closed on purpose.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use marbles::api::Api;
use marbles::auth::{Auth, AuthConfig};
use marbles::cache::Cache;
use marbles::client::{Client, Options};
use marbles::db::Db;
use marbles::types::*;

struct TestServer {
    url: String,
    token: String,
    handle: tokio::task::JoinHandle<()>,
    _state: tempfile::TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let state = tempfile::tempdir().unwrap();
        let tokens = state.path().join("tokens");
        let (_, token) =
            marbles::auth::mint_token(&tokens, ActorKind::Human, "cache-test").unwrap();
        let db = Arc::new(Db::open(state.path().join("marbles.db")).unwrap());
        let api = Arc::new(Api {
            db,
            company_stores: None,
            auth: Arc::new(Auth::new(AuthConfig::default(), tokens)),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, api.router()).await.unwrap();
        });
        Self {
            url: format!("http://{addr}"),
            token,
            handle,
            _state: state,
        }
    }

    /// Simulate the 2026-10-21 incident: the process is gone and the port refuses connections.
    async fn stop(self) -> String {
        self.handle.abort();
        let _ = self.handle.await;
        self.url
    }
}

fn make_client(url: &str, token: &str, cache: Option<Cache>) -> Client {
    Client::with_options(
        url,
        token,
        Options {
            timeout: Some(Duration::from_millis(750)),
            cache,
        },
    )
}

#[tokio::test]
async fn cached_reads_survive_an_outage_while_writes_refuse() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Cache::at(cache_dir.path().join("cache"));
    let server = TestServer::start().await;
    let url = server.url.clone();
    let online = make_client(&url, &server.token, Some(cache.clone()));

    let _: serde_json::Value = online
        .call(
            "projects.ensure",
            &serde_json::json!({"slug": "demo", "root": "/demo", "prefix": "demo"}),
        )
        .await
        .unwrap();
    let created: serde_json::Value = online
        .call(
            "issues.create",
            &serde_json::json!({
                "title": "survive the outage",
                "project": "demo",
                "available_at": 0,
                "created_by": "human/cache-test",
            }),
        )
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // A successful read is cached under exactly this request body.
    let ready: Vec<Issue> = online
        .call("issues.ready", &serde_json::json!({"project": "demo"}))
        .await
        .unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(
        std::fs::read_dir(cache.root()).unwrap().count(),
        1,
        "one cached read response expected"
    );

    let url = server.stop().await;

    // Offline read: same key, served from cache, bounded by the client timeout.
    let offline = make_client(&url, "irrelevant-now", Some(cache.clone()));
    let started = Instant::now();
    let replayed: Vec<Issue> = offline
        .call("issues.ready", &serde_json::json!({"project": "demo"}))
        .await
        .expect("an outage must replay the cached read");
    let elapsed = started.elapsed();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].id, id);
    assert!(
        elapsed < Duration::from_secs(3),
        "offline read took {elapsed:?}; a timeout must fail in seconds"
    );

    // A read the cache has never seen cannot be invented.
    let err = offline
        .call::<Vec<Issue>>("issues.ready", &serde_json::json!({"project": "other"}))
        .await
        .unwrap_err();
    assert!(err.contains("offline: no cached issues.ready"), "{err}");

    // Writes never come from cache and are never silently queued.
    let err = offline
        .call::<serde_json::Value>(
            "claims.claim",
            &serde_json::json!({"id": id, "assignee": "agent/x"}),
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("offline: claims.claim not applied (no server)"),
        "{err}"
    );
    let err = offline
        .call::<serde_json::Value>(
            "issues.close",
            &serde_json::json!({"id": id, "evidence": [{"kind": "pr", "value": "x"}]}),
        )
        .await
        .unwrap_err();
    assert!(err.contains("not applied"), "{err}");

    // The write attempt must not have created or overwritten a cache entry.
    assert_eq!(std::fs::read_dir(cache.root()).unwrap().count(), 1);
}

#[tokio::test]
async fn a_server_error_replays_a_cached_read_but_a_rejection_does_not() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Cache::at(cache_dir.path().join("cache"));

    // 503: unavailable, replay the cache.
    let (url, handle) = serve_router(Router::new().route(
        "/v1/stats",
        post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "storage unavailable") }),
    ))
    .await;
    // Seed exactly what a healthy server would have returned for this URL.
    cache.store(&url, "stats", "{}", "{\"open\":3}").unwrap();
    let client = make_client(&url, "token", Some(cache.clone()));
    let stats: serde_json::Value = client.call("stats", &serde_json::json!({})).await.unwrap();
    assert_eq!(stats["open"], 3);
    handle.abort();

    // 403: the server answered and said no; the cache must not paper over a credential problem.
    let (url, handle) = serve_router(Router::new().route(
        "/v1/stats",
        post(|| async { (StatusCode::FORBIDDEN, "no principal") }),
    ))
    .await;
    cache.store(&url, "stats", "{}", "{\"open\":3}").unwrap();
    let client = make_client(&url, "token", Some(cache.clone()));
    let err = client
        .call::<serde_json::Value>("stats", &serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("HTTP 403"), "{err}");
    handle.abort();
}

#[tokio::test]
async fn a_hung_server_is_cut_off_by_the_timeout() {
    let (url, handle) = serve_router(Router::new().route(
        "/v1/issues.list",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            (StatusCode::OK, "[]").into_response()
        }),
    ))
    .await;
    let client = make_client(&url, "token", None);
    let started = Instant::now();
    let err = client
        .call::<Vec<Issue>>("issues.list", &serde_json::json!({"project": "demo"}))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout did not bound the call: {elapsed:?}"
    );
    assert!(err.contains("offline"), "{err}");
    handle.abort();
}

async fn serve_router(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), handle)
}

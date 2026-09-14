//! The wire protocol: one operation per POST, JSON both ways, every caller a resolved
//! [`Principal`]. Bodies may *request*; credentials may *assert* — `actor_kind` and a human's
//! identity are always taken from the principal, never the payload.
//!
//! The route set is intentionally 1:1 with the store operations so the CLI is a thin shell and
//! any client (web UI, AlfAlpha, an agent loop) speaks the same thing. Every endpoint accepts a
//! JSON body, even if only `{}`, so there is one request shape to teach clients.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{Auth, Principal};
use crate::db::{Db, Error as DbError, SweepReport};
use crate::types::*;

pub struct Api {
    pub db: Arc<Db>,
    pub auth: Arc<Auth>,
}

impl Api {
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .route("/v1/projects.ensure", post(projects_ensure))
            .route("/v1/projects.list", post(projects_list))
            .route("/v1/issues.create", post(issues_create))
            .route("/v1/issues.get", post(issues_get))
            .route("/v1/issues.list", post(issues_list))
            .route("/v1/issues.ready", post(issues_ready))
            .route("/v1/issues.update", post(issues_update))
            .route("/v1/issues.close", post(issues_close))
            .route("/v1/issues.supersede", post(issues_supersede))
            .route("/v1/deps.add", post(deps_add))
            .route("/v1/deps.remove", post(deps_remove))
            .route("/v1/labels.add", post(labels_add))
            .route("/v1/labels.remove", post(labels_remove))
            .route("/v1/claims.claim", post(claims_claim))
            .route("/v1/claims.touch", post(claims_touch))
            .route("/v1/claims.release", post(claims_release))
            .route("/v1/claims.sweep", post(claims_sweep))
            .route("/v1/history.get", post(history_get))
            .route("/v1/stats", post(stats))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&self),
                require_principal,
            ))
            .with_state(self)
    }
}

async fn require_principal(State(api): State<Arc<Api>>, mut req: Request, next: Next) -> Response {
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match api.auth.authenticate(bearer).await {
        Some(principal) => {
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        None => (StatusCode::UNAUTHORIZED, "unknown or missing credential").into_response(),
    }
}

#[derive(Debug)]
struct ApiError(ApiErrorKind);

#[derive(Debug)]
enum ApiErrorKind {
    Status(StatusCode, String),
    Db(DbError),
}

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        ApiError(ApiErrorKind::Db(err))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self.0 {
            ApiErrorKind::Status(status, message) => (status, message),
            ApiErrorKind::Db(err) => {
                let status = match &err {
                    DbError::NotFound(_) | DbError::NoProject(_) => StatusCode::NOT_FOUND,
                    DbError::AlreadyClaimed(..) | DbError::Cycle { .. } => StatusCode::CONFLICT,
                    DbError::BadStatus(_) | DbError::Bad(_) | DbError::NoEvidence(_) => {
                        StatusCode::UNPROCESSABLE_ENTITY
                    }
                    DbError::Sqlite(_) | DbError::Json(_) => StatusCode::INTERNAL_SERVER_ERROR,
                };
                (status, err.to_string())
            }
        };
        (status, message).into_response()
    }
}

type Res<T> = Result<Json<T>, ApiError>;

#[derive(Deserialize)]
struct Empty {}

// ---- projects ----

#[derive(Deserialize)]
struct EnsureProject {
    slug: String,
    root: String,
    prefix: String,
}

async fn projects_ensure(
    State(api): State<Arc<Api>>,
    Extension(_principal): Extension<Principal>,
    Json(body): Json<EnsureProject>,
) -> Res<serde_json::Value> {
    api.db
        .ensure_project(&body.slug, &body.root, &body.prefix)?;
    Ok(Json(serde_json::json!({"slug": body.slug})))
}

async fn projects_list(
    State(api): State<Arc<Api>>,
    Extension(_principal): Extension<Principal>,
    Json(_body): Json<Empty>,
) -> Res<serde_json::Value> {
    let rows = api.db.projects()?;
    Ok(Json(serde_json::json!(
        rows.into_iter()
            .map(|(slug, root, prefix)| {
                serde_json::json!({"slug": slug, "root": root, "prefix": prefix})
            })
            .collect::<Vec<_>>()
    )))
}

// ---- issues ----

async fn issues_create(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(mut spec): Json<NewIssue>,
) -> Res<OkId> {
    if spec.created_by.is_none() {
        spec.created_by = Some(principal.name);
    }
    let id = api.db.create(&spec, crate::db::now())?;
    Ok(Json(OkId { id }))
}

#[derive(Serialize)]
struct OkId {
    id: String,
}

#[derive(Deserialize)]
struct IdBody {
    id: String,
}

async fn issues_get(
    State(api): State<Arc<Api>>,
    Extension(_principal): Extension<Principal>,
    Json(body): Json<IdBody>,
) -> Res<Issue> {
    Ok(Json(api.db.get(&body.id)?))
}

#[derive(Deserialize)]
struct ListBody {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    id: Option<String>,
}

async fn issues_list(
    State(api): State<Arc<Api>>,
    Extension(_principal): Extension<Principal>,
    Json(body): Json<ListBody>,
) -> Res<Vec<Issue>> {
    Ok(Json(api.db.list(
        body.project.as_deref(),
        body.all,
        body.id.as_deref(),
    )?))
}

async fn issues_ready(
    State(api): State<Arc<Api>>,
    Extension(_principal): Extension<Principal>,
    Json(body): Json<ListBody>,
) -> Res<Vec<Issue>> {
    Ok(Json(
        api.db.ready(body.project.as_deref(), crate::db::now())?,
    ))
}

#[derive(Deserialize)]
struct UpdateBody {
    id: String,
    patch: IssuePatch,
}

async fn issues_update(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<UpdateBody>,
) -> Res<Issue> {
    Ok(Json(api.db.update(
        &body.id,
        &body.patch,
        &principal.name,
        crate::db::now(),
    )?))
}

#[derive(Deserialize)]
struct CloseBody {
    id: String,
    #[serde(default)]
    reason: Option<String>,
    /// Evidence list, or the flat conveniences: at least one of pr/commit/ack must arrive.
    #[serde(default)]
    evidence: Vec<Evidence>,
    #[serde(default)]
    pr: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    ack: Option<String>,
}

async fn issues_close(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<CloseBody>,
) -> Res<Issue> {
    let mut evidence = body.evidence;
    if let Some(pr) = body.pr {
        evidence.push(Evidence::pr(pr));
    }
    if let Some(commit) = body.commit {
        evidence.push(Evidence::commit(commit));
    }
    if let Some(ack) = body.ack {
        evidence.push(Evidence::ack(ack));
    }
    Ok(Json(api.db.close(
        &body.id,
        body.reason.as_deref(),
        &evidence,
        &principal.name,
        crate::db::now(),
    )?))
}

#[derive(Deserialize)]
struct SupersedeBody {
    id: String,
    replacement: String,
}

async fn issues_supersede(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<SupersedeBody>,
) -> Res<serde_json::Value> {
    api.db.supersede(
        &body.id,
        &body.replacement,
        &principal.name,
        crate::db::now(),
    )?;
    Ok(Json(
        serde_json::json!({"closed": body.id, "replacement": body.replacement}),
    ))
}

// ---- dependencies & labels ----

#[derive(Deserialize)]
struct DepBody {
    child: String,
    parent: String,
}

async fn deps_add(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<DepBody>,
) -> Res<serde_json::Value> {
    api.db
        .add_dep(&body.child, &body.parent, &principal.name, crate::db::now())?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn deps_remove(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<DepBody>,
) -> Res<serde_json::Value> {
    api.db
        .remove_dep(&body.child, &body.parent, &principal.name, crate::db::now())?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
struct LabelBody {
    id: String,
    label: String,
}

async fn labels_add(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<LabelBody>,
) -> Res<serde_json::Value> {
    api.db
        .add_label(&body.id, &body.label, &principal.name, crate::db::now())?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn labels_remove(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<LabelBody>,
) -> Res<serde_json::Value> {
    api.db
        .remove_label(&body.id, &body.label, &principal.name, crate::db::now())?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ---- claims ----

/// The scope rule in code: a human principal may only ever hold work as itself; an agent
/// principal claims on behalf of a named run/session — the daemon claiming for a not-yet-spawned
/// Edgerunner session is the normal, legitimate move.
#[derive(Deserialize)]
struct ClaimBody {
    id: String,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    ttl_seconds: Option<i64>,
}

async fn claims_claim(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<ClaimBody>,
) -> Res<ClaimReceipt> {
    let req = ClaimRequest {
        id: body.id,
        assignee: match principal.kind {
            ActorKind::Human => principal.name.clone(),
            ActorKind::Agent => body.assignee.clone().unwrap_or(principal.name.clone()),
        },
        actor_kind: principal.kind,
        ttl_seconds: body.ttl_seconds,
    };
    Ok(Json(api.db.claim(&req, crate::db::now())?))
}

async fn claims_touch(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<ClaimBody>,
) -> Res<serde_json::Value> {
    // Renewal only succeeds when the named holder matches the row; the store enforces it, so a
    // token cannot heartbeat a claim it does not own.
    let holder = match principal.kind {
        ActorKind::Human => principal.name.clone(),
        ActorKind::Agent => body.assignee.clone().unwrap_or(principal.name.clone()),
    };
    let expires = api.db.touch(&body.id, &holder, crate::db::now())?;
    Ok(Json(
        serde_json::json!({"id": body.id, "expires_at": expires}),
    ))
}

async fn claims_release(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<ClaimBody>,
) -> Res<serde_json::Value> {
    // Releasing someone else's live claim would be a steal; check the holder before clearing.
    let issue = api.db.get(&body.id)?;
    if let Some(holder) = &issue.assignee {
        let owns = match principal.kind {
            ActorKind::Human => holder == &principal.name,
            ActorKind::Agent => {
                body.assignee.as_deref() == Some(holder.as_str()) || holder == &principal.name
            }
        };
        if !owns {
            return Err(ApiError(ApiErrorKind::Status(
                StatusCode::FORBIDDEN,
                format!("{} is held by {holder}", body.id),
            )));
        }
    }
    api.db
        .release(&body.id, &principal.name, crate::db::now())?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Sweeping requeues other people's work; it is an operator action, and agent tokens — including
/// a daemon's — may not perform it.
async fn claims_sweep(
    State(api): State<Arc<Api>>,
    Extension(principal): Extension<Principal>,
    Json(_body): Json<Empty>,
) -> Res<SweepReport> {
    if principal.kind != ActorKind::Human {
        return Err(ApiError(ApiErrorKind::Status(
            StatusCode::FORBIDDEN,
            "only a human principal may sweep claims".to_string(),
        )));
    }
    Ok(Json(api.db.sweep(crate::db::now())?))
}

// ---- history & stats ----

async fn history_get(
    State(api): State<Arc<Api>>,
    Extension(_principal): Extension<Principal>,
    Json(body): Json<IdBody>,
) -> Res<Vec<HistoryEvent>> {
    Ok(Json(api.db.history(&body.id)?))
}

async fn stats(
    State(api): State<Arc<Api>>,
    Extension(_principal): Extension<Principal>,
    Json(_body): Json<Empty>,
) -> Res<serde_json::Value> {
    Ok(Json(api.db.stats()?))
}

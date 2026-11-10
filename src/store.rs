//! Store contract shared by the native SQLite backend and the wasm records backend.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[cfg(not(target_arch = "wasm32"))]
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[cfg(not(target_arch = "wasm32"))]
    #[error("postgres: {0}")]
    Postgres(#[from] postgres::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no issue {0}")]
    NotFound(String),
    #[error("no project {0}")]
    NoProject(String),
    #[error(
        "invalid status {0} (expected one of: open, in_progress, review, blocked, deferred, closed)"
    )]
    BadStatus(String),
    #[error("issue {0} is already claimed by {1}")]
    AlreadyClaimed(String, String),
    #[error("adding {child} -> {parent} would create a dependency cycle")]
    Cycle { child: String, parent: String },
    #[error("{0}")]
    Bad(String),
    #[error(
        "refusing to close {0}: done means delivered. Attach --pr/--commit evidence, or --ack \"no delivery expected: <reason>\" for non-code outcomes"
    )]
    NoEvidence(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Storage contract used by the hosted API.
///
/// Local operation deliberately remains a concrete database. Hosted backends implement this
/// narrow contract so transport/auth code cannot depend on SQLite internals.
pub trait Store: Send + Sync {
    fn ensure_project(&self, slug: &str, root: &str, prefix: &str) -> Result<()>;
    fn projects(&self) -> Result<Vec<(String, String, String)>>;
    fn ensure_folder(&self, folder: &Folder, at: i64) -> Result<()>;
    fn set_project_folder(&self, project: &str, folder: Option<&str>, at: i64) -> Result<()>;
    fn folder_view(&self) -> Result<FolderView>;
    fn pending_events(&self, at: i64, limit: usize) -> Result<Vec<OutboundEvent>>;
    /// The audit row that produced outbox event `seq` (one `seq` feeds both).
    fn event_context(&self, seq: i64) -> Result<Option<EventContext>>;
    fn mark_event_delivered(&self, seq: i64, at: i64) -> Result<()>;
    fn mark_event_failed(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()>;
    fn create(&self, spec: &NewIssue, at: i64) -> Result<String>;
    fn get(&self, id: &str) -> Result<Issue>;
    fn list(&self, project: Option<&str>, all: bool, only_id: Option<&str>) -> Result<Vec<Issue>>;
    fn ready(&self, project: Option<&str>, at: i64) -> Result<Vec<Issue>>;
    fn update(&self, id: &str, patch: &IssuePatch, actor: &str, at: i64) -> Result<Issue>;
    fn add_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()>;
    fn remove_label(&self, id: &str, label: &str, actor: &str, at: i64) -> Result<()>;
    fn add_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()>;
    fn remove_dep(&self, child: &str, parent: &str, actor: &str, at: i64) -> Result<()>;
    fn claim(&self, req: &ClaimRequest, at: i64) -> Result<ClaimReceipt>;
    fn touch(&self, id: &str, assignee: &str, at: i64) -> Result<i64>;
    fn release(&self, id: &str, actor: &str, at: i64) -> Result<()>;
    fn sweep(&self, at: i64) -> Result<SweepReport>;
    fn close(
        &self,
        id: &str,
        reason: Option<&str>,
        evidence: &[Evidence],
        actor: &str,
        at: i64,
    ) -> Result<Issue>;
    fn supersede(&self, id: &str, replacement: &str, actor: &str, at: i64) -> Result<()>;
    fn history(&self, id: &str) -> Result<Vec<HistoryEvent>>;
    fn stats(&self) -> Result<serde_json::Value>;
    fn prometheus_metrics(&self, at: i64) -> Result<String>;
}

pub trait CompanyStoreRegistry: Send + Sync {
    fn for_company(&self, company_id: &str) -> Result<Arc<dyn Store>>;
    fn sweep_open(&self, now: i64) -> Result<Vec<(String, SweepReport)>>;
    fn open_stores(&self) -> Result<Vec<(String, Arc<dyn Store>)>>;
    /// Read-only readiness check. Backends may probe their shared connection
    /// even before the first authenticated tenant has opened a store.
    fn health_check(&self) -> Result<()> {
        for (_, store) in self.open_stores()? {
            store.projects()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundEvent {
    pub seq: i64,
    pub store_id: String,
    pub issue_id: String,
    pub project: String,
    pub occurred_at: i64,
    pub event: String,
    pub attempts: i64,
}

/// The human-readable half of an audit row: who did it, and what it says.
///
/// The outbox carries the machine facts (`event`, `issue_id`, …); the sentence a
/// notification wants (`claimed by …`, `→ in_review`) is already in `history`,
/// written by the same `log_event` under the same `seq`. Reading it back beats
/// denormalizing two more columns into every outbound row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventContext {
    pub actor: String,
    pub detail: String,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Debug, Default, Serialize)]
pub struct ProjectRemoval {
    pub issues: i64,
}

#[derive(Debug, Default, Serialize)]
pub struct SweepReport {
    pub requeued: Vec<String>,
    pub escalated: Vec<String>,
}

//! The domain vocabulary. Field names on the wire deliberately mirror the Beads JSON shapes
//! (attribution: the interface this project follows was built by
//! [steveyegge/beads](https://github.com/steveyegge/beads), now gastownhall/beads), so a client
//! written against `bd --json` reads `marbles --json` unchanged.

use serde::{Deserialize, Serialize};

pub const STATUSES: [&str; 6] = [
    "open",
    "in_progress",
    "review",
    "blocked",
    "deferred",
    "closed",
];

pub fn is_terminal(status: &str) -> bool {
    matches!(status, "closed")
}

/// A delivered artifact proving the issue's outcome: the PR, the commit, or — for work that
/// legitimately produces no code — an operator's explicit acknowledgement. `closed` without one
/// of these is a claim without a receipt, which is the failure mode the state machine exists to
/// refuse: *done* means merged, `review` means there is something to look at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// "pr" | "commit" | "ack"
    pub kind: String,
    pub value: String,
}

impl Evidence {
    pub fn pr(url: impl Into<String>) -> Self {
        Self {
            kind: "pr".into(),
            value: url.into(),
        }
    }
    pub fn commit(sha: impl Into<String>) -> Self {
        Self {
            kind: "commit".into(),
            value: sha.into(),
        }
    }
    pub fn ack(reason: impl Into<String>) -> Self {
        Self {
            kind: "ack".into(),
            value: reason.into(),
        }
    }
}

/// Who holds a claim. The kind selects the TTL and the expiry behaviour, not just a label:
/// agents ride short heartbeat-renewed leases that re-queue silently, humans hold business-hours
/// deadlines whose expiry escalates rather than stealing the work back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Agent,
    Human,
}

impl ActorKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent" => Some(Self::Agent),
            "human" => Some(Self::Human),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Human => "human",
        }
    }
}

/// A dependency edge as reported on a bead: `dependencies` lists the beads this one waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub depends_on_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Unresolved outbound blockers (open things this issue waits on).
    #[serde(default)]
    pub dependency_count: i64,
    /// Open inbound dependents.
    #[serde(default)]
    pub dependent_count: i64,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    /// What justified the close (empty until it happens). See [`Evidence`].
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    /// `"agent"` | `"human"` | absent while unclaimed.
    #[serde(default)]
    pub actor_kind: Option<String>,
    /// Epoch seconds. The claim lapses at this instant unless heartbeated.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Epoch seconds. Ready-set eligibility starts here: creation grace plus edit deferral.
    #[serde(default)]
    pub available_at: Option<i64>,
    #[serde(default)]
    pub closed_at: Option<i64>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub project: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewIssue {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// Epoch seconds; `None` applies the default grace.
    #[serde(default)]
    pub available_at: Option<i64>,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssuePatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub append_notes: Option<String>,
    #[serde(default)]
    pub available_at: Option<i64>,
    /// Record delivery artifacts (PR opened, commit pushed) without closing: `review` is the
    /// state where the evidence exists but nobody has merged it yet.
    #[serde(default)]
    pub evidence: Option<Vec<Evidence>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloseRequest {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub id: String,
    pub assignee: String,
    pub actor_kind: ActorKind,
    /// Overrides the kind's default TTL, clamped by policy.
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimReceipt {
    pub id: String,
    pub assignee: String,
    pub actor_kind: ActorKind,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub issue_id: String,
    pub ts: i64,
    pub actor: String,
    pub event: String,
    #[serde(default)]
    pub detail: String,
}

/// Claim policy knobs. Defaults encode the FPL position: agent leases are minutes and mechanical,
/// human holds are days and escalate rather than reclaim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub agent_ttl_seconds: i64,
    /// Business hours a human claim spans, not wall-clock hours.
    pub human_ttl_business_hours: i64,
    /// Silence after which a new issue becomes swarm-visible.
    pub creation_grace_seconds: i64,
    /// Each edit pushes eligibility this far out, so live speccing sessions never leak work.
    pub edit_deferral_seconds: i64,
    /// Hard ceiling on any explicit ttl override, so a typo cannot park a bead for a year.
    pub max_ttl_seconds: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            agent_ttl_seconds: 30 * 60,
            human_ttl_business_hours: 48,
            creation_grace_seconds: 10 * 60,
            edit_deferral_seconds: 2 * 60,
            max_ttl_seconds: 7 * 24 * 3600,
        }
    }
}

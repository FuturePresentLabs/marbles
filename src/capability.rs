//! Persistent capabilities and the evidence that they actually work.
//!
//! A task is work: it is claimed, it closes, and it stops existing as a question. A
//! **capability** is functionality that outlives any one task — Transmog's native engine,
//! Panopticon's geospatial provider, the Soil-to-provider bridge. It is improved by many tasks,
//! verified at several layers, measured repeatedly, and eventually superseded. Keeping that in
//! an issue row would mean the issue's status had to mean four unrelated things at once, which is
//! exactly the conflation the integration ledger warned about.
//!
//! These are therefore *typed records in the same `soil:records` store*, not a second database
//! and not another project-management surface:
//!
//! ```text
//! capability/["transmog.batch-provider"]        the persistent thing
//! verification/["…loam.1795632523"]             one observation at one layer, bound to a revision
//! measurement/["…dfm_seconds.1795632523"]       one observation of one metric
//! decision/["DEC-002"]                          an architectural or commercial decision
//! ```
//!
//! Four independent axes, deliberately not one enum:
//!
//! | axis | values | meaning |
//! |---|---|---|
//! | `implementation` | not_started … execution_verified | how far the code has got |
//! | `health` | healthy, degraded, blocked, unknown | whether it works *right now* |
//! | `lifecycle` | proposed, active, deprecated, superseded, retired | whether it is still the thing |
//! | `commercial_readiness` | research … production_ready | what may be sold or piloted |
//!
//! Work status stays on issues. `health = blocked` is not an issue status and does not stop a
//! claim; it means a capability known to be broken should not be counted as pilot-ready.
//!
//! ## Verification is bound to an implementation
//!
//! A verification observation names the revision (`implementation_id`, usually a commit) it
//! tested. A pass at revision A is *not* inherited by revision B: the layer reports
//! `revision_mismatch` and the capability has a gap. That is the mechanical form of “a changed
//! implementation must not silently inherit verification that only applied to an earlier
//! revision.” Freshness is computed on read, never stored, exactly like remote assertions:
//!
//! * `fresh` — same revision and inside the age window;
//! * `stale` — same revision, older than the window;
//! * `revision_mismatch` — the observation tested a different revision;
//! * `unverifiable` — a future timestamp (broken clock or forgery), fails closed;
//! * `unknown` — no observation exists for the layer.
//!
//! A signed assertion would establish *who* observed it, not that the engineering result is
//! true. These records are deliberately unsigned local observations; the existing
//! `marbles.issue-assertion/1` signer/pin machinery is the right transport if one is ever
//! federated, and a signature would not upgrade `pass` into independent truth.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::Adapter;
use crate::records::{Column, ColumnType, Key, RecordSchema};

pub const CAPABILITY: &str = "capability";
pub const VERIFICATION: &str = "verification";
pub const MEASUREMENT: &str = "measurement";
pub const DECISION: &str = "decision";

/// Contract maturity answers “how settled is the interface?”, not “how finished is the code?”.
pub const CONTRACT_MATURITY: &[&str] =
    &["not_started", "proposed", "draft", "validated", "versioned"];
/// Implementation progress. `blocked` and `deprecated` are deliberately absent: they are health
/// and lifecycle, and folding them in here would make “how far did we get?” unanswerable.
pub const IMPLEMENTATION: &[&str] = &[
    "not_started",
    "in_progress",
    "implemented",
    "build_verified",
    "execution_verified",
];
pub const HEALTH: &[&str] = &["healthy", "degraded", "blocked", "unknown"];
pub const LIFECYCLE: &[&str] = &["proposed", "active", "deprecated", "superseded", "retired"];
pub const COMMERCIAL_READINESS: &[&str] = &[
    "research",
    "internal_prototype",
    "internal_validated",
    "demo_ready",
    "pilot_ready",
    "production_ready",
];
/// The execution layers an observation can name, independently of each other.
pub const LAYERS: &[&str] = &[
    "native",
    "sidecar",
    "wit",
    "loam",
    "soil",
    "studio",
    "commercial",
];
pub const RESULTS: &[&str] = &["pass", "fail", "not_tested", "not_applicable", "blocked"];
/// How the observation was obtained. `reported` is a real record (a commit, a delivery log, an
/// operator report); `reproduced` means the observer re-ran the workload and saw the result.
/// Neither word upgrades the engineering outcome, and readiness reports them side by side.
pub const REPRODUCTION: &[&str] = &["reported", "reproduced", "not_applicable"];
pub const DECISION_STATUS: &[&str] = &["proposed", "accepted", "rejected", "superseded"];

/// Default age after which a same-revision observation is labelled `stale`. Verification is not
/// a per-minute poll like a remote issue; thirty days is short enough to force a re-check when a
/// release is cut and long enough not to nag about a stable native engine.
pub const DEFAULT_MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
/// A timestamp materially in the future is a broken clock or a forged record. Fail closed.
pub const MAX_CLOCK_SKEW_SECONDS: i64 = 5 * 60;

/// Record type declarations. One JSON body each; the natural key is `id` for all four, because
/// observations are immutable facts keyed by when they were made (or by a stable decision id).
pub fn schemas() -> Vec<RecordSchema> {
    let cols = |name: &str| RecordSchema {
        name: name.to_string(),
        key: vec!["id".into()],
        columns: vec![
            Column {
                name: "id".into(),
                ty: ColumnType::Text,
            },
            Column {
                name: "seq".into(),
                ty: ColumnType::Int,
            },
            Column {
                name: "deleted".into(),
                ty: ColumnType::Bool,
            },
            Column {
                name: "data".into(),
                ty: ColumnType::Json,
            },
        ],
        data_column: "data".into(),
    };
    vec![
        cols(CAPABILITY),
        cols(VERIFICATION),
        cols(MEASUREMENT),
        cols(DECISION),
    ]
}

fn require_vocab(field: &str, value: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "{field} {value:?} is not one of: {}",
            allowed.join(", ")
        ))
    }
}

fn check_id(field: &str, id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    if id.chars().any(char::is_whitespace) {
        return Err(format!("{field} {id:?} contains whitespace"));
    }
    if id.contains(crate::records::KEY_SEPARATOR) {
        return Err(format!("{field} {id:?} contains a reserved separator"));
    }
    if id.len() > 200 {
        return Err(format!("{field} is longer than 200 bytes"));
    }
    Ok(())
}

/// A stable id for an observation: readable prefix plus a fingerprint of everything that makes
/// the observation distinct. Two observations in the same second do not collide, and the same
/// observation submitted twice is the same record rather than a duplicate.
fn observation_id(prefix: &[&str], observed_at: i64, fields: &[&str]) -> String {
    // FNV-1a: tiny, portable, and no dependency beyond core. This is an identity, not a
    // security boundary.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for field in fields {
        eat(field.as_bytes());
    }
    eat(&observed_at.to_le_bytes());
    let mut id = prefix.join(".");
    id.push('.');
    id.push_str(&observed_at.to_string());
    id.push('.');
    id.push_str(&format!("{hash:016x}"));
    id
}

// ---------------------------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub owner: String,
    /// The workstream or repository this belongs to. Not an issue project: a capability can be
    /// shared by several repos, and naming one does not create a project row.
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub contract_version: String,
    pub contract_maturity: String,
    pub implementation: String,
    pub health: String,
    pub lifecycle: String,
    pub commercial_readiness: String,
    /// The revision this capability currently is. Verification only counts against this value.
    #[serde(default)]
    pub implementation_id: String,
    /// Platform integration gates: optional infrastructure layers (Soil, Studio, …). A missing
    /// platform gate is reported, but it does not block a commercial pilot on its own.
    #[serde(default)]
    pub required_layers: Vec<String>,
    /// Commercial acceptance gates: the layers a customer-facing pilot actually requires. These
    /// are the gates `pilot-blockers` reports. Empty means “no commercial gate declared yet”.
    #[serde(default)]
    pub commercial_gates: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub contract_version: Option<String>,
    #[serde(default)]
    pub contract_maturity: Option<String>,
    #[serde(default)]
    pub implementation: Option<String>,
    #[serde(default)]
    pub health: Option<String>,
    #[serde(default)]
    pub lifecycle: Option<String>,
    #[serde(default)]
    pub commercial_readiness: Option<String>,
    #[serde(default)]
    pub implementation_id: Option<String>,
    #[serde(default)]
    pub required_layers: Option<Vec<String>>,
    #[serde(default)]
    pub commercial_gates: Option<Vec<String>>,
    #[serde(default)]
    pub superseded_by: Option<String>,
}

impl CapabilityPatch {
    fn apply(&self, capability: &mut Capability) {
        if let Some(v) = &self.title {
            capability.title = v.clone();
        }
        if let Some(v) = &self.owner {
            capability.owner = v.clone();
        }
        if let Some(v) = &self.project {
            capability.project = v.clone();
        }
        if let Some(v) = &self.description {
            capability.description = v.clone();
        }
        if let Some(v) = &self.contract_version {
            capability.contract_version = v.clone();
        }
        if let Some(v) = &self.contract_maturity {
            capability.contract_maturity = v.clone();
        }
        if let Some(v) = &self.implementation {
            capability.implementation = v.clone();
        }
        if let Some(v) = &self.health {
            capability.health = v.clone();
        }
        if let Some(v) = &self.lifecycle {
            capability.lifecycle = v.clone();
        }
        if let Some(v) = &self.commercial_readiness {
            capability.commercial_readiness = v.clone();
        }
        if let Some(v) = &self.implementation_id {
            capability.implementation_id = v.clone();
        }
        if let Some(v) = &self.required_layers {
            capability.required_layers = v.clone();
        }
        if let Some(v) = &self.commercial_gates {
            capability.commercial_gates = v.clone();
        }
        if let Some(v) = &self.superseded_by {
            capability.superseded_by = if v.is_empty() { None } else { Some(v.clone()) };
            if capability.superseded_by.is_some() && self.lifecycle.is_none() {
                capability.lifecycle = "superseded".into();
            }
        }
    }
}

fn validate_capability(capability: &Capability) -> Result<(), String> {
    check_id("capability id", &capability.id)?;
    if capability.title.trim().is_empty() {
        return Err("capability title is empty".into());
    }
    require_vocab(
        "contract_maturity",
        &capability.contract_maturity,
        CONTRACT_MATURITY,
    )?;
    require_vocab("implementation", &capability.implementation, IMPLEMENTATION)?;
    require_vocab("health", &capability.health, HEALTH)?;
    require_vocab("lifecycle", &capability.lifecycle, LIFECYCLE)?;
    require_vocab(
        "commercial_readiness",
        &capability.commercial_readiness,
        COMMERCIAL_READINESS,
    )?;
    if capability
        .implementation_id
        .chars()
        .any(char::is_whitespace)
    {
        return Err("implementation_id contains whitespace".into());
    }
    for layer in capability
        .required_layers
        .iter()
        .chain(capability.commercial_gates.iter())
    {
        require_vocab("gate layer", layer, LAYERS)?;
    }
    Ok(())
}

pub fn create(
    store: &Adapter,
    id: &str,
    patch: &CapabilityPatch,
    now: i64,
) -> Result<Capability, String> {
    check_id("capability id", id)?;
    if get(store, id).is_ok() {
        return Err(format!("capability {id:?} already exists"));
    }
    let mut capability = Capability {
        id: id.to_string(),
        title: String::new(),
        owner: String::new(),
        project: String::new(),
        description: String::new(),
        contract_version: String::new(),
        contract_maturity: "not_started".into(),
        implementation: "not_started".into(),
        health: "unknown".into(),
        lifecycle: "proposed".into(),
        commercial_readiness: "research".into(),
        implementation_id: String::new(),
        required_layers: Vec::new(),
        commercial_gates: Vec::new(),
        superseded_by: None,
        created_at: now,
        updated_at: now,
    };
    patch.apply(&mut capability);
    if capability.title.trim().is_empty() {
        return Err("capability create needs --title".into());
    }
    validate_capability(&capability)?;
    if let Some(target) = &capability.superseded_by {
        get(store, target).map_err(|_| {
            format!("superseded_by names unknown capability {target:?}; create it first")
        })?;
    }
    put(store, &capability)?;
    Ok(capability)
}

pub fn update(
    store: &Adapter,
    id: &str,
    patch: &CapabilityPatch,
    now: i64,
) -> Result<Capability, String> {
    let mut capability = get(store, id)?;
    patch.apply(&mut capability);
    capability.updated_at = now;
    validate_capability(&capability)?;
    if let Some(target) = &capability.superseded_by {
        if target == id {
            return Err("a capability cannot supersede itself".into());
        }
        get(store, target).map_err(|_| {
            format!("superseded_by names unknown capability {target:?}; create it first")
        })?;
    }
    put(store, &capability)?;
    Ok(capability)
}

fn put(store: &Adapter, capability: &Capability) -> Result<(), String> {
    let value = serde_json::to_value(capability).map_err(|e| e.to_string())?;
    store
        .record_put(CAPABILITY, &Key::new([capability.id.clone()]), value)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn get(store: &Adapter, id: &str) -> Result<Capability, String> {
    let key = Key::new([id.to_string()]);
    let value = store
        .record_get(CAPABILITY, &key)
        .ok_or_else(|| format!("no capability {id:?}"))?;
    serde_json::from_value(value).map_err(|e| format!("capability {id:?} is malformed: {e}"))
}

pub fn list(store: &Adapter) -> Vec<Capability> {
    let mut all: Vec<Capability> = store
        .record_keys(CAPABILITY)
        .iter()
        .filter_map(|key| store.record_get(CAPABILITY, key))
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect();
    all.sort_by(|a, b| a.id.cmp(&b.id));
    all
}

// ---------------------------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub id: String,
    pub capability_id: String,
    pub layer: String,
    pub result: String,
    /// The revision this observation tested. The whole point of the record.
    pub implementation_id: String,
    #[serde(default)]
    pub artifact_digest: String,
    #[serde(default)]
    pub workload: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub limitations: String,
    #[serde(default)]
    pub verifier: String,
    /// `reported` | `reproduced` | `not_applicable`. Defaults to `reported` when absent, because
    /// that is the weaker and safer claim.
    #[serde(default)]
    pub reproduction: String,
    /// The criterion this workload was judged against, so a pass cannot be read as a broader
    /// claim than the one that was tested.
    #[serde(default)]
    pub acceptance: String,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewVerification {
    #[serde(default)]
    pub capability_id: String,
    #[serde(default)]
    pub layer: String,
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub implementation_id: String,
    #[serde(default)]
    pub artifact_digest: String,
    #[serde(default)]
    pub workload: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub limitations: String,
    #[serde(default)]
    pub verifier: String,
    #[serde(default)]
    pub reproduction: Option<String>,
    #[serde(default)]
    pub acceptance: String,
    #[serde(default)]
    pub observed_at: Option<i64>,
}

/// Record one observation. Several observations of the same layer and revision are allowed and
/// preserved; the newest one is what a readiness view reports.
pub fn verify(store: &Adapter, spec: &NewVerification, now: i64) -> Result<Verification, String> {
    get(store, &spec.capability_id)?;
    require_vocab("layer", &spec.layer, LAYERS)?;
    require_vocab("result", &spec.result, RESULTS)?;
    if spec.implementation_id.trim().is_empty() {
        return Err(
            "verification needs --implementation-id: an observation must name the revision it tested"
                .into(),
        );
    }
    if spec.implementation_id.chars().any(char::is_whitespace) {
        return Err("implementation_id contains whitespace".into());
    }
    let reproduction = spec.reproduction.as_deref().unwrap_or("reported");
    require_vocab("reproduction", reproduction, REPRODUCTION)?;
    if spec.result == "pass"
        && spec.evidence.trim().is_empty()
        && spec.command.trim().is_empty()
        && spec.workload.trim().is_empty()
        && spec.artifact_digest.trim().is_empty()
    {
        return Err(
            "a pass without --evidence, --command, --workload or --artifact-digest is a claim, not an observation"
                .into(),
        );
    }
    let observed_at = spec.observed_at.unwrap_or(now);
    if observed_at > now + MAX_CLOCK_SKEW_SECONDS {
        return Err(format!(
            "observed_at {observed_at} is more than {MAX_CLOCK_SKEW_SECONDS}s in the future; refusing to record"
        ));
    }
    let verification = Verification {
        id: observation_id(
            &[&spec.capability_id, &spec.layer],
            observed_at,
            &[
                &spec.result,
                &spec.implementation_id,
                &spec.artifact_digest,
                &spec.workload,
                &spec.command,
                &spec.evidence,
                &spec.acceptance,
                reproduction,
            ],
        ),
        capability_id: spec.capability_id.clone(),
        layer: spec.layer.clone(),
        result: spec.result.clone(),
        implementation_id: spec.implementation_id.clone(),
        artifact_digest: spec.artifact_digest.clone(),
        workload: spec.workload.clone(),
        command: spec.command.clone(),
        evidence: spec.evidence.clone(),
        limitations: spec.limitations.clone(),
        verifier: spec.verifier.clone(),
        reproduction: reproduction.to_string(),
        acceptance: spec.acceptance.clone(),
        observed_at,
    };
    let value = serde_json::to_value(&verification).map_err(|e| e.to_string())?;
    store
        .record_put(VERIFICATION, &Key::new([verification.id.clone()]), value)
        .map_err(|e| e.to_string())?;
    Ok(verification)
}

pub fn verifications(store: &Adapter) -> Vec<Verification> {
    let mut all: Vec<Verification> = store
        .record_keys(VERIFICATION)
        .iter()
        .filter_map(|key| store.record_get(VERIFICATION, key))
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect();
    all.sort_by(|a, b| {
        b.observed_at
            .cmp(&a.observed_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    all
}

fn verifications_for(store: &Adapter, capability_id: &str) -> Vec<Verification> {
    verifications(store)
        .into_iter()
        .filter(|v| v.capability_id == capability_id)
        .collect()
}

/// Computed, never stored. A signed observation proves provenance, not truth; an unsigned one is
/// simply a local observation with no provenance claim at all.
pub fn freshness(
    verification: &Verification,
    capability: &Capability,
    now: i64,
    max_age_seconds: i64,
) -> &'static str {
    if verification.observed_at > now + MAX_CLOCK_SKEW_SECONDS {
        "unverifiable"
    } else if verification.implementation_id != capability.implementation_id {
        "revision_mismatch"
    } else if now - verification.observed_at > max_age_seconds {
        "stale"
    } else {
        "fresh"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LayerState {
    pub layer: String,
    /// `pass` | `fail` | `not_tested` | `not_applicable` | `blocked` | `missing`.
    pub status: String,
    /// `fresh` | `stale` | `revision_mismatch` | `unverifiable` | `unknown`.
    pub freshness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limitations: Option<String>,
    /// `reported` | `reproduced` | `not_applicable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reproduction: Option<String>,
    /// The workload-specific criterion this observation was judged against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<String>,
}

fn layer_state(
    layer: &str,
    observations: &[Verification],
    capability: &Capability,
    now: i64,
    max_age_seconds: i64,
) -> LayerState {
    let mut for_layer: Vec<&Verification> =
        observations.iter().filter(|v| v.layer == layer).collect();
    for_layer.sort_by(|a, b| {
        b.observed_at
            .cmp(&a.observed_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    let report = |v: &Verification, freshness: &str| LayerState {
        layer: layer.to_string(),
        status: v.result.clone(),
        freshness: freshness.to_string(),
        implementation_id: Some(v.implementation_id.clone()),
        observed_at: Some(v.observed_at),
        evidence: (!v.evidence.is_empty()).then(|| v.evidence.clone()),
        command: (!v.command.is_empty()).then(|| v.command.clone()),
        workload: (!v.workload.is_empty()).then(|| v.workload.clone()),
        limitations: (!v.limitations.is_empty()).then(|| v.limitations.clone()),
        reproduction: (!v.reproduction.is_empty()).then(|| v.reproduction.clone()),
        acceptance: (!v.acceptance.is_empty()).then(|| v.acceptance.clone()),
    };
    // Prefer an observation of the revision the capability currently is. If there is none, the
    // newest observation still describes a real past result — but it belongs to a different
    // revision, so it is reported as `revision_mismatch`, never silently inherited.
    if let Some(current) = for_layer
        .iter()
        .find(|v| v.implementation_id == capability.implementation_id)
    {
        return report(
            current,
            freshness(current, capability, now, max_age_seconds),
        );
    }
    match for_layer.first() {
        Some(previous) => report(previous, "revision_mismatch"),
        None => LayerState {
            layer: layer.to_string(),
            status: "missing".into(),
            freshness: "unknown".into(),
            implementation_id: None,
            observed_at: None,
            evidence: None,
            command: None,
            workload: None,
            limitations: None,
            reproduction: None,
            acceptance: None,
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityView {
    pub id: String,
    pub title: String,
    pub owner: String,
    pub project: String,
    pub description: String,
    pub contract_version: String,
    pub contract_maturity: String,
    pub implementation: String,
    pub health: String,
    pub lifecycle: String,
    pub commercial_readiness: String,
    pub implementation_id: String,
    /// Platform integration gates (optional infrastructure; a miss does not block a pilot).
    pub required_layers: Vec<String>,
    /// Commercial acceptance gates (a miss does block a commercial pilot).
    pub commercial_gates: Vec<String>,
    pub superseded_by: Option<String>,
    pub updated_at: i64,
    pub layers: Vec<LayerState>,
    /// Every declared gate that is not a fresh `pass` (platform + commercial).
    pub gaps: Vec<String>,
    /// Platform integration gates that are not a fresh `pass`.
    pub platform_gaps: Vec<String>,
    /// Commercial acceptance gates that are not a fresh `pass`.
    pub commercial_gaps: Vec<String>,
    /// Layers whose newest evidence is stale, bound to another revision, or unverifiable.
    pub stale: Vec<String>,
    /// Human-readable reasons this capability is not commercial-pilot-ready. Platform gaps are
    /// deliberately absent: an optional integration must not block a viable pilot.
    pub blockers: Vec<String>,
    /// Platform-only gaps, reported so the operator can see them without being blocked by them.
    pub platform_blockers: Vec<String>,
    pub technically_ready: bool,
    pub commercial_ready: bool,
}

fn view(
    store: &Adapter,
    capability: &Capability,
    now: i64,
    max_age_seconds: i64,
) -> CapabilityView {
    let observations = verifications_for(store, &capability.id);
    let mut layers: BTreeSet<String> = capability
        .required_layers
        .iter()
        .chain(capability.commercial_gates.iter())
        .cloned()
        .collect();
    for observation in &observations {
        layers.insert(observation.layer.clone());
    }
    let states: Vec<LayerState> = layers
        .into_iter()
        .map(|layer| layer_state(&layer, &observations, capability, now, max_age_seconds))
        .collect();

    let unmet = |gates: &[String]| -> Vec<String> {
        gates
            .iter()
            .filter(|layer| {
                !states
                    .iter()
                    .any(|s| &s.layer == *layer && s.status == "pass" && s.freshness == "fresh")
            })
            .cloned()
            .collect()
    };
    let platform_gaps = unmet(&capability.required_layers);
    let commercial_gaps = unmet(&capability.commercial_gates);
    let mut gaps: Vec<String> = platform_gaps.clone();
    for gap in &commercial_gaps {
        if !gaps.contains(gap) {
            gaps.push(gap.clone());
        }
    }
    let stale: Vec<String> = states
        .iter()
        .filter(|s| {
            matches!(
                s.freshness.as_str(),
                "stale" | "revision_mismatch" | "unverifiable"
            )
        })
        .map(|s| s.layer.clone())
        .collect();

    let describe = |gap: &String| -> String {
        match states.iter().find(|s| &s.layer == gap) {
            Some(state) => format!("{gap}: {} ({})", state.status, state.freshness),
            None => format!("{gap}: missing"),
        }
    };
    let mut blockers = Vec::new();
    if capability.health == "blocked" {
        blockers.push("health: blocked".to_string());
    }
    if capability.lifecycle != "active" {
        blockers.push(format!("lifecycle: {}", capability.lifecycle));
    }
    for gap in &commercial_gaps {
        blockers.push(format!("commercial gate {}", describe(gap)));
    }
    let platform_blockers: Vec<String> = platform_gaps
        .iter()
        .map(|gap| format!("platform gate {}", describe(gap)))
        .collect();
    let healthy = capability.lifecycle == "active" && capability.health != "blocked";
    let technically_ready = healthy && gaps.is_empty();
    let commercial_ready = healthy && commercial_gaps.is_empty();

    CapabilityView {
        id: capability.id.clone(),
        title: capability.title.clone(),
        owner: capability.owner.clone(),
        project: capability.project.clone(),
        description: capability.description.clone(),
        contract_version: capability.contract_version.clone(),
        contract_maturity: capability.contract_maturity.clone(),
        implementation: capability.implementation.clone(),
        health: capability.health.clone(),
        lifecycle: capability.lifecycle.clone(),
        commercial_readiness: capability.commercial_readiness.clone(),
        implementation_id: capability.implementation_id.clone(),
        required_layers: capability.required_layers.clone(),
        commercial_gates: capability.commercial_gates.clone(),
        superseded_by: capability.superseded_by.clone(),
        updated_at: capability.updated_at,
        layers: states,
        gaps,
        platform_gaps,
        commercial_gaps,
        stale,
        blockers,
        platform_blockers,
        technically_ready,
        commercial_ready,
    }
}

pub fn views(store: &Adapter, now: i64, max_age_seconds: i64) -> Vec<CapabilityView> {
    list(store)
        .iter()
        .map(|capability| view(store, capability, now, max_age_seconds))
        .collect()
}

pub fn show(store: &Adapter, id: &str, now: i64, max_age_seconds: i64) -> Result<Value, String> {
    let capability = get(store, id)?;
    let view = view(store, &capability, now, max_age_seconds);
    let observations: Vec<Verification> = verifications_for(store, id);
    let measurements: Vec<Measurement> = measurements(store)
        .into_iter()
        .filter(|m| m.capability_id == id)
        .collect();
    let decisions: Vec<Decision> = decisions(store)
        .into_iter()
        .filter(|d| d.affects.iter().any(|a| a == id))
        .collect();
    Ok(serde_json::json!({
        "capability": view,
        "verifications": observations,
        "measurements": measurements,
        "decisions": decisions,
    }))
}

/// Observations of one layer, across capabilities, with freshness against each capability's
/// current revision. This answers “which revisions have verified Loam execution?” without
/// pretending an old revision's pass applies to today's code.
pub fn verified(
    store: &Adapter,
    layer: Option<&str>,
    now: i64,
    max_age_seconds: i64,
) -> Result<Vec<Value>, String> {
    if let Some(layer) = layer {
        require_vocab("layer", layer, LAYERS)?;
    }
    let capabilities: BTreeMap<String, Capability> =
        list(store).into_iter().map(|c| (c.id.clone(), c)).collect();
    let mut out = Vec::new();
    for observation in verifications(store) {
        if observation.result != "pass" {
            continue;
        }
        if layer.is_some_and(|wanted| observation.layer != wanted) {
            continue;
        }
        let Some(capability) = capabilities.get(&observation.capability_id) else {
            continue;
        };
        out.push(serde_json::json!({
            "capability_id": observation.capability_id,
            "layer": observation.layer,
            "result": observation.result,
            "implementation_id": observation.implementation_id,
            "current_implementation_id": capability.implementation_id,
            "freshness": freshness(&observation, capability, now, max_age_seconds),
            "observed_at": observation.observed_at,
            "workload": observation.workload,
            "command": observation.command,
            "evidence": observation.evidence,
            "artifact_digest": observation.artifact_digest,
            "limitations": observation.limitations,
            "reproduction": observation.reproduction,
            "acceptance": observation.acceptance,
        }));
    }
    Ok(out)
}

/// Evidence that exists but cannot be relied on as current: too old, bound to another revision,
/// or carrying an impossible timestamp. `missing` belongs in `gaps`, not here.
pub fn stale(store: &Adapter, now: i64, max_age_seconds: i64) -> Vec<Value> {
    let capabilities: BTreeMap<String, Capability> =
        list(store).into_iter().map(|c| (c.id.clone(), c)).collect();
    let mut out = Vec::new();
    for observation in verifications(store) {
        let Some(capability) = capabilities.get(&observation.capability_id) else {
            continue;
        };
        let label = freshness(&observation, capability, now, max_age_seconds);
        if matches!(label, "stale" | "revision_mismatch" | "unverifiable") {
            out.push(serde_json::json!({
                "capability_id": observation.capability_id,
                "layer": observation.layer,
                "result": observation.result,
                "implementation_id": observation.implementation_id,
                "current_implementation_id": capability.implementation_id,
                "freshness": label,
                "observed_at": observation.observed_at,
                "limitations": observation.limitations,
                "evidence": observation.evidence,
                "reproduction": observation.reproduction,
                "acceptance": observation.acceptance,
            }));
        }
    }
    out
}

/// Required layers that are not a fresh pass. `--layer soil` answers “which capabilities lack
/// Soil verification?” and, because a changed revision is a gap, “which capabilities need
/// re-verification after a change?”.
pub fn gaps(
    store: &Adapter,
    layer: &str,
    now: i64,
    max_age_seconds: i64,
) -> Result<Vec<Value>, String> {
    require_vocab("layer", layer, LAYERS)?;
    let mut out = Vec::new();
    for capability in list(store) {
        let platform_gate = capability.required_layers.iter().any(|l| l == layer);
        let commercial_gate = capability.commercial_gates.iter().any(|l| l == layer);
        if !platform_gate && !commercial_gate {
            continue;
        }
        let view = view(store, &capability, now, max_age_seconds);
        let state = view.layers.iter().find(|s| s.layer == layer);
        let satisfied = state.is_some_and(|s| s.status == "pass" && s.freshness == "fresh");
        if satisfied {
            continue;
        }
        out.push(serde_json::json!({
            "capability_id": capability.id,
            "layer": layer,
            "status": state.map(|s| s.status.clone()).unwrap_or_else(|| "missing".into()),
            "freshness": state.map(|s| s.freshness.clone()).unwrap_or_else(|| "unknown".into()),
            "implementation_id": state.and_then(|s| s.implementation_id.clone()),
            "current_implementation_id": capability.implementation_id,
            "observed_at": state.and_then(|s| s.observed_at),
            "limitations": state.and_then(|s| s.limitations.clone()),
            "gate": match (platform_gate, commercial_gate) {
                (true, true) => "platform+commercial",
                (true, false) => "platform",
                (false, true) => "commercial",
                (false, false) => "none",
            },
            "blocks_commercial_pilot": commercial_gate,
        }));
    }
    Ok(out)
}

/// What is keeping a commercial pilot from starting: the capabilities that claim some commercial
/// readiness above prototype and still have a health, lifecycle, or verification blocker.
pub fn pilot_blockers(store: &Adapter, now: i64, max_age_seconds: i64) -> Vec<Value> {
    views(store, now, max_age_seconds)
        .into_iter()
        .filter(|v| {
            matches!(
                v.commercial_readiness.as_str(),
                "demo_ready" | "pilot_ready" | "production_ready"
            ) && !v.blockers.is_empty()
        })
        .map(|v| {
            serde_json::json!({
                "capability_id": v.id,
                "commercial_readiness": v.commercial_readiness,
                "implementation": v.implementation,
                "health": v.health,
                "implementation_id": v.implementation_id,
                "commercial_gates": v.commercial_gates,
                "commercial_gaps": v.commercial_gaps,
                "platform_gaps": v.platform_gaps,
                "stale": v.stale,
                "blockers": v.blockers,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub id: String,
    pub capability_id: String,
    pub metric: String,
    pub value: f64,
    pub unit: String,
    #[serde(default)]
    pub workload: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default = "one")]
    pub samples: i64,
    #[serde(default)]
    pub domain_outcome: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub limitations: String,
    pub observed_at: i64,
}

fn one() -> i64 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewMeasurement {
    #[serde(default)]
    pub capability_id: String,
    #[serde(default)]
    pub metric: String,
    #[serde(default)]
    pub value: f64,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub workload: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub samples: Option<i64>,
    #[serde(default)]
    pub domain_outcome: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub limitations: String,
    #[serde(default)]
    pub observed_at: Option<i64>,
}

pub fn measure(store: &Adapter, spec: &NewMeasurement, now: i64) -> Result<Measurement, String> {
    get(store, &spec.capability_id)?;
    if spec.metric.trim().is_empty() {
        return Err("measurement needs --metric".into());
    }
    if spec.metric.chars().any(char::is_whitespace) {
        return Err("metric contains whitespace; use a single token such as dfm_seconds".into());
    }
    if spec.unit.trim().is_empty() {
        return Err("measurement needs --unit (s, ms, usd, count, …)".into());
    }
    if !spec.value.is_finite() {
        return Err("measurement value is not finite".into());
    }
    let samples = spec.samples.unwrap_or(1);
    if samples < 1 {
        return Err("--samples must be at least 1".into());
    }
    let observed_at = spec.observed_at.unwrap_or(now);
    if observed_at > now + MAX_CLOCK_SKEW_SECONDS {
        return Err(format!(
            "observed_at {observed_at} is more than {MAX_CLOCK_SKEW_SECONDS}s in the future; refusing to record"
        ));
    }
    let measurement = Measurement {
        id: observation_id(
            &[&spec.capability_id, &spec.metric],
            observed_at,
            &[
                &spec.value.to_bits().to_string(),
                &spec.unit,
                &spec.workload,
                &spec.profile,
                &spec.domain_outcome,
            ],
        ),
        capability_id: spec.capability_id.clone(),
        metric: spec.metric.clone(),
        value: spec.value,
        unit: spec.unit.clone(),
        workload: spec.workload.clone(),
        profile: spec.profile.clone(),
        samples,
        domain_outcome: spec.domain_outcome.clone(),
        evidence: spec.evidence.clone(),
        limitations: spec.limitations.clone(),
        observed_at,
    };
    let value = serde_json::to_value(&measurement).map_err(|e| e.to_string())?;
    store
        .record_put(MEASUREMENT, &Key::new([measurement.id.clone()]), value)
        .map_err(|e| e.to_string())?;
    Ok(measurement)
}

pub fn measurements(store: &Adapter) -> Vec<Measurement> {
    let mut all: Vec<Measurement> = store
        .record_keys(MEASUREMENT)
        .iter()
        .filter_map(|key| store.record_get(MEASUREMENT, key))
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect();
    all.sort_by(|a, b| {
        b.observed_at
            .cmp(&a.observed_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    all
}

/// The latest observation of each `(capability, metric)` pair. This is the baseline a number is
/// compared against — not an average hidden in a document, and not a forecast.
pub fn baseline(store: &Adapter, metric: Option<&str>) -> Vec<Measurement> {
    let mut latest: BTreeMap<(String, String), Measurement> = BTreeMap::new();
    for measurement in measurements(store) {
        if metric.is_some_and(|wanted| measurement.metric != wanted) {
            continue;
        }
        let key = (
            measurement.capability_id.clone(),
            measurement.metric.clone(),
        );
        match latest.get(&key) {
            Some(existing) if existing.observed_at >= measurement.observed_at => {}
            _ => {
                latest.insert(key, measurement);
            }
        }
    }
    let mut out: Vec<Measurement> = latest.into_values().collect();
    out.sort_by(|a, b| {
        a.capability_id
            .cmp(&b.capability_id)
            .then_with(|| a.metric.cmp(&b.metric))
    });
    out
}

// ---------------------------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    pub status: String,
    pub rationale: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub affects: Vec<String>,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub supersedes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    pub decided_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewDecision {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub affects: Vec<String>,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub supersedes: Vec<String>,
    #[serde(default)]
    pub decided_at: Option<i64>,
}

/// Record a decision and, when it supersedes earlier ones, append the superseded marker to those
/// records. This is the existing `supersede` idea applied to decisions: the old record stays in
/// the trail, but its current state names its replacement.
pub fn decide(store: &Adapter, spec: &NewDecision, now: i64) -> Result<Decision, String> {
    check_id("decision id", &spec.id)?;
    if spec.title.trim().is_empty() {
        return Err("decision needs --title".into());
    }
    if spec.rationale.trim().is_empty() {
        return Err(
            "decision needs --rationale: an accepted decision without a reason is a rubber stamp"
                .into(),
        );
    }
    let status = if spec.status.is_empty() {
        "proposed"
    } else {
        spec.status.as_str()
    };
    require_vocab("decision status", status, DECISION_STATUS)?;
    if !spec.supersedes.is_empty() && status != "accepted" {
        return Err(
            "only an accepted decision may supersede another; a proposal does not retire a decision"
                .into(),
        );
    }
    for capability in &spec.affects {
        get(store, capability)
            .map_err(|_| format!("decision affects unknown capability {capability:?}"))?;
    }
    for decision in &spec.supersedes {
        if decision == &spec.id {
            return Err("a decision cannot supersede itself".into());
        }
        decisions(store)
            .iter()
            .find(|d| &d.id == decision)
            .ok_or_else(|| format!("supersedes unknown decision {decision:?}"))?;
    }
    if decisions(store).iter().any(|d| d.id == spec.id) {
        return Err(format!(
            "decision {:?} already exists; a decision id is stable and never rewritten",
            spec.id
        ));
    }
    let decision = Decision {
        id: spec.id.clone(),
        title: spec.title.clone(),
        status: status.to_string(),
        rationale: spec.rationale.clone(),
        owner: spec.owner.clone(),
        affects: spec.affects.clone(),
        evidence: spec.evidence.clone(),
        supersedes: spec.supersedes.clone(),
        superseded_by: None,
        decided_at: spec.decided_at.unwrap_or(now),
    };
    let value = serde_json::to_value(&decision).map_err(|e| e.to_string())?;
    store
        .record_put(DECISION, &Key::new([decision.id.clone()]), value)
        .map_err(|e| e.to_string())?;

    if !spec.supersedes.is_empty() {
        for old_id in &spec.supersedes {
            if let Some(mut old) = decisions(store).into_iter().find(|d| &d.id == old_id) {
                old.status = "superseded".into();
                old.superseded_by = Some(decision.id.clone());
                let value = serde_json::to_value(&old).map_err(|e| e.to_string())?;
                store
                    .record_put(DECISION, &Key::new([old.id.clone()]), value)
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(decision)
}

pub fn decisions(store: &Adapter) -> Vec<Decision> {
    let mut all: Vec<Decision> = store
        .record_keys(DECISION)
        .iter()
        .filter_map(|key| store.record_get(DECISION, key))
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect();
    all.sort_by(|a, b| {
        a.decided_at
            .cmp(&b.decided_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    all
}

//! Signed, offline-verifiable graph snapshots.
//!
//! A remote reader can verify a snapshot with nothing but a pinned Ed25519 public key: no
//! network, no database, no access to the signer's key storage. The signed material is the
//! canonical JSON of the body, bound into the manifest by `body_sha256`; the manifest itself is
//! signed with its `signature` field removed. Verification recomputes both, in that order, so
//! there is no second copy of the truth that could diverge from what was signed.
//!
//! `observed_at` says how fresh the data is. `seq` plus `prev_hash` are the replay guard: a
//! verifier accepts a strictly newer sequence, and only when the new body chains to the body hash
//! of the last accepted snapshot. The accepted `(store_id, seq, body_sha256)` triple is held by
//! the verifier, so replaying the same snapshot, or an older one, is refused.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use openssl::pkey::{PKey, Private};
use openssl::sign::{Signer, Verifier};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Result as StoreResult, Store};
use crate::types::{Folder, Issue, ProjectFolder, STATUSES};

/// Wire schema. A verifier refuses a schema it does not know rather than guessing.
pub const SNAPSHOT_SCHEMA: &str = "marbles.snapshot/1";

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("reading snapshot source: {0}")]
    Store(#[from] super::Error),
    #[error("openssl: {0}")]
    Crypto(String),
    #[error("unsupported snapshot schema {0}")]
    UnsupportedSchema(String),
    #[error("snapshot belongs to store {actual}, expected {expected}")]
    WrongStore { actual: String, expected: String },
    #[error("snapshot belongs to company {actual:?}, expected {expected:?}")]
    WrongCompany {
        actual: Option<String>,
        expected: Option<String>,
    },
    #[error("snapshot key {actual} does not match pinned key {expected}")]
    WrongKeyId { actual: String, expected: String },
    #[error("snapshot signature is not valid base64: {0}")]
    BadSignatureEncoding(String),
    #[error("snapshot signature is invalid")]
    BadSignature,
    #[error("snapshot body hash mismatch: manifest says {manifest}, body hashes to {actual}")]
    BodyHashMismatch { manifest: String, actual: String },
    #[error("snapshot seq {seq} is not newer than the last accepted seq {last_seq}")]
    Replayed { seq: u64, last_seq: u64 },
    #[error("first accepted snapshot must be seq 1, got {seq}")]
    InvalidFirstSequence { seq: u64 },
    #[error(
        "snapshot prev_hash {actual:?} does not chain to the last accepted body hash {expected:?}"
    )]
    PrevHashMismatch {
        actual: Option<String>,
        expected: Option<String>,
    },
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

/// One project row, as exported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotProject {
    pub slug: String,
    pub root: String,
    pub prefix: String,
}

/// One dependency edge, alongside the copies carried on each [`Issue`]. Exported explicitly so a
/// consumer can rebuild the graph without knowing which issue owns the edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDependency {
    pub issue_id: String,
    pub depends_on: String,
}

/// Per-project issue counts, in a shape the portfolio view can render directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub project: String,
    pub total: u64,
    pub by_status: BTreeMap<String, u64>,
}

/// The portfolio-ready rollup. Status keys are always all six known statuses, zero included, so
/// a reader never has to distinguish "absent" from "zero".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioSummary {
    pub total: u64,
    pub by_status: BTreeMap<String, u64>,
    pub projects: Vec<ProjectSummary>,
    pub folders: usize,
}

/// The signed payload. Everything a remote reader needs to reconstruct the graph and the
/// portfolio view, with no server-side joins required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotBody {
    pub projects: Vec<SnapshotProject>,
    pub folders: Vec<Folder>,
    pub project_folders: Vec<ProjectFolder>,
    pub issues: Vec<Issue>,
    pub dependencies: Vec<SnapshotDependency>,
    pub summary: PortfolioSummary,
}

/// The signed envelope. `body_sha256` is the SHA-256 of the canonical body bytes; `signature` is
/// the base64 Ed25519 signature over the canonical manifest with the `signature` field removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub schema: String,
    pub store_id: String,
    #[serde(default)]
    pub company: Option<String>,
    pub observed_at: i64,
    pub seq: u64,
    #[serde(default)]
    pub prev_hash: Option<String>,
    pub body_sha256: String,
    pub signature: String,
    pub key_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedSnapshot {
    pub manifest: SnapshotManifest,
    pub body: SnapshotBody,
}

/// Metadata the signer assigns at snapshot time. Kept separate from the body because it is
/// control-plane data, not graph data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub store_id: String,
    pub company: Option<String>,
    pub observed_at: i64,
    pub seq: u64,
    pub prev_hash: Option<String>,
}

impl SnapshotMeta {
    /// First snapshot in a chain: sequence 1, no previous hash.
    pub fn first(store_id: impl Into<String>, company: Option<String>, observed_at: i64) -> Self {
        Self {
            store_id: store_id.into(),
            company,
            observed_at,
            seq: 1,
            prev_hash: None,
        }
    }

    /// The next snapshot: increments the sequence and chains to the previous body hash.
    pub fn after(previous: &SnapshotManifest, observed_at: i64) -> Self {
        Self {
            store_id: previous.store_id.clone(),
            company: previous.company.clone(),
            observed_at,
            seq: previous.seq.saturating_add(1),
            prev_hash: Some(previous.body_sha256.clone()),
        }
    }
}

/// The signer boundary. Deliberately narrow: the snapshot module never learns where a key lives.
pub trait SnapshotSigner {
    fn key_id(&self) -> &str;
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>>;
}

/// Ed25519 signer backed by the existing `openssl` dependency.
pub struct Ed25519Signer {
    key: PKey<Private>,
    key_id: String,
}

impl Ed25519Signer {
    pub fn generate(key_id: impl Into<String>) -> Result<Self> {
        let key = PKey::generate_ed25519().map_err(crypto)?;
        Ok(Self {
            key,
            key_id: key_id.into(),
        })
    }

    pub fn from_private_key_bytes(key_id: impl Into<String>, bytes: &[u8]) -> Result<Self> {
        let key =
            PKey::private_key_from_raw_bytes(bytes, openssl::pkey::Id::ED25519).map_err(crypto)?;
        Ok(Self {
            key,
            key_id: key_id.into(),
        })
    }

    /// The pinned public key a verifier needs. 32 raw Ed25519 bytes.
    pub fn public_key(&self) -> Result<Vec<u8>> {
        self.key.raw_public_key().map_err(crypto)
    }
}

impl SnapshotSigner for Ed25519Signer {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut signer = Signer::new_without_digest(&self.key).map_err(crypto)?;
        signer.sign_oneshot_to_vec(payload).map_err(crypto)
    }
}

/// What a verifier pins: the store and company it belongs to, the key, and how many snapshots it
/// has already accepted. `last_seq == 0` means the next accepted snapshot must be sequence 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifierConfig {
    pub store_id: String,
    pub company: Option<String>,
    pub key_id: String,
    pub public_key: Vec<u8>,
}

impl VerifierConfig {
    pub fn new(
        store_id: impl Into<String>,
        company: Option<String>,
        key_id: impl Into<String>,
        public_key: Vec<u8>,
    ) -> Self {
        Self {
            store_id: store_id.into(),
            company,
            key_id: key_id.into(),
            public_key,
        }
    }
}

/// Verified output. The body is only returned once the hash, signature, chain and replay checks
/// have all passed.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedSnapshot {
    pub manifest: SnapshotManifest,
    pub body: SnapshotBody,
    pub body_sha256: String,
}

/// Stateful offline verifier. The only persisted state is the last accepted sequence and body
/// hash; the store it belongs to is pinned, so cross-store substitution is refused.
#[derive(Debug, Clone)]
pub struct SnapshotVerifier {
    config: VerifierConfig,
    last_seq: u64,
    last_body_sha256: Option<String>,
}

impl SnapshotVerifier {
    pub fn new(config: VerifierConfig) -> Self {
        Self {
            config,
            last_seq: 0,
            last_body_sha256: None,
        }
    }

    pub fn config(&self) -> &VerifierConfig {
        &self.config
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn last_body_sha256(&self) -> Option<&str> {
        self.last_body_sha256.as_deref()
    }

    /// Verify a snapshot and, on success, record it as the newest accepted one.
    pub fn verify(&mut self, snapshot: &SignedSnapshot) -> Result<VerifiedSnapshot> {
        let manifest = &snapshot.manifest;
        if manifest.schema != SNAPSHOT_SCHEMA {
            return Err(SnapshotError::UnsupportedSchema(manifest.schema.clone()));
        }
        if manifest.store_id != self.config.store_id {
            return Err(SnapshotError::WrongStore {
                actual: manifest.store_id.clone(),
                expected: self.config.store_id.clone(),
            });
        }
        if manifest.company != self.config.company {
            return Err(SnapshotError::WrongCompany {
                actual: manifest.company.clone(),
                expected: self.config.company.clone(),
            });
        }
        if manifest.key_id != self.config.key_id {
            return Err(SnapshotError::WrongKeyId {
                actual: manifest.key_id.clone(),
                expected: self.config.key_id.clone(),
            });
        }
        if manifest.seq <= self.last_seq {
            return Err(SnapshotError::Replayed {
                seq: manifest.seq,
                last_seq: self.last_seq,
            });
        }
        if self.last_seq == 0 && manifest.seq != 1 {
            return Err(SnapshotError::InvalidFirstSequence { seq: manifest.seq });
        }

        let body_bytes = canonical_json(&snapshot.body)?;
        let body_sha256 = hex_digest(&body_bytes);
        if body_sha256 != manifest.body_sha256 {
            return Err(SnapshotError::BodyHashMismatch {
                manifest: manifest.body_sha256.clone(),
                actual: body_sha256,
            });
        }

        let expected_prev = if self.last_seq == 0 {
            None
        } else {
            self.last_body_sha256.clone()
        };
        let actual_prev = normalize_hash(manifest.prev_hash.as_deref());
        if actual_prev != expected_prev {
            return Err(SnapshotError::PrevHashMismatch {
                actual: actual_prev,
                expected: expected_prev,
            });
        }

        let signature = BASE64
            .decode(manifest.signature.as_bytes())
            .map_err(|e| SnapshotError::BadSignatureEncoding(e.to_string()))?;
        let payload = signing_payload(manifest)?;
        verify_ed25519(&self.config.public_key, &payload, &signature)?;

        self.last_seq = manifest.seq;
        self.last_body_sha256 = Some(body_sha256.clone());
        Ok(VerifiedSnapshot {
            manifest: manifest.clone(),
            body: snapshot.body.clone(),
            body_sha256,
        })
    }
}

/// Read a snapshot body from a store. Only the public [`Store`] surface is used, so this works
/// for SQLite and the hosted backend alike.
pub fn capture(store: &dyn Store) -> StoreResult<SnapshotBody> {
    let projects: Vec<SnapshotProject> = store
        .projects()?
        .into_iter()
        .map(|(slug, root, prefix)| SnapshotProject { slug, root, prefix })
        .collect();
    let view = store.folder_view()?;
    let mut issues = store.list(None, true, None)?;
    issues.sort_by(|a, b| a.id.cmp(&b.id));
    let mut dependencies = Vec::new();
    for issue in &issues {
        for dep in &issue.dependencies {
            if let Some(depends_on) = &dep.depends_on_id {
                dependencies.push(SnapshotDependency {
                    issue_id: issue.id.clone(),
                    depends_on: depends_on.clone(),
                });
            }
        }
    }
    dependencies.sort_by(|a, b| {
        a.issue_id
            .cmp(&b.issue_id)
            .then_with(|| a.depends_on.cmp(&b.depends_on))
    });
    let summary = summarize(&projects, &view.folders, &issues);
    Ok(SnapshotBody {
        projects,
        folders: view.folders,
        project_folders: view.projects,
        issues,
        dependencies,
        summary,
    })
}

/// Hash and sign a body. The sequence and previous hash are supplied by the caller, which keeps
/// sequencing policy out of the crypto path.
pub fn sign_snapshot(
    body: SnapshotBody,
    meta: SnapshotMeta,
    signer: &dyn SnapshotSigner,
) -> Result<SignedSnapshot> {
    let body_bytes = canonical_json(&body)?;
    let body_sha256 = hex_digest(&body_bytes);
    let mut manifest = SnapshotManifest {
        schema: SNAPSHOT_SCHEMA.to_string(),
        store_id: meta.store_id,
        company: meta.company,
        observed_at: meta.observed_at,
        seq: meta.seq,
        prev_hash: normalize_hash(meta.prev_hash.as_deref()),
        body_sha256,
        signature: String::new(),
        key_id: signer.key_id().to_string(),
    };
    let payload = signing_payload(&manifest)?;
    manifest.signature = BASE64.encode(signer.sign(&payload)?);
    Ok(SignedSnapshot { manifest, body })
}

/// Canonical JSON: object keys sorted, no insignificant whitespace. This is the exact byte string
/// that is hashed and signed, so both sides must use it — there is no separate "signing form".
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value)?;
    let mut out = Vec::new();
    write_canonical(&value, &mut out)?;
    Ok(out)
}

fn signing_payload(manifest: &SnapshotManifest) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(manifest)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("signature");
    }
    let mut out = Vec::new();
    write_canonical(&value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(true) => out.extend_from_slice(b"true"),
        serde_json::Value::Bool(false) => out.extend_from_slice(b"false"),
        serde_json::Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        serde_json::Value::String(s) => {
            out.extend_from_slice(serde_json::to_string(s)?.as_bytes());
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        serde_json::Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key)?.as_bytes());
                out.push(b':');
                write_canonical(&map[key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn summarize(
    projects: &[SnapshotProject],
    folders: &[Folder],
    issues: &[Issue],
) -> PortfolioSummary {
    let mut by_status = zero_statuses();
    let mut per_project: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for project in projects {
        per_project.insert(project.slug.clone(), zero_statuses());
    }
    for issue in issues {
        *by_status.entry(issue.status.clone()).or_insert(0) += 1;
        *per_project
            .entry(issue.project.clone())
            .or_insert_with(zero_statuses)
            .entry(issue.status.clone())
            .or_insert(0) += 1;
    }
    let projects = per_project
        .into_iter()
        .map(|(project, by_status)| ProjectSummary {
            project,
            total: by_status.values().sum(),
            by_status,
        })
        .collect();
    PortfolioSummary {
        total: issues.len() as u64,
        by_status,
        projects,
        folders: folders.len(),
    }
}

fn zero_statuses() -> BTreeMap<String, u64> {
    STATUSES
        .iter()
        .map(|status| ((*status).to_string(), 0))
        .collect()
}

fn verify_ed25519(public_key: &[u8], payload: &[u8], signature: &[u8]) -> Result<()> {
    let key =
        PKey::public_key_from_raw_bytes(public_key, openssl::pkey::Id::ED25519).map_err(crypto)?;
    let mut verifier = Verifier::new_without_digest(&key).map_err(crypto)?;
    if verifier
        .verify_oneshot(signature, payload)
        .map_err(crypto)?
    {
        Ok(())
    } else {
        Err(SnapshotError::BadSignature)
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `None`, the empty string, and the all-zero hash all mean "no previous snapshot" for seq 1.
fn normalize_hash(hash: Option<&str>) -> Option<String> {
    match hash.map(str::trim) {
        None | Some("") => None,
        Some(hash) if hash.bytes().all(|b| b == b'0') => None,
        Some(hash) => Some(hash.to_ascii_lowercase()),
    }
}

fn crypto(error: openssl::error::ErrorStack) -> SnapshotError {
    SnapshotError::Crypto(error.to_string())
}

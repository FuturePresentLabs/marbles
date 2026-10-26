//! Local cache of remote issue assertions, with *named* freshness instead of a boolean.
//!
//! Federation reads are asynchronous: a remote store may be unreachable, slow, or pinned by a key
//! that has since been rotated out. Readiness and aggregation code must never collapse those
//! cases into "not blocked" / "closed" / "ready" — an absence of evidence is not evidence that a
//! remote issue is done. Each cached reference therefore reports one of four named states:
//!
//! * [`AssertionFreshness::Fresh`] — the signature verifies against the pinned key and we
//!   observed the assertion inside the freshness window.
//! * [`AssertionFreshness::Stale`] — the signature verifies, but our observation is older than
//!   the window. The data is still labelled; the caller decides whether age is acceptable.
//! * [`AssertionFreshness::Unknown`] — no assertion was ever observed, or the remote could not be
//!   reached. There is no state to read: [`AssertionView::asserted_state`] returns `None`.
//! * [`AssertionFreshness::Unverifiable`] — an entry exists but the signature, key, store id, or
//!   asserted state does not verify against the current pin. Fails closed rather than trusting it.
//!
//! Freshness is measured from `observed_at`, not from the remote's `asserted_at`: a long-open
//! issue's status was legitimately asserted a while ago, and re-observation is what refreshes our
//! knowledge. `asserted_at` is preserved for provenance and for a bounded clock-skew check.
//!
//! Writes only happen after offline verification against the pinned key in the
//! [`RemoteRegistry`]; an unverified assertion is never persisted. Reads re-verify against the
//! *current* pin, so a rotation that retires the signing key turns old entries unverifiable.
//!
//! Nothing here can mutate a remote or write across stores: the cache is keyed by
//! `(store_id, remote_issue_id)` and a mismatch is unverifiable, never a new reference.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use openssl::pkey::{Id, PKey};
use openssl::sign::Verifier;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config;
use crate::db::snapshot::SnapshotSigner;
use crate::identity::{PinnedKey, RemoteRecord, RemoteRegistry};
use crate::types::STATUSES;

/// Public, Ed25519-signed issue-state assertion schema.
pub const ISSUE_ASSERTION_SCHEMA: &str = "marbles.issue-assertion/1";
/// On-disk entry shape; bump when a field changes meaning, older files then read as unverifiable.
pub const ASSERTION_CACHE_VERSION: u32 = 1;

/// How long an observation counts as fresh: 5 minutes.
///
/// Readiness/aggregation polls are on the order of a minute, so this tolerates several missed
/// polls before a reference is labelled stale, while keeping the window short enough that a
/// disconnected client cannot present a day-old remote status as current. When in doubt the
/// shorter window is the safe choice: `stale` is visible, `fresh` is not.
pub const DEFAULT_FRESHNESS_WINDOW_SECONDS: i64 = 5 * 60;

/// Largest tolerated difference between a remote's `asserted_at` and our `observed_at`.
///
/// `observed_at` is stamped by the local clock, so a remote claim materially in the future is
/// either a broken clock or a forged timestamp; both fail closed as unverifiable.
pub const MAX_CLOCK_SKEW_SECONDS: i64 = 5 * 60;

/// Newest cache entries kept; oldest are pruned so an outage-heavy client cannot grow without
/// bound.
const MAX_ENTRIES: usize = 4096;

const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Debug, thiserror::Error)]
pub enum AssertionError {
    #[error("unsupported assertion schema {0:?}")]
    UnsupportedSchema(String),
    #[error("assertion belongs to store {actual:?}, but the verifier pins {expected:?}")]
    WrongStore { actual: String, expected: String },
    #[error("assertion key {actual:?} does not match pinned key {expected:?}")]
    WrongKeyId { actual: String, expected: String },
    #[error("pinned key {0:?} is retired at {1}")]
    KeyRetired(String, i64),
    #[error("assertion is unsigned")]
    Unsigned,
    #[error("assertion signature is malformed: {0}")]
    MalformedSignature(String),
    #[error("assertion signature is invalid")]
    BadSignature,
    #[error("assertion names an empty remote issue id")]
    EmptyRemoteIssueId,
    #[error("assertion carries unknown issue state {0:?}; refusing to guess its meaning")]
    UnknownState(String),
    #[error(
        "assertion asserted_at {asserted_at} is more than {tolerance}s after observation {observed_at}"
    )]
    FutureAssertion {
        asserted_at: i64,
        observed_at: i64,
        tolerance: i64,
    },
    #[error("crypto failure: {0}")]
    Crypto(String),
    #[error("assertion json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum AssertionCacheError {
    #[error("reading assertion cache {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing assertion cache {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Assertion(#[from] AssertionError),
    #[error("assertion cache json: {0}")]
    Json(#[from] serde_json::Error),
}

/// The named freshness of one cached remote reference. There is deliberately no `ok` boolean and
/// no `Default`: a caller must match on the state it actually got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionFreshness {
    /// Signature verifies against the pinned key and the observation is inside the window.
    Fresh,
    /// Signature verifies, but the observation is older than the window.
    Stale,
    /// No assertion was observed, or the remote could not be reached.
    Unknown,
    /// A cached entry exists but cannot be verified against the current pin.
    Unverifiable,
}

impl AssertionFreshness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
            Self::Unverifiable => "unverifiable",
        }
    }
}

impl std::fmt::Display for AssertionFreshness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an [`AssertionView`] has no trusted assertion at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingReason {
    /// No observation has ever been recorded for this reference.
    NeverObserved,
    /// The remote was attempted and could not be reached; nothing is cached.
    RemoteUnreachable,
}

/// The public wire form of an issue assertion: a store's signed claim about one remote issue.
///
/// The signature covers every field except `signature` itself, in the declared field order, so no
/// field can change after signing without invalidating it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueAssertion {
    pub schema: String,
    pub store_id: String,
    pub remote_issue_id: String,
    pub state: String,
    pub asserted_at: i64,
    pub key_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl IssueAssertion {
    pub fn new(
        store_id: impl Into<String>,
        remote_issue_id: impl Into<String>,
        state: impl Into<String>,
        asserted_at: i64,
    ) -> Self {
        Self {
            schema: ISSUE_ASSERTION_SCHEMA.to_string(),
            store_id: store_id.into(),
            remote_issue_id: remote_issue_id.into(),
            state: state.into(),
            asserted_at,
            key_id: String::new(),
            signature: None,
        }
    }

    /// Canonical bytes the signature covers: everything except the signature itself.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, AssertionError> {
        let mut copy = self.clone();
        copy.signature = None;
        Ok(serde_json::to_vec(&copy)?)
    }

    pub fn from_json(json: &str) -> Result<Self, AssertionError> {
        Ok(serde_json::from_str(json)?)
    }
}

/// Sign an issue assertion with a store's Ed25519 signer. The signer's key id is recorded so a
/// verifier can select the pinned key instead of trusting whatever the payload claims.
pub fn sign_issue_assertion(
    mut assertion: IssueAssertion,
    signer: &dyn SnapshotSigner,
) -> Result<IssueAssertion, AssertionError> {
    if assertion.schema != ISSUE_ASSERTION_SCHEMA {
        return Err(AssertionError::UnsupportedSchema(assertion.schema));
    }
    if assertion.remote_issue_id.trim().is_empty() {
        return Err(AssertionError::EmptyRemoteIssueId);
    }
    assertion.key_id = signer.key_id().to_string();
    assertion.signature = None;
    let signature = signer
        .sign(&assertion.signing_bytes()?)
        .map_err(|error| AssertionError::Crypto(error.to_string()))?;
    assertion.signature = Some(URL_SAFE_NO_PAD.encode(signature));
    Ok(assertion)
}

/// Verify an assertion offline against one pinned key. Fails closed on every mismatch.
///
/// `now` is the instant the pin is evaluated, so a key retired by rotation stops verifying even
/// if the signature is still mathematically valid.
pub fn verify_issue_assertion(
    assertion: &IssueAssertion,
    pin: &PinnedKey,
    now: i64,
) -> Result<(), AssertionError> {
    if assertion.schema != ISSUE_ASSERTION_SCHEMA {
        return Err(AssertionError::UnsupportedSchema(assertion.schema.clone()));
    }
    if pin.store_id.is_empty() || assertion.store_id != pin.store_id {
        return Err(AssertionError::WrongStore {
            actual: assertion.store_id.clone(),
            expected: pin.store_id.clone(),
        });
    }
    if assertion.key_id != pin.key_id {
        return Err(AssertionError::WrongKeyId {
            actual: assertion.key_id.clone(),
            expected: pin.key_id.clone(),
        });
    }
    if let Some(valid_until) = pin.valid_until
        && now >= valid_until
    {
        return Err(AssertionError::KeyRetired(pin.key_id.clone(), valid_until));
    }
    // An unknown state cannot be interpreted safely: readiness code would have to guess whether
    // it means open, closed, or blocked. Refuse rather than default.
    if !is_known_state(&assertion.state) {
        return Err(AssertionError::UnknownState(assertion.state.clone()));
    }
    let signature = assertion
        .signature
        .as_deref()
        .ok_or(AssertionError::Unsigned)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|error| AssertionError::MalformedSignature(error.to_string()))?;
    let public_key = URL_SAFE_NO_PAD
        .decode(&pin.public_key)
        .map_err(|error| AssertionError::MalformedSignature(error.to_string()))?;
    let key = PKey::public_key_from_raw_bytes(&public_key, Id::ED25519)
        .map_err(|error| AssertionError::Crypto(error.to_string()))?;
    let mut verifier = Verifier::new_without_digest(&key)
        .map_err(|error| AssertionError::Crypto(error.to_string()))?;
    let valid = verifier
        .verify_oneshot(&signature, &assertion.signing_bytes()?)
        .unwrap_or(false);
    if !valid {
        return Err(AssertionError::BadSignature);
    }
    Ok(())
}

fn is_known_state(state: &str) -> bool {
    STATUSES.contains(&state)
}

/// A verified assertion plus the instant *we* observed it.
///
/// This is the cached shape: `signature` is a `String` rather than an `Option` because an
/// unsigned assertion can never be stored — cache writes only happen after verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedAssertion {
    pub schema: String,
    pub store_id: String,
    pub remote_issue_id: String,
    pub state: String,
    pub asserted_at: i64,
    pub signature: String,
    pub key_id: String,
    pub observed_at: i64,
}

impl CachedAssertion {
    /// Build the persisted form from an already verified assertion.
    fn from_verified(assertion: &IssueAssertion, observed_at: i64) -> Result<Self, AssertionError> {
        let signature = assertion
            .signature
            .clone()
            .ok_or(AssertionError::Unsigned)?;
        Ok(Self {
            schema: assertion.schema.clone(),
            store_id: assertion.store_id.clone(),
            remote_issue_id: assertion.remote_issue_id.clone(),
            state: assertion.state.clone(),
            asserted_at: assertion.asserted_at,
            signature,
            key_id: assertion.key_id.clone(),
            observed_at,
        })
    }

    /// Reconstruct the signed wire form so the signature can be re-checked on read.
    fn to_assertion(&self) -> IssueAssertion {
        IssueAssertion {
            schema: self.schema.clone(),
            store_id: self.store_id.clone(),
            remote_issue_id: self.remote_issue_id.clone(),
            state: self.state.clone(),
            asserted_at: self.asserted_at,
            key_id: self.key_id.clone(),
            signature: Some(self.signature.clone()),
        }
    }
}

/// What a read tells the caller: the named state, and the data only when it is trustworthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertionView {
    pub store_id: String,
    pub remote_issue_id: String,
    pub freshness: AssertionFreshness,
    /// Present for `fresh`/`stale`; `None` for `unknown`/`unverifiable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion: Option<CachedAssertion>,
    /// Set only when `freshness` is `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<MissingReason>,
    /// Set only when `freshness` is `unverifiable`: why the entry was rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl AssertionView {
    fn unknown(store_id: &str, remote_issue_id: &str, reason: MissingReason) -> Self {
        Self {
            store_id: store_id.to_string(),
            remote_issue_id: remote_issue_id.to_string(),
            freshness: AssertionFreshness::Unknown,
            assertion: None,
            missing: Some(reason),
            problem: None,
        }
    }

    fn unverifiable(store_id: &str, remote_issue_id: &str, problem: impl Into<String>) -> Self {
        Self {
            store_id: store_id.to_string(),
            remote_issue_id: remote_issue_id.to_string(),
            freshness: AssertionFreshness::Unverifiable,
            assertion: None,
            missing: None,
            problem: Some(problem.into()),
        }
    }

    /// The asserted issue state, but only when it is backed by a verified assertion.
    ///
    /// `None` for `unknown` and `unverifiable`: callers must not fall back to a default state.
    pub fn asserted_state(&self) -> Option<&str> {
        match self.freshness {
            AssertionFreshness::Fresh | AssertionFreshness::Stale => {
                self.assertion.as_ref().map(|entry| entry.state.as_str())
            }
            AssertionFreshness::Unknown | AssertionFreshness::Unverifiable => None,
        }
    }

    pub fn observed_at(&self) -> Option<i64> {
        self.assertion.as_ref().map(|entry| entry.observed_at)
    }
}

/// On-disk per-reference entry. A failed observation is recorded as its own variant so a read can
/// distinguish "never tried" from "tried and the remote was unreachable", without inventing a
/// state for the issue itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Entry {
    Assertion {
        version: u32,
        store_id: String,
        remote_issue_id: String,
        observed_at: i64,
        assertion: CachedAssertion,
    },
    Unreachable {
        version: u32,
        store_id: String,
        remote_issue_id: String,
        attempted_at: i64,
        detail: String,
    },
}

impl Entry {
    fn header(&self) -> (u32, &str, &str) {
        match self {
            Self::Assertion {
                version,
                store_id,
                remote_issue_id,
                ..
            }
            | Self::Unreachable {
                version,
                store_id,
                remote_issue_id,
                ..
            } => (*version, store_id, remote_issue_id),
        }
    }
}

/// A directory of per-reference entries under `$MARBLES_HOME/assertions`.
///
/// One file per `(store_id, remote_issue_id)` means one damaged entry degrades exactly one
/// reference: everything else still reads.
pub struct AssertionCache {
    root: PathBuf,
    freshness_window_seconds: i64,
}

impl AssertionCache {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            freshness_window_seconds: DEFAULT_FRESHNESS_WINDOW_SECONDS,
        }
    }

    /// Override the freshness window (tests, or an operator who accepts older data).
    pub fn with_freshness_window(mut self, seconds: i64) -> Self {
        self.freshness_window_seconds = seconds.max(0);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn freshness_window_seconds(&self) -> i64 {
        self.freshness_window_seconds
    }

    /// Deterministic location of one reference's entry. The filename is a hash of the reference,
    /// so issue ids can never escape the cache directory.
    pub fn entry_path(&self, store_id: &str, remote_issue_id: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(store_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(remote_issue_id.as_bytes());
        let digest = hasher.finalize();
        let mut name = String::with_capacity(64 + 5);
        for byte in digest {
            name.push(char::from(HEX[(byte >> 4) as usize]));
            name.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
        name.push_str(".json");
        self.root.join(name)
    }

    /// Verify `assertion` against `pin` and store it. Nothing is written unless it verifies.
    ///
    /// `observed_at` is the local instant of observation; it is also the instant the pin is
    /// evaluated at, so an assertion can never be cached under a key that had already retired.
    pub fn record(
        &self,
        pin: &RemoteRecord,
        assertion: &IssueAssertion,
        observed_at: i64,
    ) -> Result<AssertionView, AssertionCacheError> {
        if assertion.remote_issue_id.trim().is_empty() {
            return Err(AssertionError::EmptyRemoteIssueId.into());
        }
        let key = pin
            .pins()
            .into_iter()
            .find(|candidate| candidate.key_id == assertion.key_id)
            .ok_or_else(|| AssertionError::WrongKeyId {
                actual: assertion.key_id.clone(),
                expected: pin.key_id.clone(),
            })?;
        verify_issue_assertion(assertion, &key, observed_at)?;
        check_clock_skew(assertion, observed_at)?;
        let cached = CachedAssertion::from_verified(assertion, observed_at)?;
        let entry = Entry::Assertion {
            version: ASSERTION_CACHE_VERSION,
            store_id: cached.store_id.clone(),
            remote_issue_id: cached.remote_issue_id.clone(),
            observed_at,
            assertion: cached.clone(),
        };
        self.write_entry(&entry)?;
        Ok(AssertionView {
            store_id: cached.store_id.clone(),
            remote_issue_id: cached.remote_issue_id.clone(),
            freshness: AssertionFreshness::Fresh,
            assertion: Some(cached),
            missing: None,
            problem: None,
        })
    }

    /// Record that a remote could not be reached for this reference.
    ///
    /// A verified assertion is never erased by a failed re-observation: last-known data stays and
    /// ages into `stale`, which is more useful (and more honest) than forgetting it. The marker is
    /// only written when there is nothing verified to keep.
    pub fn note_unreachable(
        &self,
        store_id: &str,
        remote_issue_id: &str,
        detail: impl Into<String>,
        attempted_at: i64,
    ) -> Result<(), AssertionCacheError> {
        if matches!(
            self.load_entry(store_id, remote_issue_id)?,
            Some(Entry::Assertion { .. })
        ) {
            return Ok(());
        }
        self.write_entry(&Entry::Unreachable {
            version: ASSERTION_CACHE_VERSION,
            store_id: store_id.to_string(),
            remote_issue_id: remote_issue_id.to_string(),
            attempted_at,
            detail: detail.into(),
        })
    }

    /// Read one reference against the *current* pin.
    ///
    /// The state is always named; the assertion is returned only when it verifies. Anything that
    /// fails closed (bad signature, retired key, wrong store, unknown state, unreadable file)
    /// yields `unverifiable` rather than a default judgement about the issue.
    pub fn read(
        &self,
        registry: &RemoteRegistry,
        store_id: &str,
        remote_issue_id: &str,
        now: i64,
    ) -> AssertionView {
        let entry = match self.load_entry(store_id, remote_issue_id) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                return AssertionView::unknown(
                    store_id,
                    remote_issue_id,
                    MissingReason::NeverObserved,
                );
            }
            Err(error) => {
                return AssertionView::unverifiable(store_id, remote_issue_id, error.to_string());
            }
        };
        let (version, entry_store_id, entry_issue_id) = entry.header();
        if version != ASSERTION_CACHE_VERSION {
            return AssertionView::unverifiable(
                store_id,
                remote_issue_id,
                format!("cached entry version {version} is not supported"),
            );
        }
        if entry_store_id != store_id || entry_issue_id != remote_issue_id {
            return AssertionView::unverifiable(
                store_id,
                remote_issue_id,
                "cached entry is filed under a different (store_id, remote_issue_id) reference",
            );
        }
        let cached = match entry {
            Entry::Unreachable {
                attempted_at,
                detail,
                ..
            } => {
                let mut view = AssertionView::unknown(
                    store_id,
                    remote_issue_id,
                    MissingReason::RemoteUnreachable,
                );
                view.problem = Some(format!(
                    "remote unreachable at {attempted_at}: {detail}; no assertion has been observed"
                ));
                return view;
            }
            Entry::Assertion { assertion, .. } => assertion,
        };

        let Some(record) = registry
            .list()
            .iter()
            .find(|record| record.store_id == store_id)
        else {
            return AssertionView::unverifiable(
                store_id,
                remote_issue_id,
                format!(
                    "store {store_id:?} is not pinned; the cached assertion cannot be verified"
                ),
            );
        };
        let Some(pin) = record
            .pins()
            .into_iter()
            .find(|pin| pin.key_id == cached.key_id)
        else {
            return AssertionView::unverifiable(
                store_id,
                remote_issue_id,
                format!(
                    "assertion key {:?} is not pinned for store {store_id:?}",
                    cached.key_id
                ),
            );
        };
        let assertion = cached.to_assertion();
        if let Err(error) = verify_issue_assertion(&assertion, &pin, now) {
            return AssertionView::unverifiable(
                store_id,
                remote_issue_id,
                format!("cached assertion does not verify: {error}"),
            );
        }
        if let Err(error) = check_clock_skew(&assertion, cached.observed_at) {
            return AssertionView::unverifiable(store_id, remote_issue_id, error.to_string());
        }
        if cached.observed_at > now.saturating_add(MAX_CLOCK_SKEW_SECONDS) {
            return AssertionView::unverifiable(
                store_id,
                remote_issue_id,
                format!(
                    "observation time {} is in the future relative to {now}",
                    cached.observed_at
                ),
            );
        }
        let freshness = if now.saturating_sub(cached.observed_at) > self.freshness_window_seconds {
            AssertionFreshness::Stale
        } else {
            AssertionFreshness::Fresh
        };
        AssertionView {
            store_id: store_id.to_string(),
            remote_issue_id: remote_issue_id.to_string(),
            freshness,
            assertion: Some(cached),
            missing: None,
            problem: None,
        }
    }

    /// Read every cached reference for one store, degrading per reference.
    ///
    /// A file that cannot be parsed is skipped here (a reference-scoped [`Self::read`] still
    /// reports it as unverifiable); every other reference is unaffected.
    pub fn read_all(
        &self,
        registry: &RemoteRegistry,
        store_id: &str,
        now: i64,
    ) -> Vec<AssertionView> {
        let Ok(dir) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut issues: Vec<String> = Vec::new();
        for entry in dir.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_str::<Entry>(&text) else {
                continue;
            };
            let (_, entry_store_id, entry_issue_id) = entry.header();
            if entry_store_id == store_id {
                issues.push(entry_issue_id.to_string());
            }
        }
        issues.sort();
        issues.dedup();
        issues
            .iter()
            .map(|issue| self.read(registry, store_id, issue, now))
            .collect()
    }

    fn load_entry(
        &self,
        store_id: &str,
        remote_issue_id: &str,
    ) -> Result<Option<Entry>, AssertionCacheError> {
        let path = self.entry_path(store_id, remote_issue_id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(AssertionCacheError::Read { path, source }),
        };
        let entry: Entry = serde_json::from_str(&text)?;
        Ok(Some(entry))
    }

    fn write_entry(&self, entry: &Entry) -> Result<(), AssertionCacheError> {
        fs::create_dir_all(&self.root).map_err(|source| AssertionCacheError::Write {
            path: self.root.clone(),
            source,
        })?;
        restrict_dir(&self.root);
        let (_, store_id, remote_issue_id) = entry.header();
        let path = self.entry_path(store_id, remote_issue_id);
        let encoded = serde_json::to_vec_pretty(entry)?;
        // Write beside the target then rename: a crash never leaves a half-written entry that
        // would later read as corrupt (and therefore unverifiable).
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        write_private(&tmp, &encoded)?;
        fs::rename(&tmp, &path).map_err(|source| {
            let _ = fs::remove_file(&tmp);
            AssertionCacheError::Write { path, source }
        })?;
        self.prune();
        Ok(())
    }

    fn prune(&self) {
        let Ok(dir) = fs::read_dir(&self.root) else {
            return;
        };
        let mut files: Vec<(PathBuf, std::time::SystemTime)> = dir
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    return None;
                }
                let modified = entry
                    .metadata()
                    .ok()?
                    .modified()
                    .unwrap_or(std::time::UNIX_EPOCH);
                Some((path, modified))
            })
            .collect();
        if files.len() <= MAX_ENTRIES {
            return;
        }
        files.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        for (path, _) in files.into_iter().skip(MAX_ENTRIES) {
            let _ = fs::remove_file(path);
        }
    }
}

fn check_clock_skew(assertion: &IssueAssertion, observed_at: i64) -> Result<(), AssertionError> {
    if assertion.asserted_at > observed_at.saturating_add(MAX_CLOCK_SKEW_SECONDS) {
        return Err(AssertionError::FutureAssertion {
            asserted_at: assertion.asserted_at,
            observed_at,
            tolerance: MAX_CLOCK_SKEW_SECONDS,
        });
    }
    Ok(())
}

/// Default location: `<state dir>/assertions` (`$MARBLES_HOME/assertions` or
/// `~/.marbles/assertions`).
pub fn default_root() -> PathBuf {
    config::assertion_cache_dir()
}

#[cfg(unix)]
fn restrict_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) {}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AssertionCacheError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| AssertionCacheError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| AssertionCacheError::Write {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AssertionCacheError> {
    let mut file = fs::File::create(path).map_err(|source| AssertionCacheError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes)
        .map_err(|source| AssertionCacheError::Write {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_names_are_stable() {
        assert_eq!(AssertionFreshness::Fresh.as_str(), "fresh");
        assert_eq!(AssertionFreshness::Stale.as_str(), "stale");
        assert_eq!(AssertionFreshness::Unknown.as_str(), "unknown");
        assert_eq!(AssertionFreshness::Unverifiable.as_str(), "unverifiable");
    }

    #[test]
    fn unknown_state_is_never_a_default() {
        let view = AssertionView::unknown("store_a", "issue-1", MissingReason::NeverObserved);
        assert_eq!(view.asserted_state(), None);
        assert_eq!(view.freshness, AssertionFreshness::Unknown);
        assert_eq!(view.observed_at(), None);
    }

    #[test]
    fn entry_paths_do_not_escape_the_root() {
        let cache = AssertionCache::at("/tmp/assertions");
        let path = cache.entry_path("store_a", "../../etc/passwd");
        assert_eq!(path.parent(), Some(Path::new("/tmp/assertions")));
    }
}

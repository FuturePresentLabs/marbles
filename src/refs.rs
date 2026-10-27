//! Canonical cross-store references.
//!
//! A federation reference names exactly one issue in one store:
//!
//! ```text
//! marbles://<store_id>/<company>/<project>/<issue_id>
//! ```
//!
//! The reference is an *address*, not a local row. Parsing it must never create an issue, and the
//! parser deliberately rejects anything ambiguous (missing/unknown scheme, empty or extra path
//! segments, whitespace, a trailing slash, or characters that cannot appear in a slug/id) with a
//! named error. The display form is the normal form, so `parse(display(ref)) == ref`.

use std::fmt;

use crate::assertion_cache::{
    AssertionError, AssertionFreshness, CachedAssertion, ISSUE_ASSERTION_SCHEMA, IssueAssertion,
    verify_issue_assertion,
};
use crate::identity::{PinnedKey, RemoteRegistry};
use crate::types::RemoteDependency;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefError {
    #[error("canonical ref is missing the marbles:// scheme")]
    MissingScheme,
    #[error("canonical ref uses unknown scheme {0:?}; expected marbles://")]
    UnknownScheme(String),
    #[error("canonical ref contains whitespace")]
    Whitespace,
    #[error("canonical ref has a trailing slash")]
    TrailingSlash,
    #[error("canonical ref has an empty {segment} segment")]
    EmptySegment { segment: &'static str },
    #[error("canonical ref is missing the {segment} segment")]
    MissingSegment { segment: &'static str },
    #[error("canonical ref has {found} path segments; expected exactly 4")]
    ExtraSegments { found: usize },
    #[error("canonical ref has an invalid {segment} {value:?}")]
    InvalidSegment {
        segment: &'static str,
        value: String,
    },
    #[error("canonical ref has an empty store_id")]
    EmptyStoreId,
}

/// A parsed, canonical `marbles://` reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalRef {
    pub store_id: String,
    pub company: String,
    pub project: String,
    pub issue_id: String,
}

impl CanonicalRef {
    pub fn parse(input: &str) -> Result<Self, RefError> {
        if input.chars().any(char::is_whitespace) {
            return Err(RefError::Whitespace);
        }
        let rest = if let Some(rest) = input.strip_prefix("marbles://") {
            rest
        } else if let Some((scheme, _)) = input.split_once("://") {
            return Err(RefError::UnknownScheme(scheme.to_string()));
        } else {
            return Err(RefError::MissingScheme);
        };
        if rest.is_empty() {
            return Err(RefError::EmptyStoreId);
        }
        if rest.ends_with('/') {
            return Err(RefError::TrailingSlash);
        }
        let parts: Vec<&str> = rest.split('/').collect();
        const NAMES: [&str; 4] = ["store_id", "company", "project", "issue_id"];
        for (index, part) in parts.iter().enumerate() {
            if part.is_empty() {
                return Err(RefError::EmptySegment {
                    segment: NAMES.get(index).copied().unwrap_or("path"),
                });
            }
        }
        if parts.len() < 4 {
            return Err(RefError::MissingSegment {
                segment: NAMES[parts.len()],
            });
        }
        if parts.len() > 4 {
            return Err(RefError::ExtraSegments { found: parts.len() });
        }
        for (segment, value) in NAMES.iter().zip(parts.iter()) {
            validate_segment(segment, value)?;
        }
        if parts[0].is_empty() {
            return Err(RefError::EmptyStoreId);
        }
        Ok(Self {
            store_id: parts[0].to_string(),
            company: parts[1].to_string(),
            project: parts[2].to_string(),
            issue_id: parts[3].to_string(),
        })
    }

    /// The remote issue id (the fourth segment), as stored in the assertion cache.
    pub fn remote_issue_id(&self) -> &str {
        &self.issue_id
    }

    /// True when this ref names the given store and issue.
    pub fn names(&self, store_id: &str, issue_id: &str) -> bool {
        self.store_id == store_id && self.issue_id == issue_id
    }
}

impl fmt::Display for CanonicalRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "marbles://{}/{}/{}/{}",
            self.store_id, self.company, self.project, self.issue_id
        )
    }
}

/// True when `value` is a plausible single path segment for a store/slug/id.
///
/// This intentionally accepts the id alphabets Marbles already mints (letters, digits, `_`, `-`,
/// and `.` for dotted project slugs) while rejecting punctuation that would make a reference
/// ambiguous (`?`, `#`, `%`, `:`) or traversal-like (`.`/`..`). Whitespace is rejected earlier.
fn validate_segment(segment: &'static str, value: &str) -> Result<(), RefError> {
    let invalid = value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if invalid {
        return Err(RefError::InvalidSegment {
            segment,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Re-evaluate a stored remote dependency against the *current* pinned key.
///
/// This uses the same offline Ed25519 verification as the assertion cache (construction of the
/// signed [`IssueAssertion`] and [`verify_issue_assertion`]) rather than a second verification
/// path. A missing pin, a rotated-out key, a bad signature, or a ref whose store/issue does not
/// match the signed assertion is `unverifiable`; only a verified observation can be `fresh` or
/// `stale`.
pub fn remote_dependency_freshness(
    dependency: &RemoteDependency,
    registry: &RemoteRegistry,
    now: i64,
    freshness_window_seconds: i64,
) -> AssertionFreshness {
    let assertion = IssueAssertion {
        schema: ISSUE_ASSERTION_SCHEMA.to_string(),
        store_id: dependency.store_id.clone(),
        remote_issue_id: dependency.remote_issue_id.clone(),
        state: dependency.state.clone(),
        asserted_at: dependency.asserted_at,
        key_id: dependency.key_id.clone(),
        signature: Some(dependency.signature.clone()),
    };
    let Some(record) = registry
        .list()
        .iter()
        .find(|record| record.store_id == dependency.store_id)
    else {
        return AssertionFreshness::Unverifiable;
    };
    let Some(pin) = record
        .pins()
        .into_iter()
        .find(|pin| pin.key_id == dependency.key_id)
    else {
        return AssertionFreshness::Unverifiable;
    };
    // The canonical ref is part of the edge, so it must agree with the signed issue id.
    let Ok(parsed) = CanonicalRef::parse(&dependency.canonical_ref) else {
        return AssertionFreshness::Unverifiable;
    };
    if !parsed.names(&dependency.store_id, &dependency.remote_issue_id) {
        return AssertionFreshness::Unverifiable;
    }
    if verify_issue_assertion(&assertion, &pin, now).is_err() {
        return AssertionFreshness::Unverifiable;
    }
    if now.saturating_sub(dependency.observed_at) > freshness_window_seconds.max(0) {
        AssertionFreshness::Stale
    } else {
        AssertionFreshness::Fresh
    }
}

// ---------------------------------------------------------------------------------------------
// Audit receipts
// ---------------------------------------------------------------------------------------------
//
// A remote reference is only as trustworthy as the evidence shown next to it. Every reference
// therefore carries a receipt with exactly the six fields a verifier needs, and every way that
// evidence can be wrong is a *named rejection state* rather than a silent downgrade. Verification
// is entirely offline: the pinned key comes from [`RemoteRegistry`], nothing here touches the
// network, and a rejected receipt never reads as fresh/ready.

/// The six fields every remote reference must carry. None of them is optional.
///
/// The signature covers `store_id`, `remote_issue_id`, `state`, `asserted_at`, and `key_id` in
/// that declared order, matching [`IssueAssertion::signing_bytes`], so the same Ed25519 check used
/// by the assertion cache verifies a receipt without a second crypto path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditReceipt {
    pub store_id: String,
    pub remote_issue_id: String,
    pub state: String,
    pub asserted_at: i64,
    pub signature: String,
    pub key_id: String,
}

impl AuditReceipt {
    pub fn new(
        store_id: impl Into<String>,
        remote_issue_id: impl Into<String>,
        state: impl Into<String>,
        asserted_at: i64,
        signature: impl Into<String>,
        key_id: impl Into<String>,
    ) -> Self {
        Self {
            store_id: store_id.into(),
            remote_issue_id: remote_issue_id.into(),
            state: state.into(),
            asserted_at,
            signature: signature.into(),
            key_id: key_id.into(),
        }
    }

    /// Build a receipt from the last observed assertion on a stored remote edge.
    pub fn from_dependency(dependency: &RemoteDependency) -> Self {
        Self {
            store_id: dependency.store_id.clone(),
            remote_issue_id: dependency.remote_issue_id.clone(),
            state: dependency.state.clone(),
            asserted_at: dependency.asserted_at,
            signature: dependency.signature.clone(),
            key_id: dependency.key_id.clone(),
        }
    }

    /// Build a receipt from a cached assertion that already verified once.
    pub fn from_cached(assertion: &CachedAssertion) -> Self {
        Self {
            store_id: assertion.store_id.clone(),
            remote_issue_id: assertion.remote_issue_id.clone(),
            state: assertion.state.clone(),
            asserted_at: assertion.asserted_at,
            signature: assertion.signature.clone(),
            key_id: assertion.key_id.clone(),
        }
    }

    /// Reconstruct the signed wire form so verification reuses the cache's crypto path.
    pub fn to_assertion(&self) -> IssueAssertion {
        IssueAssertion {
            schema: ISSUE_ASSERTION_SCHEMA.to_string(),
            store_id: self.store_id.clone(),
            remote_issue_id: self.remote_issue_id.clone(),
            state: self.state.clone(),
            asserted_at: self.asserted_at,
            key_id: self.key_id.clone(),
            signature: Some(self.signature.clone()),
        }
    }

    /// The exact bytes the signature covers.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, AssertionError> {
        self.to_assertion().signing_bytes()
    }
}

/// Every named way a remote receipt can be refused. There is deliberately no `ok`/`ready` variant
/// other than [`Self::Verified`], and no `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    /// Signature verified under the pinned key, the ref matches the signed issue id, and the
    /// observation is inside the freshness window.
    Verified,
    /// The receipt is signed by a key that is not the pinned `key_id` for this store.
    WrongKey,
    /// The receipt claims a different store than the pinned record for this reference.
    WrongStore,
    /// The signature verifies, but the observation is older than the freshness window.
    Stale,
    /// A valid signature over a different issue id: it proves nothing about this reference.
    DifferentIssue,
    /// Missing, undecodable, or cryptographically invalid signature under the pinned key.
    ForgedSignature,
    /// The reference's store has no local pin, so nothing can be verified offline.
    AbsentStore,
    /// There is no receipt at all for the reference (never silence, never a default).
    MissingReceipt,
    /// The stored canonical reference cannot be parsed.
    MalformedReference,
}

impl ReceiptState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::WrongKey => "wrong_key",
            Self::WrongStore => "wrong_store",
            Self::Stale => "stale",
            Self::DifferentIssue => "different_issue",
            Self::ForgedSignature => "forged_signature",
            Self::AbsentStore => "absent_store",
            Self::MissingReceipt => "missing_receipt",
            Self::MalformedReference => "malformed_reference",
        }
    }

    /// True only for a fully verified receipt. Callers must not infer trust any other way.
    pub const fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }
}

impl std::fmt::Display for ReceiptState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One audited remote reference: the named verdict plus the receipt that produced it.
///
/// `receipt` is kept even when the verdict rejects it (the evidence is what the audit is about),
/// but only [`ReceiptState::Verified`] makes it authoritative. Rendering always shows `state`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReferenceAudit {
    pub reference: String,
    pub store_id: String,
    pub remote_issue_id: String,
    pub state: ReceiptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<AuditReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ReferenceAudit {
    pub const fn is_verified(&self) -> bool {
        self.state.is_verified()
    }
}

/// Verify one receipt offline against the pinned key and the exact reference being shown.
///
/// `observed_at` is when *we* recorded the assertion; staleness is judged against it, because a
/// signed assertion has no way to prove it was delivered promptly. When no observation instant is
/// available the assertion timestamp is used, which can only make a receipt look older.
pub fn audit_receipt(
    reference: &CanonicalRef,
    receipt: Option<&AuditReceipt>,
    registry: &RemoteRegistry,
    observed_at: Option<i64>,
    now: i64,
    freshness_window_seconds: i64,
) -> ReferenceAudit {
    let mut audit = ReferenceAudit {
        reference: reference.to_string(),
        store_id: reference.store_id.clone(),
        remote_issue_id: reference.issue_id.clone(),
        state: ReceiptState::Verified,
        receipt: receipt.cloned(),
        detail: None,
    };
    let Some(record) = registry
        .list()
        .iter()
        .find(|record| record.store_id == reference.store_id)
    else {
        audit.state = ReceiptState::AbsentStore;
        audit.detail = Some(format!(
            "store {:?} is not pinned; no key can verify this reference offline",
            reference.store_id
        ));
        return audit;
    };
    let Some(receipt) = receipt else {
        audit.state = ReceiptState::MissingReceipt;
        audit.detail = Some("no audit receipt accompanies this remote reference".to_string());
        return audit;
    };
    // A signature over another issue id is valid evidence for that issue, not for this ref.
    if receipt.remote_issue_id != reference.issue_id {
        audit.state = ReceiptState::DifferentIssue;
        audit.detail = Some(format!(
            "receipt is for remote issue {:?}, but this reference names {:?}",
            receipt.remote_issue_id, reference.issue_id
        ));
        return audit;
    }
    if receipt.store_id != reference.store_id || receipt.store_id != record.store_id {
        audit.state = ReceiptState::WrongStore;
        audit.detail = Some(format!(
            "receipt claims store {:?}, but the pinned record for this reference is {:?}",
            receipt.store_id, record.store_id
        ));
        return audit;
    }
    let Some(pin) = record
        .pins()
        .into_iter()
        .find(|pin| pin.key_id == receipt.key_id)
    else {
        audit.state = ReceiptState::WrongKey;
        audit.detail = Some(format!(
            "receipt key {:?} is not pinned for store {:?} (active key {:?})",
            receipt.key_id, record.store_id, record.key_id
        ));
        return audit;
    };
    if let Err(error) = verify_issue_assertion(&receipt.to_assertion(), &pin, now) {
        audit.state = match error {
            AssertionError::WrongStore { .. } => ReceiptState::WrongStore,
            AssertionError::WrongKeyId { .. } | AssertionError::KeyRetired(..) => {
                ReceiptState::WrongKey
            }
            _ => ReceiptState::ForgedSignature,
        };
        audit.detail = Some(error.to_string());
        return audit;
    }
    let observed_at = observed_at.unwrap_or(receipt.asserted_at);
    if now.saturating_sub(observed_at) > freshness_window_seconds.max(0) {
        audit.state = ReceiptState::Stale;
        audit.detail = Some(format!(
            "observation at {observed_at} is {}s old, past the {freshness_window_seconds}s window",
            now.saturating_sub(observed_at)
        ));
        return audit;
    }
    audit
}

/// Audit a stored remote dependency against the current pin, entirely offline.
///
pub fn audit_remote_dependency(
    dependency: &RemoteDependency,
    registry: &RemoteRegistry,
    now: i64,
    freshness_window_seconds: i64,
) -> ReferenceAudit {
    let receipt = AuditReceipt::from_dependency(dependency);
    match CanonicalRef::parse(&dependency.canonical_ref) {
        Ok(reference) => audit_receipt(
            &reference,
            Some(&receipt),
            registry,
            Some(dependency.observed_at),
            now,
            freshness_window_seconds,
        ),
        Err(error) => ReferenceAudit {
            reference: dependency.canonical_ref.clone(),
            store_id: dependency.store_id.clone(),
            remote_issue_id: dependency.remote_issue_id.clone(),
            state: ReceiptState::MalformedReference,
            receipt: Some(receipt),
            detail: Some(error.to_string()),
        },
    }
}

/// Convenience for pinned-key lookup by store id (used by callers that only have a `PinnedKey`).
pub fn pinned_key_for(registry: &RemoteRegistry, store_id: &str) -> Option<PinnedKey> {
    registry
        .list()
        .iter()
        .find(|record| record.store_id == store_id)
        .map(|record| record.active_pin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_the_normal_form() {
        let text = "marbles://store_ab12/acme/widgets/issue-1";
        let parsed = CanonicalRef::parse(text).unwrap();
        assert_eq!(parsed.to_string(), text);
        assert_eq!(CanonicalRef::parse(&parsed.to_string()).unwrap(), parsed);
    }
}

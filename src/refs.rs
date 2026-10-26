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
    AssertionFreshness, ISSUE_ASSERTION_SCHEMA, IssueAssertion, verify_issue_assertion,
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

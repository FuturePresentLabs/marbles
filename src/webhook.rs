//! Durable signed publication of tracker mutations.
//!
//! The database transaction that records history also appends the outbox row. HTTP delivery is
//! deliberately asynchronous: an unavailable consumer never makes a Marble mutation fail, and
//! the consumer's ordinary reconciliation poll remains the recovery path.
//!
//! Two identity paths exist here and they must stay separate:
//!
//! * the **private webhook** is a shared delivery channel authenticated with HMAC-SHA256;
//! * a **federation assertion** is public, store-scoped, and signed with the store's Ed25519 key.
//!
//! Both derive the event id from `(store_id, seq)`. A local sequence number is only meaningful
//! inside one store, so `company:seq` was ambiguous across independent stores; `store_id:seq` is
//! not. Replay detection is keyed the same way, so the same sequence from another store is a
//! different event rather than a replay.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use openssl::pkey::{Id, PKey};
use openssl::sign::Verifier;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::config::WebhookConfig;
use crate::db::snapshot::SnapshotSigner;
use crate::db::{CompanyStoreRegistry, OutboundEvent, Store, now};
use crate::identity::PinnedKey;

type HmacSha256 = Hmac<Sha256>;

/// Private webhook envelope schema. This channel is authenticated by HMAC, not by the store key.
pub const EVENT_SCHEMA: &str = "marbles.event/1";
/// Public, Ed25519-signed federation assertion schema.
pub const ASSERTION_SCHEMA: &str = "marbles.assertion/1";

/// A store-local sequence plus the store that minted it.
///
/// `seq` alone is not globally unique; `(store_id, seq)` is, and `id()` is the wire rendering of
/// that pair. Both stores in the collision test can therefore use `seq = 7` safely.
pub fn event_id(store_id: &str, seq: i64) -> String {
    format!("{store_id}:{seq}")
}

/// The private webhook envelope. `id` is store-scoped even though the channel is private, so a
/// consumer may share its deduplication namespace with federation without ambiguity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema: String,
    pub id: String,
    pub store_id: String,
    pub company: String,
    pub project: String,
    pub issue_id: String,
    pub event: String,
    pub occurred_at: i64,
}

impl EventEnvelope {
    pub fn from_event(company: &str, event: &OutboundEvent) -> Self {
        Self {
            schema: EVENT_SCHEMA.to_string(),
            id: event_id(&event.store_id, event.seq),
            store_id: event.store_id.clone(),
            company: company.to_string(),
            project: event.project.clone(),
            issue_id: event.issue_id.clone(),
            event: event.event.clone(),
            occurred_at: event.occurred_at,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FederationError {
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
    #[error("assertion json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("crypto failure: {0}")]
    Crypto(String),
    #[error(transparent)]
    Replay(#[from] ReplayError),
}

/// A public federation assertion.
///
/// The signature covers the assertion with `signature` removed, exactly as serialized here, so
/// no field can be changed after signing without invalidating it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationAssertion {
    pub schema: String,
    pub store_id: String,
    pub seq: i64,
    pub id: String,
    pub company: String,
    pub project: String,
    pub issue_id: String,
    pub event: String,
    pub occurred_at: i64,
    pub key_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl FederationAssertion {
    /// Build the unsigned assertion for an outbox event. `key_id` is set by the signer.
    pub fn from_event(company: &str, event: &OutboundEvent) -> Self {
        Self {
            schema: ASSERTION_SCHEMA.to_string(),
            store_id: event.store_id.clone(),
            seq: event.seq,
            id: event_id(&event.store_id, event.seq),
            company: company.to_string(),
            project: event.project.clone(),
            issue_id: event.issue_id.clone(),
            event: event.event.clone(),
            occurred_at: event.occurred_at,
            key_id: String::new(),
            signature: None,
        }
    }

    /// Canonical bytes the signature covers: everything except the signature itself.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, FederationError> {
        let mut copy = self.clone();
        copy.signature = None;
        Ok(serde_json::to_vec(&copy)?)
    }
}

/// Sign an assertion with an Ed25519 signer. The signer's key id is recorded so a verifier can
/// select the pinned key rather than trusting whatever the payload claims.
pub fn sign_federation_assertion(
    mut assertion: FederationAssertion,
    signer: &dyn SnapshotSigner,
) -> Result<FederationAssertion, FederationError> {
    if assertion.schema != ASSERTION_SCHEMA {
        return Err(FederationError::UnsupportedSchema(assertion.schema));
    }
    assertion.key_id = signer.key_id().to_string();
    assertion.signature = None;
    let signature = signer
        .sign(&assertion.signing_bytes()?)
        .map_err(|error| FederationError::Crypto(error.to_string()))?;
    assertion.signature = Some(URL_SAFE_NO_PAD.encode(signature));
    Ok(assertion)
}

/// Verify an assertion against an offline pin. Fails closed on every mismatch.
pub fn verify_federation_assertion(
    assertion: &FederationAssertion,
    pin: &PinnedKey,
    now: i64,
) -> Result<(), FederationError> {
    if assertion.schema != ASSERTION_SCHEMA {
        return Err(FederationError::UnsupportedSchema(assertion.schema.clone()));
    }
    if pin.store_id.is_empty() || assertion.store_id != pin.store_id {
        return Err(FederationError::WrongStore {
            actual: assertion.store_id.clone(),
            expected: pin.store_id.clone(),
        });
    }
    if assertion.key_id != pin.key_id {
        return Err(FederationError::WrongKeyId {
            actual: assertion.key_id.clone(),
            expected: pin.key_id.clone(),
        });
    }
    if let Some(valid_until) = pin.valid_until
        && now >= valid_until
    {
        return Err(FederationError::KeyRetired(pin.key_id.clone(), valid_until));
    }
    let signature = assertion
        .signature
        .as_deref()
        .ok_or(FederationError::Unsigned)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|error| FederationError::MalformedSignature(error.to_string()))?;
    let public_key = URL_SAFE_NO_PAD
        .decode(&pin.public_key)
        .map_err(|error| FederationError::MalformedSignature(error.to_string()))?;
    let key = PKey::public_key_from_raw_bytes(&public_key, Id::ED25519)
        .map_err(|error| FederationError::Crypto(error.to_string()))?;
    let mut verifier = Verifier::new_without_digest(&key)
        .map_err(|error| FederationError::Crypto(error.to_string()))?;
    let valid = verifier
        .verify_oneshot(&signature, &assertion.signing_bytes()?)
        .unwrap_or(false);
    if !valid {
        return Err(FederationError::BadSignature);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("event seq {seq} for store {store_id:?} is not a positive sequence")]
    InvalidSequence { store_id: String, seq: i64 },
    #[error(
        "event seq {seq} for store {store_id:?} was already accepted (last accepted {last_seq})"
    )]
    Replayed {
        store_id: String,
        seq: i64,
        last_seq: i64,
    },
}

/// Receiver-side replay guard keyed by `(store_id, seq)`.
///
/// A sequence is accepted at most once per store, and only forward. Two stores may use the same
/// sequence independently: their identities differ, so they are different events.
#[derive(Debug, Default, Clone)]
pub struct ReplayGuard {
    last: BTreeMap<String, i64>,
}

impl ReplayGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, store_id: &str, seq: i64) -> Result<(), ReplayError> {
        if seq <= 0 {
            return Err(ReplayError::InvalidSequence {
                store_id: store_id.to_string(),
                seq,
            });
        }
        if let Some(&last_seq) = self.last.get(store_id)
            && seq <= last_seq
        {
            return Err(ReplayError::Replayed {
                store_id: store_id.to_string(),
                seq,
                last_seq,
            });
        }
        self.last.insert(store_id.to_string(), seq);
        Ok(())
    }

    pub fn last_seq(&self, store_id: &str) -> Option<i64> {
        self.last.get(store_id).copied()
    }
}

/// Verify a store's assertions and enforce its replay guard in one failure-closed step.
#[derive(Debug, Clone)]
pub struct FederationVerifier {
    pin: PinnedKey,
    replay: ReplayGuard,
}

impl FederationVerifier {
    pub fn new(pin: PinnedKey) -> Self {
        Self {
            pin,
            replay: ReplayGuard::new(),
        }
    }

    pub fn pin(&self) -> &PinnedKey {
        &self.pin
    }

    pub fn last_seq(&self) -> Option<i64> {
        self.replay.last_seq(&self.pin.store_id)
    }

    pub fn verify(
        &mut self,
        assertion: &FederationAssertion,
        now: i64,
    ) -> Result<(), FederationError> {
        verify_federation_assertion(assertion, &self.pin, now)?;
        self.replay.accept(&assertion.store_id, assertion.seq)?;
        Ok(())
    }
}

pub struct Publisher {
    config: WebhookConfig,
    secret: Vec<u8>,
    client: reqwest::Client,
    default_db: Arc<dyn Store>,
    companies: Option<Arc<dyn CompanyStoreRegistry>>,
}

impl Publisher {
    pub fn new(
        config: WebhookConfig,
        default_db: Arc<dyn Store>,
        companies: Option<Arc<dyn CompanyStoreRegistry>>,
    ) -> Result<Self, String> {
        let secret = std::fs::read(&config.secret_file).map_err(|e| {
            format!(
                "reading webhook secret {}: {e}",
                config.secret_file.display()
            )
        })?;
        let secret = secret.strip_suffix(b"\n").unwrap_or(&secret).to_vec();
        if secret.len() < 32 {
            return Err("webhook secret must contain at least 32 bytes".into());
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds))
            .build()
            .map_err(|e| format!("building webhook client: {e}"))?;
        Ok(Self {
            config,
            secret,
            client,
            default_db,
            companies,
        })
    }

    pub async fn run(self) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            if let Err(error) = self.deliver_once().await {
                eprintln!("marbles webhook dispatcher: {error}");
            }
        }
    }

    async fn deliver_once(&self) -> Result<(), String> {
        let stores = match &self.companies {
            Some(companies) => companies.open_stores().map_err(|e| e.to_string())?,
            None => vec![("local".to_string(), Arc::clone(&self.default_db))],
        };
        for (company, db) in stores {
            let pending_db = Arc::clone(&db);
            let events = tokio::task::spawn_blocking(move || pending_db.pending_events(now(), 64))
                .await
                .map_err(|_| "outbox storage worker failed".to_string())?
                .map_err(|e| e.to_string())?;
            for event in events {
                let outcome = self.deliver(&company, &event).await;
                let update_db = Arc::clone(&db);
                tokio::task::spawn_blocking(move || match outcome {
                    Ok(()) => update_db.mark_event_delivered(event.seq, now()),
                    Err(error) => {
                        let exponent = event.attempts.clamp(0, 8) as u32;
                        let delay = 1_i64 << exponent;
                        update_db.mark_event_failed(event.seq, now() + delay, &error)
                    }
                })
                .await
                .map_err(|_| "outbox update worker failed".to_string())?
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn deliver(&self, company: &str, event: &OutboundEvent) -> Result<(), String> {
        let envelope = EventEnvelope::from_event(company, event);
        let body = serde_json::to_vec(&envelope).map_err(|e| e.to_string())?;
        let signature = hmac_signature(&self.secret, &body)?;
        let response = self
            .client
            .post(&self.config.url)
            .header("content-type", "application/json")
            .header("x-marbles-signature", format!("sha256={signature}"))
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("receiver returned {}", response.status()))
        }
    }
}

/// HMAC-SHA256 over the exact delivered body. This is the private webhook credential path and is
/// deliberately not used for federation assertions, which carry a store's Ed25519 signature.
pub fn hmac_signature(secret: &[u8], body: &[u8]) -> Result<String, String> {
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|e| e.to_string())?;
    mac.update(body);
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Verify an `x-marbles-signature` header of the form `sha256=<hex>` in constant time.
pub fn verify_hmac_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(expected) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(actual) = hmac_signature(secret, body) else {
        return false;
    };
    constant_time_eq(actual.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_and_body_bound() {
        let secret = b"01234567890123456789012345678901";
        assert_eq!(
            hmac_signature(secret, b"one").unwrap(),
            hmac_signature(secret, b"one").unwrap()
        );
        assert_ne!(
            hmac_signature(secret, b"one").unwrap(),
            hmac_signature(secret, b"two").unwrap()
        );
    }
}

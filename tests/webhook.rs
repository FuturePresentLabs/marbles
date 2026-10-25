//! Federation event identity is store-scoped. These tests exercise the wire id, the private HMAC
//! channel, Ed25519 assertion verification, and the `(store_id, seq)` replay guard.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use marbles::db::OutboundEvent;
use marbles::db::snapshot::{Ed25519Signer, SnapshotSigner};
use marbles::identity::PinnedKey;
use marbles::webhook::{
    EventEnvelope, FederationAssertion, FederationError, FederationVerifier, ReplayError,
    ReplayGuard, event_id, hmac_signature, sign_federation_assertion, verify_federation_assertion,
    verify_hmac_signature,
};

const COMPANY: &str = "acme";

fn event(store_id: &str, seq: i64) -> OutboundEvent {
    OutboundEvent {
        seq,
        store_id: store_id.to_string(),
        issue_id: "acme-1".to_string(),
        project: "acme".to_string(),
        occurred_at: 1_700_000_000,
        event: "created".to_string(),
        attempts: 0,
    }
}

fn signer(key_id: &str) -> Ed25519Signer {
    Ed25519Signer::generate(key_id).unwrap()
}

fn pin(store_id: &str, signer: &Ed25519Signer) -> PinnedKey {
    PinnedKey {
        store_id: store_id.to_string(),
        key_id: signer.key_id().to_string(),
        public_key: URL_SAFE_NO_PAD.encode(signer.public_key().unwrap()),
        valid_until: None,
    }
}

fn signed(store_id: &str, seq: i64, signer: &Ed25519Signer) -> FederationAssertion {
    sign_federation_assertion(
        FederationAssertion::from_event(COMPANY, &event(store_id, seq)),
        signer,
    )
    .unwrap()
}

#[test]
fn two_stores_same_company_and_seq_do_not_collide() {
    let first = EventEnvelope::from_event(COMPANY, &event("store_a", 7));
    let second = EventEnvelope::from_event(COMPANY, &event("store_b", 7));

    assert_eq!(first.company, second.company);
    assert_eq!(first.occurred_at, second.occurred_at);
    assert_eq!(first.id, "store_a:7");
    assert_eq!(second.id, "store_b:7");
    assert_ne!(first.id, second.id);
    assert_eq!(first.id, event_id("store_a", 7));

    // The same sequence from a different store is a different event, not a replay.
    let mut replay = ReplayGuard::new();
    replay.accept("store_a", 7).unwrap();
    replay.accept("store_b", 7).unwrap();
}

#[test]
fn envelope_includes_store_id() {
    let envelope = EventEnvelope::from_event(COMPANY, &event("store_abc123", 3));
    let wire = serde_json::to_value(&envelope).unwrap();

    assert_eq!(wire["schema"], "marbles.event/1");
    assert_eq!(wire["store_id"], "store_abc123");
    assert_eq!(wire["id"], "store_abc123:3");
    assert_eq!(wire["company"], COMPANY);
}

#[test]
fn replayed_event_seq_is_rejected() {
    let mut replay = ReplayGuard::new();
    replay.accept("store_a", 9).unwrap();

    let replayed = replay.accept("store_a", 9).unwrap_err();
    assert!(matches!(
        replayed,
        ReplayError::Replayed {
            ref store_id,
            seq: 9,
            last_seq: 9,
        } if store_id == "store_a"
    ));
    // An older sequence is also a replay: acceptance is forward-only per store.
    assert!(matches!(
        replay.accept("store_a", 8),
        Err(ReplayError::Replayed { .. })
    ));
    assert!(matches!(
        replay.accept("store_a", 0),
        Err(ReplayError::InvalidSequence { .. })
    ));

    // The assertion verifier enforces the same guard on its normal path.
    let signer = signer("store_a/key-1");
    let assertion = signed("store_a", 1, &signer);
    let mut verifier = FederationVerifier::new(pin("store_a", &signer));
    verifier.verify(&assertion, 1_700_000_001).unwrap();
    assert!(matches!(
        verifier.verify(&assertion, 1_700_000_002),
        Err(FederationError::Replay(ReplayError::Replayed { .. }))
    ));
}

#[test]
fn wrong_store_event_is_rejected() {
    let store_a = signer("store_a/key-1");
    let store_b = signer("store_b/key-1");
    let assertion = signed("store_a", 4, &store_a);

    // Signed correctly by A, but the verifier pins B: refuse rather than report a bad signature.
    let error = verify_federation_assertion(&assertion, &pin("store_b", &store_b), 1_700_000_001)
        .unwrap_err();
    assert!(matches!(
        error,
        FederationError::WrongStore {
            ref actual,
            ref expected,
        } if actual == "store_a" && expected == "store_b"
    ));

    // B cannot verify A's assertion by relabelling it either: the key id is store-qualified, so
    // the first mismatch is rejected before any signature check.
    let mut relabelled = assertion.clone();
    relabelled.store_id = "store_b".to_string();
    assert!(matches!(
        verify_federation_assertion(&relabelled, &pin("store_b", &store_b), 1_700_000_001),
        Err(FederationError::WrongKeyId { .. })
    ));

    // Even relabelling both store and key id cannot turn A's signature into B's assertion.
    let mut relabelled = assertion.clone();
    relabelled.store_id = "store_b".to_string();
    relabelled.key_id = "store_b/key-1".to_string();
    assert!(matches!(
        verify_federation_assertion(&relabelled, &pin("store_b", &store_b), 1_700_000_001),
        Err(FederationError::BadSignature)
    ));

    // The verifier that pins A accepts it.
    let mut verifier = FederationVerifier::new(pin("store_a", &store_a));
    verifier.verify(&assertion, 1_700_000_001).unwrap();
}

#[test]
fn private_hmac_webhook_still_verifies() {
    let secret = b"01234567890123456789012345678901";
    let envelope = EventEnvelope::from_event(COMPANY, &event("store_a", 11));
    let body = serde_json::to_vec(&envelope).unwrap();

    let signature = hmac_signature(secret, &body).unwrap();
    let header = format!("sha256={signature}");
    assert!(verify_hmac_signature(secret, &body, &header));

    // Body bound: a changed store id or event invalidates the private delivery signature.
    let mut tampered = envelope.clone();
    tampered.store_id = "store_b".to_string();
    let tampered_body = serde_json::to_vec(&tampered).unwrap();
    assert!(!verify_hmac_signature(secret, &tampered_body, &header));
    assert!(!verify_hmac_signature(
        b"abcdefghijklmnopqrstuvwxyz012345",
        &body,
        &header
    ));

    // A federation assertion is a different, Ed25519-signed path, not the HMAC secret.
    let signer = signer("store_a/key-1");
    let assertion = signed("store_a", 11, &signer);
    let assertion_json = serde_json::to_vec(&assertion).unwrap();
    assert!(!verify_hmac_signature(secret, &assertion_json, &header));
}

#[test]
fn legacy_outbox_rows_are_backfilled_with_store_id() {
    // A database written before event identity had no `store_id` column. Opening it must add the
    // column and backfill it with the store's stable identity, not leave an ambiguous `:seq` id.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marbles.db");
    let legacy = rusqlite::Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE outbound_event(
               seq INTEGER PRIMARY KEY,
               issue_id TEXT NOT NULL,
               project TEXT NOT NULL,
               occurred_at INTEGER NOT NULL,
               event TEXT NOT NULL,
               delivered_at INTEGER,
               attempts INTEGER NOT NULL DEFAULT 0,
               next_attempt_at INTEGER NOT NULL,
               last_error TEXT
             );
             INSERT INTO outbound_event(seq,issue_id,project,occurred_at,event,next_attempt_at)
               VALUES(1,'legacy-1','demo',1,'created',1);",
        )
        .unwrap();
    drop(legacy);

    let db = marbles::db::Db::open(&path).unwrap();
    let pending = db.pending_events(1_700_000_000, 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].store_id, db.store_id());
    assert!(!pending[0].store_id.is_empty());
    assert_eq!(
        event_id(&pending[0].store_id, pending[0].seq),
        format!("{}:1", db.store_id())
    );
}

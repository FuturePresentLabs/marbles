//! Acceptance tests for `marbles.store/1` store identity, offline descriptor verification, and
//! named key rotation with a bounded overlap window. Every test runs in a temp directory; no test
//! touches `$MARBLES_HOME` or the network.

use marbles::identity::{
    DEFAULT_OVERLAP_SECONDS, DESCRIPTOR_SCHEMA, IdentityError, InitOptions, PinnedKey,
    StoreDescriptor, StoreIdentity, verify_descriptor_json,
};

fn identity_in(dir: &tempfile::TempDir, name: &str, at: i64) -> StoreIdentity {
    let path = dir.path().join(name).join("store-identity.toml");
    let mut opts = InitOptions::new(name);
    opts.endpoint = Some(format!("https://{name}.example.invalid"));
    StoreIdentity::init_at(&path, &opts, at).expect("identity init")
}

fn descriptor_json(identity: &StoreIdentity, at: i64) -> String {
    identity.descriptor_json(at).expect("descriptor json")
}

#[test]
fn two_stores_have_distinct_store_and_key_ids() {
    let dir = tempfile::tempdir().unwrap();
    let alpha = identity_in(&dir, "alpha", 1_000);
    let beta = identity_in(&dir, "beta", 1_000);

    assert_ne!(alpha.store_id, beta.store_id, "store ids must be distinct");
    assert_ne!(
        alpha.active_key_id, beta.active_key_id,
        "key ids must be distinct"
    );
    assert_ne!(
        alpha.active_key().unwrap().public_key,
        beta.active_key().unwrap().public_key,
        "public keys must be distinct"
    );

    // Stable across reload: the same files yield the same ids.
    let alpha_path = dir.path().join("alpha/store-identity.toml");
    let reloaded = StoreIdentity::load(&alpha_path).unwrap();
    assert_eq!(reloaded.store_id, alpha.store_id);
    assert_eq!(reloaded.active_key_id, alpha.active_key_id);
}

#[test]
fn descriptor_verifies_offline_with_pinned_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = identity_in(&dir, "offline", 1_000);
    let descriptor = descriptor_json(&store, 1_100);
    let pin = store.active_pin().unwrap();

    let parsed = StoreDescriptor::from_json(&descriptor).unwrap();
    let debug = format!("{store:?}");
    assert_eq!(parsed.schema, DESCRIPTOR_SCHEMA);
    assert_eq!(parsed.store_id, store.store_id);
    assert_eq!(parsed.key_id, store.active_key_id);
    assert_eq!(parsed.public_key, pin.public_key);
    assert!(parsed.signature.is_some());
    assert_eq!(
        parsed.capabilities,
        InitOptions::new("x").capabilities,
        "descriptor advertises the store capabilities"
    );

    // The descriptor and the pin are all an offline verifier needs: drop the identity first.
    drop(store);
    let verified = verify_descriptor_json(&descriptor, &pin, 1_200).expect("offline verify");
    assert_eq!(verified.store_id, parsed.store_id);
    assert_eq!(verified.key_id, parsed.key_id);

    // Reordering JSON fields must not break signature verification.
    let value: serde_json::Value = serde_json::from_str(&descriptor).unwrap();
    let reordered = serde_json::json!({
        "signature": value["signature"],
        "issued_at": value["issued_at"],
        "capabilities": value["capabilities"],
        "public_key": value["public_key"],
        "key_id": value["key_id"],
        "display_name": value["display_name"],
        "endpoint": value["endpoint"],
        "store_id": value["store_id"],
        "schema": value["schema"],
    });
    verify_descriptor_json(&reordered.to_string(), &pin, 1_200)
        .expect("field order is not part of the signed payload");

    // Descriptor output must never leak private material.
    assert!(!descriptor.contains("PRIVATE KEY"));
    assert!(!debug.contains("PRIVATE KEY"));
}

#[test]
fn rotation_overlap_accepts_old_and_new_then_rejects_old() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotating/store-identity.toml");
    let mut store = identity_in(&dir, "rotating", 1_000);
    let old_key_id = store.active_key_id.clone();
    let old_descriptor = descriptor_json(&store, 1_000);

    let rotated_at = 2_000;
    let overlap_seconds = 300;
    let record = store.rotate(rotated_at, overlap_seconds).unwrap();
    assert_eq!(record.old_key_id, old_key_id);
    assert_eq!(record.overlap_until, rotated_at + overlap_seconds);
    assert_ne!(record.new_key_id, old_key_id);
    assert_eq!(store.active_key_id, record.new_key_id);
    store.save(&path).unwrap();

    let new_descriptor = descriptor_json(&store, rotated_at);

    // Both keys verify inside the overlap window...
    let during = rotated_at + 1;
    assert!(
        store.verify_descriptor(&old_descriptor, during).is_ok(),
        "old key must verify during the overlap"
    );
    assert!(
        store.verify_descriptor(&new_descriptor, during).is_ok(),
        "new key must verify during the overlap"
    );

    // ...and the old key fails closed once the overlap ends.
    let after = rotated_at + overlap_seconds;
    let err = store
        .verify_descriptor(&old_descriptor, after)
        .expect_err("old key must be rejected after the overlap");
    assert!(
        matches!(err, IdentityError::KeyRetired(ref key, _) if *key == old_key_id),
        "unexpected error: {err}"
    );
    assert!(
        store.verify_descriptor(&new_descriptor, after).is_ok(),
        "new key stays valid after the overlap"
    );

    // The rotation and its overlap window survive a save/reload.
    let reloaded = StoreIdentity::load(&path).unwrap();
    assert_eq!(reloaded.rotations.last().unwrap(), &record);
    assert!(reloaded.verify_descriptor(&old_descriptor, after).is_err());
    assert!(reloaded.verify_descriptor(&old_descriptor, during).is_ok());
}

#[test]
fn wrong_store_descriptor_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let alpha = identity_in(&dir, "alpha", 1_000);
    let beta = identity_in(&dir, "beta", 1_000);

    let alpha_descriptor = descriptor_json(&alpha, 1_100);
    let beta_pin = beta.active_pin().unwrap();

    let err = verify_descriptor_json(&alpha_descriptor, &beta_pin, 1_200)
        .expect_err("a descriptor from another store must be rejected");
    assert!(
        matches!(err, IdentityError::StoreMismatch { .. }),
        "must fail closed on the store boundary, got: {err}"
    );

    // An identity likewise refuses a descriptor whose signed store_id was re-labelled.
    let mut foreign: serde_json::Value = serde_json::from_str(&alpha_descriptor).unwrap();
    foreign["store_id"] = serde_json::json!(beta.store_id);
    let err = verify_descriptor_json(&foreign.to_string(), &alpha.active_pin().unwrap(), 1_200)
        .expect_err("a re-labelled store must be rejected");
    assert!(
        matches!(err, IdentityError::StoreMismatch { .. }),
        "store_id is part of the identity, got: {err}"
    );
}

#[test]
fn wrong_key_descriptor_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = identity_in(&dir, "keys", 1_000);
    let old_key_id = store.active_key_id.clone();
    let old_descriptor = descriptor_json(&store, 1_000);
    store.rotate(2_000, 3_600).unwrap();
    let new_descriptor = descriptor_json(&store, 2_000);
    let old_pin = store.pin(&old_key_id).unwrap();

    // Pinned key id says key-1, descriptor is signed by key-2: reject.
    let err = verify_descriptor_json(&new_descriptor, &old_pin, 2_100)
        .expect_err("descriptor signed by a different key must be rejected");
    assert!(
        matches!(err, IdentityError::KeyMismatch { .. }),
        "expected KeyMismatch, got: {err}"
    );

    // Pinned public key that does not match the descriptor's key: reject.
    let wrong_public_key = PinnedKey {
        store_id: store.store_id.clone(),
        key_id: store.active_key_id.clone(),
        public_key: old_pin.public_key.clone(),
        valid_until: None,
    };
    let err = verify_descriptor_json(&new_descriptor, &wrong_public_key, 2_100)
        .expect_err("descriptor whose public key differs from the pin must be rejected");
    assert!(
        matches!(err, IdentityError::PublicKeyMismatch(_)),
        "expected PublicKeyMismatch, got: {err}"
    );

    // Tampered signature bytes fail closed.
    let mut forged: serde_json::Value = serde_json::from_str(&new_descriptor).unwrap();
    let mut signature = forged["signature"].as_str().unwrap().to_string();
    let first = signature.remove(0);
    signature.insert(0, if first == 'A' { 'B' } else { 'A' });
    forged["signature"] = serde_json::json!(signature);
    let err = verify_descriptor_json(&forged.to_string(), &store.active_pin().unwrap(), 2_100)
        .expect_err("a tampered signature must be rejected");
    assert!(
        matches!(err, IdentityError::BadSignature(_)),
        "expected BadSignature, got: {err}"
    );

    // An unsigned descriptor is never accepted, even with the right pin.
    let mut unsigned: serde_json::Value = serde_json::from_str(&old_descriptor).unwrap();
    unsigned.as_object_mut().unwrap().remove("signature");
    let err = verify_descriptor_json(
        &unsigned.to_string(),
        &store.pin(&old_key_id).unwrap(),
        1_100,
    )
    .expect_err("an unsigned descriptor must be rejected");
    assert!(
        matches!(err, IdentityError::UnsignedDescriptor),
        "expected UnsignedDescriptor, got: {err}"
    );
}

#[test]
fn default_overlap_matches_documented_window() {
    assert_eq!(DEFAULT_OVERLAP_SECONDS, 24 * 60 * 60);
}

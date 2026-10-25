//! Acceptance tests for the pinned remote registry: a remote is trusted because its
//! `marbles.store/1` descriptor verified against a locally pinned `(store_id, key_id, public_key)`,
//! never because an endpoint answered. Every test runs in a temp directory; nothing here touches
//! `$MARBLES_HOME`, a live store, or the network.

use marbles::config;
use marbles::identity::{
    IdentityError, InitOptions, RemoteRegistry, RemoteRegistryError, StoreDescriptor,
    StoreIdentity, read_descriptor_source,
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

fn endpoint(name: &str) -> String {
    format!("https://{name}.example.invalid")
}

#[test]
fn remote_add_pins_ed25519_identity() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("state/remotes.json");
    let alpha = identity_in(&dir, "alpha", 1_000);
    let descriptor = descriptor_json(&alpha, 1_100);
    let public_key = alpha.active_key().unwrap().public_key.clone();

    let mut registry = RemoteRegistry::load(&registry_path).unwrap();
    assert!(registry.list().is_empty(), "a missing registry is empty");

    let record = registry
        .add("alpha", &endpoint("alpha"), &descriptor, 1_200)
        .expect("verified descriptor pins")
        .clone();
    assert_eq!(record.name, "alpha");
    assert_eq!(record.endpoint, endpoint("alpha"));
    assert_eq!(record.store_id, alpha.store_id);
    assert_eq!(record.key_id, alpha.active_key_id);
    assert_eq!(record.public_key, public_key);
    assert_eq!(record.valid_until, None);
    assert_eq!(record.added_at, 1_200);
    assert_eq!(record.updated_at, 1_200);
    assert!(record.overlap_keys.is_empty());
    assert_eq!(
        record.capabilities,
        StoreDescriptor::from_json(&descriptor)
            .unwrap()
            .capabilities
    );

    // The pin is persisted and reloads to exactly the same identity.
    registry.save(&registry_path).unwrap();
    let reloaded = RemoteRegistry::load(&registry_path).unwrap();
    assert_eq!(reloaded.list(), registry.list());
    assert_eq!(reloaded.get("alpha").unwrap(), &record);

    // Offline verification needs only the pin: the store identity is not consulted again.
    let verified = reloaded
        .verify("alpha", &descriptor, 1_200)
        .expect("pinned key verifies the descriptor offline");
    assert_eq!(verified.store_id, alpha.store_id);
    assert_eq!(verified.key_id, alpha.active_key_id);

    // Re-adding the same name is refused rather than silently re-pinning.
    let mut registry = reloaded.clone();
    let err = registry
        .add("alpha", &endpoint("alpha"), &descriptor, 1_300)
        .expect_err("duplicate name must fail");
    assert!(matches!(err, RemoteRegistryError::Exists(ref name) if name == "alpha"));

    // No private key material in the registry or its Debug rendering.
    let text = std::fs::read_to_string(&registry_path).unwrap();
    assert!(!text.contains("private_key"));
    assert!(!text.contains("PRIVATE KEY"));
    assert!(!format!("{registry:?}").contains("PRIVATE KEY"));
}

#[test]
fn wrong_key_remote_descriptor_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = identity_in(&dir, "keys", 1_000);
    let old_descriptor = descriptor_json(&store, 1_000);

    let mut registry = RemoteRegistry::default();
    registry
        .add("keys", &endpoint("keys"), &old_descriptor, 1_100)
        .unwrap();
    let pinned_key_id = store.active_key_id.clone();

    // The store rotates; a descriptor signed by the new key is not the pinned key.
    store.rotate(2_000, 3_600).unwrap();
    let new_descriptor = descriptor_json(&store, 2_000);
    let err = registry
        .verify("keys", &new_descriptor, 2_100)
        .expect_err("a descriptor from an unpinned key must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::KeyMismatch { ref expected, ref found })
                if *expected == pinned_key_id && *found == store.active_key_id
        ),
        "unexpected error: {err}"
    );
    assert_eq!(
        registry.get("keys").unwrap().key_id,
        pinned_key_id,
        "a rejected descriptor never re-pins the key"
    );

    // Tampered signature bytes fail closed, even under the pinned key id.
    let mut forged: serde_json::Value = serde_json::from_str(&old_descriptor).unwrap();
    let mut signature = forged["signature"].as_str().unwrap().to_string();
    let first = signature.remove(0);
    signature.insert(0, if first == 'A' { 'B' } else { 'A' });
    forged["signature"] = serde_json::json!(signature);
    let err = registry
        .verify("keys", &forged.to_string(), 1_100)
        .expect_err("a tampered signature must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::BadSignature(ref key))
                if *key == pinned_key_id
        ),
        "unexpected error: {err}"
    );

    // A descriptor that simply drops its signature is never accepted.
    let mut unsigned: serde_json::Value = serde_json::from_str(&old_descriptor).unwrap();
    unsigned.as_object_mut().unwrap().remove("signature");
    let err = registry
        .verify("keys", &unsigned.to_string(), 1_100)
        .expect_err("an unsigned descriptor must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::UnsignedDescriptor)
        ),
        "unexpected error: {err}"
    );

    // A malformed signature encoding fails closed as a named error too.
    let mut malformed: serde_json::Value = serde_json::from_str(&old_descriptor).unwrap();
    malformed["signature"] = serde_json::json!("not base64url!!!");
    let err = registry
        .verify("keys", &malformed.to_string(), 1_100)
        .expect_err("a malformed signature must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::MalformedSignature(_))
        ),
        "unexpected error: {err}"
    );

    // A descriptor from another schema is rejected before any key comparison.
    let mut foreign_schema: serde_json::Value = serde_json::from_str(&old_descriptor).unwrap();
    foreign_schema["schema"] = serde_json::json!("marbles.store/2");
    let err = registry
        .verify("keys", &foreign_schema.to_string(), 1_100)
        .expect_err("an unknown descriptor schema must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::WrongSchema { .. })
        ),
        "unexpected error: {err}"
    );
}

#[test]
fn wrong_store_remote_descriptor_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let alpha = identity_in(&dir, "alpha", 1_000);
    let beta = identity_in(&dir, "beta", 1_000);
    let alpha_descriptor = descriptor_json(&alpha, 1_100);
    let beta_descriptor = descriptor_json(&beta, 1_100);

    let mut registry = RemoteRegistry::default();
    registry
        .add("alpha", &endpoint("alpha"), &alpha_descriptor, 1_200)
        .unwrap();
    let pinned_store_id = alpha.store_id.clone();

    // A descriptor from a different store never verifies against alpha's pin.
    let err = registry
        .verify("alpha", &beta_descriptor, 1_200)
        .expect_err("another store's descriptor must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::StoreMismatch { ref expected, ref found })
                if *expected == pinned_store_id && *found == beta.store_id
        ),
        "unexpected error: {err}"
    );

    // Re-labelling the signed store_id does not launder the descriptor.
    let mut relabelled: serde_json::Value = serde_json::from_str(&alpha_descriptor).unwrap();
    relabelled["store_id"] = serde_json::json!(beta.store_id);
    let err = registry
        .verify("alpha", &relabelled.to_string(), 1_200)
        .expect_err("a re-labelled store must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::StoreMismatch { .. })
        ),
        "unexpected error: {err}"
    );

    // Rotation across a store boundary is refused as well.
    let err = registry
        .rotate("alpha", &beta_descriptor, None, 300, 1_300)
        .expect_err("rotating to another store's key must be rejected");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::StoreMismatch { .. })
        ),
        "unexpected error: {err}"
    );
    assert_eq!(registry.get("alpha").unwrap().store_id, pinned_store_id);
    assert_eq!(registry.get("alpha").unwrap().key_id, alpha.active_key_id);
}

#[test]
fn rotation_overlap_accepts_old_and_new_then_rejects_old() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("state/remotes.json");
    let mut store = identity_in(&dir, "rotating", 1_000);
    let old_descriptor = descriptor_json(&store, 1_000);
    let old_key_id = store.active_key_id.clone();

    let mut registry = RemoteRegistry::default();
    registry
        .add("rotating", &endpoint("rotating"), &old_descriptor, 1_100)
        .unwrap();

    let rotated_at = 2_000;
    let overlap_seconds = 300;
    let new_key_id = store
        .rotate(rotated_at, overlap_seconds)
        .unwrap()
        .new_key_id;
    let new_descriptor = descriptor_json(&store, rotated_at);
    let new_public_key = store.active_key().unwrap().public_key.clone();

    // An out-of-band fingerprint that does not match the new key is refused, unchanged.
    let err = registry
        .rotate(
            "rotating",
            &new_descriptor,
            Some("AAAfingerprint-that-is-not-the-new-key"),
            0,
            rotated_at,
        )
        .expect_err("a fingerprint for the wrong key must be refused");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::PublicKeyMismatch(ref key))
                if *key == new_key_id
        ),
        "unexpected error: {err}"
    );
    assert_eq!(registry.get("rotating").unwrap().key_id, old_key_id);

    // The real rotation keeps the old key as a named overlap key.
    let record = registry
        .rotate(
            "rotating",
            &new_descriptor,
            Some(&new_public_key),
            overlap_seconds,
            rotated_at,
        )
        .expect("rotation with the confirmed fingerprint");
    assert_eq!(record.old_key_id, old_key_id);
    assert_eq!(record.new_key_id, new_key_id);
    assert_eq!(record.rotated_at, rotated_at);
    assert_eq!(record.overlap_until, rotated_at + overlap_seconds);
    let pinned = registry.get("rotating").unwrap();
    assert_eq!(pinned.key_id, new_key_id);
    assert_eq!(pinned.valid_until, None);
    assert_eq!(pinned.overlap_keys.len(), 1);
    assert_eq!(pinned.overlap_keys[0].key_id, old_key_id);
    assert_eq!(
        pinned.overlap_keys[0].valid_until,
        Some(rotated_at + overlap_seconds)
    );

    // Inside the overlap both keys verify...
    let during = rotated_at + 1;
    assert!(
        registry.verify("rotating", &old_descriptor, during).is_ok(),
        "the old key must verify during the overlap"
    );
    assert!(
        registry.verify("rotating", &new_descriptor, during).is_ok(),
        "the new key must verify during the overlap"
    );

    // ...and the old key fails closed afterwards, while the new one keeps verifying.
    let after = rotated_at + overlap_seconds;
    let err = registry
        .verify("rotating", &old_descriptor, after)
        .expect_err("the old key must be rejected once its overlap ends");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::KeyRetired(ref key, until))
                if *key == old_key_id && until == rotated_at + overlap_seconds
        ),
        "unexpected error: {err}"
    );
    assert!(
        registry.verify("rotating", &new_descriptor, after).is_ok(),
        "the new key stays valid after the overlap"
    );

    // The overlap window is persisted, not an in-memory courtesy.
    registry.save(&registry_path).unwrap();
    let reloaded = RemoteRegistry::load(&registry_path).unwrap();
    assert_eq!(reloaded.get("rotating").unwrap(), pinned);
    assert!(reloaded.verify("rotating", &old_descriptor, during).is_ok());
    assert!(reloaded.verify("rotating", &old_descriptor, after).is_err());
    assert!(reloaded.verify("rotating", &new_descriptor, after).is_ok());

    // A retired key cannot be resurrected by rotating back to it.
    let err = reloaded
        .clone()
        .rotate("rotating", &old_descriptor, None, 0, after + 1)
        .expect_err("a retired key must not come back");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::KeyRetired(ref key, _))
                if *key == old_key_id
        ),
        "unexpected error: {err}"
    );
}

#[test]
fn remote_registry_never_grants_write() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let registry_path = state.join("remotes.json");
    let alpha = identity_in(&dir, "alpha", 1_000);
    let descriptor = descriptor_json(&alpha, 1_100);

    let mut registry = RemoteRegistry::default();
    let record = registry
        .add("alpha", &endpoint("alpha"), &descriptor, 1_200)
        .unwrap()
        .clone();

    // A pin authenticates read-only aggregation; it never authorizes a write.
    assert!(
        record.granted_capabilities().is_empty(),
        "a pin must never grant capabilities, got {:?}",
        record.granted_capabilities()
    );
    registry.save(&registry_path).unwrap();

    // Verification is a pure read: it cannot mutate the pin or anything else.
    let before = RemoteRegistry::load(&registry_path).unwrap();
    before.verify("alpha", &descriptor, 1_300).unwrap();
    assert_eq!(RemoteRegistry::load(&registry_path).unwrap(), before);

    // Registry state is a plain client-side file, separate from the tracker database, and pinning
    // writes nothing but that file.
    assert_ne!(config::remotes_path(), config::db_path());
    assert!(config::remotes_path().starts_with(config::state_dir()));
    assert!(config::remotes_path().ends_with("remotes.json"));
    let entries: Vec<_> = std::fs::read_dir(&state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["remotes.json".to_string()]);
    assert!(!state.join("marbles.db").exists());
}

#[test]
fn remote_list_and_show_expose_pinned_identity() {
    let dir = tempfile::tempdir().unwrap();
    let alpha = identity_in(&dir, "alpha", 1_000);
    let beta = identity_in(&dir, "beta", 1_000);

    let mut registry = RemoteRegistry::default();
    registry
        .add(
            "alpha",
            &endpoint("alpha"),
            &descriptor_json(&alpha, 1_100),
            1_200,
        )
        .unwrap();
    registry
        .add(
            "beta",
            &endpoint("beta"),
            &descriptor_json(&beta, 1_100),
            1_200,
        )
        .unwrap();

    // What `mb remote list --json` prints: every entry carries the pinned identity, so a reader
    // can never mistake the endpoint for the trust anchor.
    let listed = serde_json::to_value(registry.list()).unwrap();
    let rows = listed.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for (row, store) in rows.iter().zip([&alpha, &beta]) {
        assert_eq!(row["store_id"], serde_json::json!(store.store_id));
        assert_eq!(row["key_id"], serde_json::json!(store.active_key_id));
        assert_eq!(
            row["public_key"],
            serde_json::json!(store.active_key().unwrap().public_key)
        );
        assert!(row["endpoint"].is_string());
        assert!(row.get("private_key").is_none());
    }

    // `mb remote show <name> --json` exposes the same identity plus provenance timestamps.
    let shown = serde_json::to_value(registry.get("alpha").unwrap()).unwrap();
    assert_eq!(shown["name"], serde_json::json!("alpha"));
    assert_eq!(shown["store_id"], serde_json::json!(alpha.store_id));
    assert_eq!(shown["key_id"], serde_json::json!(alpha.active_key_id));
    assert_eq!(shown["added_at"], serde_json::json!(1_200));
    assert_eq!(shown["updated_at"], serde_json::json!(1_200));

    // An unknown name is a named error, not an empty identity.
    assert!(matches!(
        registry.verify("nope", &descriptor_json(&alpha, 1_100), 1_200),
        Err(RemoteRegistryError::NotFound(name)) if name == "nope"
    ));
}

#[test]
fn missing_descriptor_fails_closed_and_persists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let registry_path = dir.path().join("state/remotes.json");
    let missing = dir.path().join("absent-descriptor.json");

    // Reading a descriptor that is not there is an error, never an empty descriptor.
    let err = read_descriptor_source(missing.to_str().unwrap())
        .expect_err("a missing descriptor file must fail closed");
    assert!(
        matches!(err, RemoteRegistryError::DescriptorRead { ref source, .. }
            if source.kind() == std::io::ErrorKind::NotFound),
        "unexpected error: {err}"
    );

    // A descriptor that carries no signature, or the wrong schema, is not pinnable either.
    let alpha = identity_in(&dir, "alpha", 1_000);
    let descriptor = descriptor_json(&alpha, 1_100);
    let mut unsigned: serde_json::Value = serde_json::from_str(&descriptor).unwrap();
    unsigned.as_object_mut().unwrap().remove("signature");
    let mut registry = RemoteRegistry::default();
    let err = registry
        .add("alpha", &endpoint("alpha"), &unsigned.to_string(), 1_200)
        .expect_err("an unsigned descriptor must not be pinned");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::UnsignedDescriptor)
        ),
        "unexpected error: {err}"
    );
    let mut bad_schema: serde_json::Value = serde_json::from_str(&descriptor).unwrap();
    bad_schema["schema"] = serde_json::json!("marbles.store/0");
    let err = registry
        .add("alpha", &endpoint("alpha"), &bad_schema.to_string(), 1_200)
        .expect_err("an unknown schema must not be pinned");
    assert!(
        matches!(
            err,
            RemoteRegistryError::Identity(IdentityError::WrongSchema { .. })
        ),
        "unexpected error: {err}"
    );

    // Nothing was pinned and nothing was written: the failure cannot be mistaken for success.
    assert!(registry.list().is_empty());
    assert!(!registry_path.exists());
}

//! The verifier is the trust boundary: a remote reader holds only a pinned public key, a store
//! id, and the last sequence it accepted. These tests exercise exactly that surface.

use marbles::db::Db;
use marbles::db::snapshot::{
    Ed25519Signer, SignedSnapshot, SnapshotError, SnapshotMeta, SnapshotVerifier, VerifierConfig,
    canonical_json, capture, sign_snapshot,
};
use marbles::types::{Evidence, Folder, IssuePatch, NewIssue};
use sha2::{Digest, Sha256};

const STORE: &str = "store-a";
const COMPANY: &str = "acme";
const KEY_ID: &str = "key-1";

fn fixture() -> Db {
    let db = Db::in_memory().unwrap();
    db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    db.ensure_project("other", "/tmp/other", "other").unwrap();
    db.ensure_folder(
        &Folder {
            slug: "work".into(),
            name: "Work".into(),
            parent: None,
            created_at: 10,
        },
        10,
    )
    .unwrap();
    db.set_project_folder("demo", Some("work"), 11).unwrap();

    let ship = db
        .create(
            &NewIssue {
                title: "ship the signed snapshot".into(),
                description: "signed and replay-guarded".into(),
                issue_type: "task".into(),
                priority: 1,
                labels: vec!["federation".into()],
                parent: None,
                id: Some("demo-ship".into()),
                project: Some("demo".into()),
                available_at: Some(0),
                created_by: Some("tester".into()),
                metadata: Some(serde_json::json!({"cost_usd": 1.5, "session": "s-1"})),
                external_ref: Some("gh-1".into()),
            },
            100,
        )
        .unwrap();
    db.update(
        &ship,
        &IssuePatch {
            status: Some("review".into()),
            evidence: Some(vec![Evidence::pr("https://example.test/pr/1")]),
            ..Default::default()
        },
        "worker",
        110,
    )
    .unwrap();
    db.close(
        &ship,
        None,
        &[Evidence::commit("deadbeef")],
        "reviewer",
        120,
    )
    .unwrap();

    let blocked = db
        .create(
            &NewIssue {
                title: "wait for the snapshot".into(),
                description: String::new(),
                issue_type: "task".into(),
                priority: 2,
                labels: vec![],
                parent: Some(ship.clone()),
                id: Some("demo-blocked".into()),
                project: Some("demo".into()),
                available_at: Some(0),
                created_by: Some("tester".into()),
                metadata: None,
                external_ref: None,
            },
            130,
        )
        .unwrap();
    db.add_dep(&blocked, &ship, "tester", 140).unwrap();
    db
}

fn signer() -> Ed25519Signer {
    Ed25519Signer::generate(KEY_ID).unwrap()
}

fn verifier_for(signer: &Ed25519Signer) -> SnapshotVerifier {
    SnapshotVerifier::new(VerifierConfig::new(
        STORE,
        Some(COMPANY.to_string()),
        KEY_ID,
        signer.public_key().unwrap(),
    ))
}

fn sign_first(db: &Db, signer: &Ed25519Signer) -> SignedSnapshot {
    sign_snapshot(
        capture(db).unwrap(),
        SnapshotMeta::first(STORE, Some(COMPANY.to_string()), 1_000),
        signer,
    )
    .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn body_hash(snapshot: &SignedSnapshot) -> String {
    hex(&Sha256::digest(canonical_json(&snapshot.body).unwrap()))
}

#[test]
fn snapshot_verifies_offline_with_pinned_key() {
    let db = fixture();
    let signer = signer();
    let signed = sign_first(&db, &signer);

    // Verification uses only the wire bytes and the pinned public key: no database, no network.
    let wire = serde_json::to_vec(&signed).unwrap();
    let received: SignedSnapshot = serde_json::from_slice(&wire).unwrap();
    let mut verifier = verifier_for(&signer);
    let verified = verifier.verify(&received).unwrap();

    assert_eq!(verified.manifest.store_id, STORE);
    assert_eq!(verified.manifest.company.as_deref(), Some(COMPANY));
    assert_eq!(verified.manifest.seq, 1);
    assert_eq!(verified.body.projects.len(), 2);
    assert_eq!(verified.body.folders.len(), 1);
    assert_eq!(verified.body.project_folders.len(), 1);
    assert_eq!(verified.body.issues.len(), 2);
    assert_eq!(verified.body.dependencies.len(), 1);
    assert_eq!(verified.body.dependencies[0].issue_id, "demo-blocked");
    assert_eq!(verified.body.dependencies[0].depends_on, "demo-ship");
    assert_eq!(verified.body.summary.total, 2);
    assert_eq!(verified.body.summary.by_status.get("closed"), Some(&1));
    assert_eq!(verified.body.summary.by_status.get("open"), Some(&1));
    assert_eq!(verifier.last_seq(), 1);
    assert_eq!(
        verifier.last_body_sha256(),
        Some(body_hash(&signed).as_str())
    );
}

#[test]
fn tampered_snapshot_is_rejected() {
    let db = fixture();
    let signer = signer();
    let mut signed = sign_first(&db, &signer);
    signed.body.issues[0].title = "rewritten after signing".into();

    let mut verifier = verifier_for(&signer);
    let error = verifier.verify(&signed).unwrap_err();
    assert!(
        matches!(error, SnapshotError::BodyHashMismatch { .. }),
        "expected body hash mismatch, got {error:?}"
    );
    assert_eq!(verifier.last_seq(), 0, "rejected snapshot must not advance");
}

#[test]
fn wrong_key_snapshot_is_rejected() {
    let db = fixture();
    let trusted = signer();
    let impostor = signer();
    let signed = sign_first(&db, &impostor);

    // Same key_id, different key material: the rejection must be cryptographic, not a label check.
    let mut verifier = verifier_for(&trusted);
    let error = verifier.verify(&signed).unwrap_err();
    assert!(
        matches!(error, SnapshotError::BadSignature),
        "expected invalid signature, got {error:?}"
    );
    assert_eq!(verifier.last_seq(), 0);
}

#[test]
fn wrong_store_snapshot_is_rejected() {
    let db = fixture();
    let signer = signer();
    let signed = sign_snapshot(
        capture(&db).unwrap(),
        SnapshotMeta::first("store-b", Some(COMPANY.to_string()), 1_000),
        &signer,
    )
    .unwrap();

    let mut verifier = verifier_for(&signer);
    let error = verifier.verify(&signed).unwrap_err();
    assert!(
        matches!(error, SnapshotError::WrongStore { .. }),
        "expected wrong store, got {error:?}"
    );
    assert_eq!(verifier.last_seq(), 0);
}

#[test]
fn replayed_snapshot_is_rejected() {
    let db = fixture();
    let signer = signer();
    let first = sign_first(&db, &signer);

    let mut verifier = verifier_for(&signer);
    verifier.verify(&first).unwrap();

    // Same snapshot again: the same (store_id, seq, body_sha256) is never accepted twice.
    let error = verifier.verify(&first).unwrap_err();
    assert!(
        matches!(
            error,
            SnapshotError::Replayed {
                seq: 1,
                last_seq: 1
            }
        ),
        "expected replay rejection, got {error:?}"
    );

    let second = sign_snapshot(
        capture(&db).unwrap(),
        SnapshotMeta::after(&first.manifest, 2_000),
        &signer,
    )
    .unwrap();
    verifier.verify(&second).unwrap();
    assert_eq!(verifier.last_seq(), 2);

    // An older but internally valid snapshot cannot rewind the accepted sequence.
    let error = verifier.verify(&first).unwrap_err();
    assert!(
        matches!(
            error,
            SnapshotError::Replayed {
                seq: 1,
                last_seq: 2
            }
        ),
        "expected older seq rejection, got {error:?}"
    );
}

#[test]
fn valid_signature_over_different_issue_id_is_rejected() {
    let db = fixture();
    let signer = signer();
    let mut signed = sign_first(&db, &signer);
    let original = signed.body.issues[0].id.clone();
    assert_ne!(original, "demo-forged");

    // The signature was made over a body containing `original`. Swapping the id invalidates the
    // body hash even though every manifest field and all signature bytes are untouched.
    signed.body.issues[0].id = "demo-forged".into();
    let mut verifier = verifier_for(&signer);
    let error = verifier.verify(&signed).unwrap_err();
    assert!(
        matches!(error, SnapshotError::BodyHashMismatch { .. }),
        "expected body hash mismatch, got {error:?}"
    );

    // Even if the attacker recomputes the body hash to match the forged body, the manifest is no
    // longer the one that was signed, so the signature itself must fail.
    signed.manifest.body_sha256 = body_hash(&signed);
    let mut verifier = verifier_for(&signer);
    let error = verifier.verify(&signed).unwrap_err();
    assert!(
        matches!(error, SnapshotError::BadSignature),
        "expected invalid signature after hash repair, got {error:?}"
    );
}

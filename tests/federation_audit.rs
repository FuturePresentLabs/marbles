//! Acceptance tests for the federation audit receipt and rejection matrix (F2.3).
//!
//! Every remote reference carries a six-field receipt (`store_id`, remote issue id, assertion
//! state, `asserted_at`, signature, verifying `key_id`). Verification is offline against the
//! pinned key and yields a *named* rejection state for each way the evidence can lie: wrong key,
//! wrong store, stale, a valid signature over a different issue id, a forged/malformed signature,
//! or an absent/unpinned store. Nothing here touches the network and no failure ever reads as
//! fresh or ready.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use marbles::assertion_cache::{IssueAssertion, sign_issue_assertion};
use marbles::db::snapshot::{Ed25519Signer, SnapshotSigner};
use marbles::db::{Db, now};
use marbles::identity::{DESCRIPTOR_SCHEMA, RemoteRecord, RemoteRegistry, StoreDescriptor};
use marbles::refs::{AuditReceipt, CanonicalRef, ReceiptState, audit_receipt};
use marbles::types::NewIssue;

const STORE: &str = "store_ab12";
const REF: &str = "marbles://store_ab12/acme/widgets/issue-1";
const REMOTE_ISSUE: &str = "issue-1";
/// The cache default is 5 minutes; observations older than this are stale.
const STALE_AGE: i64 = 600;

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        fs::create_dir_all(home.join("assertions")).unwrap();
        fs::create_dir_all(repo.join(".marbles")).unwrap();
        fs::write(
            repo.join(".marbles/project.toml"),
            "slug = \"local\"\nprefix = \"local\"\n",
        )
        .unwrap();
        Self {
            _dir: dir,
            home,
            repo,
        }
    }

    fn registry_path(&self) -> PathBuf {
        self.home.join("remotes.json")
    }

    fn db_path(&self) -> PathBuf {
        self.home.join("marbles.db")
    }

    fn db(&self) -> Db {
        let db = Db::open(self.db_path()).unwrap();
        db.ensure_project("local", self.repo.to_str().unwrap(), "local")
            .unwrap();
        db
    }

    fn child(&self, at: i64) -> String {
        self.db()
            .create(
                &NewIssue {
                    title: "child".into(),
                    project: Some("local".into()),
                    available_at: Some(0),
                    ..Default::default()
                },
                at - 3_600,
            )
            .unwrap()
    }

    fn mb(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_marbles"))
            .args(["-C", self.repo.to_str().unwrap()])
            .args(args)
            .env("MARBLES_HOME", &self.home)
            .env_remove("MARBLES_URL")
            .env_remove("MARBLES_TOKEN")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.mb(args);
        assert!(
            output.status.success(),
            "mb {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn pin(&self, at: i64) -> (RemoteRecord, Ed25519Signer) {
        let signer = Ed25519Signer::generate(format!("{STORE}/key-1")).unwrap();
        let mut descriptor = StoreDescriptor {
            schema: DESCRIPTOR_SCHEMA.to_string(),
            store_id: STORE.to_string(),
            display_name: "alpha".to_string(),
            endpoint: Some("https://alpha.example.invalid".to_string()),
            key_id: signer.key_id().to_string(),
            public_key: URL_SAFE_NO_PAD.encode(signer.public_key().unwrap()),
            capabilities: vec!["issues".to_string()],
            issued_at: at,
            signature: None,
        };
        let signature = signer.sign(&descriptor.signing_bytes().unwrap()).unwrap();
        descriptor.signature = Some(URL_SAFE_NO_PAD.encode(signature));
        let path = self.registry_path();
        let mut registry = RemoteRegistry::load(&path).unwrap();
        registry
            .add(
                "alpha",
                "https://alpha.example.invalid",
                &serde_json::to_string(&descriptor).unwrap(),
                at,
            )
            .unwrap();
        registry.save(&path).unwrap();
        (registry.get("alpha").unwrap().clone(), signer)
    }

    fn registry(&self) -> RemoteRegistry {
        RemoteRegistry::load(&self.registry_path()).unwrap()
    }

    fn receipt(
        &self,
        signer: &Ed25519Signer,
        store_id: &str,
        remote_issue_id: &str,
        state: &str,
        at: i64,
    ) -> AuditReceipt {
        let assertion = sign_issue_assertion(
            IssueAssertion::new(store_id, remote_issue_id, state, at),
            signer,
        )
        .unwrap();
        AuditReceipt::new(
            assertion.store_id,
            assertion.remote_issue_id,
            assertion.state,
            assertion.asserted_at,
            assertion.signature.unwrap(),
            assertion.key_id,
        )
    }

    fn observe(&self, record: &RemoteRecord, signer: &Ed25519Signer, state: &str, at: i64) {
        let assertion = sign_issue_assertion(
            IssueAssertion::new(&record.store_id, REMOTE_ISSUE, state, at),
            signer,
        )
        .unwrap();
        marbles::assertion_cache::AssertionCache::at(self.home.join("assertions"))
            .record(record, &assertion, at)
            .unwrap();
    }
}

fn reference() -> CanonicalRef {
    CanonicalRef::parse(REF).unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn receipt_contains_all_six_fields() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "open", at);
    fx.ok(&["dep", "add", &child, REF]);

    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&stdout(&fx.ok(&["show", &child, "--json"]))).unwrap();
    let receipt = &rows[0]["remote_audit"][0]["receipt"];
    let keys: BTreeSet<&str> = receipt
        .as_object()
        .expect("verified audit carries a receipt object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "store_id",
            "remote_issue_id",
            "state",
            "asserted_at",
            "signature",
            "key_id",
        ]),
        "a receipt must carry exactly the six required fields, never more, never fewer"
    );
    assert_eq!(receipt["store_id"], serde_json::json!(STORE));
    assert_eq!(receipt["remote_issue_id"], serde_json::json!(REMOTE_ISSUE));
    assert_eq!(receipt["state"], serde_json::json!("open"));
    assert_eq!(receipt["asserted_at"], serde_json::json!(at));
    assert_eq!(receipt["key_id"], serde_json::json!(signer.key_id()));
    assert!(
        !receipt["signature"].as_str().unwrap().is_empty(),
        "signature must be present"
    );
    // Round-tripping proves none of the fields is optional on deserialize either.
    let parsed: AuditReceipt = serde_json::from_value(receipt.clone()).unwrap();
    assert_eq!(parsed.store_id, STORE);
    assert_eq!(parsed.remote_issue_id, REMOTE_ISSUE);
}

#[test]
fn receipt_verifies_offline_with_no_network() {
    let fx = Fixture::new();
    let at = now();
    let (_record, signer) = fx.pin(at);
    let receipt = fx.receipt(&signer, STORE, REMOTE_ISSUE, "open", at);

    // Only the local registry (pinned public key) is consulted: no fetch, no cache, no network.
    let audit = audit_receipt(
        &reference(),
        Some(&receipt),
        &fx.registry(),
        Some(at),
        at,
        300,
    );
    assert_eq!(audit.state, ReceiptState::Verified, "{audit:?}");
    assert!(audit.is_verified());
    assert_eq!(audit.receipt.as_ref(), Some(&receipt));
    assert_eq!(
        receipt.signing_bytes().unwrap(),
        receipt.to_assertion().signing_bytes().unwrap()
    );
}

#[test]
fn wrong_key_receipt_is_rejected() {
    let fx = Fixture::new();
    let at = now();
    let (_record, signer) = fx.pin(at);
    let mut receipt = fx.receipt(&signer, STORE, REMOTE_ISSUE, "open", at);
    // A key id that is not pinned for this store cannot verify anything.
    receipt.key_id = format!("{STORE}/key-9");

    let audit = audit_receipt(
        &reference(),
        Some(&receipt),
        &fx.registry(),
        Some(at),
        at,
        300,
    );
    assert_eq!(audit.state, ReceiptState::WrongKey, "{audit:?}");
    assert!(!audit.is_verified());
}

#[test]
fn wrong_store_receipt_is_rejected() {
    let fx = Fixture::new();
    let at = now();
    let (_record, signer) = fx.pin(at);
    // Validly signed, but it is a claim about another store, not the pinned one for this ref.
    let receipt = fx.receipt(&signer, "store_zzzz", REMOTE_ISSUE, "open", at);

    let audit = audit_receipt(
        &reference(),
        Some(&receipt),
        &fx.registry(),
        Some(at),
        at,
        300,
    );
    assert_eq!(audit.state, ReceiptState::WrongStore, "{audit:?}");
    assert!(!audit.is_verified());
}

#[test]
fn stale_beyond_window_receipt_is_rejected() {
    let fx = Fixture::new();
    let at = now();
    let (_record, signer) = fx.pin(at);
    let receipt = fx.receipt(&signer, STORE, REMOTE_ISSUE, "open", at - STALE_AGE);

    let audit = audit_receipt(
        &reference(),
        Some(&receipt),
        &fx.registry(),
        Some(at - STALE_AGE),
        at,
        300,
    );
    // The signature is still valid; only the observation age rejects it. It must say `stale`, not
    // `forged`, and it must never be treated as verified.
    assert_eq!(audit.state, ReceiptState::Stale, "{audit:?}");
    assert!(!audit.is_verified());
}

#[test]
fn signature_over_different_issue_id_is_rejected() {
    let fx = Fixture::new();
    let at = now();
    let (_record, signer) = fx.pin(at);
    // A perfectly valid receipt — for a different issue.
    let receipt = fx.receipt(&signer, STORE, "issue-2", "open", at);

    let audit = audit_receipt(
        &reference(),
        Some(&receipt),
        &fx.registry(),
        Some(at),
        at,
        300,
    );
    assert_eq!(audit.state, ReceiptState::DifferentIssue, "{audit:?}");
    assert!(!audit.is_verified());
}

#[test]
fn forged_or_malformed_signature_is_rejected() {
    let fx = Fixture::new();
    let at = now();
    let (_record, signer) = fx.pin(at);
    let good = fx.receipt(&signer, STORE, REMOTE_ISSUE, "open", at);

    for forged in ["AAAA", "not-base64!!", ""] {
        let mut receipt = good.clone();
        receipt.signature = forged.to_string();
        let audit = audit_receipt(
            &reference(),
            Some(&receipt),
            &fx.registry(),
            Some(at),
            at,
            300,
        );
        assert_eq!(
            audit.state,
            ReceiptState::ForgedSignature,
            "forged {forged:?} must be named, got {audit:?}"
        );
        assert!(!audit.is_verified());
    }
}

#[test]
fn absent_store_degrades_to_named_state() {
    let fx = Fixture::new();
    let at = now();
    let (_record, signer) = fx.pin(at);
    let receipt = fx.receipt(&signer, STORE, REMOTE_ISSUE, "open", at);
    let mut registry = fx.registry();
    registry.remove("alpha").unwrap();
    registry.save(&fx.registry_path()).unwrap();

    let audit = audit_receipt(
        &reference(),
        Some(&receipt),
        &fx.registry(),
        Some(at),
        at,
        300,
    );
    assert_eq!(audit.state, ReceiptState::AbsentStore, "{audit:?}");
    assert!(!audit.is_verified());
    assert!(
        audit.detail.as_deref().unwrap_or("").contains("not pinned"),
        "the state must explain itself: {audit:?}"
    );
}

#[test]
fn federation_failure_never_reports_ready() {
    let fx = Fixture::new();
    let at = now();
    let (record, signer) = fx.pin(at);
    let good = fx.receipt(&signer, STORE, REMOTE_ISSUE, "open", at);

    let mut wrong_key = good.clone();
    wrong_key.key_id = format!("{STORE}/key-9");
    let wrong_store = fx.receipt(&signer, "store_zzzz", REMOTE_ISSUE, "open", at);
    let different_issue = fx.receipt(&signer, STORE, "issue-2", "open", at);
    let mut forged = good.clone();
    forged.signature = "AAAA".into();
    let stale = fx.receipt(&signer, STORE, REMOTE_ISSUE, "open", at - STALE_AGE);
    let empty = RemoteRegistry::default();

    let outcomes = [
        audit_receipt(
            &reference(),
            Some(&wrong_key),
            &fx.registry(),
            Some(at),
            at,
            300,
        ),
        audit_receipt(
            &reference(),
            Some(&wrong_store),
            &fx.registry(),
            Some(at),
            at,
            300,
        ),
        audit_receipt(
            &reference(),
            Some(&stale),
            &fx.registry(),
            Some(at - STALE_AGE),
            at,
            300,
        ),
        audit_receipt(
            &reference(),
            Some(&different_issue),
            &fx.registry(),
            Some(at),
            at,
            300,
        ),
        audit_receipt(
            &reference(),
            Some(&forged),
            &fx.registry(),
            Some(at),
            at,
            300,
        ),
        audit_receipt(&reference(), Some(&good), &empty, Some(at), at, 300),
    ];
    for audit in &outcomes {
        assert!(!audit.is_verified(), "failure read as verified: {audit:?}");
        assert_ne!(audit.state.as_str(), "fresh");
        assert_ne!(audit.state.as_str(), "ready");
        assert_ne!(audit.state.as_str(), "verified");
        // Every failure is a *named* state; there is no empty/unknown fallback.
        assert!(!audit.state.as_str().is_empty(), "{audit:?}");
    }

    // And the CLI path shows the same named state rather than "ready".
    let child = fx.child(at);
    fx.observe(&record, &signer, "open", at);
    fx.ok(&["dep", "add", &child, REF]);
    let mut registry = fx.registry();
    registry.remove("alpha").unwrap();
    registry.save(&fx.registry_path()).unwrap();

    let text = stdout(&fx.ok(&["show", &child]));
    assert!(text.contains("absent_store"), "{text}");
    assert!(
        !text.contains(" ready"),
        "a federation failure read as ready: {text}"
    );
}

//! Acceptance tests for the assertion cache: a remote issue's state is only usable when the
//! signature verifies against the pinned key *and* the observation is inside a named freshness
//! window. Every other outcome — never observed, unreachable, retired key, tampered entry, wrong
//! store — has its own name and must never read as `closed`/`ready`. Tests run in temp dirs and
//! touch neither `$MARBLES_HOME` nor the network.

use std::fs;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use marbles::assertion_cache::{
    AssertionCache, AssertionFreshness, CachedAssertion, IssueAssertion, MAX_CLOCK_SKEW_SECONDS,
    MissingReason, sign_issue_assertion,
};
use marbles::db::snapshot::{Ed25519Signer, SnapshotSigner};
use marbles::identity::{DESCRIPTOR_SCHEMA, RemoteRegistry, StoreDescriptor};

const STORE: &str = "store_a";
const REMOTE: &str = "alpha";

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A signer the registry can actually pin: descriptors are self-signed, so any Ed25519 key works.
fn make_signer(key_id: &str) -> Ed25519Signer {
    Ed25519Signer::generate(key_id).expect("ed25519 signer")
}

fn public_key(signer: &Ed25519Signer) -> String {
    b64(&signer.public_key().expect("public key"))
}

/// Pin `signer` as the identity of `store_id` in a fresh registry.
fn registry_with(
    dir: &tempfile::TempDir,
    remote: &str,
    store_id: &str,
    signer: &Ed25519Signer,
    at: i64,
) -> RemoteRegistry {
    let mut descriptor = StoreDescriptor {
        schema: DESCRIPTOR_SCHEMA.to_string(),
        store_id: store_id.to_string(),
        display_name: remote.to_string(),
        endpoint: None,
        key_id: signer.key_id().to_string(),
        public_key: public_key(signer),
        capabilities: vec!["issues".to_string()],
        issued_at: at,
        signature: None,
    };
    let signature = signer
        .sign(&descriptor.signing_bytes().expect("signing bytes"))
        .expect("descriptor signature");
    descriptor.signature = Some(b64(&signature));
    let descriptor_json = serde_json::to_string(&descriptor).expect("descriptor json");

    let path = dir.path().join("remotes.json");
    let mut registry = RemoteRegistry::load(&path).expect("empty registry");
    registry
        .add(
            remote,
            "https://alpha.example.invalid",
            &descriptor_json,
            at,
        )
        .expect("pin verified descriptor");
    registry
}

fn assertion(
    store_id: &str,
    issue: &str,
    state: &str,
    asserted_at: i64,
    signer: &Ed25519Signer,
) -> IssueAssertion {
    sign_issue_assertion(
        IssueAssertion::new(store_id, issue, state, asserted_at),
        signer,
    )
    .expect("signed assertion")
}

fn cache(dir: &tempfile::TempDir) -> AssertionCache {
    AssertionCache::at(dir.path().join("state/assertions"))
}

/// Edit one reference's stored entry, the way a corrupted disk or a hostile writer would.
fn tamper(
    cache: &AssertionCache,
    store_id: &str,
    issue: &str,
    edit: impl FnOnce(&mut serde_json::Value),
) {
    let path = cache.entry_path(store_id, issue);
    let text = fs::read_to_string(&path).expect("cached entry");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("entry json");
    edit(&mut value);
    fs::write(&path, serde_json::to_string(&value).expect("entry json")).expect("rewrite entry");
}

fn cached(view: &marbles::assertion_cache::AssertionView) -> &CachedAssertion {
    view.assertion.as_ref().expect("verified assertion")
}

#[test]
fn fresh_assertion_is_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir);
    let record = registry.get(REMOTE).unwrap();

    let signed = assertion(STORE, "issue-1", "in_progress", 1_010, &signer);
    let written = cache
        .record(record, &signed, 1_020)
        .expect("verified write");
    assert_eq!(written.freshness, AssertionFreshness::Fresh);

    let view = cache.read(&registry, STORE, "issue-1", 1_030);
    assert_eq!(view.freshness, AssertionFreshness::Fresh);
    assert_eq!(view.freshness.as_str(), "fresh");
    assert_eq!(view.asserted_state(), Some("in_progress"));
    assert_eq!(view.missing, None);
    assert_eq!(view.problem, None);
    let entry = cached(&view);
    assert_eq!(entry.store_id, STORE);
    assert_eq!(entry.remote_issue_id, "issue-1");
    assert_eq!(entry.state, "in_progress");
    assert_eq!(entry.asserted_at, 1_010);
    assert_eq!(entry.key_id, signer.key_id());
    assert_eq!(entry.observed_at, 1_020);
    assert_eq!(entry.signature, signed.signature.unwrap());
}

#[test]
fn expired_assertion_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir).with_freshness_window(60);
    let record = registry.get(REMOTE).unwrap();

    let signed = assertion(STORE, "issue-2", "open", 1_000, &signer);
    cache.record(record, &signed, 1_010).unwrap();

    // Inside the window: fresh.
    assert_eq!(
        cache.read(&registry, STORE, "issue-2", 1_060).freshness,
        AssertionFreshness::Fresh
    );
    // Past the window: stale, still verifiable, still labelled.
    let view = cache.read(&registry, STORE, "issue-2", 1_071);
    assert_eq!(view.freshness, AssertionFreshness::Stale);
    assert_eq!(view.freshness.as_str(), "stale");
    assert_eq!(view.asserted_state(), Some("open"));
    assert_eq!(view.problem, None);
    assert_eq!(cached(&view).observed_at, 1_010);
}

#[test]
fn missing_assertion_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir);

    let view = cache.read(&registry, STORE, "never-seen", 1_000);
    assert_eq!(view.freshness, AssertionFreshness::Unknown);
    assert_eq!(view.asserted_state(), None);
    assert_eq!(view.assertion, None);
    assert_eq!(view.missing, Some(MissingReason::NeverObserved));
    assert_eq!(view.observed_at(), None);
}

#[test]
fn unreachable_remote_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir);

    cache
        .note_unreachable(STORE, "issue-3", "connection refused", 1_000)
        .unwrap();
    let view = cache.read(&registry, STORE, "issue-3", 1_000);
    assert_eq!(view.freshness, AssertionFreshness::Unknown);
    assert_eq!(view.asserted_state(), None);
    assert_eq!(view.assertion, None);
    assert_eq!(view.missing, Some(MissingReason::RemoteUnreachable));
    assert!(
        view.problem
            .as_deref()
            .unwrap_or("")
            .contains("unreachable"),
        "unexpected problem: {:?}",
        view.problem
    );

    // A failed re-observation never erases a verified assertion: the last known state stays and
    // ages into `stale` instead of silently disappearing.
    let signed = assertion(STORE, "issue-4", "blocked", 1_000, &signer);
    cache
        .record(registry.get(REMOTE).unwrap(), &signed, 1_000)
        .unwrap();
    cache
        .note_unreachable(STORE, "issue-4", "timeout", 1_010)
        .unwrap();
    let kept = cache.read(&registry, STORE, "issue-4", 1_010);
    assert_eq!(kept.freshness, AssertionFreshness::Fresh);
    assert_eq!(kept.asserted_state(), Some("blocked"));
}

#[test]
fn bad_signature_is_unverifiable() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir);

    let signed = assertion(STORE, "issue-5", "closed", 1_000, &signer);
    cache
        .record(registry.get(REMOTE).unwrap(), &signed, 1_000)
        .unwrap();
    tamper(&cache, STORE, "issue-5", |value| {
        let signature = value["assertion"]["signature"]
            .as_str()
            .expect("signature")
            .to_string();
        let first = signature.chars().next().unwrap();
        let flipped = if first == 'A' { 'B' } else { 'A' };
        value["assertion"]["signature"] =
            serde_json::json!(format!("{flipped}{}", &signature[1..]));
    });

    let view = cache.read(&registry, STORE, "issue-5", 1_000);
    assert_eq!(view.freshness, AssertionFreshness::Unverifiable);
    assert_eq!(view.asserted_state(), None);
    assert_eq!(view.assertion, None);
    assert!(
        view.problem.as_deref().unwrap_or("").contains("signature"),
        "unexpected problem: {:?}",
        view.problem
    );

    // Dropping the signature entirely is unverifiable too, never fresh.
    tamper(&cache, STORE, "issue-5", |value| {
        value["assertion"]["signature"] = serde_json::json!("not base64url!!!");
    });
    assert_eq!(
        cache.read(&registry, STORE, "issue-5", 1_000).freshness,
        AssertionFreshness::Unverifiable
    );
}

#[test]
fn wrong_key_or_store_is_unverifiable() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let mut registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir);

    let signed = assertion(STORE, "issue-6", "open", 1_000, &signer);
    cache
        .record(registry.get(REMOTE).unwrap(), &signed, 1_000)
        .unwrap();
    assert_eq!(
        cache.read(&registry, STORE, "issue-6", 1_000).freshness,
        AssertionFreshness::Fresh
    );

    // Operator re-pins the store to a different key: the old signature is no longer trusted.
    let rotated = make_signer("store_a/key-2");
    registry.remotes[0].key_id = rotated.key_id().to_string();
    registry.remotes[0].public_key = public_key(&rotated);
    let view = cache.read(&registry, STORE, "issue-6", 1_000);
    assert_eq!(view.freshness, AssertionFreshness::Unverifiable);
    assert_eq!(view.asserted_state(), None);
    assert!(
        view.problem
            .as_deref()
            .unwrap_or("")
            .contains("not pinned for store"),
        "unexpected problem: {:?}",
        view.problem
    );

    // Restore the pin, then misfile the entry under another store id: a cross-store read cannot
    // inherit the assertion.
    let mut registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    registry.remotes[0].key_id = signer.key_id().to_string();
    registry.remotes[0].public_key = public_key(&signer);
    tamper(&cache, STORE, "issue-6", |value| {
        value["assertion"]["store_id"] = serde_json::json!("store_b");
    });
    let view = cache.read(&registry, STORE, "issue-6", 1_000);
    assert_eq!(view.freshness, AssertionFreshness::Unverifiable);
    assert_eq!(view.asserted_state(), None);
    assert!(
        view.problem
            .as_deref()
            .unwrap_or("")
            .contains("does not verify"),
        "unexpected problem: {:?}",
        view.problem
    );

    // A record with no pin at all is unverifiable, not a licence to trust the cache.
    let empty = RemoteRegistry::default();
    assert_eq!(
        cache.read(&empty, STORE, "issue-6", 1_000).freshness,
        AssertionFreshness::Unverifiable
    );
}

#[test]
fn one_bad_reference_does_not_discard_others() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir);
    let record = registry.get(REMOTE).unwrap();

    for (issue, state) in [
        ("issue-a", "open"),
        ("issue-b", "blocked"),
        ("issue-c", "closed"),
    ] {
        cache
            .record(
                record,
                &assertion(STORE, issue, state, 1_000, &signer),
                1_000,
            )
            .unwrap();
    }
    tamper(&cache, STORE, "issue-b", |value| {
        value["assertion"]["state"] = serde_json::json!("concatenated");
        value["assertion"]["signature"] = serde_json::json!("AAAA");
    });

    let views = cache.read_all(&registry, STORE, 1_000);
    assert_eq!(views.len(), 3, "one bad reference must not discard others");
    let by_issue = |issue: &str| {
        views
            .iter()
            .find(|view| view.remote_issue_id == issue)
            .unwrap_or_else(|| panic!("missing view for {issue}"))
    };
    assert_eq!(by_issue("issue-a").freshness, AssertionFreshness::Fresh);
    assert_eq!(by_issue("issue-a").asserted_state(), Some("open"));
    assert_eq!(
        by_issue("issue-b").freshness,
        AssertionFreshness::Unverifiable
    );
    assert_eq!(by_issue("issue-b").asserted_state(), None);
    assert_eq!(by_issue("issue-c").freshness, AssertionFreshness::Fresh);
    assert_eq!(by_issue("issue-c").asserted_state(), Some("closed"));
}

#[test]
fn cache_never_reports_absent_remote_as_closed_or_ready() {
    let dir = tempfile::tempdir().unwrap();
    let signer = make_signer("store_a/key-1");
    let registry = registry_with(&dir, REMOTE, STORE, &signer, 1_000);
    let cache = cache(&dir);

    // Never observed and unreachable both yield `unknown`, and `unknown` exposes no state at all.
    let never = cache.read(&registry, STORE, "issue-unknown", 1_000);
    cache
        .note_unreachable(STORE, "issue-unreachable", "no route to host", 1_000)
        .unwrap();
    let unreachable = cache.read(&registry, STORE, "issue-unreachable", 1_000);

    for view in [&never, &unreachable] {
        assert_eq!(view.freshness, AssertionFreshness::Unknown);
        assert_eq!(view.assertion, None);
        assert_eq!(view.asserted_state(), None);
        assert_ne!(view.asserted_state(), Some("closed"));
        assert_ne!(view.freshness.as_str(), "fresh");
        assert_ne!(view.freshness.as_str(), "stale");
    }

    // A future-dated assertion is refused rather than trusted as current.
    let future = assertion(STORE, "issue-future", "closed", 10_000, &signer);
    cache
        .record(registry.get(REMOTE).unwrap(), &future, 1_000)
        .unwrap_err();
    assert_eq!(
        cache
            .read(&registry, STORE, "issue-future", 1_000)
            .freshness,
        AssertionFreshness::Unknown
    );

    // An assertion carrying a state the vocabulary cannot interpret is refused outright.
    let unknown_state = assertion(STORE, "issue-weird", "ready", 1_000, &signer);
    assert!(
        cache
            .record(registry.get(REMOTE).unwrap(), &unknown_state, 1_000)
            .is_err()
    );
    assert_eq!(
        cache.read(&registry, STORE, "issue-weird", 1_000).freshness,
        AssertionFreshness::Unknown
    );

    // Sanity: the skew tolerance is the only future grace, and it is small.
    assert!(MAX_CLOCK_SKEW_SECONDS > 0 && MAX_CLOCK_SKEW_SECONDS <= 600);
}

//! Acceptance tests for strict remote readiness gating (F2.2).
//!
//! A local issue is ready only when every blocker is satisfied. A remote blocker is satisfied
//! only by a **fresh, signature-verified** assertion that says the remote issue is `closed`; every
//! other state blocks and names the reason. Re-verification is offline (pinned keys + assertion
//! cache) and remote refs never become `mb ready` rows. Tests use a temp `$MARBLES_HOME` and no
//! network.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use marbles::assertion_cache::{AssertionCache, IssueAssertion, sign_issue_assertion};
use marbles::db::snapshot::{Ed25519Signer, SnapshotSigner};
use marbles::db::{Db, now};
use marbles::identity::{DESCRIPTOR_SCHEMA, RemoteRecord, RemoteRegistry, StoreDescriptor};
use marbles::types::NewIssue;

const STORE: &str = "store_ab12";
const REF: &str = "marbles://store_ab12/acme/widgets/issue-1";
const REMOTE_ISSUE: &str = "issue-1";
/// Older than the 5-minute default freshness window.
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

    fn cache(&self) -> AssertionCache {
        AssertionCache::at(self.home.join("assertions"))
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

    fn issue(&self, title: &str, created: i64) -> String {
        self.db()
            .create(
                &NewIssue {
                    title: title.into(),
                    project: Some("local".into()),
                    available_at: Some(0),
                    ..Default::default()
                },
                created,
            )
            .unwrap()
    }

    fn child(&self, at: i64) -> String {
        self.issue("child", at - 3_600)
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

    /// Cache a verified assertion for the remote issue at `observed_at`.
    fn observe(&self, record: &RemoteRecord, signer: &Ed25519Signer, state: &str, at: i64) {
        let assertion = sign_issue_assertion(
            IssueAssertion::new(&record.store_id, REMOTE_ISSUE, state, at),
            signer,
        )
        .unwrap();
        self.cache().record(record, &assertion, at).unwrap();
    }

    fn cache_path(&self) -> PathBuf {
        self.cache().entry_path(STORE, REMOTE_ISSUE)
    }

    fn drop_cache_entry(&self) {
        let path = self.cache_path();
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }

    fn tamper_signature(&self) {
        let path = self.cache_path();
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["assertion"]["signature"] = serde_json::json!("AAAA");
        fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
    }

    fn edges(&self, child: &str) -> Vec<marbles::types::RemoteDependency> {
        Db::open(self.db_path())
            .unwrap()
            .remote_dependencies(child)
            .unwrap()
    }

    /// `mb ready --json` rows, plus the raw text so tests can prove no remote row leaked.
    fn ready(&self) -> (Vec<serde_json::Value>, String) {
        let out = self.ok(&["ready", "--json"]);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        (serde_json::from_str(&text).unwrap(), text)
    }

    fn blocked_reasons(row: &serde_json::Value) -> Vec<String> {
        row.get("blocking_reasons")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn assert_only_child(rows: &[serde_json::Value], child: &str) {
    assert_eq!(
        rows.len(),
        1,
        "only the local dependent should appear: {rows:?}"
    );
    assert_eq!(rows[0]["id"], serde_json::json!(child));
}

fn assert_no_remote_text(text: &str) {
    assert!(!text.contains("marbles://"), "remote ref leaked: {text}");
    assert!(!text.contains(STORE), "remote store leaked: {text}");
    assert!(!text.contains(REMOTE_ISSUE), "remote issue leaked: {text}");
}

#[test]
fn fresh_verified_closed_remote_dep_is_ready() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "closed", at);
    fx.ok(&["dep", "add", &child, REF]);

    let (rows, text) = fx.ready();
    assert_only_child(&rows, &child);
    assert!(
        Fixture::blocked_reasons(&rows[0]).is_empty(),
        "a fresh closed remote dep must not block: {rows:?}"
    );
    assert_no_remote_text(&text);
    assert_eq!(fx.edges(&child).len(), 1, "readiness must keep the edge");
}

#[test]
fn stale_closed_remote_dep_blocks_and_names_stale() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "closed", at - STALE_AGE);
    fx.ok(&["dep", "add", &child, REF]);

    let (rows, text) = fx.ready();
    assert_only_child(&rows, &child);
    let reasons = Fixture::blocked_reasons(&rows[0]);
    assert_eq!(reasons, vec!["remote: stale".to_string()], "{rows:?}");
    assert_no_remote_text(&text);
    assert_eq!(
        fx.edges(&child).len(),
        1,
        "a stale assertion must not remove the edge"
    );
}

#[test]
fn unreachable_remote_dep_blocks_and_names_unknown() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "closed", at);
    fx.ok(&["dep", "add", &child, REF]);
    // The keep-a-verified-assertion rule means an unreachable marker is only written when nothing
    // verified is cached; that is exactly the "remote went away" degradation this test names.
    fx.drop_cache_entry();
    fx.cache()
        .note_unreachable(STORE, REMOTE_ISSUE, "offline", at)
        .unwrap();

    let (rows, text) = fx.ready();
    assert_only_child(&rows, &child);
    assert_eq!(
        Fixture::blocked_reasons(&rows[0]),
        vec!["remote: unknown".to_string()],
        "{rows:?}"
    );
    assert_no_remote_text(&text);
    assert_eq!(fx.edges(&child).len(), 1);
}

#[test]
fn forged_remote_dep_blocks_and_names_unverifiable() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "closed", at);
    fx.ok(&["dep", "add", &child, REF]);
    fx.tamper_signature();

    let (rows, text) = fx.ready();
    assert_only_child(&rows, &child);
    assert_eq!(
        Fixture::blocked_reasons(&rows[0]),
        vec!["remote: unverifiable".to_string()],
        "a bad signature must block, not degrade to ready: {rows:?}"
    );
    assert_no_remote_text(&text);
    assert_eq!(
        fx.edges(&child).len(),
        1,
        "a verification failure must not remove the edge"
    );
}

#[test]
fn absent_remote_dep_blocks_and_names_unknown() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "closed", at);
    fx.ok(&["dep", "add", &child, REF]);
    fx.drop_cache_entry();

    let (rows, text) = fx.ready();
    assert_only_child(&rows, &child);
    assert_eq!(
        Fixture::blocked_reasons(&rows[0]),
        vec!["remote: unknown".to_string()],
        "no cached assertion must block as unknown: {rows:?}"
    );
    assert_no_remote_text(&text);
    assert_eq!(fx.edges(&child).len(), 1);
}

#[test]
fn fresh_open_remote_dep_blocks_and_names_open() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "open", at);
    fx.ok(&["dep", "add", &child, REF]);

    let (rows, text) = fx.ready();
    assert_only_child(&rows, &child);
    assert_eq!(
        Fixture::blocked_reasons(&rows[0]),
        vec!["remote: open".to_string()],
        "a fresh non-closed state must block and name itself: {rows:?}"
    );
    assert_no_remote_text(&text);
    assert_eq!(fx.edges(&child).len(), 1);
}

#[test]
fn ready_output_contains_zero_remote_rows() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let other = fx.issue("second local", at - 3_600);
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, "closed", at);

    // One local issue waits on the satisfied remote; the other has no remote edge at all.
    fx.ok(&["dep", "add", &child, REF]);

    let (rows, text) = fx.ready();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let texts = stdout_rows(&rows);
    assert!(texts.contains(&child), "{rows:?}");
    assert!(texts.contains(&other), "{rows:?}");
    for row in &rows {
        let id = row["id"].as_str().unwrap();
        assert!(!id.contains(STORE) && !id.contains(REMOTE_ISSUE), "{row}");
    }
    assert_no_remote_text(&text);
    assert_eq!(
        fx.db().list(None, true, None).unwrap().len(),
        2,
        "readiness must never mint a row for the remote issue"
    );
}

fn stdout_rows(rows: &[serde_json::Value]) -> Vec<String> {
    rows.iter()
        .map(|row| row["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn local_dependencies_keep_current_behavior() {
    let fx = Fixture::new();
    let at = now();
    let child = fx.child(at);
    let parent = fx.issue("parent", at - 3_600);
    fx.ok(&["dep", "add", &child, &parent]);

    // Local blocker open: the child is not in the ready report at all, exactly as before. The
    // open parent itself is ready work.
    let (rows, _) = fx.ready();
    assert_eq!(rows.len(), 1, "only the open parent is ready: {rows:?}");
    assert_eq!(rows[0]["id"], serde_json::json!(parent));
    assert!(
        rows.iter().all(|row| row["id"] != serde_json::json!(child)),
        "a local blocker still hides dependent work: {rows:?}"
    );

    fx.ok(&["update", &parent, "--status", "closed"]);
    let (rows, _) = fx.ready();
    assert_only_child(&rows, &child);
    assert!(
        Fixture::blocked_reasons(&rows[0]).is_empty(),
        "a closed local dep must not add a remote reason: {rows:?}"
    );
    let row = fx
        .db()
        .get(&child)
        .map(|issue| issue.dependencies.len())
        .unwrap();
    assert_eq!(row, 1, "the local dep edge is untouched");
    assert!(fx.edges(&child).is_empty(), "local deps stay local");
}

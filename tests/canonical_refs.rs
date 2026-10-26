//! Acceptance tests for canonical cross-store refs and remote dependency edges.
//!
//! A remote edge stores an address plus the last verified assertion. It never creates a local
//! issue and never enters the local `dep` graph; unpinned stores and bad signatures fail closed
//! before any edge is written. Tests use a temp `$MARBLES_HOME` and no network.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use marbles::assertion_cache::{AssertionCache, IssueAssertion, sign_issue_assertion};
use marbles::db::snapshot::{Ed25519Signer, SnapshotSigner};
use marbles::db::{Db, now};
use marbles::identity::{DESCRIPTOR_SCHEMA, RemoteRecord, RemoteRegistry, StoreDescriptor};
use marbles::refs::{CanonicalRef, RefError};
use marbles::types::NewIssue;

const STORE: &str = "store_ab12";
const REF: &str = "marbles://store_ab12/acme/widgets/issue-1";

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

    fn child(&self) -> String {
        self.db()
            .create(
                &NewIssue {
                    title: "child".into(),
                    project: Some("local".into()),
                    available_at: Some(0),
                    ..Default::default()
                },
                1_000,
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

    fn observe(&self, record: &RemoteRecord, signer: &Ed25519Signer, at: i64) {
        let assertion = sign_issue_assertion(
            IssueAssertion::new(&record.store_id, "issue-1", "open", at),
            signer,
        )
        .unwrap();
        self.cache().record(record, &assertion, at).unwrap();
    }

    fn edges(&self, child: &str) -> Vec<marbles::types::RemoteDependency> {
        Db::open(self.db_path())
            .unwrap()
            .remote_dependencies(child)
            .unwrap()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn tamper_signature(fx: &Fixture) {
    let path = fx.cache().entry_path(STORE, "issue-1");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    value["assertion"]["signature"] = serde_json::json!("AAAA");
    fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
}

#[test]
fn canonical_ref_round_trips() {
    let parsed = CanonicalRef::parse(REF).unwrap();
    assert_eq!(parsed.store_id, STORE);
    assert_eq!(parsed.company, "acme");
    assert_eq!(parsed.project, "widgets");
    assert_eq!(parsed.issue_id, "issue-1");
    assert_eq!(parsed.to_string(), REF);
    assert_eq!(CanonicalRef::parse(&parsed.to_string()).unwrap(), parsed);
}

#[test]
fn invalid_canonical_refs_are_rejected() {
    assert!(matches!(
        CanonicalRef::parse("store/co/proj/issue"),
        Err(RefError::MissingScheme)
    ));
    assert!(matches!(
        CanonicalRef::parse("https://store/co/proj/issue"),
        Err(RefError::UnknownScheme(_))
    ));
    assert!(matches!(
        CanonicalRef::parse("marbles://store/co//issue"),
        Err(RefError::EmptySegment { .. })
    ));
    assert!(matches!(
        CanonicalRef::parse("marbles://store/co/proj"),
        Err(RefError::MissingSegment { .. })
    ));
    assert!(matches!(
        CanonicalRef::parse("marbles://store/co/proj/issue/extra"),
        Err(RefError::ExtraSegments { .. })
    ));
    assert!(matches!(
        CanonicalRef::parse("marbles://store/co/proj/issue/"),
        Err(RefError::TrailingSlash)
    ));
    assert!(matches!(
        CanonicalRef::parse("marbles://store/co proj/issue"),
        Err(RefError::Whitespace)
    ));
    assert!(matches!(
        CanonicalRef::parse("marbles://store!bad/co/proj/issue"),
        Err(RefError::InvalidSegment { .. })
    ));
    assert!(matches!(
        CanonicalRef::parse("marbles://store/co/proj/issue?x"),
        Err(RefError::InvalidSegment { .. })
    ));
}

#[test]
fn remote_dep_creates_no_local_issue_row() {
    let fx = Fixture::new();
    let child = fx.child();
    let at = now();
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, at);

    fx.ok(&["dep", "add", &child, REF]);

    let conn = rusqlite::Connection::open(fx.db_path()).unwrap();
    let issues: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM issue WHERE id LIKE '%issue-1%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let local_deps: i64 = conn
        .query_row("SELECT COUNT(*) FROM dep", [], |r| r.get(0))
        .unwrap();
    assert_eq!(issues, 0, "remote ref must never create a local issue row");
    assert_eq!(local_deps, 0, "remote edge must not become a local dep row");
    let edges = fx.edges(&child);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].canonical_ref, REF);
    assert_eq!(edges[0].state, "open");
    assert_eq!(edges[0].key_id, signer.key_id());
    assert!(!edges[0].signature.is_empty());
}

#[test]
fn remote_dep_is_absent_from_list_and_ready() {
    let fx = Fixture::new();
    let child = fx.child();
    let at = now();
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, at);
    fx.ok(&["dep", "add", &child, REF]);

    for verb in ["list", "ready"] {
        let out = stdout(&fx.ok(&[verb, "--json"]));
        let rows: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(rows.len(), 1, "{verb} should only show the local child");
        assert_eq!(rows[0]["id"], serde_json::json!(child));
        assert!(!out.contains(REF));
        assert!(!out.contains("issue-1"));
    }
}

#[test]
fn show_displays_canonical_remote_ref_and_freshness() {
    let fx = Fixture::new();
    let child = fx.child();
    let at = now();
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, at);
    fx.ok(&["dep", "add", &child, REF]);

    let text = stdout(&fx.ok(&["show", &child]));
    assert!(text.contains(REF), "{text}");
    assert!(text.contains("fresh"), "{text}");

    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&stdout(&fx.ok(&["show", &child, "--json"]))).unwrap();
    let remote = &rows[0]["remote_dependencies"][0];
    assert_eq!(remote["canonical_ref"], serde_json::json!(REF));
    assert_eq!(remote["freshness"], serde_json::json!("fresh"));
    assert_eq!(remote["state"], serde_json::json!("open"));
}

#[test]
fn unpinned_store_remote_dep_fails_closed() {
    let fx = Fixture::new();
    let child = fx.child();
    let at = now();
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, at);
    let mut registry = RemoteRegistry::load(&fx.registry_path()).unwrap();
    registry.remove("alpha").unwrap();
    registry.save(&fx.registry_path()).unwrap();

    let out = fx.mb(&["dep", "add", &child, REF]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("unverifiable") || stderr(&out).contains("not pinned"),
        "{}",
        stderr(&out)
    );
    assert!(
        fx.edges(&child).is_empty(),
        "failed add must not write an edge"
    );
}

#[test]
fn bad_signature_remote_dep_fails_closed() {
    let fx = Fixture::new();
    let child = fx.child();
    let at = now();
    let (record, signer) = fx.pin(at);
    fx.observe(&record, &signer, at);
    tamper_signature(&fx);

    let out = fx.mb(&["dep", "add", &child, REF]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unverifiable"), "{}", stderr(&out));
    assert!(
        fx.edges(&child).is_empty(),
        "failed add must not write an edge"
    );
}

#[test]
fn local_dependencies_keep_current_behavior() {
    let fx = Fixture::new();
    let child = fx.child();
    let parent = fx
        .db()
        .create(
            &NewIssue {
                title: "parent".into(),
                project: Some("local".into()),
                available_at: Some(0),
                ..Default::default()
            },
            1_001,
        )
        .unwrap();
    fx.ok(&["dep", "add", &child, &parent]);

    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&stdout(&fx.ok(&["show", &child, "--json"]))).unwrap();
    assert_eq!(
        rows[0]["dependencies"][0]["depends_on_id"],
        serde_json::json!(parent)
    );
    assert!(
        rows[0].get("remote_dependencies").is_none(),
        "local deps must not grow a remote edge"
    );
    assert!(fx.edges(&child).is_empty());
}

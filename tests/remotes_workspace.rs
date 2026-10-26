//! Acceptance tests for the read-only Remotes workspace.
//!
//! Remote stores are client-side pins, not local projects. The workspace lists them with a named
//! freshness, while the authoritative project/folder/issue tables never learn they exist, and
//! every mutation of a remote reference fails with a named error before a row can be written.
//! Tests run the real binary in temp homes/repos; no network and no `$MARBLES_HOME` are touched.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use marbles::assertion_cache::{AssertionCache, IssueAssertion, sign_issue_assertion};
use marbles::db::snapshot::{Ed25519Signer, SnapshotSigner};
use marbles::identity::{DESCRIPTOR_SCHEMA, RemoteRecord, RemoteRegistry, StoreDescriptor};
use marbles::types::Folder;

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

    fn assert_ok(&self, args: &[&str]) -> Output {
        let output = self.mb(args);
        assert!(
            output.status.success(),
            "mb {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn pin(&self, name: &str, store_id: &str, at: i64) -> (RemoteRecord, Ed25519Signer) {
        let signer = Ed25519Signer::generate(format!("{store_id}/key-1")).unwrap();
        let mut descriptor = StoreDescriptor {
            schema: DESCRIPTOR_SCHEMA.to_string(),
            store_id: store_id.to_string(),
            display_name: name.to_string(),
            endpoint: Some(format!("https://{name}.example.invalid")),
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
                name,
                &format!("https://{name}.example.invalid"),
                &serde_json::to_string(&descriptor).unwrap(),
                at,
            )
            .unwrap();
        registry.save(&path).unwrap();
        let record = registry.get(name).unwrap().clone();
        (record, signer)
    }

    fn observe(
        &self,
        record: &RemoteRecord,
        signer: &Ed25519Signer,
        issue: &str,
        state: &str,
        observed_at: i64,
    ) {
        let assertion = sign_issue_assertion(
            IssueAssertion::new(&record.store_id, issue, state, observed_at),
            signer,
        )
        .unwrap();
        self.cache()
            .record(record, &assertion, observed_at)
            .unwrap();
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|err| {
        panic!(
            "not JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            stdout(output),
            stderr(output)
        )
    })
}

fn local_db(fx: &Fixture) -> marbles::Db {
    let db = marbles::Db::open(fx.db_path()).unwrap();
    db.ensure_project("local", fx.repo.to_str().unwrap(), "local")
        .unwrap();
    db
}

#[test]
fn remotes_workspace_lists_store_id_and_freshness() {
    let fx = Fixture::new();
    let now = marbles::db::now();
    let (alpha, alpha_signer) = fx.pin("alpha", "store_ab12", now);
    fx.observe(&alpha, &alpha_signer, "issue-1", "open", now);
    let _ = fx.pin("beta", "store_cd34", now);

    let report = json(&fx.assert_ok(&["remotes", "--json"]));
    assert_eq!(report["registry_present"], serde_json::json!(true));
    assert_eq!(
        report["registry"],
        serde_json::json!(fx.registry_path().display().to_string())
    );
    let remotes = report["remotes"].as_array().unwrap();
    assert_eq!(remotes.len(), 2);

    let alpha_row = remotes
        .iter()
        .find(|row| row["name"] == serde_json::json!("alpha"))
        .expect("alpha listed");
    assert_eq!(alpha_row["store_id"], serde_json::json!("store_ab12"));
    assert_eq!(alpha_row["key_id"], serde_json::json!(alpha.key_id));
    assert_eq!(
        alpha_row["endpoint"],
        serde_json::json!("https://alpha.example.invalid")
    );
    assert_eq!(alpha_row["freshness"], serde_json::json!("fresh"));
    assert_eq!(alpha_row["references"], serde_json::json!(1));
    assert!(alpha_row["observed_at"].is_number());

    let beta_row = remotes
        .iter()
        .find(|row| row["name"] == serde_json::json!("beta"))
        .expect("beta listed");
    assert_eq!(beta_row["store_id"], serde_json::json!("store_cd34"));
    assert_eq!(
        beta_row["freshness"],
        serde_json::json!("unknown"),
        "no observations is unknown, never healthy/zero"
    );

    let text = stdout(&fx.assert_ok(&["remotes"]));
    assert!(text.contains("alpha"));
    assert!(text.contains("store_ab12"));
    assert!(text.contains("fresh"));
    assert!(text.contains("unknown"));
    assert!(text.contains("read-only"));

    // A verified observation ages into `stale`, still named rather than silently fresh.
    let (gamma, gamma_signer) = fx.pin("gamma", "store_ef56", now);
    fx.observe(&gamma, &gamma_signer, "issue-1", "open", now - 10_000);
    let report = json(&fx.assert_ok(&["remotes", "--json"]));
    let gamma_row = report["remotes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == serde_json::json!("gamma"))
        .unwrap();
    assert_eq!(gamma_row["freshness"], serde_json::json!("stale"));

    // An unreachable observation is `unknown`, not a healthy zero.
    let (delta, _) = fx.pin("delta", "store_gh78", now);
    fx.cache()
        .note_unreachable(&delta.store_id, "issue-1", "connection refused", now)
        .unwrap();
    let report = json(&fx.assert_ok(&["remotes", "--json"]));
    let delta_row = report["remotes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == serde_json::json!("delta"))
        .unwrap();
    assert_eq!(delta_row["freshness"], serde_json::json!("unknown"));
}

#[test]
fn remote_projects_are_absent_from_local_tree() {
    let fx = Fixture::new();
    let db = local_db(&fx);
    db.ensure_folder(
        &Folder {
            slug: "local-folder".into(),
            name: "Local folder".into(),
            parent: None,
            created_at: 0,
        },
        0,
    )
    .unwrap();
    db.set_project_folder("local", Some("local-folder"), 0)
        .unwrap();
    let now = marbles::db::now();
    let (remote, signer) = fx.pin("remote", "store_remote99", now);
    fx.observe(&remote, &signer, "issue-1", "open", now);

    // Reading the Remotes workspace first must not mutate the local tree.
    fx.assert_ok(&["remotes"]);

    let tree = stdout(&fx.assert_ok(&["folder", "tree", "--json"]));
    assert!(tree.contains("local-folder"));
    assert!(tree.contains("\"project\": \"local\""));
    assert!(!tree.contains("store_remote99"));
    assert!(!tree.contains("remote.example.invalid"));

    let view = db.folder_view().unwrap();
    assert_eq!(view.projects.len(), 1);
    assert_eq!(view.projects[0].project, "local");
}

#[test]
fn remote_projects_are_absent_from_project_listing() {
    let fx = Fixture::new();
    let db = local_db(&fx);
    let now = marbles::db::now();
    let (remote, signer) = fx.pin("remote", "store_remote99", now);
    fx.observe(&remote, &signer, "issue-1", "open", now);

    fx.assert_ok(&["remotes"]);
    let projects = json(&fx.assert_ok(&["projects"]));
    let rows = projects.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["slug"], serde_json::json!("local"));
    let text = stdout(&fx.assert_ok(&["projects"]));
    assert!(!text.contains("store_remote99"));
    assert!(!text.contains("remote.example.invalid"));

    let projects = db.projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].0, "local");
}

#[test]
fn ready_never_returns_a_remote_row() {
    let fx = Fixture::new();
    let db = local_db(&fx);
    let now = marbles::db::now();
    let local = db
        .create(
            &marbles::types::NewIssue {
                title: "local ready work".into(),
                project: Some("local".into()),
                ..Default::default()
            },
            now - 3_600,
        )
        .unwrap();

    let (remote, signer) = fx.pin("remote", "store_remote99", now);
    // The remote assertion is open and would be ready if it were local.
    fx.observe(&remote, &signer, "issue-1", "open", now);

    let ready = json(&fx.assert_ok(&["ready", "--json"]));
    let rows = ready.as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the local issue is ready: {rows:?}");
    assert_eq!(rows[0]["id"], serde_json::json!(local));
    let text = stdout(&fx.assert_ok(&["ready", "--json"]));
    assert!(!text.contains("store_remote99"));
    assert!(!text.contains("issue-1"));

    let all = db.list(None, true, None).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, local);
}

#[test]
fn claim_assign_close_on_remote_ref_fail_with_named_error() {
    let fx = Fixture::new();
    let now = marbles::db::now();
    fx.pin("remote", "store_ab12", now);

    let attempts: &[(&[&str], &str)] = &[
        (&["claim", "store_ab12/issue-1", "--as", "agent-1"], "claim"),
        (
            &["update", "store_ab12/issue-1", "--assignee", "agent-1"],
            "assign",
        ),
        (
            &[
                "close",
                "store_ab12/issue-1",
                "--ack",
                "no delivery expected: remote",
            ],
            "close",
        ),
    ];
    for (args, action) in attempts {
        let output = fx.mb(args);
        assert!(
            !output.status.success(),
            "mb {args:?} unexpectedly succeeded"
        );
        let err = stderr(&output);
        assert!(
            err.contains("remote refs are read-only: store_ab12/issue-1"),
            "mb {args:?} did not name the remote ref ({action}): {err}"
        );
        assert!(
            err.contains("mb remotes"),
            "the error must be actionable: {err}"
        );
    }

    // No local issue/project row was created by the rejected attempts.
    let db = marbles::Db::open(fx.db_path()).unwrap();
    assert!(db.list(None, true, None).unwrap().is_empty());

    // A pin name is accepted as the ref's first segment and reported as the store_id.
    let output = fx.mb(&["claim", "remote/issue-1"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("remote refs are read-only: store_ab12/issue-1"));

    // Local ids still work: the guard must not swallow ordinary local work.
    let db = local_db(&fx);
    let local = db
        .create(
            &marbles::types::NewIssue {
                title: "local".into(),
                project: Some("local".into()),
                ..Default::default()
            },
            now - 3_600,
        )
        .unwrap();
    let output = fx.mb(&["claim", &local, "--as", "agent-1"]);
    assert!(
        output.status.success(),
        "local claim broke: {}",
        stderr(&output)
    );
}

#[test]
fn missing_registry_is_stated_plainly() {
    let fx = Fixture::new();
    assert!(!fx.registry_path().exists());
    let output = fx.assert_ok(&["remotes"]);
    let text = stdout(&output);
    assert!(text.contains("no remotes pinned"), "{text}");
    assert!(text.contains("registry does not exist"), "{text}");
    assert!(text.contains("read-only"), "{text}");

    let report = json(&fx.assert_ok(&["remotes", "--json"]));
    assert_eq!(report["registry_present"], serde_json::json!(false));
    assert_eq!(report["remotes"], serde_json::json!([]));
}

#[test]
fn corrupt_registry_fails_closed() {
    let fx = Fixture::new();
    fs::write(fx.registry_path(), "{ this is not json").unwrap();
    let output = fx.mb(&["remotes"]);
    assert!(!output.status.success(), "a corrupt registry must fail");
    let err = stderr(&output);
    assert!(err.contains("remote registry"), "{err}");
    assert!(err.contains("not valid JSON"), "{err}");
    assert!(!stdout(&output).contains("fresh"), "{err}");

    // The same corruption cannot turn a remote mutation into a local one.
    let output = fx.mb(&["claim", "store_ab12/issue-1"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("remote refs are read-only"));
}

//! A real, isolated, durable records book for the MCP grooming contract.
//!
//! Unlike the in-memory runner tests, this exercises the same append-only trail
//! the wasm Spore opens, including revision CAS and dependency-cycle refusal.
//! Nothing here touches the authorized fleet book.

use std::collections::BTreeMap;

use marbles::adapter::Adapter;
use marbles::records_cli;
use marbles::records_exec;
use marbles::store::Store as _;
use serde_json::Value;

const NOW: i64 = 1_800_000_000;
const PROJECT: &str = "groom";

fn invoke(store: &Adapter, line: &[&str]) -> Result<Value, String> {
    let argv: Vec<String> = line.iter().map(|arg| (*arg).to_string()).collect();
    let env = BTreeMap::from([("MARBLES_PROJECT".to_string(), PROJECT.to_string())]);
    let invocation = records_cli::parse(&argv, &env)?;
    records_exec::execute(store, &invocation, NOW)
}

fn run(store: &Adapter, line: &[&str]) -> Value {
    invoke(store, line).unwrap_or_else(|error| panic!("{}: {error}", line.join(" ")))
}

fn id(value: &Value) -> String {
    value["id"].as_str().expect("id").to_string()
}

fn open_book(path: &std::path::Path) -> Adapter {
    let store = Adapter::open_file(path.to_path_buf()).expect("open durable book");
    store
        .ensure_project(PROJECT, "/tmp/groom", "GROOM")
        .expect("project");
    store
}

#[test]
fn a_durable_book_refuses_a_stale_revision_and_a_cycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("book.trail");
    let store = open_book(&path);

    let first = id(&run(&store, &["create", "first", "--available-at", "0"]));
    let second = id(&run(&store, &["create", "second", "--available-at", "0"]));
    let revision = run(&store, &["revision", &first])["revision"]
        .as_u64()
        .expect("revision");

    let stale = invoke(
        &store,
        &[
            "update",
            &first,
            "--priority",
            "1",
            "--expect-revision",
            "0",
        ],
    )
    .expect_err("stale revision");
    assert!(stale.contains("revision conflict"), "{stale}");

    let updated = run(
        &store,
        &[
            "update",
            &first,
            "--priority",
            "1",
            "--expect-revision",
            &revision.to_string(),
        ],
    );
    assert_eq!(updated["priority"], 1);

    run(&store, &["dep", "add", &second, &first]);
    let cycle = invoke(&store, &["dep", "add", &first, &second]).expect_err("cycle");
    assert!(cycle.contains("cycle"), "{cycle}");

    // The trail is a real file: reopen and the work is still there.
    drop(store);
    let reopened = open_book(&path);
    let shown = run(&reopened, &["show", &first]);
    assert_eq!(shown[0]["priority"], 1);
    let deps = run(&reopened, &["show", &second]);
    assert_eq!(deps[0]["dependencies"][0]["depends_on_id"], first);
}

#[test]
fn a_durable_book_searches_and_exports_the_manifest_without_writing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_book(&dir.path().join("book.trail"));
    let alpha = id(&run(
        &store,
        &[
            "create",
            "alpha founder onboarding",
            "--available-at",
            "0",
            "--labels",
            "founder",
        ],
    ));
    run(&store, &["create", "beta quote", "--available-at", "0"]);

    let hits = run(&store, &["search", "--query", "founder", "--limit", "10"]);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["id"], alpha);

    let blocked = run(&store, &["search", "--blocked", "--limit", "10"]);
    assert!(blocked.as_array().unwrap().is_empty());

    let manifest = run(&store, &["manifest"]);
    assert_eq!(manifest["schema"], "marbles.command-manifest/1");
    assert_eq!(manifest["scope"]["store_wide_default"], true);
    assert!(
        manifest["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["name"] == "update" && op["cas"] == "--expect-revision")
    );
}

//! The records-backed command runner, exercised on the host.
//!
//! The shipped `mb` is wasm, but `records_exec` is a translation over the
//! `Store` contract, and `Adapter::in_memory` implements that contract. These
//! tests drive the real command surface against the real records adapter — the
//! same code the Spore runs — so a command cannot be "ported" and still answer
//! differently on the two backends.

use std::collections::BTreeMap;

use marbles::adapter::Adapter;
use marbles::records_cli;
use marbles::records_exec;
use serde_json::Value;

const NOW: i64 = 1_800_000_000;
const PROJECT: &str = "demo";

fn invoke(store: &Adapter, line: &[&str]) -> Result<Value, String> {
    let argv: Vec<String> = line.iter().map(|arg| (*arg).to_string()).collect();
    let env = BTreeMap::from([("MARBLES_PROJECT".to_string(), PROJECT.to_string())]);
    let invocation = records_cli::parse(&argv, &env)?;
    records_exec::execute(store, &invocation, NOW)
}

fn run(store: &Adapter, line: &[&str]) -> Value {
    invoke(store, line).expect("command succeeds")
}

fn ids(value: &Value) -> Vec<String> {
    value["id"]
        .as_str()
        .map(|id| vec![id.to_string()])
        .unwrap_or_default()
}

#[test]
fn create_carries_metadata_external_ref_and_plan_sugar() {
    let store = Adapter::in_memory().unwrap();
    let created = run(
        &store,
        &[
            "create",
            "ship it",
            "--available-at",
            "0",
            "--external-ref",
            "gh:7",
            "--metadata",
            r#"{"k":1}"#,
            "--plan",
            "ship",
            "--done-means",
            "cargo test",
            "--estimate",
            "45",
        ],
    );
    let id = ids(&created).remove(0);
    let shown = run(&store, &["show", &id]);
    let issue = &shown[0];
    assert_eq!(issue["metadata"]["k"], 1);
    assert_eq!(issue["metadata"]["plan"], "ship");
    assert_eq!(issue["metadata"]["done_means"], "cargo test");
    assert_eq!(issue["metadata"]["estimate"], 45);
    assert_eq!(issue["external_ref"], "gh:7");
}

#[test]
fn update_edits_metadata_appends_notes_and_claims_an_assignee() {
    let store = Adapter::in_memory().unwrap();
    let id = ids(&run(
        &store,
        &[
            "create",
            "before",
            "--description",
            "before",
            "--available-at",
            "0",
        ],
    ))
    .remove(0);
    let updated = run(
        &store,
        &[
            "update",
            &id,
            "--title",
            "after",
            "--append-notes",
            "a note",
            "--status",
            "in_progress",
            "--priority",
            "1",
            "--assignee",
            "worker-2",
            "--metadata",
            r#"{"keep":true,"drop":null}"#,
            "--set-metadata",
            "extra=two",
        ],
    );
    assert_eq!(updated["title"], "after");
    assert_eq!(updated["status"], "in_progress");
    assert_eq!(updated["priority"], 1);
    assert_eq!(updated["assignee"], "worker-2");
    assert_eq!(updated["description"], "before\na note");
    assert_eq!(updated["metadata"]["keep"], true);
    assert_eq!(updated["metadata"]["extra"], "two");
}

#[test]
fn metadata_from_a_file_is_refused_by_name() {
    let store = Adapter::in_memory().unwrap();
    let error = invoke(&store, &["create", "x", "--metadata", "@/tmp/meta.json"]).unwrap_err();
    assert!(error.contains("no filesystem"), "{error}");
}

#[test]
fn folders_are_created_listed_and_assigned() {
    let store = Adapter::in_memory().unwrap();
    let folder = run(&store, &["folder", "create", "Work Stuff"]);
    assert_eq!(folder["slug"], "work-stuff");
    run(&store, &["folder", "create", "Personal"]);

    let created = run(&store, &["create", "filed work", "--available-at", "0"]);
    assert_eq!(ids(&created).len(), 1);

    let moved = run(&store, &["folder", "move", PROJECT, "work-stuff"]);
    assert_eq!(moved["folder"], "work-stuff");

    let view = run(&store, &["folder", "list"]);
    assert_eq!(view["folders"].as_array().unwrap().len(), 2);
    assert_eq!(view["projects"][0]["project"], PROJECT);
    assert_eq!(view["projects"][0]["folder"], "work-stuff");

    let projects = run(&store, &["projects"]);
    assert_eq!(projects[0]["slug"], PROJECT);
    assert_eq!(projects[0]["folder"], "work-stuff");

    // Unfiling tombstones the assignment; the folder itself remains.
    run(&store, &["folder", "move", PROJECT, "none"]);
    let view = run(&store, &["folder", "list"]);
    assert_eq!(view["projects"].as_array().unwrap().len(), 0);
}

#[test]
fn project_rm_removes_the_project_and_its_issues() {
    let store = Adapter::in_memory().unwrap();
    run(&store, &["create", "one", "--available-at", "0"]);
    run(&store, &["create", "two", "--available-at", "0"]);
    let removed = run(&store, &["project", "rm", PROJECT]);
    assert_eq!(removed["removed"], PROJECT);
    assert_eq!(removed["issues"], 2);
    assert_eq!(run(&store, &["projects"]).as_array().unwrap().len(), 0);
    assert_eq!(run(&store, &["list"]).as_array().unwrap().len(), 0);
}

#[test]
fn goal_epic_task_chain_rolls_up_and_renders_a_tree() {
    let store = Adapter::in_memory().unwrap();
    let goal = ids(&run(
        &store,
        &["goal", "create", "Ship it", "--available-at", "0"],
    ))
    .remove(0);
    let epic = ids(&run(
        &store,
        &[
            "epic",
            "create",
            "--goal",
            &goal,
            "An epic",
            "--available-at",
            "0",
        ],
    ))
    .remove(0);
    let task = ids(&run(
        &store,
        &[
            "task",
            "create",
            "--epic",
            &epic,
            "A task",
            "--available-at",
            "0",
        ],
    ))
    .remove(0);

    let listed = run(&store, &["goal", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"], goal);
    // The goal is the container; its progress counts the epic and task under it.
    assert_eq!(listed[0]["progress"]["total"], 2);

    let shown = run(&store, &["goal", "show", &goal]);
    assert_eq!(shown["issue"]["id"], goal);
    assert_eq!(shown["progress"]["total"], 2);
    let tree = shown["tree"].as_array().unwrap();
    assert!(
        tree.iter()
            .any(|line| line.as_str().unwrap().contains(&task)),
        "tree should name the task: {tree:?}"
    );

    // A task cannot be made under a non-epic, and an epic needs a goal.
    assert!(invoke(&store, &["task", "create", "--epic", &goal, "x"]).is_err());
    assert!(invoke(&store, &["epic", "create", "--goal", &task, "x"]).is_err());
}

#[test]
fn plan_create_show_ready_and_gantt_use_the_records_store() {
    let store = Adapter::in_memory().unwrap();
    let spec = r#"
name = "ship"
project = "demo"
[[nodes]]
key = "a"
title = "Schema"
estimate = 10
[[nodes]]
key = "b"
title = "API"
estimate = 30
deps = ["a"]
"#;
    // A dry run writes nothing.
    let draft = run(&store, &["plan", "create", spec, "--dry-run"]);
    assert_eq!(draft["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(run(&store, &["list", "--all"]).as_array().unwrap().len(), 0);

    let created = run(&store, &["plan", "create", spec]);
    assert_eq!(created["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(created["edges"].as_array().unwrap().len(), 1);

    let shown = run(&store, &["plan", "show", "ship"]);
    assert_eq!(shown["plan"], "ship");
    assert_eq!(shown["nodes"].as_array().unwrap().len(), 2);

    let ready = run(&store, &["plan", "ready", "ship"]);
    assert_eq!(ready["ready"].as_array().unwrap().len(), 1);
    assert_eq!(ready["ready"][0]["key"], "a");

    let gantt = run(&store, &["plan", "gantt", "ship"]);
    assert_eq!(gantt["total_minutes"], 40);
    assert_eq!(gantt["critical_path"].as_array().unwrap().len(), 2);

    // A path is not silently parsed as TOML.
    let error = invoke(&store, &["plan", "create", "plan.toml"]).unwrap_err();
    assert!(error.contains("not a path"), "{error}");
}

#[test]
fn burndown_and_portfolio_answer_over_the_same_issues() {
    let store = Adapter::in_memory().unwrap();
    run(&store, &["folder", "create", "Work"]);
    run(&store, &["create", "one", "--available-at", "0"]);
    run(&store, &["create", "two", "--available-at", "0"]);

    let burndown = run(&store, &["burndown", "--days", "7"]);
    assert_eq!(burndown["days"], 7);
    assert_eq!(burndown["points"].as_array().unwrap().len(), 8);

    let unfiled = run(&store, &["portfolio"]);
    assert_eq!(unfiled["totals"]["projects"], 1);
    assert_eq!(unfiled["unfiled"].as_array().unwrap().len(), 1);

    run(&store, &["folder", "move", PROJECT, "work"]);
    let filed = run(&store, &["portfolio", "--folder", "work"]);
    assert_eq!(filed["folders"].as_array().unwrap().len(), 1);
    assert_eq!(filed["unfiled"].as_array().unwrap().len(), 0);
}

#[test]
fn doctor_reads_the_book_and_reports_what_it_found() {
    let store = Adapter::in_memory().unwrap();
    let healthy = run(&store, &["doctor"]);
    assert_eq!(healthy["backend"], "soil:records");
    assert_eq!(healthy["integrity"], "ok");
    assert_eq!(healthy["quick_check"], "ok");

    let a = ids(&run(&store, &["create", "a", "--available-at", "0"])).remove(0);
    let b = ids(&run(&store, &["create", "b", "--available-at", "0"])).remove(0);
    run(&store, &["dep", "add", &b, &a]);
    let report = run(&store, &["doctor"]);
    assert_eq!(report["projects"], 1);
    assert_eq!(report["issues"], 2);
    assert_eq!(report["dependencies"], 1);
    assert_eq!(report["problems"].as_array().unwrap().len(), 0);
}

#[test]
fn a_write_without_a_project_says_which_switch_it_needs() {
    let store = Adapter::in_memory().unwrap();
    let argv: Vec<String> = ["create", "x"]
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
    let invocation = records_cli::parse(&argv, &BTreeMap::new()).unwrap();
    let error = records_exec::execute(&store, &invocation, NOW).unwrap_err();
    assert!(error.contains("--project"), "{error}");
}

#[test]
fn revision_is_exposed_and_a_stale_update_is_refused() {
    let store = Adapter::in_memory().unwrap();
    let id = ids(&run(
        &store,
        &["create", "conditional", "--available-at", "0"],
    ))
    .remove(0);
    let first = run(&store, &["revision", &id]);
    let revision = first["revision"].as_u64().expect("revision");
    assert!(revision > 0);

    let error = invoke(
        &store,
        &[
            "update",
            &id,
            "--priority",
            "1",
            "--expect-revision",
            &(revision - 1).to_string(),
        ],
    )
    .expect_err("stale revision");
    assert!(error.contains("revision conflict"), "{error}");

    let updated = run(
        &store,
        &[
            "update",
            &id,
            "--priority",
            "1",
            "--expect-revision",
            &revision.to_string(),
        ],
    );
    assert_eq!(updated["priority"], 1);

    let replayed = invoke(
        &store,
        &[
            "update",
            &id,
            "--priority",
            "2",
            "--expect-revision",
            &revision.to_string(),
        ],
    )
    .expect_err("the revision has moved");
    assert!(replayed.contains("revision conflict"), "{replayed}");
}

#[test]
fn search_filters_are_bounded_and_do_not_claim_a_total() {
    let store = Adapter::in_memory().unwrap();
    let a = ids(&run(
        &store,
        &[
            "create",
            "alpha ship",
            "--available-at",
            "0",
            "--labels",
            "sdk",
        ],
    ))
    .remove(0);
    let b = ids(&run(
        &store,
        &["create", "beta quote", "--available-at", "0"],
    ))
    .remove(0);
    run(&store, &["update", &b, "--status", "blocked"]);

    let hits = run(&store, &["search", "--query", "ship", "--limit", "1"]);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["id"], a);

    let blocked = run(&store, &["search", "--blocked", "--limit", "10"]);
    assert_eq!(blocked.as_array().unwrap().len(), 1);
    assert_eq!(blocked[0]["id"], b);

    let labelled = run(&store, &["search", "--label", "sdk", "--limit", "10"]);
    assert_eq!(labelled.as_array().unwrap().len(), 1);
    assert_eq!(labelled[0]["id"], a);
}

#[test]
fn manifest_exports_the_operation_and_authority_contract() {
    let store = Adapter::in_memory().unwrap();
    let manifest = run(&store, &["manifest"]);
    assert_eq!(manifest["schema"], "marbles.command-manifest/1");
    assert_eq!(manifest["lineage"], "records");
    let operations = manifest["operations"].as_array().expect("operations");
    let update = operations
        .iter()
        .find(|op| op["name"] == "update")
        .expect("update operation");
    assert_eq!(update["mode"], "routine_write");
    assert_eq!(update["cas"], "--expect-revision");
    let close = operations
        .iter()
        .find(|op| op["name"] == "close")
        .expect("close operation");
    assert_eq!(close["mode"], "sensitive");
}

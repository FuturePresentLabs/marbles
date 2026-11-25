//! The `mb capability` surface: the six readiness questions are queries over typed records,
//! not a second ledger to keep in sync.

use std::collections::BTreeMap;

use marbles::adapter::Adapter;
use marbles::{records_cli, records_exec};
use serde_json::Value;

const NOW: i64 = 1_800_000_000;

fn run(store: &Adapter, argv: &[&str]) -> Value {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let invocation = records_cli::parse(&argv, &BTreeMap::new()).expect("parse capability command");
    records_exec::execute(store, &invocation, NOW).expect("execute capability command")
}

fn seed(store: &Adapter) {
    run(
        store,
        &[
            "capability",
            "create",
            "transmog.batch-provider",
            "--title",
            "Transmog batch provider",
            "--owner",
            "Transmog",
            "--project",
            "transmog",
            "--implementation",
            "execution_verified",
            "--health",
            "healthy",
            "--lifecycle",
            "active",
            "--commercial-readiness",
            "demo_ready",
            "--implementation-id",
            "b8c93fb4",
            "--required-layers",
            "loam,soil",
        ],
    );
    run(
        store,
        &[
            "capability",
            "verify",
            "transmog.batch-provider",
            "--layer",
            "loam",
            "--result",
            "pass",
            "--implementation-id",
            "b8c93fb4",
            "--workload",
            "DFM 369684-byte artifact",
            "--evidence",
            "commit b8c93fb4",
            "--limitations",
            "reported, not re-run",
        ],
    );
    run(
        store,
        &[
            "capability",
            "measure",
            "transmog.batch-provider",
            "--metric",
            "dfm_seconds",
            "--value",
            "3.887",
            "--unit",
            "s",
            "--workload",
            "DFM",
            "--profile",
            "debug-native",
            "--samples",
            "1",
            "--outcome",
            "analysis returned",
            "--limitations",
            "single reported observation",
        ],
    );
}

#[test]
fn six_questions_are_queries_over_the_same_store() {
    let store = Adapter::in_memory().expect("store");
    seed(&store);

    // What capabilities exist?
    let list = run(&store, &["capability", "list"]);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], "transmog.batch-provider");

    // Which revisions have verified Loam execution?
    let verified = run(&store, &["capability", "verified", "--layer", "loam"]);
    assert_eq!(verified[0]["implementation_id"], "b8c93fb4");
    assert_eq!(verified[0]["freshness"], "fresh");

    // Which capabilities lack Soil verification?
    let gaps = run(&store, &["capability", "gaps", "--layer", "soil"]);
    assert_eq!(gaps[0]["capability_id"], "transmog.batch-provider");
    assert_eq!(gaps[0]["status"], "missing");

    // Which verification evidence is stale? (none yet, so the command is empty, not an error)
    assert_eq!(run(&store, &["capability", "stale"]), Value::Array(vec![]));

    // What blocks a commercial pilot?
    let blockers = run(&store, &["capability", "pilot-blockers"]);
    assert_eq!(blockers[0]["capability_id"], "transmog.batch-provider");
    assert_eq!(blockers[0]["gaps"], serde_json::json!(["soil"]));

    // Which measurements establish the current baseline?
    let baseline = run(
        &store,
        &["capability", "baseline", "--metric", "dfm_seconds"],
    );
    assert_eq!(baseline[0]["value"], 3.887);
    assert_eq!(baseline[0]["profile"], "debug-native");
    assert_eq!(baseline[0]["samples"], 1);
}

#[test]
fn a_new_revision_turns_its_old_verification_into_a_stale_answer() {
    let store = Adapter::in_memory().expect("store");
    seed(&store);
    run(
        &store,
        &[
            "capability",
            "update",
            "transmog.batch-provider",
            "--implementation-id",
            "afd8dd0",
        ],
    );

    let stale = run(&store, &["capability", "stale"]);
    assert_eq!(stale[0]["freshness"], "revision_mismatch");
    assert_eq!(stale[0]["implementation_id"], "b8c93fb4");
    assert_eq!(stale[0]["current_implementation_id"], "afd8dd0");

    let gaps = run(&store, &["capability", "gaps", "--layer", "loam"]);
    assert_eq!(gaps[0]["status"], "pass");
    assert_eq!(gaps[0]["freshness"], "revision_mismatch");
}

#[test]
fn read_commands_are_marked_read_only() {
    for argv in [
        vec!["capability", "list"],
        vec!["capability", "show", "x"],
        vec!["capability", "verified", "--layer", "loam"],
        vec!["capability", "gaps", "--layer", "soil"],
        vec!["capability", "stale"],
        vec!["capability", "pilot-blockers"],
        vec!["capability", "baseline"],
    ] {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let invocation = records_cli::parse(&argv, &BTreeMap::new()).expect("parse");
        assert!(invocation.request.is_read_only(), "{argv:?}");
    }
}

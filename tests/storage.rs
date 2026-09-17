//! The behavior the design decisions were made for. Each test names the decision it protects.

use marbles::db::{CompanyStores, Db, now};
use marbles::types::*;

fn db_with_project() -> Db {
    let db = Db::in_memory().unwrap();
    db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    db
}

fn new(title: &str) -> NewIssue {
    NewIssue {
        title: title.into(),
        description: String::new(),
        issue_type: "task".into(),
        priority: 2,
        labels: vec![],
        parent: None,
        id: None,
        project: Some("demo".into()),
        available_at: None,
        created_by: Some("tester".into()),
        metadata: None,
        external_ref: None,
    }
}

// ---- claims ----

#[test]
fn a_live_claim_cannot_be_taken_and_the_error_names_the_holder() {
    let db = db_with_project();
    let id = db.create(&new("race"), now()).unwrap();
    db.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "agent-a".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: None,
        },
        now(),
    )
    .unwrap();
    let err = db
        .claim(
            &ClaimRequest {
                id: id.clone(),
                assignee: "agent-b".into(),
                actor_kind: ActorKind::Agent,
                ttl_seconds: None,
            },
            now(),
        )
        .unwrap_err();
    assert!(
        matches!(&err, marbles::db::Error::AlreadyClaimed(_, holder) if holder == "agent-a"),
        "{err}"
    );
}

#[test]
fn the_same_holder_renewing_its_own_claim_is_not_a_conflict() {
    let db = db_with_project();
    let id = db.create(&new("reclaim"), now()).unwrap();
    let req = ClaimRequest {
        id: id.clone(),
        assignee: "agent-a".into(),
        actor_kind: ActorKind::Agent,
        ttl_seconds: None,
    };
    db.claim(&req, now()).unwrap();
    let receipt = db.claim(&req, now()).unwrap();
    assert!(receipt.expires_at > now());
}

#[test]
fn an_expired_agent_lease_is_claimable_by_the_next_agent() {
    let db = db_with_project();
    let id = db.create(&new("stale"), now()).unwrap();
    db.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "crashed-agent".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(60),
        },
        1_000,
    )
    .unwrap();
    let receipt = db
        .claim(
            &ClaimRequest {
                id: id.clone(),
                assignee: "next-agent".into(),
                actor_kind: ActorKind::Agent,
                ttl_seconds: Some(60),
            },
            1_061,
        )
        .unwrap();
    assert_eq!(receipt.assignee, "next-agent");
}

#[test]
fn human_claims_store_a_business_hours_deadline_not_wall_clock_48h() {
    let db = Db::in_memory().unwrap().with_policy(
        Policy::default(),
        marbles::time::WorkWeek {
            days: vec![1, 2, 3, 4, 5],
            start_hour: 9,
            end_hour: 17,
            utc_offset_minutes: 0,
        },
    );
    db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    let id = db.create(&new("human"), 1_000_000_000).unwrap(); // a Wednesday
    let receipt = db
        .claim(
            &ClaimRequest {
                id,
                assignee: "avery".into(),
                actor_kind: ActorKind::Human,
                ttl_seconds: None,
            },
            1_000_000_000,
        )
        .unwrap();
    // 48 business hours from a Wednesday is a week and a half of wall clock, not two days.
    assert!(
        receipt.expires_at - 1_000_000_000 > 4 * 24 * 3600,
        "{:?}",
        receipt.expires_at
    );
}

#[test]
fn heartbeat_renews_the_holder_and_only_the_holder() {
    let db = db_with_project();
    let id = db.create(&new("beat"), now()).unwrap();
    db.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "agent-a".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(60),
        },
        now(),
    )
    .unwrap();
    assert!(
        db.touch(&id, "agent-b", now()).is_err(),
        "renewal is holder-only"
    );
    let expiry = db.touch(&id, "agent-a", now() + 30).unwrap();
    assert!(expiry > now() + 30);
}

// ---- sweep ----

#[test]
fn the_sweeper_requeues_expired_agents_but_only_escalates_expired_humans() {
    // A 24/7 working week makes business time == wall clock so the test can pin expiries.
    let db = Db::in_memory().unwrap().with_policy(
        Policy::default(),
        marbles::time::WorkWeek {
            days: vec![1, 2, 3, 4, 5, 6, 7],
            start_hour: 0,
            end_hour: 24,
            utc_offset_minutes: 0,
        },
    );
    db.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    let agent = db.create(&new("agent work"), 100).unwrap();
    let human = db.create(&new("human work"), 100).unwrap();
    db.claim(
        &ClaimRequest {
            id: agent.clone(),
            assignee: "dead-agent".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(10),
        },
        200,
    )
    .unwrap();
    db.claim(
        &ClaimRequest {
            id: human.clone(),
            assignee: "avery".into(),
            actor_kind: ActorKind::Human,
            ttl_seconds: Some(10),
        },
        200,
    )
    .unwrap();
    let report = db.sweep(300).unwrap();
    assert_eq!(report.requeued, vec![agent.clone()]);
    assert_eq!(report.escalated, vec![format!("{human} (avery)")]);
    let held = db.get(&human).unwrap();
    assert_eq!(
        held.assignee.as_deref(),
        Some("avery"),
        "a human's work is not quietly re-stolen"
    );
    assert!(db.get(&agent).unwrap().assignee.is_none());
    assert!(
        !db.history(&human)
            .unwrap()
            .iter()
            .any(|e| e.event == "claim_expired")
    );
}

// ---- grace & readiness ----

#[test]
fn fresh_work_is_invisible_to_the_swarm_until_its_grace_lapses() {
    let db = db_with_project();
    let id = db.create(&new("still speccing"), 100).unwrap();
    assert!(
        db.ready(Some("demo"), 100).unwrap().is_empty(),
        "created_at is not up-for-grabs"
    );
    assert!(db.ready(Some("demo"), 400).unwrap().is_empty());
    assert_eq!(db.ready(Some("demo"), 701).unwrap().len(), 1, "10m grace");
    let _ = id;
}

#[test]
fn an_edit_mid_planning_defers_the_swarm_again() {
    let db = db_with_project();
    let id = db.create(&new("editing"), 100).unwrap();
    // Grace has lapsed…
    assert_eq!(db.ready(Some("demo"), 701).unwrap().len(), 1);
    // …but a spec edit pushes it out by the deferral window from the edit.
    db.update(
        &id,
        &IssuePatch {
            append_notes: Some("not done yet".into()),
            ..Default::default()
        },
        "avery",
        900,
    )
    .unwrap();
    assert!(db.ready(Some("demo"), 901).unwrap().is_empty());
    assert_eq!(db.ready(Some("demo"), 900 + 121).unwrap().len(), 1);
}

#[test]
fn dependencies_decide_eligibility() {
    let db = db_with_project();
    let parent = db.create(&new("parent"), 100).unwrap();
    let child = db.create(&new("child"), 100).unwrap();
    db.add_dep(&child, &parent, "tester", 100).unwrap();
    assert_eq!(
        db.ready(Some("demo"), 701)
            .unwrap()
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        vec![parent.as_str()]
    );
    db.close(
        &parent,
        None,
        &[Evidence::pr("https://example/pr/1")],
        "tester",
        702,
    )
    .unwrap();
    assert_eq!(
        db.ready(Some("demo"), 703).unwrap().len(),
        1,
        "closing the parent unblocks the child"
    );
}

#[test]
fn a_dependency_cycle_is_refused_at_creation_not_at_dispatch() {
    let db = db_with_project();
    let a = db.create(&new("a"), 100).unwrap();
    let b = db.create(&new("b"), 100).unwrap();
    let c = db.create(&new("c"), 100).unwrap();
    db.add_dep(&a, &b, "t", 100).unwrap();
    db.add_dep(&b, &c, "t", 100).unwrap();
    let err = db.add_dep(&c, &a, "t", 100).unwrap_err();
    assert!(matches!(err, marbles::db::Error::Cycle { .. }), "{err}");
    let err = db.add_dep(&a, &a, "t", 100).unwrap_err();
    assert!(
        matches!(err, marbles::db::Error::Cycle { .. }),
        "self-cycle: {err}"
    );
}

// ---- review & close ----

#[test]
fn closing_without_evidence_is_refused_and_review_is_the_correct_move() {
    let db = db_with_project();
    let id = db.create(&new("delivered"), 100).unwrap();
    db.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "agent-a".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: None,
        },
        101,
    )
    .unwrap();
    let err = db.close(&id, None, &[], "agent-a", 200).unwrap_err();
    assert!(matches!(err, marbles::db::Error::NoEvidence(_)), "{err}");
    // The agent moves it to review with the PR link — never to closed.
    db.update(
        &id,
        &IssuePatch {
            status: Some("review".into()),
            evidence: Some(vec![Evidence::pr("https://example/pr/9")]),
            ..Default::default()
        },
        "agent-a",
        200,
    )
    .unwrap();
    assert_eq!(db.get(&id).unwrap().status, "review");
    // Only a human (or the delivery loop acting as one) closes, with merge evidence.
    db.close(
        &id,
        Some("merged"),
        &[Evidence::commit("abc123")],
        "avery",
        300,
    )
    .unwrap();
    let closed = db.get(&id).unwrap();
    assert_eq!(closed.status, "closed");
    assert_eq!(
        closed.evidence.len(),
        2,
        "review evidence is kept alongside the merge receipt"
    );
    assert_eq!(closed.evidence[0].kind, "pr");
}

#[test]
fn non_code_outcomes_close_with_an_explicit_ack() {
    let db = db_with_project();
    let id = db
        .create(&new("research: should we use dolt"), 100)
        .unwrap();
    db.close(
        &id,
        Some("answered"),
        &[Evidence::ack(
            "no delivery expected: research finding recorded in description",
        )],
        "avery",
        200,
    )
    .unwrap();
    assert_eq!(db.get(&id).unwrap().status, "closed");
}

#[test]
fn review_state_blocks_dependents_until_merged() {
    let db = db_with_project();
    let parent = db.create(&new("parent"), 100).unwrap();
    let child = db.create(&new("child"), 100).unwrap();
    db.add_dep(&child, &parent, "t", 100).unwrap();
    db.update(
        &parent,
        &IssuePatch {
            status: Some("review".into()),
            ..Default::default()
        },
        "t",
        700,
    )
    .unwrap();
    assert_eq!(
        db.ready(Some("demo"), 800).unwrap().len(),
        0,
        "a PR is not a merge"
    );
}

#[test]
fn supersede_closes_with_a_pointer_not_a_claim() {
    let db = db_with_project();
    let stale = db.create(&new("stale"), 100).unwrap();
    let fresh = db.create(&new("fresh"), 100).unwrap();
    db.supersede(&stale, &fresh, "avery", 200).unwrap();
    let issue = db.get(&stale).unwrap();
    assert_eq!(issue.status, "closed");
    assert!(
        issue
            .labels
            .iter()
            .any(|l| l == &format!("supersedes:{fresh}"))
    );
    assert_eq!(
        issue.evidence[0].kind, "ack",
        "the receipt says supersession, not delivery"
    );
}

// ---- listing & history ----

#[test]
fn closed_work_stays_readable_and_the_ready_set_moves_on() {
    let db = db_with_project();
    let id = db.create(&new("finish me"), 100).unwrap();
    db.close(&id, None, &[Evidence::pr("https://example/pr/1")], "t", 200)
        .unwrap();
    assert!(
        db.list(Some("demo"), false, None)
            .unwrap()
            .iter()
            .all(|i| i.id != id)
    );
    assert!(
        db.list(Some("demo"), true, None)
            .unwrap()
            .iter()
            .any(|i| i.id == id),
        "history is queryable"
    );
    let events: Vec<String> = db
        .history(&id)
        .unwrap()
        .into_iter()
        .map(|e| e.event)
        .collect();
    assert_eq!(events, vec!["created", "closed"], "append-only, in order");
}

#[test]
fn projects_are_isolated_until_someone_asks_for_the_merged_view() {
    let db = Db::in_memory().unwrap();
    db.ensure_project("one", "/tmp/one", "one").unwrap();
    db.ensure_project("two", "/tmp/two", "two").unwrap();
    let mut spec = new("one only");
    spec.project = Some("one".into());
    let a = db.create(&spec, 100).unwrap();
    spec.title = "two only".into();
    spec.project = Some("two".into());
    let b = db.create(&spec, 100).unwrap();
    let ids: Vec<String> = db
        .ready(None, 701)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(
        ids,
        vec![a.clone(), b.clone()],
        "cross-repo coordination is a query away"
    );
    let one_only: Vec<String> = db
        .list(Some("one"), false, None)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(one_only, vec![a]);
}

#[test]
fn ids_are_stable_and_explicit_when_the_caller_needs_idempotency() {
    let db = db_with_project();
    let id = db
        .create(
            &NewIssue {
                id: Some("demo-fixed1".into()),
                ..new("stable")
            },
            100,
        )
        .unwrap();
    assert_eq!(id, "demo-fixed1");
    db.create(
        &NewIssue {
            id: Some("demo-fixed1".into()),
            ..new("dupe")
        },
        100,
    )
    .unwrap_err();
    let generated = db.create(&new("generated"), 100).unwrap();
    assert!(
        generated.starts_with("demo-"),
        "{generated} carries the project prefix"
    );
}

#[test]
fn labels_round_trip_and_stage_labels_survive_updates() {
    let db = db_with_project();
    let id = db
        .create(
            &NewIssue {
                labels: vec!["alfalfa".into(), "alfalfa:stage:specced".into()],
                ..new("labelled")
            },
            100,
        )
        .unwrap();
    db.remove_label(&id, "alfalfa:stage:specced", "t", 150)
        .unwrap();
    db.add_label(&id, "alfalfa:stage:red", "t", 150).unwrap();
    db.update(
        &id,
        &IssuePatch {
            append_notes: Some("note".into()),
            ..Default::default()
        },
        "t",
        160,
    )
    .unwrap();
    let issue = db.get(&id).unwrap();
    assert!(issue.labels.contains(&"alfalfa:stage:red".to_string()));
    assert!(!issue.labels.contains(&"alfalfa:stage:specced".to_string()));
    assert!(issue.description.contains("note"));
}

// ---- metadata ----

#[test]
fn metadata_attaches_merges_and_deletes() {
    let db = db_with_project();
    let id = db
        .create(
            &NewIssue {
                metadata: Some(
                    serde_json::json!({"alfalfa_evidence": {"expenses": []}, "drop_me": 1}),
                ),
                external_ref: Some("gap:F-1".into()),
                ..new("receipted")
            },
            100,
        )
        .unwrap();
    let issue = db.get(&id).unwrap();
    assert_eq!(
        issue.metadata["alfalfa_evidence"]["expenses"],
        serde_json::json!([])
    );
    assert_eq!(issue.external_ref.as_deref(), Some("gap:F-1"));
    db.update(
        &id,
        &IssuePatch {
            metadata: Some(serde_json::json!({"alfalfa_evidence": {"expenses": [{"id": "e1"}]}, "drop_me": null})),
            ..Default::default()
        },
        "t",
        150,
    )
    .unwrap();
    let issue = db.get(&id).unwrap();
    assert_eq!(
        issue.metadata["alfalfa_evidence"]["expenses"][0]["id"],
        "e1"
    );
    assert!(
        issue.metadata.get("drop_me").is_none(),
        "null deletes; merge is shallow"
    );
    assert_eq!(
        issue.metadata["external_ref"], "gap:F-1",
        "create-time external_ref lands in metadata too"
    );
}

// ---- project removal ----

#[test]
fn drop_project_cascades_edges_in_both_directions() {
    let db = Db::in_memory().unwrap();
    db.ensure_project("gone", "/tmp/gone", "gone").unwrap();
    db.ensure_project("stays", "/tmp/stays", "stays").unwrap();
    let mut spec = new("doomed");
    spec.project = Some("gone".into());
    let a = db.create(&spec, 100).unwrap();
    let b = {
        let mut s = new("survivor");
        s.project = Some("stays".into());
        s.available_at = Some(0);
        db.create(&s, 100).unwrap()
    };
    db.add_dep(&b, &a, "t", 100).unwrap(); // edge INTO the doomed store
    let report = db.drop_project("gone").unwrap();
    assert_eq!(report.issues, 1);
    assert!(db.get(&a).is_err());
    assert_eq!(
        db.get(&b).unwrap().dependencies.len(),
        0,
        "dangling edge removed, survivor intact"
    );
    assert!(db.stats().unwrap().get("stays").is_some());
}

#[test]
fn drop_unknown_project_is_not_silent() {
    let db = Db::in_memory().unwrap();
    assert!(matches!(
        db.drop_project("nope"),
        Err(marbles::db::Error::NoProject(_))
    ));
}

#[test]
fn companies_get_physically_separate_stores_that_survive_reopen() {
    let root = tempfile::tempdir().unwrap();
    let stores = CompanyStores::new(root.path()).unwrap();
    let one = stores.for_company("one").unwrap();
    let two = stores.for_company("two").unwrap();
    one.ensure_project("shared", "/one/shared", "one").unwrap();
    two.ensure_project("shared", "/two/shared", "two").unwrap();
    let mut spec = new("only company one can see this");
    spec.project = Some("shared".into());
    let id = one.create(&spec, 100).unwrap();
    assert!(two.get(&id).is_err());
    assert!(root.path().join("one/marbles.db").is_file());
    assert!(root.path().join("two/marbles.db").is_file());

    drop(one);
    drop(two);
    drop(stores);
    let reopened = CompanyStores::new(root.path()).unwrap();
    assert_eq!(
        reopened.for_company("one").unwrap().get(&id).unwrap().title,
        "only company one can see this"
    );
}

#[test]
fn company_store_names_cannot_escape_the_storage_root() {
    let root = tempfile::tempdir().unwrap();
    let stores = CompanyStores::new(root.path()).unwrap();
    assert!(stores.for_company("../other").is_err());
    assert!(stores.for_company("").is_err());
}

#[test]
fn company_stores_ignore_ext4_housekeeping_directories() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("lost+found")).unwrap();

    let stores = CompanyStores::new(root.path()).unwrap();
    stores.for_company("fpl").unwrap();

    assert!(root.path().join("fpl/marbles.db").is_file());
}

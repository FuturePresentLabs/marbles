//! Contract test for the records-backed `Store` adapter. It exercises the adapter through the
//! same `Store` trait the SQLite `Db` implements, asserting the Marbles invariants survive the
//! append-only-records representation. This is a spike: it does not replace the SQLite backend.

use marbles::adapter::Adapter;
use marbles::db::{Store, now};
use marbles::types::*;

fn adapter_with_project() -> Adapter {
    let a = Adapter::in_memory().unwrap();
    a.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    a
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

#[test]
fn create_get_list_round_trip_and_project_isolation() {
    let a = adapter_with_project();
    let id = a.create(&new("first"), 100).unwrap();
    assert_eq!(a.get(&id).unwrap().title, "first");

    let mut other = new("second");
    other.project = Some("other".into());
    // ensure the other project exists first
    a.ensure_project("other", "/tmp/other", "other").unwrap();
    let id2 = a.create(&other, 100).unwrap();

    let demo: Vec<String> = a
        .list(Some("demo"), true, None)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(demo, vec![id.clone()]);
    assert_eq!(a.get(&id2).unwrap().project, "other");
}

#[test]
fn fresh_work_is_invisible_to_the_swarm_until_grace_lapses() {
    let a = adapter_with_project();
    let _ = a.create(&new("still speccing"), 100).unwrap();
    assert!(a.ready(Some("demo"), 100).unwrap().is_empty());
    assert!(a.ready(Some("demo"), 400).unwrap().is_empty());
    assert_eq!(a.ready(Some("demo"), 701).unwrap().len(), 1);
}

#[test]
fn an_edit_mid_planning_defers_the_swarm_again() {
    let a = adapter_with_project();
    let id = a.create(&new("editing"), 100).unwrap();
    assert_eq!(a.ready(Some("demo"), 701).unwrap().len(), 1);
    a.update(
        &id,
        &IssuePatch {
            append_notes: Some("not done yet".into()),
            ..Default::default()
        },
        "avery",
        900,
    )
    .unwrap();
    assert!(a.ready(Some("demo"), 901).unwrap().is_empty());
    assert_eq!(a.ready(Some("demo"), 900 + 121).unwrap().len(), 1);
}

#[test]
fn dependencies_decide_eligibility_and_a_cycle_is_refused() {
    let a = adapter_with_project();
    let parent = a.create(&new("parent"), 100).unwrap();
    let child = a.create(&new("child"), 100).unwrap();
    a.add_dep(&child, &parent, "tester", 100).unwrap();
    assert_eq!(
        a.ready(Some("demo"), 701)
            .unwrap()
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        vec![parent.as_str()]
    );
    a.close(
        &parent,
        None,
        &[Evidence::pr("https://example/pr/1")],
        "tester",
        702,
    )
    .unwrap();
    assert_eq!(a.ready(Some("demo"), 703).unwrap().len(), 1);

    let a2 = a.create(&new("a"), 100).unwrap();
    let b = a.create(&b_new(), 100).unwrap();
    a.add_dep(&a2, &b, "t", 100).unwrap();
    let err = a.add_dep(&b, &a2, "t", 100).unwrap_err();
    assert!(matches!(err, marbles::db::Error::Cycle { .. }), "{err}");
    let err = a.add_dep(&a2, &a2, "t", 100).unwrap_err();
    assert!(
        matches!(err, marbles::db::Error::Cycle { .. }),
        "self-cycle: {err}"
    );
}

fn b_new() -> NewIssue {
    let mut n = new("b");
    n.title = "b".into();
    n
}

#[test]
fn a_live_claim_cannot_be_taken_and_the_error_names_the_holder() {
    let a = adapter_with_project();
    let id = a.create(&new("race"), now()).unwrap();
    a.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "agent-a".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: None,
        },
        now(),
    )
    .unwrap();
    let err = a
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
fn an_expired_agent_lease_is_claimable_by_the_next_agent() {
    let a = adapter_with_project();
    let id = a.create(&new("stale"), now()).unwrap();
    a.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "crashed-agent".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(60),
        },
        1_000,
    )
    .unwrap();
    let receipt = a
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
fn the_sweeper_requeues_expired_agents_but_only_escalates_expired_humans() {
    let a = Adapter::in_memory()
        .unwrap()
        .with_week(marbles::time::WorkWeek {
            days: vec![1, 2, 3, 4, 5, 6, 7],
            start_hour: 0,
            end_hour: 24,
            utc_offset_minutes: 0,
        });
    a.ensure_project("demo", "/tmp/demo", "demo").unwrap();
    let agent = a.create(&new("agent work"), 100).unwrap();
    let human = a.create(&new("human work"), 100).unwrap();
    a.claim(
        &ClaimRequest {
            id: agent.clone(),
            assignee: "dead-agent".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(10),
        },
        200,
    )
    .unwrap();
    a.claim(
        &ClaimRequest {
            id: human.clone(),
            assignee: "avery".into(),
            actor_kind: ActorKind::Human,
            ttl_seconds: Some(10),
        },
        200,
    )
    .unwrap();
    let report = a.sweep(300).unwrap();
    assert_eq!(report.requeued, vec![agent.clone()]);
    assert_eq!(report.escalated, vec![format!("{human} (avery)")]);
    let held = a.get(&human).unwrap();
    assert_eq!(held.assignee.as_deref(), Some("avery"));
    assert!(a.get(&agent).unwrap().assignee.is_none());
}

#[test]
fn closing_without_evidence_is_refused() {
    let a = adapter_with_project();
    let id = a.create(&new("delivered"), 100).unwrap();
    a.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "agent-a".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: None,
        },
        101,
    )
    .unwrap();
    let err = a.close(&id, None, &[], "agent-a", 200).unwrap_err();
    assert!(matches!(err, marbles::db::Error::NoEvidence(_)), "{err}");
    a.update(
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
    assert_eq!(a.get(&id).unwrap().status, "review");
    a.close(
        &id,
        Some("merged"),
        &[Evidence::commit("abc123")],
        "avery",
        300,
    )
    .unwrap();
    assert_eq!(a.get(&id).unwrap().status, "closed");
}

#[test]
fn mutation_history_is_append_only_and_outbox_delivers() {
    let a = adapter_with_project();
    let id = a.create(&new("publish me"), 100).unwrap();

    let pending = a.pending_events(100, 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].issue_id, id);
    assert_eq!(pending[0].project, "demo");
    assert_eq!(pending[0].event, "created");

    let events: Vec<String> = a
        .history(&id)
        .unwrap()
        .into_iter()
        .map(|e| e.event)
        .collect();
    assert_eq!(events, vec!["created"]);
}

#[test]
fn supersede_closes_with_a_pointer_not_a_claim() {
    let a = adapter_with_project();
    let stale = a.create(&new("stale"), 100).unwrap();
    let fresh = a.create(&new("fresh"), 100).unwrap();
    a.supersede(&stale, &fresh, "avery", 200).unwrap();
    let issue = a.get(&stale).unwrap();
    assert_eq!(issue.status, "closed");
    assert!(
        issue
            .labels
            .iter()
            .any(|l| l == &format!("supersedes:{fresh}"))
    );
    assert_eq!(issue.evidence[0].kind, "ack");
}

#[test]
fn drop_project_cascades_edges_in_both_directions() {
    let a = Adapter::in_memory().unwrap();
    a.ensure_project("gone", "/tmp/gone", "gone").unwrap();
    a.ensure_project("stays", "/tmp/stays", "stays").unwrap();
    let mut spec = new("doomed");
    spec.project = Some("gone".into());
    let a_id = a.create(&spec, 100).unwrap();
    let mut s = new("survivor");
    s.project = Some("stays".into());
    s.available_at = Some(0);
    let b_id = a.create(&s, 100).unwrap();
    a.add_dep(&b_id, &a_id, "t", 100).unwrap();
    let report = a.drop_project("gone").unwrap();
    assert_eq!(report.issues, 1);
    assert!(a.get(&a_id).is_err());
    assert_eq!(a.get(&b_id).unwrap().dependencies.len(), 0);
    assert!(a.stats().unwrap().get("stays").is_some());
}

#[test]
fn prometheus_metrics_derive_latency_and_capacity_without_issue_id_labels() {
    let a = adapter_with_project();
    let id = a.create(&new("deliver"), 100).unwrap();
    a.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "worker".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(100),
        },
        110,
    )
    .unwrap();
    a.update(
        &id,
        &IssuePatch {
            status: Some("review".into()),
            ..Default::default()
        },
        "worker",
        130,
    )
    .unwrap();
    a.close(&id, None, &[Evidence::commit("deadbeef")], "reviewer", 150)
        .unwrap();
    let mut waiting = new("waiting");
    waiting.available_at = Some(0);
    a.create(&waiting, 120).unwrap();

    let rendered = a.prometheus_metrics(200).unwrap();
    assert!(rendered.contains("marbles_claim_latency_seconds_sum{project=\"demo\"} 10"));
    assert!(rendered.contains("marbles_cycle_time_seconds_sum{project=\"demo\"} 50"));
    assert!(rendered.contains("marbles_review_time_seconds_sum{project=\"demo\"} 20"));
    assert!(rendered.contains("marbles_queue_oldest_age_seconds{project=\"demo\"} 80"));
    assert!(
        !rendered.contains(&id),
        "issue ids must never become labels"
    );
}

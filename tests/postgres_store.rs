use marbles::db::CompanyStoreRegistry;
use marbles::postgres_migration;
use marbles::postgres_store::PostgresCompanyStores;
use marbles::types::{ActorKind, ClaimRequest, Evidence, IssuePatch, NewIssue};

fn test_url() -> Option<String> {
    std::env::var("MARBLES_TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

fn cleanup(url: &str, companies: &[String]) {
    let mut client = postgres_migration::connect(url).expect("connect for PostgreSQL cleanup");
    for company in companies {
        for table in [
            "marbles_outbound_event",
            "marbles_history",
            "marbles_dep",
            "marbles_issue",
            "marbles_project",
        ] {
            client
                .execute(
                    &format!("DELETE FROM {table} WHERE company_id=$1"),
                    &[company],
                )
                .expect("clean PostgreSQL test tenant");
        }
    }
}

#[test]
fn hosted_postgres_store_is_transactional_and_company_scoped() {
    let Some(url) = test_url() else {
        eprintln!("skipping: MARBLES_TEST_DATABASE_URL is not set");
        return;
    };
    let nonce = format!("{}-{}", std::process::id(), marbles::db::now());
    let companies = [format!("pg-a-{nonce}"), format!("pg-b-{nonce}")];
    let stores = PostgresCompanyStores::connect(&url).expect("open PostgreSQL stores");
    cleanup(&url, &companies);
    let a = stores.for_company(&companies[0]).unwrap();
    let b = stores.for_company(&companies[1]).unwrap();
    for store in [&a, &b] {
        store.ensure_project("same", "/same", "same").unwrap();
    }
    assert_eq!(a.projects().unwrap().len(), 1);

    let at = 1_800_000_000;
    let spec = NewIssue {
        id: Some("same-0001".into()),
        project: Some("same".into()),
        title: "tenant A".into(),
        available_at: Some(at),
        created_by: Some("tester".into()),
        ..Default::default()
    };
    assert_eq!(a.create(&spec, at).unwrap(), "same-0001");
    let mut other = spec.clone();
    other.title = "tenant B".into();
    assert_eq!(b.create(&other, at).unwrap(), "same-0001");
    assert_eq!(a.get("same-0001").unwrap().title, "tenant A");
    assert_eq!(b.get("same-0001").unwrap().title, "tenant B");
    assert_eq!(a.list(Some("same"), false, None).unwrap().len(), 1);
    assert_eq!(a.ready(Some("same"), at).unwrap().len(), 1);

    a.update(
        "same-0001",
        &IssuePatch {
            status: Some("in_progress".into()),
            metadata: Some(serde_json::json!({"receipt": "one"})),
            ..Default::default()
        },
        "tester",
        at + 1,
    )
    .unwrap();
    a.add_label("same-0001", "alpha", "tester", at + 2).unwrap();
    a.remove_label("same-0001", "alpha", "tester", at + 3)
        .unwrap();

    let blocker = NewIssue {
        id: Some("same-0002".into()),
        project: Some("same".into()),
        title: "blocker".into(),
        available_at: Some(at),
        ..Default::default()
    };
    a.create(&blocker, at).unwrap();
    a.add_dep("same-0001", "same-0002", "tester", at + 4)
        .unwrap();
    assert!(
        a.close(
            "same-0001",
            None,
            &[Evidence::ack("blocked test")],
            "tester",
            at + 5,
        )
        .is_err()
    );
    a.remove_dep("same-0001", "same-0002", "tester", at + 6)
        .unwrap();

    let claim = ClaimRequest {
        id: "same-0001".into(),
        assignee: "worker".into(),
        actor_kind: ActorKind::Agent,
        ttl_seconds: Some(60),
    };
    assert_eq!(a.claim(&claim, at + 7).unwrap().assignee, "worker");
    assert!(a.touch("same-0001", "worker", at + 8).unwrap() > at);
    a.release("same-0001", "worker", at + 9).unwrap();
    let expired = ClaimRequest {
        ttl_seconds: Some(1),
        ..claim
    };
    a.claim(&expired, at + 10).unwrap();
    assert_eq!(a.sweep(at + 12).unwrap().requeued, ["same-0001"]);

    let closed = a
        .close(
            "same-0001",
            Some("done"),
            &[Evidence::ack("integration test")],
            "tester",
            at + 13,
        )
        .unwrap();
    assert_eq!(closed.status, "closed");
    a.supersede("same-0002", "same-0001", "tester", at + 14)
        .unwrap();
    assert!(!a.history("same-0001").unwrap().is_empty());
    assert!(a.stats().unwrap().get("same").is_some());
    let metrics = a.prometheus_metrics(at + 15).unwrap();
    for family in [
        "marbles_issues",
        "marbles_queue_oldest_age_seconds",
        "marbles_claims_active",
        "marbles_events_total",
        "marbles_claim_latency_seconds",
        "marbles_cycle_time_seconds",
        "marbles_review_time_seconds",
    ] {
        assert!(metrics.contains(family), "missing {family}");
    }
    let pending = a.pending_events(at + 15, 100).unwrap();
    assert!(!pending.is_empty());
    a.mark_event_failed(pending[0].seq, at + 16, "retry")
        .unwrap();
    a.mark_event_delivered(pending[0].seq, at + 17).unwrap();

    // Company B remained byte-for-byte distinct despite sharing project and issue ids.
    let untouched = b.get("same-0001").unwrap();
    assert_eq!(untouched.title, "tenant B");
    assert_eq!(untouched.status, "open");
    assert!(untouched.labels.is_empty());
    cleanup(&url, &companies);
}

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

#[test]
fn sqlite_migration_preserves_binary_copy_values_and_refuses_populated_tenants() {
    let Some(url) = test_url() else {
        eprintln!("skipping migration integration: MARBLES_TEST_DATABASE_URL unset");
        return;
    };
    let company = format!("migration-{}-{}", std::process::id(), marbles::db::now());
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(&company);
    std::fs::create_dir(&directory).unwrap();
    let source = marbles::db::Db::open(directory.join("marbles.db")).unwrap();
    source.ensure_project("copy", "/copy", "copy").unwrap();
    let at = 1_800_000_000;
    let issue = source
        .create(
            &NewIssue {
                id: Some("copy-one".into()),
                project: Some("copy".into()),
                title: "UTF-8 → tabs\tand\nnewlines".into(),
                ..Default::default()
            },
            at,
        )
        .unwrap();
    source
        .update(
            &issue,
            &IssuePatch {
                metadata: Some(serde_json::json!({"text":"→\t\n\\"})),
                ..Default::default()
            },
            "actor\t→\n",
            at + 1,
        )
        .unwrap();
    let events = source.pending_events(at + 2, 64).unwrap();
    source
        .mark_event_failed(events[0].seq, at + 3, "retry\t→\n")
        .unwrap();
    // Legacy dependency-removal calls intentionally retained audit records
    // for the '_' sentinel without inventing an issue or an outbox delivery.
    source.remove_dep("_", &issue, "legacy", at + 4).unwrap();
    let reports = postgres_migration::migrate_root(root.path(), &url).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].issues, 1);
    assert_eq!(reports[0].history_events, 3);
    let stores = PostgresCompanyStores::connect(&url).unwrap();
    let target = stores.for_company(&company).unwrap();
    assert_eq!(target.get(&issue).unwrap(), source.get(&issue).unwrap());
    assert_eq!(
        serde_json::to_value(target.history(&issue).unwrap()).unwrap(),
        serde_json::to_value(source.history(&issue).unwrap()).unwrap()
    );
    assert_eq!(
        serde_json::to_value(target.history("_").unwrap()).unwrap(),
        serde_json::to_value(source.history("_").unwrap()).unwrap()
    );
    assert_eq!(
        target.pending_events(at + 4, 64).unwrap(),
        source.pending_events(at + 4, 64).unwrap()
    );
    assert!(
        postgres_migration::migrate_root(root.path(), &url)
            .unwrap_err()
            .contains("refusing to merge")
    );
    cleanup(&url, &[company]);
}

#[test]
fn failed_binary_copy_rolls_back_the_entire_tenant_import() {
    let Some(url) = test_url() else {
        eprintln!("skipping binary copy integration: MARBLES_TEST_DATABASE_URL unset");
        return;
    };
    let company = format!("rollback-{}-{}", std::process::id(), marbles::db::now());
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join(&company);
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("marbles.db");
    let source = marbles::db::Db::open(&path).unwrap();
    source.ensure_project("copy", "/copy", "copy").unwrap();
    // An undeliverable outbox entry is not an audit-only historical event.
    // PostgreSQL must reject it and roll back the earlier project import.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO outbound_event(seq,issue_id,project,occurred_at,event,next_attempt_at) VALUES(1,'missing','copy',1,'bad',1)",
        [],
    )
    .unwrap();
    assert!(postgres_migration::migrate_root(root.path(), &url).is_err());
    let mut client = postgres_migration::connect(&url).unwrap();
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM marbles_project WHERE company_id=$1",
            &[&company],
        )
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "failed COPY must not partially import the tenant");
    cleanup(&url, &[company]);
}

/// Tag the connection string so the test can terminate exactly its own backend without
/// disturbing other tests or tenants sharing the database.
fn with_application_name(url: &str, name: &str) -> String {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}application_name={name}")
    } else {
        format!("{url} application_name={name}")
    }
}

/// Terminate the store's backend server-side and wait until PostgreSQL drops it, so the next
/// statement really has to notice a dead socket instead of racing an asynchronous shutdown.
fn terminate_store_backend(admin: &mut postgres::Client, application_name: &str) {
    let rows = admin
        .query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name=$1 AND pid <> pg_backend_pid()",
            &[&application_name],
        )
        .expect("terminate the store backend");
    let terminated = rows.iter().filter(|row| row.get::<_, bool>(0)).count();
    assert_eq!(
        terminated, 1,
        "expected exactly one live store backend to terminate"
    );
    for _ in 0..100 {
        let alive: i64 = admin
            .query_one(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE application_name=$1",
                &[&application_name],
            )
            .unwrap()
            .get(0);
        if alive == 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("backend {application_name} was terminated but is still listed");
}

#[test]
fn store_reconnects_after_the_server_terminates_its_backend() {
    let Some(base_url) = test_url() else {
        eprintln!("skipping reconnect integration: MARBLES_TEST_DATABASE_URL unset");
        return;
    };
    let nonce = format!("{}-{}", std::process::id(), marbles::db::now());
    let company = format!("pg-reconnect-{nonce}");
    let application_name = format!("marbles-reconnect-{nonce}");
    let url = with_application_name(&base_url, &application_name);

    let stores = PostgresCompanyStores::connect(&url).expect("open PostgreSQL stores");
    // Put the process-long connection into use before killing it.
    stores.health_check().expect("initial health check");
    let store = stores.for_company(&company).unwrap();
    store
        .ensure_project("reconnect", "/reconnect", "rec")
        .unwrap();

    let mut admin = postgres_migration::connect(&base_url).expect("admin connection");

    // No 500, no restart: the next store operation reconnects and retries once.
    terminate_store_backend(&mut admin, &application_name);
    let projects = store
        .projects()
        .expect("store operation after backend termination must reconnect");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].0, "reconnect");

    // /healthz probes the same connection, so it must recover on its own too.
    terminate_store_backend(&mut admin, &application_name);
    stores
        .health_check()
        .expect("health_check must recover after backend termination");

    // A write on the replacement connection proves the new backend is fully usable.
    store
        .ensure_project("reconnect-b", "/reconnect-b", "recb")
        .unwrap();
    assert_eq!(store.projects().unwrap().len(), 2);

    cleanup(&url, &[company]);
}

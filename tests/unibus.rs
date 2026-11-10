//! The producer half of `soil:unibus`: which events become frames, what a frame carries,
//! and the audit detail a notification needs to be worth reading.

use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;

use marbles::config::UnibusConfig;
use marbles::db::{Db, now};
use marbles::outbox::Sink;
use marbles::types::*;
use marbles::unibus::{UnibusSink, kind_for};
use marbles::webhook::{EventEnvelope, event_id};
use unibus::{Envelope, Message};

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

fn sink_at(endpoint: &str, token: &std::path::Path) -> UnibusSink {
    UnibusSink::new(&UnibusConfig {
        hub_endpoint: endpoint.to_string(),
        token_file: token.to_path_buf(),
        installation_id: "test-install".into(),
        producer_id: "marbles".into(),
        zones: vec!["agents".into()],
    })
    .expect("sink config")
}

#[test]
fn only_the_four_actionable_events_publish() {
    assert_eq!(kind_for("created"), Some("marbles.issue.created.v1"));
    assert_eq!(kind_for("claimed"), Some("marbles.issue.claimed.v1"));
    assert_eq!(kind_for("status"), Some("marbles.issue.status.v1"));
    assert_eq!(kind_for("closed"), Some("marbles.issue.closed.v1"));

    // The trail also records these; none of them is news, and publishing them would
    // bury the four that are.
    for noise in ["heartbeat", "labels", "updated", "depends_on", "released"] {
        assert_eq!(kind_for(noise), None, "{noise} must not publish");
    }
}

#[test]
fn a_claim_carries_its_actor_detail_and_title() {
    let db = db_with_project();
    let id = db.create(&new("fix the retry loop"), 100).unwrap();
    db.claim(
        &ClaimRequest {
            id: id.clone(),
            assignee: "ajmwagar".into(),
            actor_kind: ActorKind::Agent,
            ttl_seconds: Some(600),
        },
        110,
    )
    .unwrap();

    let pending = db.pending_events(now(), 10).unwrap();
    let claim = pending
        .iter()
        .find(|event| event.event == "claimed")
        .expect("the claim is in the outbox");

    // The outbox row and the audit row share one seq; that is what makes the words
    // one lookup away instead of three denormalized columns.
    let context = db.event_context(claim.seq).unwrap().expect("audit row");
    assert_eq!(context.actor, "ajmwagar");
    assert_eq!(context.detail, "agent");
    assert_eq!(db.get(&id).unwrap().title, "fix the retry loop");
}

#[test]
fn a_published_frame_names_the_kind_and_carries_the_envelope() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake hub");
    let endpoint = listener.local_addr().expect("address").to_string();
    let mut token = tempfile::NamedTempFile::new().expect("token file");
    token.write_all(b"private-test-token").expect("write token");

    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let (registration, _) = unibus::read_frame(&mut stream).expect("register frame");
        assert_eq!(registration.message.kind, "register");
        let reply = Envelope::new(
            "test-install",
            "hub",
            None,
            "private-test-token",
            Message::new(
                "reply",
                serde_json::json!({"request_id": registration.request_id, "success": true}),
            ),
        );
        unibus::write_frame(&mut stream, &reply, &[]).expect("ack registration");
        unibus::read_frame(&mut stream).expect("published frame")
    });

    let sink = sink_at(&endpoint, token.path());
    let mut envelope = EventEnvelope::from_event("fpl", &event("store_a", 7));
    envelope.actor = "ajmwagar".into();
    envelope.detail = "agent".into();
    envelope.title = "fix the retry loop".into();
    sink.publish("marbles.issue.claimed.v1", &envelope)
        .expect("publish");

    let (frame, payload) = peer.join().expect("hub thread");
    assert!(payload.is_empty(), "the SDK publishes envelopes, not blobs");
    assert_eq!(frame.message.kind, "marbles.issue.claimed.v1");
    assert_eq!(frame.producer_id, "marbles");
    assert_eq!(
        frame.message.body.get("issue_id").and_then(|v| v.as_str()),
        Some("acme-1")
    );
    assert_eq!(
        frame.message.body.get("actor").and_then(|v| v.as_str()),
        Some("ajmwagar")
    );
    assert_eq!(
        frame.message.body.get("detail").and_then(|v| v.as_str()),
        Some("agent")
    );
    assert_eq!(
        frame.message.body.get("title").and_then(|v| v.as_str()),
        Some("fix the retry loop")
    );
    // The credential is the hub's business, never the payload's.
    assert!(
        !serde_json::to_string(&frame.message.body)
            .unwrap()
            .contains("private-test-token")
    );
}

#[test]
fn an_uninteresting_event_is_handled_without_being_published() {
    let token = tempfile::NamedTempFile::new().expect("token file");
    std::fs::write(token.path(), b"private-test-token").expect("write token");
    // Deliberately unroutable: `publishes` must answer without connecting, because it
    // runs for every pending row and the hub is allowed to be down.
    let sink = Sink::Unibus(Arc::new(sink_at("127.0.0.1:1", token.path())));
    assert!(sink.publishes("claimed"));
    assert!(!sink.publishes("heartbeat"));
    assert!(!sink.publishes("labels"));
}

#[test]
fn a_missing_token_fails_at_startup_not_at_the_first_claim() {
    let error = UnibusSink::new(&UnibusConfig {
        hub_endpoint: "127.0.0.1:18790".into(),
        token_file: "/nonexistent/marbles.token".into(),
        installation_id: "test-install".into(),
        producer_id: "marbles".into(),
        zones: vec!["agents".into()],
    })
    .err()
    .expect("a producer that cannot authenticate must not start");
    assert!(error.contains("marbles.token"), "{error}");
}

fn event(store_id: &str, seq: i64) -> marbles::db::OutboundEvent {
    marbles::db::OutboundEvent {
        seq,
        store_id: store_id.to_string(),
        issue_id: "acme-1".to_string(),
        project: "acme".to_string(),
        occurred_at: 1_700_000_000,
        event: "claimed".to_string(),
        attempts: 0,
    }
}

#[test]
fn event_identity_matches_the_webhook_channel() {
    // One envelope shape for both sinks: a consumer that can parse the webhook can
    // parse the bus, and the id is the same store-scoped `store:seq` either way.
    let envelope = EventEnvelope::from_event("fpl", &event("store_a", 7));
    assert_eq!(envelope.id, event_id("store_a", 7));
    assert_eq!(envelope.schema, "marbles.event/1");
    let wire = serde_json::to_value(&envelope).unwrap();
    assert!(
        wire.get("actor").is_none(),
        "absent means absent on the wire"
    );
}

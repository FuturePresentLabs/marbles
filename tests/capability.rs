//! Capability records are typed records in the same single-writer store: orthogonal axes,
//! revision-bound verification, structured measurements, decisions, and a readiness view.

use marbles::adapter::Adapter;
use marbles::capability::{self, CapabilityPatch, NewDecision, NewMeasurement, NewVerification};

const NOW: i64 = 1_800_000_000;
const DAY: i64 = 24 * 60 * 60;

fn adapter() -> Adapter {
    Adapter::in_memory().expect("in-memory records store")
}

fn create(store: &Adapter, id: &str) -> capability::Capability {
    capability::create(
        store,
        id,
        &CapabilityPatch {
            title: Some(format!("capability {id}")),
            owner: Some("test".into()),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("create")
}

fn verify_pass(store: &Adapter, id: &str, layer: &str, revision: &str) {
    capability::verify(
        store,
        &NewVerification {
            capability_id: id.into(),
            layer: layer.into(),
            result: "pass".into(),
            implementation_id: revision.into(),
            evidence: "test evidence".into(),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect("record verification");
}

fn layer<'a>(view: &'a capability::CapabilityView, name: &str) -> &'a capability::LayerState {
    view.layers
        .iter()
        .find(|l| l.layer == name)
        .expect("layer present")
}

#[test]
fn axes_are_independent() {
    let store = adapter();
    let capability = capability::create(
        &store,
        "transmog.native",
        &CapabilityPatch {
            title: Some("Native Transmog".into()),
            owner: Some("Transmog".into()),
            implementation: Some("in_progress".into()),
            health: Some("blocked".into()),
            lifecycle: Some("active".into()),
            commercial_readiness: Some("research".into()),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("create");

    // Blocked is health, not progress and not commercial readiness.
    assert_eq!(capability.implementation, "in_progress");
    assert_eq!(capability.health, "blocked");
    assert_eq!(capability.lifecycle, "active");
    assert_eq!(capability.commercial_readiness, "research");

    let updated = capability::update(
        &store,
        "transmog.native",
        &CapabilityPatch {
            health: Some("healthy".into()),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("update");
    assert_eq!(updated.implementation, "in_progress");
    assert_eq!(updated.health, "healthy");
}

#[test]
fn vocabulary_is_enforced_at_write_time() {
    let store = adapter();
    let error = capability::create(
        &store,
        "bad",
        &CapabilityPatch {
            title: Some("Bad".into()),
            implementation: Some("execution-verified".into()),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect_err("hyphenated value is not the vocabulary");
    assert!(error.contains("execution_verified"), "{error}");

    create(&store, "ok");
    let error = capability::verify(
        &store,
        &NewVerification {
            capability_id: "ok".into(),
            layer: "kubernetes".into(),
            result: "pass".into(),
            implementation_id: "abc".into(),
            command: "true".into(),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect_err("unknown layer rejected");
    assert!(error.contains("native"), "{error}");
}

#[test]
fn verification_is_bound_to_the_revision_it_tested() {
    let store = adapter();
    let capability = capability::create(
        &store,
        "transmog.batch",
        &CapabilityPatch {
            title: Some("Transmog batch provider".into()),
            implementation: Some("execution_verified".into()),
            implementation_id: Some("b8c93fb4".into()),
            required_layers: Some(vec!["loam".into(), "soil".into()]),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("create");
    assert_eq!(capability.implementation_id, "b8c93fb4");

    verify_pass(&store, "transmog.batch", "loam", "b8c93fb4");
    let view = capability::views(&store, NOW, 30 * DAY)
        .into_iter()
        .find(|v| v.id == "transmog.batch")
        .expect("view");
    assert_eq!(layer(&view, "loam").freshness, "fresh");
    assert!(view.gaps.contains(&"soil".to_string()), "soil is a gap");

    // A new revision must not inherit the old pass.
    capability::update(
        &store,
        "transmog.batch",
        &CapabilityPatch {
            implementation_id: Some("deadbeef".into()),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("advance revision");

    let view = capability::views(&store, NOW, 30 * DAY)
        .into_iter()
        .find(|v| v.id == "transmog.batch")
        .expect("view");
    assert_eq!(layer(&view, "loam").status, "pass");
    assert_eq!(layer(&view, "loam").freshness, "revision_mismatch");
    assert!(view.gaps.contains(&"loam".to_string()));
    assert!(!view.technically_ready);

    // Re-observing the new revision restores it, and the old observation is still in the trail.
    verify_pass(&store, "transmog.batch", "loam", "deadbeef");
    let view = capability::views(&store, NOW, 30 * DAY)
        .into_iter()
        .find(|v| v.id == "transmog.batch")
        .expect("view");
    assert_eq!(layer(&view, "loam").freshness, "fresh");
    assert!(!view.gaps.contains(&"loam".to_string()));
}

#[test]
fn a_pass_without_evidence_is_refused() {
    let store = adapter();
    create(&store, "x");
    let error = capability::verify(
        &store,
        &NewVerification {
            capability_id: "x".into(),
            layer: "native".into(),
            result: "pass".into(),
            implementation_id: "abc".into(),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect_err("claim, not observation");
    assert!(error.contains("claim"), "{error}");

    // A failure is allowed to be bare: the absence of a way to reproduce it is itself evidence.
    capability::verify(
        &store,
        &NewVerification {
            capability_id: "x".into(),
            layer: "native".into(),
            result: "fail".into(),
            implementation_id: "abc".into(),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect("record a bare failure");
}

#[test]
fn age_makes_evidence_stale() {
    let store = adapter();
    create(&store, "x");
    capability::verify(
        &store,
        &NewVerification {
            capability_id: "x".into(),
            layer: "loam".into(),
            result: "pass".into(),
            implementation_id: String::new(),
            command: "cargo test".into(),
            observed_at: Some(NOW - 40 * DAY),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect_err("an empty implementation id is not a revision");

    // The capability's current revision is empty by default, so name it explicitly.
    capability::update(
        &store,
        "x",
        &CapabilityPatch {
            implementation_id: Some("rev".into()),
            required_layers: Some(vec!["loam".into()]),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("update");

    capability::verify(
        &store,
        &NewVerification {
            capability_id: "x".into(),
            layer: "loam".into(),
            result: "pass".into(),
            implementation_id: "rev".into(),
            command: "cargo test".into(),
            observed_at: Some(NOW - 40 * DAY),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect("record old pass");

    let view = capability::views(&store, NOW, 30 * DAY)
        .into_iter()
        .next()
        .expect("view");
    assert_eq!(layer(&view, "loam").freshness, "stale");
    assert!(view.gaps.contains(&"loam".to_string()));

    let stale = capability::stale(&store, NOW, 30 * DAY);
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0]["freshness"], "stale");
}

#[test]
fn measurements_preserve_shape_and_baseline_uses_latest() {
    let store = adapter();
    create(&store, "transmog.batch");
    let measurement = capability::measure(
        &store,
        &NewMeasurement {
            capability_id: "transmog.batch".into(),
            metric: "dfm_seconds".into(),
            value: 3.887,
            unit: "s".into(),
            workload: "DFM, 369684-byte artifact".into(),
            profile: "debug-native".into(),
            samples: Some(1),
            domain_outcome: "analysis returned".into(),
            limitations: "single reported observation".into(),
            observed_at: Some(NOW - DAY),
            ..NewMeasurement::default()
        },
        NOW,
    )
    .expect("measure");
    assert_eq!(measurement.unit, "s");
    assert_eq!(measurement.samples, 1);
    assert_eq!(measurement.workload, "DFM, 369684-byte artifact");

    capability::measure(
        &store,
        &NewMeasurement {
            capability_id: "transmog.batch".into(),
            metric: "dfm_seconds".into(),
            value: 3.87,
            unit: "s".into(),
            workload: "DFM regression".into(),
            profile: "debug-native".into(),
            samples: Some(1),
            observed_at: Some(NOW),
            ..NewMeasurement::default()
        },
        NOW,
    )
    .expect("measure later");

    let baseline = capability::baseline(&store, Some("dfm_seconds"));
    assert_eq!(baseline.len(), 1, "one latest value per capability+metric");
    assert_eq!(baseline[0].value, 3.87);
    assert_eq!(
        capability::measurements(&store).len(),
        2,
        "history retained"
    );
}

#[test]
fn decisions_supersede_and_are_listed_on_the_capability() {
    let store = adapter();
    create(&store, "soil.host");
    let first = capability::decide(
        &store,
        &NewDecision {
            id: "DEC-010".into(),
            title: "Soil owns the shared crates".into(),
            status: "proposed".into(),
            rationale: "divergent copies would drift".into(),
            owner: "principal engineer".into(),
            affects: vec!["soil.host".into()],
            ..NewDecision::default()
        },
        NOW,
    )
    .expect("first decision");
    assert_eq!(first.status, "proposed");

    let second = capability::decide(
        &store,
        &NewDecision {
            id: "DEC-014".into(),
            title: "Loam owns the provider host".into(),
            status: "accepted".into(),
            rationale: "resolution and grants already live there".into(),
            affects: vec!["soil.host".into()],
            supersedes: vec!["DEC-010".into()],
            ..NewDecision::default()
        },
        NOW + 1,
    )
    .expect("superseding decision");
    assert_eq!(second.status, "accepted");

    let decisions = capability::decisions(&store);
    let old = decisions
        .iter()
        .find(|d| d.id == "DEC-010")
        .expect("old decision kept");
    assert_eq!(old.status, "superseded");
    assert_eq!(old.superseded_by.as_deref(), Some("DEC-014"));

    let shown = capability::show(&store, "soil.host", NOW, 30 * DAY).expect("show");
    assert_eq!(shown["decisions"].as_array().expect("array").len(), 2);

    let error = capability::decide(
        &store,
        &NewDecision {
            id: "DEC-015".into(),
            title: "Affects nothing real".into(),
            status: "accepted".into(),
            rationale: "typo".into(),
            affects: vec!["no.such.capability".into()],
            ..NewDecision::default()
        },
        NOW,
    )
    .expect_err("unknown affected capability");
    assert!(error.contains("no.such.capability"), "{error}");
}

#[test]
fn platform_gaps_do_not_block_commercial_pilots() {
    let store = adapter();
    capability::create(
        &store,
        "transmog.batch",
        &CapabilityPatch {
            title: Some("Transmog batch".into()),
            lifecycle: Some("active".into()),
            implementation: Some("execution_verified".into()),
            implementation_id: Some("rev".into()),
            // Soil is optional platform integration; Loam execution is the commercial seam.
            required_layers: Some(vec!["soil".into()]),
            commercial_gates: Some(vec!["loam".into()]),
            commercial_readiness: Some("demo_ready".into()),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("create");
    verify_pass(&store, "transmog.batch", "loam", "rev");

    let view = capability::views(&store, NOW, 30 * DAY)
        .into_iter()
        .next()
        .expect("view");
    assert_eq!(view.platform_gaps, vec!["soil"]);
    assert!(view.commercial_gaps.is_empty());
    assert!(view.commercial_ready);
    assert!(!view.technically_ready);
    assert_eq!(
        view.platform_blockers,
        vec!["platform gate soil: missing (unknown)"]
    );
    assert!(
        capability::pilot_blockers(&store, NOW, 30 * DAY).is_empty(),
        "a missing optional platform integration must not block a viable pilot"
    );

    // A commercial gate does block, and the same query says why.
    capability::update(
        &store,
        "transmog.batch",
        &CapabilityPatch {
            commercial_gates: Some(vec!["loam".into(), "commercial".into()]),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("add a commercial gate");
    let blockers = capability::pilot_blockers(&store, NOW, 30 * DAY);
    assert_eq!(blockers.len(), 1);
    assert_eq!(
        blockers[0]["commercial_gaps"],
        serde_json::json!(["commercial"])
    );
    assert_eq!(blockers[0]["platform_gaps"], serde_json::json!(["soil"]));
    assert_eq!(
        blockers[0]["blockers"],
        serde_json::json!(["commercial gate commercial: missing (unknown)"])
    );
}

#[test]
fn reported_and_reproduced_observations_are_distinguished() {
    let store = adapter();
    create(&store, "x");
    capability::update(
        &store,
        "x",
        &CapabilityPatch {
            implementation_id: Some("rev".into()),
            ..CapabilityPatch::default()
        },
        NOW,
    )
    .expect("update");

    capability::verify(
        &store,
        &NewVerification {
            capability_id: "x".into(),
            layer: "loam".into(),
            result: "pass".into(),
            implementation_id: "rev".into(),
            evidence: "commit abc".into(),
            reproduction: Some("reported".into()),
            acceptance: "provider returns identity".into(),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect("reported");
    capability::verify(
        &store,
        &NewVerification {
            capability_id: "x".into(),
            layer: "soil".into(),
            result: "pass".into(),
            implementation_id: "rev".into(),
            command: "cargo test -p loam soil_providers".into(),
            reproduction: Some("reproduced".into()),
            acceptance: "identity reaches Soil".into(),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect("reproduced");

    let verified = capability::verified(&store, None, NOW, 30 * DAY).unwrap();
    let by_layer = |layer: &str| {
        verified
            .iter()
            .find(|v| v["layer"] == layer)
            .expect("observation")
    };
    assert_eq!(by_layer("loam")["reproduction"], "reported");
    assert_eq!(by_layer("loam")["acceptance"], "provider returns identity");
    assert_eq!(by_layer("soil")["reproduction"], "reproduced");

    // Omitting the flag is the weaker claim by default.
    capability::verify(
        &store,
        &NewVerification {
            capability_id: "x".into(),
            layer: "native".into(),
            result: "pass".into(),
            implementation_id: "rev".into(),
            command: "true".into(),
            ..NewVerification::default()
        },
        NOW,
    )
    .expect("default reproduction");
    let native = capability::verified(&store, Some("native"), NOW, 30 * DAY).unwrap();
    assert_eq!(native[0]["reproduction"], "reported");
}

#[test]
fn capability_records_survive_reopen_of_the_append_only_trail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("marbles.trail");
    {
        let store = Adapter::open_file(path.clone()).expect("open durable");
        create(&store, "studio.editor");
        capability::measure(
            &store,
            &NewMeasurement {
                capability_id: "studio.editor".into(),
                metric: "ttwc_seconds".into(),
                value: 3600.0,
                unit: "s".into(),
                ..NewMeasurement::default()
            },
            NOW,
        )
        .expect("measure");
    }
    let store = Adapter::open_file(path).expect("reopen");
    assert_eq!(
        capability::get(&store, "studio.editor").unwrap().id,
        "studio.editor"
    );
    assert_eq!(capability::measurements(&store).len(), 1);
}

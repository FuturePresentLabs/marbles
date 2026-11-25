//! Seed a capability ledger from actual implementation evidence, then answer the six readiness
//! questions through the same `mb capability` command path the Spore uses.
//!
//! Run it with:
//!
//! ```sh
//! cargo run --example capability_ledger
//! ```
//!
//! The observations name commits that exist in the local checkouts (`7ebce24d`, `b8c93fb4`,
//! `f7313e5`, `7dcceff`, `8795eb1`, `ed385ff`). Where the only evidence is the integration ledger
//! rather than a commit present in the checkout (`b33b2c3`), the record says so in `limitations`
//! instead of inventing a re-run.
//!
//! This is a demo against a throwaway append-only trail. It is not a second store: the same
//! records work in the fleet's `soil:records` book.

use std::collections::BTreeMap;
use std::path::PathBuf;

use marbles::adapter::Adapter;
use marbles::capability::{self, CapabilityPatch, NewDecision, NewMeasurement, NewVerification};
use marbles::{records_cli, records_exec};
use serde_json::Value;

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("marbles-capability-{}.trail", std::process::id()))
        });
    let _ = std::fs::remove_file(&path);
    let store = Adapter::open_file(path.clone()).expect("open trail");
    let now = marbles::store::now();

    seed_capabilities(&store, now);
    seed_verifications(&store, now);
    seed_measurements(&store, now);
    seed_decisions(&store, now);

    println!("# capability ledger in {}", path.display());
    println!("\n## what capabilities exist?");
    ask(&store, now, &["capability", "list"]);

    println!("\n## which revisions have verified Loam execution?");
    ask(&store, now, &["capability", "verified", "--layer", "loam"]);

    println!("\n## which capabilities lack Soil verification?");
    ask(&store, now, &["capability", "gaps", "--layer", "soil"]);

    println!("\n## which verification evidence is stale?");
    ask(&store, now, &["capability", "stale"]);

    println!("\n## what blocks a commercial pilot?");
    ask(&store, now, &["capability", "pilot-blockers"]);

    println!("\n## which measurements establish the current baseline?");
    ask(&store, now, &["capability", "baseline"]);
}

fn ask(store: &Adapter, now: i64, argv: &[&str]) {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let invocation = records_cli::parse(&argv, &BTreeMap::new()).expect("parse");
    let answer = records_exec::execute(store, &invocation, now).expect("execute");
    println!("{}", serde_json::to_string_pretty(&answer).unwrap());
    let _: &Value = &answer;
}

#[allow(clippy::too_many_arguments)] // a seed fixture reads better as one row per line
fn capability(
    store: &Adapter,
    now: i64,
    id: &str,
    title: &str,
    owner: &str,
    project: &str,
    implementation: &str,
    revision: &str,
    readiness: &str,
    required_layers: &[&str],
) {
    capability::create(
        store,
        id,
        &CapabilityPatch {
            title: Some(title.into()),
            owner: Some(owner.into()),
            project: Some(project.into()),
            implementation: Some(implementation.into()),
            implementation_id: Some(revision.into()),
            health: Some("healthy".into()),
            lifecycle: Some("active".into()),
            commercial_readiness: Some(readiness.into()),
            required_layers: Some(required_layers.iter().map(|l| l.to_string()).collect()),
            ..CapabilityPatch::default()
        },
        now,
    )
    .expect("create capability");
}

fn seed_capabilities(store: &Adapter, now: i64) {
    capability(
        store,
        now,
        "transmog.native",
        "Native Transmog",
        "Transmog",
        "transmog",
        "execution_verified",
        "b33b2c3",
        "internal_validated",
        &["native"],
    );
    capability(
        store,
        now,
        "transmog.batch-provider",
        "Transmog batch provider",
        "Transmog",
        "transmog",
        "execution_verified",
        "b8c93fb4",
        "demo_ready",
        &["sidecar", "loam", "soil"],
    );
    capability(
        store,
        now,
        "loam.provider-host",
        "Loam provider host for Soil",
        "Loam",
        "loam",
        "execution_verified",
        "f7313e5",
        "internal_validated",
        &["soil"],
    );
    capability(
        store,
        now,
        "panopticon.provider",
        "Panopticon geospatial provider",
        "Agent 1 / Panopticon",
        "panopticon",
        "implemented",
        "7dcceff",
        "internal_prototype",
        &["native", "loam", "soil"],
    );
    capability(
        store,
        now,
        "twenty.provider",
        "Twenty CRM provider",
        "Agent 2 / Twenty",
        "fpl-twenty",
        "implemented",
        "8795eb1",
        "internal_prototype",
        &["loam", "soil"],
    );
    capability(
        store,
        now,
        "studio.editor",
        "Studio visual editor",
        "Agent 3 / Studio",
        "studio",
        "implemented",
        "ed385ff",
        "internal_prototype",
        &["native"],
    );

    // The contract is a capability too, but it gates no execution layer yet.
    capability::create(
        store,
        "transmog.contract",
        &CapabilityPatch {
            title: Some("fpl:transmog contract".into()),
            owner: Some("Transmog".into()),
            project: Some("transmog".into()),
            contract_version: Some("0.1.0".into()),
            contract_maturity: Some("draft".into()),
            implementation: Some("implemented".into()),
            implementation_id: Some("7ebce24d".into()),
            health: Some("healthy".into()),
            lifecycle: Some("active".into()),
            commercial_readiness: Some("research".into()),
            ..CapabilityPatch::default()
        },
        now,
    )
    .expect("create contract capability");
}

#[allow(clippy::too_many_arguments)] // one observation per call, fields named at the call site
fn verify(
    store: &Adapter,
    now: i64,
    capability_id: &str,
    layer: &str,
    result: &str,
    revision: &str,
    workload: &str,
    command: &str,
    evidence: &str,
    limitations: &str,
    observed_at: i64,
) {
    capability::verify(
        store,
        &NewVerification {
            capability_id: capability_id.into(),
            layer: layer.into(),
            result: result.into(),
            implementation_id: revision.into(),
            workload: workload.into(),
            command: command.into(),
            evidence: evidence.into(),
            limitations: limitations.into(),
            observed_at: Some(observed_at),
            ..NewVerification::default()
        },
        now,
    )
    .expect("record verification");
}

fn seed_verifications(store: &Adapter, now: i64) {
    // The native baseline is the ledger's report. The commit is not in this checkout, so the
    // record carries that limitation rather than pretending it was re-run.
    verify(
        store,
        now,
        "transmog.native",
        "native",
        "pass",
        "b33b2c3",
        "DFM",
        "transmog analyze",
        "integration ledger 2026-11-25; commit b33b2c3 not present in the local checkout",
        "reported, not independently audited",
        now,
    );

    verify(
        store,
        now,
        "transmog.batch-provider",
        "sidecar",
        "pass",
        "b8c93fb4",
        "5 stateless ops over a 361 KB STL",
        "transmog-batch sidecar",
        "commit b8c93fb4; plugins/transmog-batch",
        "reported by the commit and its README, not re-run here",
        now,
    );
    verify(
        store,
        now,
        "transmog.batch-provider",
        "loam",
        "pass",
        "b8c93fb4",
        "DFM, 369684-byte artifact",
        "loam provider call fpl:transmog analyze",
        "commit b8c93fb4; integration ledger 2026-11-25",
        "debug-native profile; one reported observation",
        now,
    );
    verify(
        store,
        now,
        "transmog.batch-provider",
        "loam",
        "pass",
        "b8c93fb4",
        "quote, Taig profile",
        "loam provider call fpl:transmog quote",
        "commit b8c93fb4; integration ledger 2026-11-25",
        "commercial quote validity not established",
        now - 1,
    );

    // The provider host landed after the ledger's baseline; these older observations are kept so
    // the stale view can show why an earlier pass does not apply to the current revision.
    verify(
        store,
        now,
        "loam.provider-host",
        "loam",
        "pass",
        "c4fc171",
        "provider call by interface and function name",
        "cargo test -p loam provider_host",
        "commit c4fc171",
        "superseded by f7313e5; not the current revision",
        now,
    );
    verify(
        store,
        now,
        "loam.provider-host",
        "loam",
        "pass",
        "afd8dd0",
        "composed runtime artefact identity",
        "cargo test -p loam runtime",
        "commit afd8dd0",
        "superseded by f7313e5; not the current revision",
        now,
    );
    verify(
        store,
        now,
        "loam.provider-host",
        "soil",
        "pass",
        "f7313e5",
        "Soil defun calling (host-call \"fpl:transmog\" \"identity\")",
        "cargo test -p loam soil_providers",
        "commit f7313e5",
        "proves the identity call and marshalling; not a DFM or quote call through Soil",
        now,
    );

    verify(
        store,
        now,
        "panopticon.provider",
        "native",
        "pass",
        "7dcceff",
        "OSM index, Nominatim hydration, resolve",
        "panopticon sidecar",
        "commit 7dcceff; provider/docs/DELIVERY.md",
        "reported by the provider delivery doc, not re-run here",
        now,
    );
    verify(
        store,
        now,
        "panopticon.provider",
        "loam",
        "pass",
        "7dcceff",
        "real Loam consumer integration tests",
        "cargo test -p panopticon-provider-consumer",
        "commit 7dcceff; provider/consumer/tests/provider.rs",
        "Soil orchestration example is still labelled unverified",
        now,
    );

    verify(
        store,
        now,
        "twenty.provider",
        "loam",
        "pass",
        "8795eb1",
        "gtm consumer imports fpl:twenty/crm",
        "cargo test -p fpl-twenty-consumer",
        "commit 8795eb1; consumer/",
        "adapter and sidecar exercised against deployment config; no Soil path",
        now,
    );

    verify(
        store,
        now,
        "studio.editor",
        "native",
        "pass",
        "ed385ff",
        "standalone Studio with a pinned Coherence project",
        "studio build",
        "commit ed385ff",
        "no Soil or capability-view integration yet",
        now,
    );
}

fn seed_measurements(store: &Adapter, now: i64) {
    let measure = |capability_id: &str,
                   metric: &str,
                   value: f64,
                   unit: &str,
                   workload: &str,
                   profile: &str,
                   outcome: &str,
                   limitations: &str,
                   observed_at: i64| {
        capability::measure(
            store,
            &NewMeasurement {
                capability_id: capability_id.into(),
                metric: metric.into(),
                value,
                unit: unit.into(),
                workload: workload.into(),
                profile: profile.into(),
                samples: Some(1),
                domain_outcome: outcome.into(),
                limitations: limitations.into(),
                observed_at: Some(observed_at),
                ..NewMeasurement::default()
            },
            now,
        )
        .expect("record measurement");
    };

    let reported = "single reported observation; not a statistically characterised distribution";
    measure(
        "transmog.native",
        "dfm_seconds",
        2.38,
        "s",
        "DFM",
        "debug-native",
        "analysis returned",
        reported,
        now,
    );
    measure(
        "transmog.batch-provider",
        "dfm_seconds",
        3.887,
        "s",
        "DFM, 369684-byte artifact",
        "debug-native",
        "analysis returned",
        reported,
        now,
    );
    measure(
        "transmog.batch-provider",
        "dfm_total_seconds",
        4.089,
        "s",
        "DFM, 369684-byte artifact",
        "debug-native",
        "analysis returned",
        reported,
        now,
    );
    measure(
        "transmog.batch-provider",
        "quote_seconds",
        3.595,
        "s",
        "quote, Taig profile",
        "debug-native",
        "execution verified; commercial quote validity not established",
        reported,
        now,
    );
    measure(
        "transmog.batch-provider",
        "quote_total_seconds",
        3.764,
        "s",
        "quote, Taig profile",
        "debug-native",
        "execution verified; commercial quote validity not established",
        reported,
        now,
    );
}

fn seed_decisions(store: &Adapter, now: i64) {
    let decide = |id: &str,
                  title: &str,
                  status: &str,
                  rationale: &str,
                  affects: &[&str],
                  supersedes: &[&str]| {
        capability::decide(
            store,
            &NewDecision {
                id: id.into(),
                title: title.into(),
                status: status.into(),
                rationale: rationale.into(),
                owner: "principal engineer".into(),
                affects: affects.iter().map(|a| a.to_string()).collect(),
                supersedes: supersedes.iter().map(|s| s.to_string()).collect(),
                decided_at: Some(now),
                ..NewDecision::default()
            },
            now,
        )
        .expect("record decision");
    };

    decide(
        "DEC-002",
        "Native batch provider first",
        "accepted",
        "Use the stateless Transmog CLI behind a Loam provider; defer live modelling and portable lowering.",
        &["transmog.batch-provider"],
        &[],
    );
    decide(
        "DEC-004",
        "No fake providers",
        "accepted",
        "Missing runtime capabilities fail explicitly; a stub must not conceal an integration failure.",
        &["transmog.batch-provider", "loam.provider-host"],
        &[],
    );
    decide(
        "DEC-005",
        "Soil is authoritative workflow source",
        "accepted",
        "Studio's node editor is a view of Soil, not a second workflow language.",
        &["studio.editor"],
        &[],
    );
    decide(
        "DEC-010",
        "Soil crate ownership is unresolved",
        "proposed",
        "Divergent shared crates between Soil and Loam must not produce competing authoritative implementations.",
        &["loam.provider-host"],
        &[],
    );
    // Evidence for the supersession: f7313e5 enforces crate canon and lands the generic provider
    // host in Loam rather than adding a parallel Soil implementation.
    decide(
        "DEC-014",
        "Loam owns the generic provider host; Soil consumes it",
        "accepted",
        "The host needs the registry and grants, which already live in Loam. f7313e5 maps Value <-> WIT there and enforces crate canon, so DEC-010's unresolved ownership is settled by implementation.",
        &["loam.provider-host"],
        &["DEC-010"],
    );
}

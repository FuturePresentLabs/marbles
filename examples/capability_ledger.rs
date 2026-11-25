//! Seed a capability ledger from actual implementation evidence, then answer the readiness
//! questions and the orchestrator inventory through the same `mb` command path the Spore uses.
//!
//! ```sh
//! cargo run --example capability_ledger            # throwaway trail
//! cargo run --example capability_ledger /tmp/book  # keep it
//! ```
//!
//! The observations name commits that exist in the local checkouts (`86cfdc8`, `6cc857f`,
//! `7dcceff`, `8795eb1`, `b8c93fb4`, `f7313e5`, `ed385ff`). Where the only evidence is a report
//! or the integration ledger rather than a command re-run here (`b33b2c3`), the record says
//! `reproduction: reported` and names the limitation instead of inventing a reproduction.
//!
//! This is a demo against an append-only trail. It is not a second store: the same records work
//! in the fleet's `soil:records` book.

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

    println!("\n## one read-only call for an orchestrator (inventory)");
    let inventory = ask(&store, now, &["inventory"]);
    println!(
        "counts: {}",
        serde_json::to_string_pretty(&inventory["counts"]).unwrap()
    );
    println!(
        "commercial blockers: {}",
        serde_json::to_string_pretty(
            &inventory["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["commercial_ready"] == false)
                .map(|c| serde_json::json!({
                    "id": c["id"],
                    "commercial_gaps": c["commercial_gaps"],
                    "platform_gaps": c["platform_gaps"],
                }))
                .collect::<Vec<_>>()
        )
        .unwrap()
    );
}

fn ask(store: &Adapter, now: i64, argv: &[&str]) -> Value {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let invocation = records_cli::parse(&argv, &BTreeMap::new()).expect("parse");
    let answer = records_exec::execute(store, &invocation, now).expect("execute");
    println!("{}", serde_json::to_string_pretty(&answer).unwrap());
    answer
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
    platform_gates: &[&str],
    commercial_gates: &[&str],
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
            required_layers: Some(platform_gates.iter().map(|l| l.to_string()).collect()),
            commercial_gates: Some(commercial_gates.iter().map(|l| l.to_string()).collect()),
            ..CapabilityPatch::default()
        },
        now,
    )
    .expect("create capability");
}

fn seed_capabilities(store: &Adapter, now: i64) {
    // The native baseline is the ledger's report; the commit is not in this checkout.
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
        &[],
        &["native"],
    );
    // Soil is optional platform integration; Loam execution is the commercial seam.
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
        &["soil"],
        &["loam"],
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
        &[],
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
        &["soil"],
        &["native"],
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
        &["soil"],
        &["loam"],
    );
    // The relay's live delivery is the commercial fact; Soil is not on that path.
    capability(
        store,
        now,
        "twenty.webhook-relay",
        "Twenty webhook relay",
        "Agent 2 / Twenty",
        "fpl-twenty",
        "execution_verified",
        "6cc857f",
        "internal_validated",
        &["soil"],
        &["native"],
    );
    // The Factory sells websites. A Studio inventory view is optional platform work.
    capability(
        store,
        now,
        "website-factory.proposals",
        "Website Factory proposals",
        "Agent 4 / Website Factory",
        "website-factory",
        "execution_verified",
        "86cfdc8",
        "demo_ready",
        &["studio"],
        &["native", "commercial"],
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
        &[],
    );
    // The RFQ vertical is real, but the PDF is internal-test and no customer has accepted it.
    capability(
        store,
        now,
        "quoting.waterjet-rfq",
        "Waterjet RFQ-to-quote vertical",
        "Agent 5 / Quoting",
        "quotron3000",
        "execution_verified",
        "28b96fb",
        "demo_ready",
        &["studio"],
        &["native", "commercial"],
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
    acceptance: &str,
    limitations: &str,
    reproduction: &str,
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
            acceptance: acceptance.into(),
            limitations: limitations.into(),
            reproduction: Some(reproduction.into()),
            observed_at: Some(observed_at),
            ..NewVerification::default()
        },
        now,
    )
    .expect("record verification");
}

fn seed_verifications(store: &Adapter, now: i64) {
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
        "native analysis returns for the DFM workload",
        "reported, not independently audited",
        "reported",
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
        "sidecar answers the stateless operations",
        "reported by the commit and its README, not re-run here",
        "reported",
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
        "DFM analysis returns; commercial quote validity is not asserted",
        "debug-native profile; one reported observation",
        "reported",
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
        "quote call executes; commercial validity not established",
        "one reported observation",
        "reported",
        now - 1,
    );

    // Older revisions stay in the trail and are visible as revision_mismatch, never inherited.
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
        "provider host resolves and calls by name",
        "superseded by f7313e5; not the current revision",
        "reported",
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
        "composed artefact is verified, not trusted",
        "superseded by f7313e5; not the current revision",
        "reported",
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
        "identity reaches Soil; an ungranted interface is refused by name",
        "proves the identity call and marshalling; not a DFM or quote call through Soil",
        "reported",
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
        "discovery and hydration answer from the NZ index",
        "reported by the provider delivery doc, not re-run here",
        "reported",
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
        "consumer imports fpl:panopticon and completes its calls",
        "Soil orchestration example is still labelled unverified",
        "reported",
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
        "typed CRM calls succeed through the adapter and sidecar",
        "no Soil path; deployment config external",
        "reported",
        now,
    );

    verify(
        store,
        now,
        "twenty.webhook-relay",
        "native",
        "pass",
        "6cc857f",
        "live Twenty company.created/updated -> relay -> Confluence -> Unibus -> Elmers",
        "curl the relay with valid, invalid, and duplicate signatures",
        "commit 6cc857f; docs/events.md evidence run",
        "invalid signature 401; duplicate 202 duplicate=true with no second consumer run; retry after a 503 delivered exactly one event",
        "reported by docs/events.md, not re-run here; relay is on a personal Cloudflare account; Confluence records the verifier family as github",
        "reported",
        now,
    );

    verify(
        store,
        now,
        "website-factory.proposals",
        "native",
        "pass",
        "86cfdc8",
        "ten NZ manufacturer proposals, build + QA + audit",
        "node src/factory.mjs profiles/<id>.json <archetype>",
        "commit 86cfdc8; out/internal/buckley-systems/qa.json",
        "qa.ok true, 21 checks, 0 failed; signed preview links; sales package produced",
        "reported by repo artifacts; not re-run here; no business has been contacted",
        "reported",
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
        "npm test",
        "commit ed385ff",
        "editor serves from the extracted repo",
        "no Soil or capability-view integration yet",
        "reported",
        now,
    );

    verify(
        store,
        now,
        "quoting.waterjet-rfq",
        "native",
        "pass",
        "28b96fb",
        "DXF bracket waterjet RFQ, Mild Steel 0.25 in, qty 2",
        "QUOTRON_BIN=.tools/quotron npm test",
        "commit 28b96fb; docs/RFQ-VERTICAL.md; docs/artifacts/internal-test-quotation.pdf",
        "price total 79.801 USD, calibration true, factory status eligible, internal-test PDF recorded in the outbox",
        "internal-test PDF only; shop approval required; never sent to a customer; no customer acceptance recorded",
        "reported",
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

    // Actual timings from out/internal/buckley-systems/report.json at 86cfdc8.
    let factory = "one report.json artifact; Node 26 local build";
    measure(
        "website-factory.proposals",
        "proposal_build_ms",
        36.0,
        "ms",
        "buckley-systems, automate preview",
        "node-26",
        "site rendered",
        factory,
        now,
    );
    measure(
        "website-factory.proposals",
        "proposal_qa_ms",
        1.0,
        "ms",
        "buckley-systems, automate preview",
        "node-26",
        "21 checks, 0 failed",
        factory,
        now,
    );
    measure(
        "website-factory.proposals",
        "proposal_audit_ms",
        13949.0,
        "ms",
        "buckley-systems, automate preview",
        "node-26",
        "existing-site audit completed (14 findings)",
        factory,
        now,
    );
    measure(
        "website-factory.proposals",
        "proposal_total_ms",
        13987.0,
        "ms",
        "buckley-systems, automate preview",
        "node-26",
        "proposal packaged",
        factory,
        now,
    );

    // Actual observed run from docs/RFQ-VERTICAL.md at 28b96fb.
    let quoting = "one observed run; internal-test quotation; rate revision pinned and verified";
    measure(
        "quoting.waterjet-rfq",
        "quote_total_usd",
        79.801,
        "usd",
        "bracket.dxf, Mild Steel 0.25 in, qty 2",
        "quotron 0.1.0, pinned rate revision",
        "factory status eligible; internal-test PDF",
        quoting,
        now,
    );
    measure(
        "quoting.waterjet-rfq",
        "cut_path_in",
        23.169,
        "in",
        "bracket.dxf",
        "quotron 0.1.0",
        "measurement reported by transmog-estimate via Quotron",
        quoting,
        now,
    );
    measure(
        "quoting.waterjet-rfq",
        "machine_time_h",
        0.672,
        "h",
        "bracket.dxf, waterjet, qty 2",
        "quotron 0.1.0",
        "machine time priced",
        quoting,
        now,
    );
    measure(
        "quoting.waterjet-rfq",
        "pierces",
        5.0,
        "count",
        "bracket.dxf",
        "quotron 0.1.0",
        "pierce count priced",
        quoting,
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
    decide(
        "DEC-014",
        "Loam owns the generic provider host; Soil consumes it",
        "accepted",
        "The host needs the registry and grants, which already live in Loam. f7313e5 maps Value <-> WIT there and enforces crate canon.",
        &["loam.provider-host"],
        &["DEC-010"],
    );
    decide(
        "DEC-015",
        "Optional platform integrations do not gate commercial pilots",
        "accepted",
        "A missing Soil or Studio path is reported as a platform gap. It blocks platform readiness, not a customer pilot whose commercial gates are met.",
        &[
            "transmog.batch-provider",
            "twenty.webhook-relay",
            "website-factory.proposals",
        ],
        &[],
    );
    decide(
        "DEC-016",
        "Reported and reproduced evidence are recorded separately",
        "accepted",
        "A commit message, delivery log, or document is a reported observation. Only a re-run is reproduced. Both are useful; collapsing them loses the distinction.",
        &[
            "transmog.native",
            "twenty.webhook-relay",
            "website-factory.proposals",
        ],
        &[],
    );
    decide(
        "DEC-017",
        "Quotron owns price policy; Studio does not reimplement rates",
        "accepted",
        "Studio calls the existing Quotron waterjet CLI. Missing or changed rate records block; no price is invented. The internal-test PDF is not a customer quote.",
        &["quoting.waterjet-rfq"],
        &[],
    );
}

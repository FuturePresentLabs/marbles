#!/bin/sh
# Seed the authorized Marbles records book with the capability ledger.
#
# This is a live write script. It refuses to run twice unless --force is given,
# so an accidental re-run does not append a second copy of every observation.
#
#   ./docs/capability-ledger-seed.sh
#   ./docs/capability-ledger-seed.sh --force
#
# It writes through `mb` (the records spore), never through the native SQLite
# store and never by touching the records database directly.
set -eu

MB=${MB:-mb}
FORCE=no
[ "${1:-}" = "--force" ] && FORCE=yes

existing=$($MB capability list --json)
if [ "$existing" != "[]" ] && [ "$FORCE" != yes ]; then
    echo "capabilities already exist in this book; pass --force to append" >&2
    exit 1
fi

cap() {
    id=$1
    shift
    if $MB capability show "$id" >/dev/null 2>&1; then
        $MB capability update "$id" "$@" >/dev/null
    else
        $MB capability create "$id" "$@" >/dev/null
    fi
}

ver() {
    cap_id=$1
    layer=$2
    result=$3
    revision=$4
    workload=$5
    command=$6
    evidence=$7
    acceptance=$8
    limitations=$9
    reproduction=${10}
    observed_at=${11:-}
    if [ -n "$observed_at" ]; then
        $MB capability verify "$cap_id" --layer "$layer" --result "$result" \
            --implementation-id "$revision" --workload "$workload" --command "$command" \
            --evidence "$evidence" --acceptance "$acceptance" --limitations "$limitations" \
            --reproduction "$reproduction" --observed-at "$observed_at" >/dev/null
    else
        $MB capability verify "$cap_id" --layer "$layer" --result "$result" \
            --implementation-id "$revision" --workload "$workload" --command "$command" \
            --evidence "$evidence" --acceptance "$acceptance" --limitations "$limitations" \
            --reproduction "$reproduction" >/dev/null
    fi
}

meas() {
    cap_id=$1
    metric=$2
    value=$3
    unit=$4
    workload=$5
    profile=$6
    outcome=$7
    limitations=$8
    $MB capability measure "$cap_id" --metric "$metric" --value "$value" --unit "$unit" \
        --workload "$workload" --profile "$profile" --samples 1 --outcome "$outcome" \
        --limitations "$limitations" >/dev/null
}

dec() {
    id=$1
    title=$2
    status=$3
    rationale=$4
    affects=$5
    supersedes=${6:-}
    if $MB inventory --max-age-days 30 2>/dev/null | grep -q "\"$id\""; then
        return 0
    fi
    if [ -n "$supersedes" ]; then
        $MB capability decide "$id" --title "$title" --status "$status" \
            --rationale "$rationale" --owner "principal engineer" --affects "$affects" \
            --supersedes "$supersedes" >/dev/null
    else
        $MB capability decide "$id" --title "$title" --status "$status" \
            --rationale "$rationale" --owner "principal engineer" --affects "$affects" >/dev/null
    fi
}

# ---- capabilities ----------------------------------------------------------

cap transmog.native --title "Native Transmog" --owner Transmog --project transmog \
    --implementation execution_verified --implementation-id b33b2c3 --health healthy \
    --lifecycle active --commercial-readiness internal_validated \
    --commercial-gates native

cap transmog.batch-provider --title "Transmog batch provider" --owner Transmog --project transmog \
    --implementation execution_verified --implementation-id b8c93fb4 --health healthy \
    --lifecycle active --commercial-readiness demo_ready \
    --platform-gates soil --commercial-gates loam

cap loam.provider-host --title "Loam provider host for Soil" --owner Loam --project loam \
    --implementation execution_verified --implementation-id f7313e5 --health healthy \
    --lifecycle active --commercial-readiness internal_validated --platform-gates soil

cap panopticon.provider --title "Panopticon geospatial provider" \
    --owner "Agent 1 / Panopticon" --project panopticon --implementation implemented \
    --implementation-id 7dcceff --health healthy --lifecycle active \
    --commercial-readiness internal_prototype --platform-gates soil --commercial-gates native

cap twenty.provider --title "Twenty CRM provider" --owner "Agent 2 / Twenty" \
    --project fpl-twenty --implementation implemented --implementation-id 8795eb1 \
    --health healthy --lifecycle active --commercial-readiness internal_prototype \
    --platform-gates soil --commercial-gates loam

cap twenty.webhook-relay --title "Twenty webhook relay" --owner "Agent 2 / Twenty" \
    --project fpl-twenty --implementation execution_verified --implementation-id 6cc857f \
    --health healthy --lifecycle active --commercial-readiness internal_validated \
    --platform-gates soil --commercial-gates native

cap website-factory.proposals --title "Website Factory proposals" \
    --owner "Agent 4 / Website Factory" --project website-factory \
    --implementation execution_verified --implementation-id 86cfdc8 --health healthy \
    --lifecycle active --commercial-readiness demo_ready --platform-gates studio \
    --commercial-gates native,commercial

cap studio.editor --title "Studio visual editor" --owner "Agent 3 / Studio" --project studio \
    --implementation implemented --implementation-id ed385ff --health healthy --lifecycle active \
    --commercial-readiness internal_prototype --platform-gates native

cap quoting.waterjet-rfq --title "Waterjet RFQ-to-quote vertical" --owner "Agent 5 / Quoting" \
    --project quotron3000 --implementation execution_verified --implementation-id 28b96fb \
    --health healthy --lifecycle active --commercial-readiness demo_ready \
    --platform-gates studio --commercial-gates native,commercial

cap transmog.contract --title "fpl:transmog contract" --owner Transmog --project transmog \
    --contract-version 0.1.0 --contract-maturity draft --implementation implemented \
    --implementation-id 7ebce24d --health healthy --lifecycle active \
    --commercial-readiness research

# ---- verification (reported; not re-run here) ------------------------------

ver transmog.native native pass b33b2c3 "DFM" "transmog analyze" \
    "integration ledger 2026-11-25; commit b33b2c3 not present in the local checkout" \
    "native analysis returns for the DFM workload" "reported, not independently audited" reported

ver transmog.batch-provider sidecar pass b8c93fb4 "5 stateless ops over a 361 KB STL" \
    "transmog-batch sidecar" "commit b8c93fb4; plugins/transmog-batch" \
    "sidecar answers the stateless operations" "reported by the commit and its README, not re-run here" reported

ver transmog.batch-provider loam pass b8c93fb4 "DFM, 369684-byte artifact" \
    "loam provider call fpl:transmog analyze" "commit b8c93fb4; integration ledger 2026-11-25" \
    "DFM analysis returns; commercial quote validity is not asserted" \
    "debug-native profile; one reported observation" reported

ver transmog.batch-provider loam pass b8c93fb4 "quote, Taig profile" \
    "loam provider call fpl:transmog quote" "commit b8c93fb4; integration ledger 2026-11-25" \
    "quote call executes; commercial validity not established" \
    "one reported observation" reported "$(( $(date +%s) - 1 ))"

ver loam.provider-host loam pass c4fc171 "provider call by interface and function name" \
    "cargo test -p loam provider_host" "commit c4fc171" \
    "provider host resolves and calls by name" "superseded by f7313e5; not the current revision" reported

ver loam.provider-host loam pass afd8dd0 "composed runtime artefact identity" \
    "cargo test -p loam runtime" "commit afd8dd0" \
    "composed artefact is verified, not trusted" "superseded by f7313e5; not the current revision" reported

ver loam.provider-host soil pass f7313e5 \
    "Soil defun calling (host-call \"fpl:transmog\" \"identity\")" \
    "cargo test -p loam soil_providers" "commit f7313e5" \
    "identity reaches Soil; an ungranted interface is refused by name" \
    "proves the identity call and marshalling; not a DFM or quote call through Soil" reported

ver panopticon.provider native pass 7dcceff "OSM index, Nominatim hydration, resolve" \
    "panopticon sidecar" "commit 7dcceff; provider/docs/DELIVERY.md" \
    "discovery and hydration answer from the NZ index" \
    "reported by the provider delivery doc, not re-run here" reported

ver panopticon.provider loam pass 7dcceff "real Loam consumer integration tests" \
    "cargo test -p panopticon-provider-consumer" \
    "commit 7dcceff; provider/consumer/tests/provider.rs" \
    "consumer imports fpl:panopticon and completes its calls" \
    "Soil orchestration example is still labelled unverified" reported

ver twenty.provider loam pass 8795eb1 "gtm consumer imports fpl:twenty/crm" \
    "cargo test -p fpl-twenty-consumer" "commit 8795eb1; consumer/" \
    "typed CRM calls succeed through the adapter and sidecar" \
    "no Soil path; deployment config external" reported

ver twenty.webhook-relay native pass 6cc857f \
    "live Twenty company.created/updated -> relay -> Confluence -> Unibus -> Elmers" \
    "curl the relay with valid, invalid, and duplicate signatures" \
    "commit 6cc857f; docs/events.md evidence run" \
    "invalid signature 401; duplicate 202 duplicate=true with no second consumer run; retry after a 503 delivered exactly one event" \
    "reported by docs/events.md, not re-run here; relay is on a personal Cloudflare account; Confluence records the verifier family as github" reported

ver website-factory.proposals native pass 86cfdc8 \
    "ten NZ manufacturer proposals, build + QA + audit" \
    "node src/factory.mjs profiles/<id>.json <archetype>" \
    "commit 86cfdc8; out/internal/buckley-systems/qa.json" \
    "qa.ok true, 21 checks, 0 failed; signed preview links; sales package produced" \
    "reported by repo artifacts; not re-run here; no business has been contacted" reported

ver studio.editor native pass ed385ff "standalone Studio with a pinned Coherence project" \
    "npm test" "commit ed385ff" "editor serves from the extracted repo" \
    "no Soil or capability-view integration yet" reported

ver quoting.waterjet-rfq native pass 28b96fb \
    "DXF bracket waterjet RFQ, Mild Steel 0.25 in, qty 2" \
    "QUOTRON_BIN=.tools/quotron npm test" \
    "commit 28b96fb; docs/RFQ-VERTICAL.md; docs/artifacts/internal-test-quotation.pdf" \
    "price total 79.801 USD, calibration true, factory status eligible, internal-test PDF recorded in the outbox" \
    "internal-test PDF only; shop approval required; never sent to a customer; no customer acceptance recorded" reported

# ---- measurements ----------------------------------------------------------

LIM="single reported observation; not a statistically characterised distribution"

meas transmog.native dfm_seconds 2.38 s "DFM" debug-native "analysis returned" "$LIM"
meas transmog.batch-provider dfm_seconds 3.887 s "DFM, 369684-byte artifact" debug-native \
    "analysis returned" "$LIM"
meas transmog.batch-provider dfm_total_seconds 4.089 s "DFM, 369684-byte artifact" debug-native \
    "analysis returned" "$LIM"
meas transmog.batch-provider quote_seconds 3.595 s "quote, Taig profile" debug-native \
    "execution verified; commercial quote validity not established" "$LIM"
meas transmog.batch-provider quote_total_seconds 3.764 s "quote, Taig profile" debug-native \
    "execution verified; commercial quote validity not established" "$LIM"

FACTORY="one report.json artifact; Node 26 local build"
meas website-factory.proposals proposal_build_ms 36 ms "buckley-systems, automate preview" node-26 \
    "site rendered" "$FACTORY"
meas website-factory.proposals proposal_qa_ms 1 ms "buckley-systems, automate preview" node-26 \
    "21 checks, 0 failed" "$FACTORY"
meas website-factory.proposals proposal_audit_ms 13949 ms "buckley-systems, automate preview" node-26 \
    "existing-site audit completed (14 findings)" "$FACTORY"
meas website-factory.proposals proposal_total_ms 13987 ms "buckley-systems, automate preview" node-26 \
    "proposal packaged" "$FACTORY"

QUOTING="one observed run; internal-test quotation; rate revision pinned and verified"
meas quoting.waterjet-rfq quote_total_usd 79.801 usd "bracket.dxf, Mild Steel 0.25 in, qty 2" \
    "quotron 0.1.0, pinned rate revision" "factory status eligible; internal-test PDF" "$QUOTING"
meas quoting.waterjet-rfq cut_path_in 23.169 in "bracket.dxf" "quotron 0.1.0" \
    "measurement reported by transmog-estimate via Quotron" "$QUOTING"
meas quoting.waterjet-rfq machine_time_h 0.672 h "bracket.dxf, waterjet, qty 2" "quotron 0.1.0" \
    "machine time priced" "$QUOTING"
meas quoting.waterjet-rfq pierces 5 count "bracket.dxf" "quotron 0.1.0" "pierce count priced" "$QUOTING"

# ---- decisions -------------------------------------------------------------

dec DEC-002 "Native batch provider first" accepted \
    "Use the stateless Transmog CLI behind a Loam provider; defer live modelling and portable lowering." \
    "transmog.batch-provider"

dec DEC-004 "No fake providers" accepted \
    "Missing runtime capabilities fail explicitly; a stub must not conceal an integration failure." \
    "transmog.batch-provider,loam.provider-host"

dec DEC-005 "Soil is authoritative workflow source" accepted \
    "Studio's node editor is a view of Soil, not a second workflow language." "studio.editor"

dec DEC-010 "Soil crate ownership is unresolved" proposed \
    "Divergent shared crates between Soil and Loam must not produce competing authoritative implementations." \
    "loam.provider-host"

dec DEC-014 "Loam owns the generic provider host; Soil consumes it" accepted \
    "The host needs the registry and grants, which already live in Loam. f7313e5 maps Value <-> WIT there and enforces crate canon." \
    "loam.provider-host" "DEC-010"

dec DEC-015 "Optional platform integrations do not gate commercial pilots" accepted \
    "A missing Soil or Studio path is reported as a platform gap. It blocks platform readiness, not a customer pilot whose commercial gates are met." \
    "transmog.batch-provider,twenty.webhook-relay,website-factory.proposals"

dec DEC-016 "Reported and reproduced evidence are recorded separately" accepted \
    "A commit message, delivery log, or document is a reported observation. Only a re-run is reproduced. Both are useful; collapsing them loses the distinction." \
    "transmog.native,twenty.webhook-relay,website-factory.proposals"

dec DEC-017 "Quotron owns price policy; Studio does not reimplement rates" accepted \
    "Studio calls the existing Quotron waterjet CLI. Missing or changed rate records block; no price is invented. The internal-test PDF is not a customer quote." \
    "quoting.waterjet-rfq"

dec DEC-018 "Records book is the authoritative lineage" proposed \
    "New fleet work writes to the soil:records book. The native single-file store is read-only historical until each open issue is deliberately recreated with an external_ref to its native id; no rows are copied and no automatic merge is performed." \
    ""

echo "seeded capability ledger into the authorized records book"

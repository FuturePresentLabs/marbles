# Capabilities, evidence, measurements, and decisions

Marbles tracks two different kinds of thing, and they are not the same record:

* an **issue** is work. It is claimed, it moves through review, it closes with a receipt.
* a **capability** is functionality that outlives any one task — Transmog's native engine, the
  Panopticon provider, the Soil-to-provider bridge. Many tasks improve it, several layers verify
  it, measurements track it, and a later capability supersedes it.

This page is the reference for the second kind. It is implemented as typed records in the same
`soil:records` store, under the same single-writer lock as the work graph. There is no new
server, no event bus, no replicated ledger, and no second project list.

```text
issue/["marbles-abc"]                       work: claim, review, close
capability/["transmog.batch-provider"]      persistent functionality
verification/["…loam.1795633082.4f2a…"]     one observation at one layer, one revision
measurement/["…dfm_seconds.…"]              one observation of one metric
decision/["DEC-002"]                        an architectural or commercial decision
```

## Four axes, not one status

A capability has four independent fields. Collapsing them into one enum is what makes a ledger
row say “EXECUTION-VERIFIED” while quietly meaning “and also currently broken and also
deprecated.”

| field | values | question it answers |
|---|---|---|
| `implementation` | `not_started`, `in_progress`, `implemented`, `build_verified`, `execution_verified` | how far did the code get? |
| `health` | `healthy`, `degraded`, `blocked`, `unknown` | does it work right now? |
| `lifecycle` | `proposed`, `active`, `deprecated`, `superseded`, `retired` | is this still the thing? |
| `commercial_readiness` | `research`, `internal_prototype`, `internal_validated`, `demo_ready`, `pilot_ready`, `production_ready` | what may be sold or piloted? |

Two more fields describe the interface rather than the code: `contract_maturity`
(`not_started` … `versioned`) and `contract_version`. Work status is still an issue status;
`health = blocked` is not a claim and does not stop anyone from claiming a task.

## Verification is bound to a revision

Every verification names the revision it tested in `implementation_id` (usually a commit) and
one execution layer:

`native`, `sidecar`, `wit`, `loam`, `soil`, `studio`, `commercial`

An observation is `pass`, `fail`, `not_tested`, `not_applicable`, or `blocked`, and carries the
workload, test command, evidence reference, artifact digest, date, and limitations. Several
observations of the same layer are kept; the newest is what the view reports. Two more fields
keep the claim honest:

- `reproduction` — `reported` (a commit message, delivery log, or document says it happened),
  `reproduced` (the observer re-ran the workload and saw the result), or `not_applicable`.
  Omitting it means `reported`, the weaker claim.
- `acceptance` — the workload-specific criterion the observation was judged against, so a pass
  cannot be read as a broader claim than the one that was tested.

The readiness views print reproduction and acceptance next to the result. A signature, if one is
ever added, would establish provenance; it would not turn `reported` into `reproduced`.

**A changed implementation does not inherit an old pass.** Freshness is computed on read:

| freshness | meaning |
|---|---|
| `fresh` | same revision, inside the age window (default 30 days) |
| `stale` | same revision, older than the window |
| `revision_mismatch` | the observation tested a different revision |
| `unverifiable` | a future timestamp; fails closed |
| `unknown` | no observation exists for that layer |

A `pass` without `--evidence`, `--command`, `--workload`, or `--artifact-digest` is refused: a
claim is not an observation.

A signed assertion would prove *who* observed something. It would not make the engineering
result true. These records are local and unsigned by default; the existing
`marbles.issue-assertion/1` signer and pinned-key machinery is the transport to reuse if a
capability assertion is ever federated between stores. It is not a substitute for re-verifying
the new revision.

## Measurements

Measurements are observations, not task rows:

| field | example |
|---|---|
| `metric` | `dfm_seconds`, `ttva_seconds`, `ttr_usd`, `lead_conversion` |
| `value` + `unit` | `3.887` `s` |
| `workload` | `DFM, 369684-byte artifact` |
| `profile` | `debug-native` |
| `samples` | `1` |
| `domain_outcome` | `analysis returned` |
| `limitations` | `single reported observation; not statistically characterised` |

`mb capability baseline` returns the latest value of each `(capability, metric)` pair. History is
retained: a new measurement appends, it does not rewrite the old one. TTWC, TTVA, TTR, latency,
resource consumption, human labour, and conversion are all just metrics; nothing infers
commercial success from a number.

## Decisions

A decision has a stable id (`DEC-002`), status (`proposed`, `accepted`, `rejected`,
`superseded`), rationale, owner, affected capabilities, evidence, and a `supersedes` list.
Recording a decision that supersedes `DEC-010` appends a `superseded` state to `DEC-010` and
names its replacement — the existing supersede idea, not a new mechanism. A decision without a
rationale is refused.

## Commands

```bash
# define and maintain
mb capability create ID --title T --owner O [--project P] \
    [--implementation P] [--health H] [--lifecycle L] \
    [--commercial-readiness R] [--implementation-id REV] \
    [--platform-gates soil,studio] [--commercial-gates native,commerce] \
    [--contract-version V] [--contract-maturity M]
mb capability update ID [same flags]
mb capability show ID [--max-age-days N]

# observe
mb capability verify ID --layer L --result R --implementation-id REV \
    [--workload W] [--command C] [--evidence E] [--artifact-digest D] \
    [--acceptance A] [--reproduction reported|reproduced] \
    [--limitations L] [--verifier WHO] [--observed-at EPOCH]
mb capability measure ID --metric M --value V --unit U \
    [--workload W] [--profile P] [--samples N] [--outcome O] \
    [--evidence E] [--limitations L] [--observed-at EPOCH]
mb capability decide ID --title T --status S --rationale R \
    [--owner O] [--affects a,b] [--evidence E] [--supersedes a,b]

# ask
mb capability list [--implementation P] [--health H] \
    [--commercial-readiness R] [--missing-layer L] [--stale] [--max-age-days N]
mb capability verified [--layer L]        # which revisions passed this layer?
mb capability gaps --layer L              # who lacks a fresh pass at this layer?
mb capability stale                       # evidence too old or from another revision
mb capability pilot-blockers              # demo/pilot claims with an unmet gate
mb capability baseline [--metric M]       # the latest value of each metric

# one read-only call for an orchestrator or MCP connector
mb [--project P] inventory [--max-age-days N]
```

`mb inventory` returns `capabilities`, `ready_tasks`, `decisions`, and `measurements` (the
baseline) in one read-only answer. It is the query an external orchestration agent uses; it
writes nothing and creates no delivery channel.

## Two kinds of gate

`required_layers` are **platform integration gates**: optional infrastructure paths such as Soil
or Studio. `commercial_gates` are **commercial acceptance gates**: the layers a customer-facing
pilot actually needs. The view reports them separately:

| field | meaning |
|---|---|
| `platform_gaps` | platform gates without a fresh pass; reported, not pilot-blocking |
| `commercial_gaps` | commercial gates without a fresh pass; these block a pilot |
| `platform_blockers` | human-readable platform gaps |
| `blockers` | health, lifecycle, and commercial gaps only |
| `technically_ready` | healthy, active, and every declared gate satisfied |
| `commercial_ready` | healthy, active, and every commercial gate satisfied |

`pilot-blockers` filters on `commercial_ready`, so a missing optional Soil or Studio path cannot
block an otherwise viable pilot. `gaps --layer L` still reports both kinds and tags each row with
`gate` (`platform`, `commercial`, or `platform+commercial`) and `blocks_commercial_pilot`.

Empty lists mean “no gate declared yet”, which is honest. A capability that claims `demo_ready`
should name the gates a demo actually requires.

## What this deliberately does not do

* It does not create a project or an issue per capability.
* It does not replicate, deliver, or coordinate across stores. Capability records live in the
  store their lineage owns; cross-store claims remain signed, pinned, read-only assertions with
  named freshness.
* It does not sign verification. Reuse the existing assertion machinery if that is ever needed,
  and remember a signature is provenance, not truth.
* It does not replace the Studio view. It gives Studio a queryable record shape to render; the
  view is derived, not authoritative.
* It does not infer commercial readiness from technical execution. The two are separate fields,
  and `pilot-blockers` reports both.

## Worked example

`examples/capability_ledger.rs` seeds a throwaway trail with observations from the actual
Transmog, Loam, Panopticon, Twenty, and Studio commits, then prints the six questions:

```sh
cargo run --example capability_ledger
```

The seed is honest about its limits: `transmog.native` carries the integration ledger's
`b33b2c3` report and says the commit is not present in this checkout; the Loam
`c4fc171`/`afd8dd0` passes are kept as `revision_mismatch` against `f7313e5`, which is exactly
the “do not inherit verification from an earlier revision” rule. Running it prints (abridged):

```text
## which capabilities lack Soil verification?
[ { "capability_id": "panopticon.provider", "layer": "soil", "status": "missing",
    "freshness": "unknown", "gate": "platform", "blocks_commercial_pilot": false },
  { "capability_id": "transmog.batch-provider", "layer": "soil", "status": "missing",
    "freshness": "unknown", "gate": "platform", "blocks_commercial_pilot": false },
  { "capability_id": "twenty.webhook-relay", "layer": "soil", "status": "missing",
    "freshness": "unknown", "gate": "platform", "blocks_commercial_pilot": false } ]

## what blocks a commercial pilot?
[ { "capability_id": "website-factory.proposals",
    "commercial_readiness": "demo_ready", "implementation": "execution_verified",
    "commercial_gaps": ["commercial"], "platform_gaps": ["studio"],
    "blockers": ["commercial gate commercial: missing (unknown)"] } ]
```

The Transmog provider is no longer listed by a missing Soil path: Soil is a platform gate, and
its commercial gate (Loam execution) is a fresh pass. The Website Factory is listed because no
customer acceptance exists yet — the commercial fact, not an optional integration.

The `Loam` capability no longer carries the ledger's “Soil-to-WIT marshalling BLOCKED” row:
`f7313e5` landed the generic provider host, and the record says what that test does and does not
prove (an identity call, not a DFM or quote through Soil).

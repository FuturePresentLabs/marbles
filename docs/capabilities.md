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
observations of the same layer are kept; the newest is what the view reports.

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
    [--required-layers native,sidecar,loam,soil,studio,commercial] \
    [--contract-version V] [--contract-maturity M]
mb capability update ID [same flags]
mb capability show ID [--max-age-days N]

# observe
mb capability verify ID --layer L --result R --implementation-id REV \
    [--workload W] [--command C] [--evidence E] [--artifact-digest D] \
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
```

`--required-layers` is what makes readiness falsifiable. Empty means “not gating anything
yet”, which is honest; a capability that claims `demo_ready` should name the layers a demo
actually requires. `pilot-blockers` reports capabilities whose commercial readiness is
`demo_ready` or above and whose health, lifecycle, or required layers still have a blocker.

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
    "freshness": "unknown", "current_implementation_id": "7dcceff" },
  { "capability_id": "transmog.batch-provider", "layer": "soil", "status": "missing",
    "freshness": "unknown", "current_implementation_id": "b8c93fb4" },
  { "capability_id": "twenty.provider", "layer": "soil", "status": "missing",
    "freshness": "unknown", "current_implementation_id": "8795eb1" } ]

## what blocks a commercial pilot?
[ { "capability_id": "transmog.batch-provider", "commercial_readiness": "demo_ready",
    "implementation": "execution_verified", "gaps": ["soil"],
    "blockers": ["soil: missing (unknown)"] } ]
```

The `Loam` capability no longer carries the ledger's “Soil-to-WIT marshalling BLOCKED” row:
`f7313e5` landed the generic provider host, and the record says what that test does and does not
prove (an identity call, not a DFM or quote through Soil).

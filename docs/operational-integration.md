# Operational integration: MCP and Studio consumers

This page is the contract for systems that *read* Marbles capability records. It adds no store,
server, scheduler, or event bus. Both consumers use the same single-writer book through the
host-opened `mb` command, exactly as a Spore does.

## The one read

```sh
mb [--project SLUG] inventory [--max-age-days N]
```

`mb` always answers JSON. `inventory` is read-only, does not drain the outbox, and does not write
an issue, claim, or revision. `--project` scopes `ready_tasks`; capabilities, decisions, and
measurements are store-wide because they outlive any one project.

```json
{
  "generated_at": 1795634371,
  "max_age_seconds": 2592000,
  "scope": "marbles",
  "counts": {
    "capabilities": 9,
    "ready_tasks": 0,
    "decisions": 7,
    "measurements": 9
  },
  "capabilities": [
    {
      "id": "transmog.batch-provider",
      "title": "Transmog batch provider",
      "owner": "Transmog",
      "project": "transmog",
      "implementation": "execution_verified",
      "health": "healthy",
      "lifecycle": "active",
      "commercial_readiness": "demo_ready",
      "implementation_id": "b8c93fb4",
      "required_layers": ["soil"],
      "commercial_gates": ["loam"],
      "layers": [
        {
          "layer": "loam",
          "status": "pass",
          "freshness": "fresh",
          "implementation_id": "b8c93fb4",
          "observed_at": 1795634371,
          "reproduction": "reported",
          "acceptance": "DFM analysis returns; commercial quote validity is not asserted",
          "evidence": "commit b8c93fb4; integration ledger 2026-11-25",
          "limitations": "debug-native profile; one reported observation"
        }
      ],
      "platform_gaps": ["soil"],
      "commercial_gaps": [],
      "stale": [],
      "blockers": [],
      "platform_blockers": ["platform gate soil: missing (unknown)"],
      "technically_ready": false,
      "commercial_ready": true
    }
  ],
  "ready_tasks": [],
  "decisions": [],
  "measurements": []
}
```

### Rules a consumer must preserve

1. **Read the store the host opened.** Do not add `--url`/`--token`; do not let a caller choose an
   endpoint. A connector that can point at an arbitrary book is a second writer by another name.
2. **Treat `freshness` and `gaps` as the answer, not a boolean.** A pass is only current when
   `freshness == "fresh"` and `implementation_id` equals the capability's current revision.
   `revision_mismatch` means the evidence belongs to an earlier revision.
3. **Keep `reported` and `reproduced` distinct.** A consumer that ignores `reproduction` will
   present a commit message as if it had re-run the workload.
4. **Do not gate a pilot on `platform_gaps`.** Only `commercial_gaps` (and `health`/`lifecycle`)
   block a commercial pilot. `platform_blockers` is information for platform planning.
5. **Do not infer readiness.** `commercial_readiness` is an operator field; `commercial_ready` is
   derived from declared gates and evidence. Neither is a forecast.
6. **Do not write through a read surface.** Any mutation stays on the claimed-work path
   (`mb claim`/`close`) or the explicit capability commands.

## Lineage selection

A task id belongs to exactly one store. Marbles has two legitimate single-writer lineages:
the `soil:records` book that `mb` opens, and the native `~/.marbles/marbles.db` that the native
`marbles` binary opens. They are not synchronized; a project must not be split across them.

The connector must select one explicitly and report it:

- `MYCELIUM_TASKS_LINEAGE=records` (default) reads the book named by
  `~/.config/fungos/commands/mb.env`.
- `MYCELIUM_TASKS_LINEAGE=native` reads the native SQLite store.

`tasks_list` reports `lineage` and `other_lineage`. `tasks_get` reads the selected lineage, then
probes the other read-only and returns `split_lineage: true` with a warning rather than a bare
not-found. It never copies or writes the row. Neither lineage is silently preferred.

A not-found is an ordinary exit, not a panic. The connector classifies the guest's terminal
`mb: issue not found: …` / `marbles: no issue …` line even when an older loam printed a wasm
backtrace first, and returns `kind: "not_found"` with the terminal line only. `unknown command`,
`usage`, and `invalid` are `usage`; a real trap with no guest error line is `panic`.

## MCP surface

The Mycelium MCP connector does not need a new protocol. It can expose one tool per record kind,
or one aggregate tool. Both call the same command:

| tool | call | result |
|---|---|---|
| `marbles.inventory` | `mb inventory` | the object above |
| `marbles.lineage` | `mb` policy + binary paths | which book/root/database is read |
| `marbles.capabilities` | `mb capability list` | capability views |
| `marbles.capability` | `mb capability show ID` | one capability plus its observations, measurements, decisions |
| `marbles.verified` | `mb capability verified --layer L` | revisions that passed one layer, with freshness |
| `marbles.pilot_blockers` | `mb capability pilot-blockers` | commercial blockers only |
| `marbles.ready` | `mb ready` | unblocked, unclaimed work past grace |
| `marbles.decisions` | part of `inventory` | decisions with status and supersession |
| `marbles.baseline` | `mb capability baseline` | latest measurement per capability+metric |
| `marbles.lineage` | read the shim policy; do not guess | selected book, records root, native database |

An orchestration loop is then: `inventory` → choose a `ready_task` → `claim` → inspect the
capability's evidence before assuming a revision is verified → `review`/`close` with a receipt.
The capability surface never makes a claim or starts a run.

**Authority boundary.** The MCP connector authenticates the caller (Edgerunner grant or the host's
policy) and passes only a project/actor scope. It must not accept an endpoint, book path, or
credential from the model. Read-only tools are read-only in the command implementation
(`Request::is_read_only`), not merely by prompt convention.

## Studio capability inventory view

Studio is a **consumer**, not an owner. It reads `mb inventory` (or `mb capability list`) and
renders:

* a capability row with the four axes shown as separate columns, never collapsed into a single
  status;
* a layer matrix with `status` / `freshness` / `reproduction` / `acceptance`, plus the revision
  the observation tested and the capability's current revision;
* a "commercial gates" lane and a separate "platform gates" lane, so a missing optional Soil
  path reads as optional;
* the decision list for the selected capability, with superseded decisions visibly retained;
* the measurement baseline, with workload, profile, unit, samples, and limitations.

The view must not cache a projection into its own store, invent a readiness value, or hide
`revision_mismatch`/`stale` rows. If Studio wants a writable action, it issues the same `mb`
mutation under the same claim rules; it does not get a private write path.

## Backend availability

`mb capability …` and `mb inventory` are implemented in the Rust records CLI in this repository.
The live command is now that spore (module SHA-256
`89e25ba15680d9dfd32b5b4c0c3ce53d00e6b82086ab98c948658460bbd6299c`, built from the
`capability-evidence` branch for `wasm32-wasip2`). The previous Soil-language port is preserved
as `~/.local/share/fungos/commands/mb/module.wasm.before-capability-<timestamp>` for rollback.

Build and install, with the fix that lets bob pass the "already remote" marker through the
builder limiter:

```sh
fpl-bob remote --host <builder> --artifact target/wasm32-wasip2/release/mb.wasm --   cargo build --release --target wasm32-wasip2 --no-default-features --features soil --bin mb
scripts/install-records-cli.sh --records-book personal/<you>/marbles
```

Verify against a throwaway command root and book before the live install by setting
`XDG_CONFIG_HOME`, `FUNGOS_COMMAND_ROOT`, `MARBLES_BIN_DIR`, and `FUNGOS_RECORDS_ROOT`; the
script honours all four. The live seed is `docs/capability-ledger-seed.sh` (refuses a second run
without `--force`).

`mb inventory` is bounded by default: `--limit` caps ready tasks at 50 (max 200) and returns
`ready_total`/`ready_truncated` so a caller can see what it did not receive.

## Versioning

The record bodies are `v0.x` and follow the same interface-first policy as the rest of Marbles.
Fields are additive. A consumer should ignore fields it does not know and must not assume that
`gaps` is only platform or only commercial — read `platform_gaps` and `commercial_gaps` directly.

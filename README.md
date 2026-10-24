<p align="center">
  <img src="docs/img/marbles-logo.png" alt="Marbles — a small issue tracker for fleets of coding agents" width="900">
</p>

<p align="center">
  <b>One server. One database. One claim race at a time.</b><br>
  A work graph built for agents that actually ship.
</p>

<p align="center">
  <a href="https://github.com/FuturePresentLabs/marbles/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/FuturePresentLabs/marbles/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-black"></a>
  <img alt="Rust 1.88+" src="https://img.shields.io/badge/rust-1.88%2B-black">
</p>

---

Marbles is a small, single-writer issue tracker for fleets of coding agents. It
keeps the parts of [Beads](https://github.com/gastownhall/beads) that made
agent work legible — short stable IDs, a dependency-ready queue, claims that
serialize workers, a CLI you can pipe into an agent — and drops the
per-checkout replicated database that made fleet-scale bookkeeping the work.

The core idea is deliberately boring: **the work graph has exactly one writer
per lineage.** Humans and agents are HTTP clients of that writer. Nothing is
replicated, so there is nothing to merge.

<p align="center">
  <img src="docs/img/marbles-hero.jpg" alt="A ring of colorful marbles, with one silver marble striking the ring and starting a chain reaction" width="820">
</p>

## Why Marbles

- **Claims are a race with an answer.** One `BEGIN IMMEDIATE`; a winner, a
  lease, and a loser that can pick different work. No three-way merge, no
  last-writer-wins resurrection.
- **`review` is not `done`.** An agent that finished writing code has produced
  something to look at, not a delivery. Closing requires merge evidence or an
  explicit acknowledgement that no delivery is expected.
- **The queue is queryable.** Ready work, blockers, claims, and history
  all live in one store, so a portfolio view is a query rather than a sync job.

## Install

From source (Rust 1.88+):

```bash
cargo install --path .      # installs `marbles` and `mb`
marbles --version
```

Tagged releases publish checksum-addressed archives for Linux and macOS on
x86-64 and ARM64. On an FPL workstation, `fpl install marbles` selects and
verifies the right archive and exposes both `marbles` and `mb` in
`~/.local/bin`.

Container builds work without a remote cache or AWS credentials. To opt into
the DigitalOcean/mycelium `sccache`, see [`docs/build-cache.md`](docs/build-cache.md).

## Quickstart

### Local, one operator

```bash
cd my-repo
mb init                                  # register this project, install agent instructions
mb create "Fix the thing" -p 1           # new issue, visible to the fleet after grace
mb ready                                 # unblocked, unclaimed, past grace
mb claim <id>                            # take it
mb review <id> --pr <url>                # PR is open; not done
mb close  <id> --pr <url> --commit <sha> # merged; done
```

With no server URL configured, commands act directly on
`~/.marbles/marbles.db` — one file, every project on the machine. Cross-repo
views are just queries.

### Fleet mode

```bash
mkdir -p ~/.marbles
marbles serve                            # local server on 127.0.0.1:7878
marbles login --name avery               # prints your human token
marbles agent-token worker-1             # per-agent/sandbox token
export MARBLES_URL=http://127.0.0.1:7878
export MARBLES_TOKEN=<token>
```

`server.toml` configures the listen address and optional OIDC. The server binds
loopback by default; put TLS at the edge. A 60-second sweeper requeues expired
agent leases and escalates expired human holds.

Sandboxes get an agent token and zero file access to the database. That is the
point.

## The work model

```text
open ──(claim)──> in_progress ──(PR opened)──> review ──(merged + receipt)──> closed
```

- **Grace at creation.** A new issue is swarm-invisible for 10 minutes; edits
  re-defer it by 2 minutes. Live human↔agent speccing does not leak
  half-formed work to the fleet, but nothing sits untouched at dawn.
- **Claims carry identity and kind.** An *agent* claim is a minutes-long lease
  renewed by heartbeats; when it lapses, the work is silently requeued — a dead
  agent is an interruption, not a decision. A *human* claim lasts 48 business
  hours; when it lapses it escalates to the owner rather than being re-stolen.
- **Dependencies gate readiness.** `ready` = open, unblocked, past grace, not
  held. Cycles are refused when the edge is created.
- **History is append-only.** Every create, claim, renew, release, status, and
  close is an event; a bead can always explain itself with `mb history <id>`.
- **Metadata is a first-class sidecar.** Attach structured receipts with
  `--set-metadata key=value` or `--metadata @file` (RFC-7386 shallow merge,
  `null` deletes) without inventing label grammars for JSON.

## Organizing work

A **project** is one tracker: a repo or a coherent body of work. Every project
has its own prefix (`marbles-…`), and all projects share one database, so
portfolio queries are cheap.

### Folders

Projects can be grouped into folders (`personal`, `business`, `business/acme`).
Folders nest; a project belongs to at most one folder, and unfiled is explicit.

```bash
mb folder create "Business" --slug business
mb folder move acme business
mb folder tree
mb projects                       # includes each project's folder
```

### Goals → epics → tasks

A **goal** is a project-local root. An **epic** is a body of work under a goal;
a **task** is an executable unit under an epic. All three are ordinary issues,
so they inherit claims, history, labels, dependencies, review, and evidence.

```bash
mb goal create "Ship v1" -p 1
mb epic create "Storage" --goal <goal-id>
mb task create "Write the WAL test" --epic <epic-id>

mb goal list                      # descendant rollups
mb goal show <goal-id>            # tree + progress bar
```

Hierarchy is project-local; use an ordinary dependency edge for a blocker in
another project. Dependencies remain cross-project and cross-folder.

### Burndown

```bash
mb burndown --project acme --days 30
mb burndown --goal <goal-id> --days 14
mb burndown --epic <epic-id> --days 7 --json
```

The series reports daily `scope`, `done`, and `remaining`, plus burn rate and a
projected days-to-zero when there are closes in the window.

See [`docs/organizing-work.md`](docs/organizing-work.md) for the data model and
worked examples.

## Plans are DAGs

A plan is not a second database. It is a **view over issues**: nodes are marbles carrying a
small metadata block, edges are ordinary `dep` rows. One claim race, one company scope, and an
executor consumes exactly the same `ready` semantics it already understands. Because membership
is `metadata.plan`, a plan can span projects (repos) inside one store.

Node metadata is flat and machine-readable, so it is queryable with JSON extraction on every
backend (`metadata_json` in SQLite, `metadata` in PostgreSQL):

| key | meaning |
| --- | --- |
| `plan` / `plan_key` | plan membership and the spec's stable node key |
| `done_means` | the **command** that proves the node done |
| `repo` | which checkout this node edits |
| `conflict_group` | nodes sharing a group must never run concurrently |
| `acceptance` | the test a reviewer runs to accept the outcome |
| `risk`, `estimate` | `low\|medium\|high\|critical` and minutes (feeds the Gantt) |

```bash
mb plan create plan.toml --dry-run     # review the draft; nothing written
mb plan create plan.toml               # write nodes + dependency edges
mb plan show checkout-v2               # DAG in topological order, with metadata and edges
mb plan gantt checkout-v2              # estimates, critical path, current/peak concurrency
mb plan ready checkout-v2              # the parallel frontier the executor consumes
```

`plan ready` is deliberately stricter than plain `ready`: a node is offered only when every
dependency is `closed`, no live claim holds it, and **no node in its conflict group is already
running** (or earlier in the same frontier). At most one node per group is offered, so a naive
dispatcher that claims the whole list cannot race two same-group edits. A group is freed when its
running node leaves `in_progress` — `review` does not block it, because the code work is done.

Specs are TOML (the repo's config language) or JSON; `deps` refer to node keys, never to issue ids
(which do not exist until the plan is written):

```toml
name = "checkout-v2"
project = "marbles"

[[nodes]]
key = "schema"
title = "Add the plan schema"
repo = "marbles"
conflict_group = "db"
risk = "high"
estimate = 30
done_means = "cargo test -p marbles plan_schema"
acceptance = "migrations apply on an empty database"

[[nodes]]
key = "api"
title = "Expose plan endpoints"
repo = "marbles"
conflict_group = "api"
estimate = 45
done_means = "cargo test --test server plans"
deps = ["schema"]
```

A node may set `project = "checkout"` when the plan spans repos. `mb create` also accepts the
metadata directly (`--plan`, `--done-means`, `--repo`, `--conflict-group`, `--risk`,
`--acceptance`, `--estimate`) for hand-authored nodes. `plan show|gantt|ready` and `plan create`
are also served as `/v1/plans.*`, so local mode and the hosted API return identical views.

A full local-mode run — draft, create, show, gantt with critical path, and the conflict-group
frontier before/after a claim — is captured in [`docs/plan-dag-demo.md`](docs/plan-dag-demo.md)
with the reproducer at [`docs/plan-dag-demo.sh`](docs/plan-dag-demo.sh) and the raw verbatim
run in [`docs/plan-dag-demo.transcript.txt`](docs/plan-dag-demo.transcript.txt).

## Identity

`marbles serve` resolves every request to a `Principal` before it reaches the
store:

- **OIDC** (multi-machine/cloud): bearer JWTs verified against your issuer's
  JWKS (`sso.fpl.dev`, Keycloak, Auth0…). `client_id`s listed in config are
  agents; user tokens are humans.
- **Static tokens** (one machine): files under `~/.marbles/tokens/` whose
  filename is the identity — `human-avery`, `agent-codex-thread-1`. Created by
  `marbles login` / `marbles agent-token`.

The rule enforced at the API: **bodies may request, credentials may assert.**
A human token cannot claim work as anyone but itself; an agent cannot sweep
other people's holds; releasing a live claim requires being its holder.

Hosted deployments set `DATABASE_URL` and `company_claim`. Each verified
company claim routes to company-scoped rows in PostgreSQL; an unscoped
credential is rejected instead of falling back to a shared database. The
checked-in `deploy/server.toml` is the FPL Auth production shape.

## Durability

The local store is SQLite in WAL mode with a single writer, `synchronous=FULL`,
and a five-second busy timeout. Every mutation runs inside `BEGIN IMMEDIATE`, so
a claim is serialized even when several projects and a sweeper write at once,
and an acknowledged claim or close survives a crash. The append-only `history`
table is the audit trail; `outbound_event` is a transactional outbox, written in
the same transaction as the mutation it describes.

```bash
mb doctor                          # integrity, WAL size, settings, row counts
mb backup ~/backups/marbles.db     # atomic VACUUM INTO snapshot
```

`backup` refuses to overwrite an existing file and checks source integrity
first. `doctor` runs `integrity_check` and `foreign_key_check`. Both are local
commands: run them on the host that owns the store. See
[`docs/durability.md`](docs/durability.md).

An optional signed webhook wakes an event-driven consumer without making it
part of the mutation transaction:

```toml
[webhook]
url = "http://alfalfad:8787/api/v1/webhooks/marbles"
secret_file = "/run/secrets/marbles-webhook"
timeout_seconds = 5
```

Every history mutation appends an outbox row in the same SQLite transaction.
The dispatcher signs the exact JSON body with HMAC-SHA256 in
`X-Marbles-Signature`, retries failures with bounded exponential backoff, and
marks only a successful 2xx response delivered. Rotate by accepting old and new
receiver secrets during a deployment, changing the mounted sender secret, then
removing the old receiver secret. Polling remains the consumer's reconciliation
path; webhook delivery is a latency optimization, not a second source of truth.

### When the server is down

A hung or unreachable tracker must not hang the fleet, and it must never look like a successful
write. Every CLI request is bounded — `--timeout-ms` (or `MARBLES_TIMEOUT_MS`, default 8000) caps
both the whole request and the connect attempt — and the client splits failures in two:

- **Reads** (`list`, `ready`, `show`, `history`, `stats`, `projects`) replay the newest response
  that server already returned, loudly: `marbles: offline (cached <time>, <age> old, from <url>)`.
  The exit code stays 0 because the data is real, just old; the banner is the staleness contract.
- **Writes** are never cached and never queued. An unavailable server exits non-zero with
  `offline: <op> not applied (no server)` — an outage cannot produce a phantom claim.

Cached bodies live in `$MARBLES_HOME/cache` (default `~/.marbles/cache`), one 0600 file per
`(server, op, request body)` keyed by SHA-256 so neither endpoints nor tokens appear in names, and
pruned to the newest 128 entries / 8 MiB. Local mode never writes this cache. This is a cache, not
replication: one server still owns the truth, clients only remember what it last told them.

## CLI cheat sheet

```bash
# Work
mb ready [--project P]                  # eligible work
mb list [--all] [--project P] [--json]
mb show <id> ; mb history <id>
mb create "Title" -p 2 -t task -d "…"
mb update <id> --status blocked --append-notes "waiting on …"
mb claim <id> [--as agent-x] [--ttl SECS]
mb touch <id> ; mb release <id>

# Delivery
mb review <id> --pr URL
mb close  <id> --pr URL --commit SHA
mb close  <id> --ack "no delivery expected: …"

# Dependencies and labels
mb dep add <child> <parent> ; mb dep rm <child> <parent>
mb label add <id> bug ; mb label remove <id> bug

# Organizing
mb folder create "Business" --slug business
mb folder tree ; mb folder move <project> <folder|none>
mb goal create "Ship v1" -p 1
mb epic create "Storage" --goal <goal-id>
mb task create "Write the WAL test" --epic <epic-id>
mb goal list ; mb goal show <goal-id>
mb burndown --project <project> --days 30 [--json]

# Projects and operations
mb init [--folder NAME] [--slug S] [--prefix P]
mb projects ; mb stats ; mb sweep
mb doctor ; mb backup <new-path>
```

## Migrating from Beads

```bash
bd export > beads.jsonl
mb import-bd beads.jsonl --project myrepo [--dry-run]
```

IDs, statuses, labels, metadata, close reasons, and dependency edges are
preserved verbatim, so references in commits, docs, and run ledgers stay valid.
The importer is re-runnable: a crash mid-import is resume, not cleanup. Rows
carry provenance (`metadata.marbles_import`) and re-encountered rows are
`unchanged`, never conflicts. The whole import aborts before writing on any
real conflict and lists the offending IDs.

`--rewrite-prefix old:new` fixes the one genuine migration hazard — two
independent stores configured with the same prefix — by renaming one store's
IDs and its dependency edges at import. IDs within one store never change.

## Layout and status

Single binary (`src/main.rs`), `axum` + bearer-token auth, PostgreSQL for hosted
operation, and `rusqlite` (WAL) for local operation and tests.

`v0.x`: interface-first. The HTTP contract and JSON shapes are what we intend to
freeze before 1.0. Legacy hosted installs without `DATABASE_URL` can still use
one physical SQLite store per OIDC company during cutover.

## Attribution

The command surface, JSON shapes, and several semantics deliberately mirror
[Beads](https://github.com/gastownhall/beads), by
[Steve Yegge](https://github.com/steveyegge) and contributors. Beads is open
source; any remaining interface debt is gratitude, not accident. The critique
above is of one component — its Dolt-backed sync layer — not of the project.

MIT. Go build it.

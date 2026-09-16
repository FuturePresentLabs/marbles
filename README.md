# Marbles

A small issue tracker for fleets of coding agents. One server, one database,
one claim race at a time.

Marbles keeps the things that made [Beads](https://github.com/gastownhall/beads)
good to work with — short stable IDs, a dependency-ready set, claims that
actually serialize agents, a CLI you can pipe into an agent — and drops the
thing we could not make work at fleet scale: a replicated, per-checkout
database synced by every machine and agent that touches it.

> **Attribution.** The command surface, JSON shapes, and several semantics here
> deliberately mirror Beads, by [Steve Yegge](https://github.com/steveyegge) and
> contributors, whose design made this shape worth copying. Beads is open
> source; any remaining interface debt is gratitude, not accident. What follows
> is a critique of one component (its Dolt-backed sync layer), not of the
> project.

## Why not Dolt

Beads' first-party storage is Dolt: a git-like database, replicated per clone,
merged on push. For humans working from one laptop and one branch, that is
reasonably invisible. For an agent swarm it inverts every cost:

- **Every clone is a writer on one lineage.** A fleet of short-lived sandboxes
  each pulls, mutates, and pushes the same database. The merge path is not the
  exception; it is the routine — and last-writer-wins per column silently
  resurrects closed issues and drops claims when sandboxes pull a stale base.
- **The sync boundary is the trust boundary, and it is everywhere.** Git hooks
  fire `bd` mutations on checkout/merge, a background push-state machine races
  the daemon, and `refs/dolt/data` is one non-fast-forward away from agents
  "fixing" each other's bookkeeping. We measured this failure mode in tokens:
  tens of thousands of dollars of agent time burned on loops where the work
  tracker's state, not the task, was the moving target.
- **Scale is the wrong axis.** A hundred repositories with per-repo Dolt
  databases is a hundred lineage graphs plus one merge engine nobody
  operating-staffed. Agents don't need local *writes*; they need fast local
  *reads* of one truth — which a server and an in-memory cache give you in
  fewer moving parts than replication ever will.

Marbles' position: **the work graph has exactly one writer per lineage.**
Agents and humans are HTTP clients of that writer. There is nothing to sync
because there is nothing replicated. Concurrency becomes what it should have
been — a row-level claim race with an answer, not a three-way merge with a
lawyer.

## The model

```
open ──(claim)──> in_progress ──(PR opened)──> review ──(merged + receipt)──> closed
```

- **Grace at creation.** A new issue is swarm-invisible for 10 minutes, and any
  edit re-defers it by 2 minutes: live human↔agent speccing sessions do not
  leak half-formed work to the fleet, but nothing is still sitting there
  untouched at dawn.
- **Claims carry identity and kind.** An *agent* claim is a minutes-long lease
  renewed by heartbeats; when it lapses, the work is silently requeued — a dead
  agent is an interruption, not a decision. A *human* claim lasts 48 **business
  hours** (weekends and nights extend the wall-clock deadline, never shorten
  it); when it lapses it **escalates** to the owner rather than being
  re-stolen.
- **`review` is not `done`.** An agent that finished writing code has produced
  something to look at, not a delivery.
- **`closed` requires evidence.** A merged commit, a PR link, or — for
  research/coordination outcomes — an explicit `--ack "no delivery expected:
  …"`. A closed issue with no receipt is a claim, and claims need receipts.
- **Dependencies gate readiness.** `ready` = open, unblocked, past grace, not
  held. Cycles are refused at edge-creation time.
- **History is append-only.** Every create, claim, renew, release, status, and
  close is an event; a bead can always explain itself (`marbles history <id>`).
- **Metadata is a first-class sidecar.** Tools attach structured receipts to
  work (`--set-metadata key=value`, `--metadata @file`, RFC-7386 shallow merge,
  null deletes) without inventing label grammars for JSON.

## Identity

`marbles serve` resolves every request to a `Principal` before it reaches the
store:

- **OIDC** (multi-machine/cloud): bearer JWTs verified against your issuer's
  JWKS (`sso.fpl.dev`, Keycloak, Auth0…). `client_id`s listed in config are
  agents; user tokens are humans.
- **Static tokens** (one machine): files under `~/.marbles/tokens/` whose
  *filename is the identity* — `human-avery`, `agent-codex-thread-1`. Created
  by `marbles login` / `marbles agent-token`.

The rule enforced at the API: **bodies may request, credentials may assert.**
A human token cannot claim work as anyone but itself; an agent cannot sweep
other people's holds; releasing a live claim requires being its holder.

## Install

From source (Rust 1.88+):

```bash
cargo install --path .            # installs `marbles`
marbles --version
```

### Local, single operator

```bash
cd my-repo
marbles init                       # writes .marbles/project.toml
marbles create "Fix the thing" -p 1
marbles ready                      # eligible work, already
marbles list --json
```

With no server URL configured, commands act directly on `~/.marbles/marbles.db`
— one file, every project on the machine, so cross-repo views are a query.

### Fleet (server) mode

```bash
mkdir -p ~/.marbles
marbles serve                      # or run it under launchd/systemd
marbles login --name avery          # prints your human token
marbles agent-token worker-1        # per-agent/sandbox tokens
export MARBLES_URL=http://127.0.0.1:7878 MARBLES_TOKEN=<token>
```

`server.toml` in the same directory configures the listen address and the OIDC
block (`oidc_issuer`, `oidc_audience`, `agent_client_ids`, `company_claim`).
The server binds loopback by default; put your TLS at the edge, same as
everything else you run. A 60-second sweeper requeues expired agent leases and
escalates expired human holds. Sandboxes get one agent token and zero file
access to the database — that's the point.

Hosted deployments set `company_store_root` and `company_claim`. Each verified
company claim is routed to `<company_store_root>/<company_id>/marbles.db`; an
unscoped credential is rejected instead of falling back to a shared database.
The checked-in `deploy/server.toml` is the FPL Auth production shape.

### Instruction files (`marbles setup`)

`marbles setup --profile maintainer` writes a marker-delimited managed block
into `AGENTS.md` (or `--target CLAUDE.md`), with the profile and a content hash
in the begin-line so drift is visible. Profiles:

- `conservative` (default): track work with `mb`; commit and push only when the
  repo or user says so.
- `maintainer` (the FPL fleet): agents commit verified work early and often,
  rebase rather than rot, never leave a dirty worktree without a live claim,
  move marbles to `review` with a PR link, and never close — closing is the
  delivery loop's job with merge evidence. Push credentials and agent processes
  do not cohabit.

## Migrating from Beads

```bash
bd export > beads.jsonl            # one store from inside its git checkout
mb import-bd beads.jsonl --project myrepo [--dry-run]
```

Ids, statuses, labels, metadata, close reasons, and dependency edges are
preserved verbatim, so every reference in commits, docs, and run ledgers stays
valid. The importer is written to survive being interesting:

- **Re-runnable.** A crash mid-import is resume, not cleanup: rows are marked
  with provenance (`metadata.marbles_import`) and re-encountered rows are
  `unchanged`, never conflicts. The whole thing aborts *before writing* on any
  real conflict and lists the offending ids.
- **Prefix surgery.** `--rewrite-prefix old:new` fixes the one genuine
  migration hazard — two independent stores that were configured with the same
  prefix — by renaming one store's ids (and their dependency edges) at import.
  This is the fix for a misconfiguration; ids within one store never change.
- **A store is a store.** `marbles` treats each project as an independent
  tracker — the successor to one `.beads` database, not a database *of*
  databases. A portfolio (e.g. AlfAlpha's Pedalkernel project referencing both
  `pedalkernel` and `pedalkernel-pro` trackers) composes stores at read time.
- **Source state lands as-is.** If the export says a bead closed while its
  blocker is still open, the importer refuses to *reorder your history* into
  its own invariants: it closes with an explicit exception note and reports
  `exception_closed`. Status names it doesn't recognize abort rather than
  guess. `_type:memory` records (beads wisps) are skipped with a count.

## Layout & status

Single binary (`src/main.rs`), `axum` + `rusqlite(WAL)` + bearer-token auth.
v0.x: interface-first; the HTTP contract and JSON shapes are what we intend to
freeze before 1.0. Hosted mode uses one physical SQLite store per OIDC company.
Postgres remains a possible future engine; the store is one module precisely so
swapping engines is a deploy, not a rewrite.

MIT. Go build it.

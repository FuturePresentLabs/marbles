<p align="center">
  <img src="docs/img/marbles-logo.svg" alt="Marbles — a small issue tracker for fleets of coding agents" width="900">
</p>

<p align="center">
  <b>One owner per lineage. One claim race at a time.</b><br>
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
per lineage.** Humans and agents talk to that writer — the `mb` spore over a
`soil:records` book, or the local CLI over one SQLite file. Nothing is
replicated, so there is nothing to merge.

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

There are two interfaces, and they are installed separately.

**`mb` — the tracked-work interface.** A wasm spore over a `soil:records` book. This is the one
to use; it is how a book syncs between machines and the only interface that will grow.

```bash
scripts/install-records-cli.sh --records-book personal/<you>/marbles
```

That builds `mb.wasm` for `wasm32-wasip2`, installs it as a command package, and asks
`soil-command shims` to generate `mb` — see [`docs/records-backend.md`](docs/records-backend.md).
It needs a `soil-command` from a Soil checkout on `PATH`.

**`marbles` — the local CLI.** The native binary. It is now only for work that belongs to the
machine rather than to the book: `init` and `setup` (they write `.marbles/project.toml` and the
`AGENTS.md` block), `import-bd` (it reads a Beads export from the filesystem), `backup` (the host
owns the records book), and `store`/`remote`/`remotes` (native Ed25519 store identity and the
on-disk remote pin registry). Everything else — `create`, `update`, `claim`, `close`, `plan`,
`portfolio`, `burndown`, `folder`, `goal`/`epic`/`task`, `project rm`, `doctor` — runs on both
interfaces. It reads and writes its own SQLite file at `~/.marbles/marbles.db`, so a project must
not be split across both.

From a tagged release (Linux x86-64 and ARM64, macOS ARM64):

```bash
# check it against the .sha256 beside it first
tar -xzf marbles-<version>-<target>.tar.gz -C ~/.local/bin marbles
```

The archive contains `marbles` only. It used to ship a second copy named `mb`; that alias is
gone, because `mb` now resolves to the records-backed spore and a native binary under that
name silently shadowed it.

From source (Rust 1.88+):

```bash
cargo install --path .          # installs `marbles`
```

With Homebrew, from a tap over this repository:

```bash
brew tap futurepresentlabs/marbles https://github.com/FuturePresentLabs/marbles
brew install futurepresentlabs/marbles/marbles
```

The formula installs the published archive for the platform, so it tracks tagged releases
rather than `main` — v0.1.0 today, which predates the `soil:records` backend.

On an FPL workstation, `fpl install marbles` selects and verifies the right archive.

## Quickstart

### Over `soil:records` (the fleet interface)

```bash
cd my-repo
mb create "Fix the thing" --project my-repo -p 1   # new issue, visible after grace
mb ready                                          # unblocked, unclaimed, past grace
mb claim <id>                                     # take it
mb review <id> --pr <url>                         # PR is open; not done
mb close <id> --pr <url> --commit <sha>           # merged; done
```

The book lives under `~/.local/share/fungos/records`; which book and collection the command
sees is fixed by its policy at `~/.config/fungos/commands/mb.env`, not by a flag. Every answer
is JSON, and `--url`/`--token` are refused by name: the command talks to the store the host
opened, not to an endpoint of the caller's choosing.

### Local, one operator

```bash
cd my-repo
marbles init                             # register this project, install agent instructions
marbles create "Fix the thing" -p 1
marbles ready
marbles claim <id>
```

Commands act directly on `~/.marbles/marbles.db` — one file, every project on the machine.
Cross-repo views are just queries.

## Read-only remotes

A remote store is a **pin**, not a project. `marbles remote add` verifies another store's signed
`marbles.store/1` descriptor and pins its `(store_id, key_id, public_key)` locally; `marbles
remotes` then shows the separate, read-only **Remotes workspace**:

```bash
marbles remote add acme ./acme-descriptor.json
marbles remotes                       # name, store_id, key_id, endpoint, named freshness
marbles remote assertions acme        # per-issue verified state + fresh/stale/unknown/unverifiable
```

Descriptors come from a file or stdin. Trust comes from the pinned key, never from where the
bytes were found.

Remote stores never enter the local folder tree, `marbles projects`, `marbles portfolio`, or
`marbles ready`, and no read/list path creates a local project or issue row for them.
`marbles claim`, `marbles update --assignee`, and `marbles close` on a remote reference fail with
a named `remote refs are read-only: <store_id>/<issue>` error rather than falling through to
local work. A missing registry prints `no remotes pinned` and exits 0; a corrupt one fails
closed.

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
marbles folder create "Business" --slug business
marbles folder move acme business
marbles folder tree
mb projects                       # includes each project's folder
```

### Goals → epics → tasks

A **goal** is a project-local root. An **epic** is a body of work under a goal;
a **task** is an executable unit under an epic. All three are ordinary issues,
so they inherit claims, history, labels, dependencies, review, and evidence.

```bash
marbles goal create "Ship v1" -p 1
marbles epic create "Storage" --goal <goal-id>
marbles task create "Write the WAL test" --epic <epic-id>

marbles goal list                      # descendant rollups
marbles goal show <goal-id>            # tree + progress bar
```

Hierarchy is project-local; use an ordinary dependency edge for a blocker in
another project. Dependencies remain cross-project and cross-folder.

### Burndown

```bash
marbles burndown --project acme --days 30
marbles burndown --goal <goal-id> --days 14
marbles burndown --epic <epic-id> --days 7 --json
```

The series reports daily `scope`, `done`, and `remaining`, plus burn rate and a
projected days-to-zero when there are closes in the window.

### Portfolio

`marbles portfolio` rolls folders → projects → goals into one read-only view: ready
work, WIP, review/blocked counts, stale work, oldest-ready age, and per-project
burn rate.

```bash
marbles portfolio
marbles portfolio --folder business --days 30 --stale-days 14
marbles portfolio --json
```

See [`docs/organizing-work.md`](docs/organizing-work.md) and
[`docs/portfolio.md`](docs/portfolio.md) for the data model and field meanings.

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
marbles plan create plan.toml --dry-run     # review the draft; nothing written
marbles plan create plan.toml               # write nodes + dependency edges
marbles plan show checkout-v2               # DAG in topological order, with metadata and edges
marbles plan gantt checkout-v2              # estimates, critical path, current/peak concurrency
marbles plan ready checkout-v2              # the parallel frontier the executor consumes
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
done_means = "cargo test --test plans"
deps = ["schema"]
```

A node may set `project = "checkout"` when the plan spans repos. `mb create` also accepts the
metadata directly (`--plan`, `--done-means`, `--repo`, `--conflict-group`, `--risk`,
`--acceptance`, `--estimate`) for hand-authored nodes. `plan show|gantt|ready` and `plan create`
are local reads and writes over the store.

A full local-mode run — draft, create, show, gantt with critical path, and the conflict-group
frontier before/after a claim — is captured in [`docs/plan-dag-demo.md`](docs/plan-dag-demo.md)
with the reproducer at [`docs/plan-dag-demo.sh`](docs/plan-dag-demo.sh) and the raw verbatim
run in [`docs/plan-dag-demo.transcript.txt`](docs/plan-dag-demo.transcript.txt).

## Identity

The store has an Ed25519 identity: `marbles store identity init` creates it,
`marbles store descriptor` prints the signed `marbles.store/1` JSON, and `marbles store verify`
checks one offline against a pinned `(store_id, key_id, public_key)`. Rotating a key keeps the
old one verifying for a named overlap window.

That identity is what `marbles remote` pins when it reads another store. Identity proves *which
store wrote a row*; a claim is authorized by the store that owns it, not by a bearer credential.

## Durability

The local store is SQLite in WAL mode with a single writer, `synchronous=FULL`,
and a five-second busy timeout. Every mutation runs inside `BEGIN IMMEDIATE`, so
a claim is serialized even when several projects and a sweeper write at once,
and an acknowledged claim or close survives a crash. The append-only `history`
table is the audit trail.

```bash
marbles doctor                          # integrity, WAL size, settings, row counts
marbles backup ~/backups/marbles.db     # atomic VACUUM INTO snapshot
```

`backup` refuses to overwrite an existing file and checks source integrity
first. `doctor` runs `integrity_check` and `foreign_key_check`. Both are local
commands: run them on the host that owns the store. See
[`docs/durability.md`](docs/durability.md).

There is no server and no event delivery. `outbound_event` remains in the schema
as part of Marbles' own audit trail; nothing delivers it, and no consumer should
wait on a webhook that will not arrive. A fleet book is read over `soil:records`.

## CLI cheat sheet

```bash
# Work — `mb` (over soil:records)
mb ready [--project P]                  # eligible work
mb list [--all] [--project P] [--json]
mb show <id> ; mb history <id>
mb create "Title" -p 2 -t task -d "…"
mb claim <id> [--as agent-x] [--ttl SECS]
mb touch <id> ; mb release <id>

# Delivery
mb review <id> --pr URL
mb close  <id> --pr URL --commit SHA
mb close  <id> --ack "no delivery expected: …"

# Dependencies and labels
mb dep add <child> <parent> ; mb dep rm <child> <parent>
mb label add <id> bug ; mb label remove <id> bug

# Local CLI only — `marbles` (a separate SQLite store)
marbles init [--folder NAME] [--slug S] [--prefix P]
marbles setup [--profile maintainer] [--target AGENTS.md] [--remove]
marbles import-bd beads.jsonl --project myrepo [--dry-run]
marbles backup <new-path>
marbles store identity init | show | rotate
marbles remote add|list|show|assertions|remove|rotate
marbles remotes
```

Everything else is on both interfaces. The records Spore answers the same commands with the
same JSON shapes:

```bash
mb update <id> --status blocked --append-notes "waiting on …"
mb folder create "Business" --slug business
mb folder list ; mb folder tree ; mb folder move <project> <folder|none>
mb goal create "Ship v1" -p 1
mb epic create "Storage" --goal <goal-id>
mb task create "Write the WAL test" --epic <epic-id>
mb goal list [--all] ; mb goal show <goal-id>
mb burndown --project <project> --days 30
mb portfolio [--folder <slug>] [--days N] [--stale-days N]
mb plan show|gantt|ready <plan>
mb plan create <spec|-> [--dry-run]
mb project rm <slug>
mb doctor
```

## Migrating from Beads

```bash
bd export > beads.jsonl
marbles import-bd beads.jsonl --project myrepo [--dry-run]
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

Two targets. `mb` (`src/bin/mb.rs`) is a wasm spore over `soil:records`, built with
`--target wasm32-wasip2 --no-default-features --features soil`. `marbles` (`src/main.rs`) is the
native CLI over a local `rusqlite` (WAL) store.

The HTTP server, its auth stack, the PostgreSQL backend, the webhook/Unibus outbox, and the
Canvas viewer have been removed. There is one writer per lineage, and it is a store, not a
process.

`v0.x`: interface-first. The JSON shapes are what we intend to freeze before 1.0. The records
spore now answers the whole read surface (`plan`, `portfolio`, `burndown`, `folder`,
`goal`/`epic`/`task`, `update`, `project rm`, `doctor`) as well as the agent workflow; what stays
native is only the work that needs the host's filesystem or a native store identity (`init`,
`setup`, `import-bd`, `backup`, `store`, `remote`/`remotes`).

## Attribution

The command surface, JSON shapes, and several semantics deliberately mirror
[Beads](https://github.com/gastownhall/beads), by
[Steve Yegge](https://github.com/steveyegge) and contributors. Beads is open
source; any remaining interface debt is gratitude, not accident. The critique
above is of one component — its Dolt-backed sync layer — not of the project.

MIT. Go build it.

# Porting Marbles to a records backend

**Status: cut over (2026-11).** `mb` on a records-backed host is the spore, not the
native binary. `~/.local/bin/mb` is the host shim; the commands are a wasm
component running against `soil:records`; the native `marbles` binary is still
installed for the operations the spore does not have (`init`, `import`, `plan`,
`portfolio`, `serve`).

This document covers the record model (unchanged since the spike), the CLI
surface, the install, the one-time consolidation, and the sync topology.

```text
zsh / mush  ->  mb (host shim)  ->  soil-command  ->  [ wasm mb ]
                                       |                   |
                                       |                   +-- soil:records (guest)
                                       +-- RecordsBookHost ----+
                                                               |
                                             ~/.local/share/fungos/records
                                                book personal/<you>/marbles
```

Nothing in that path is Marbles-specific except the collection name. The host
names the records root and book; the Spore names only `marbles` and its schemas.

## Installing the records CLI

```sh
scripts/install-records-cli.sh                    # book seeded from Mush's config
scripts/install-records-cli.sh --records-book personal/avery/marbles
```

That builds `mb.wasm` for `wasm32-wasip2`, installs it as the `mb` command
package under `~/.local/share/fungos/commands/mb`, installs
`scripts/mb-records.sh` as `mb`, and writes the binding to
`~/.config/fungos/marbles/records.env`. It needs `soil-command` on PATH (or
`--soil-command`), because that is the native runner that instantiates the
component; it does not build Soil.

Reverting is one line: `ln -sf marbles ~/.local/bin/mb`.

## The CLI surface

The grammar is [src/records_cli.rs](../src/records_cli.rs) and is tested on the
host by [tests/records_cli.rs](../tests/records_cli.rs), so the wasm target is
not the only place the surface is written down.

```text
claim close create dep history label list projects ready release review
show stats supersede sweep touch
```

Rules that matter:

- A missing project is **every project**, not a project called `default`. A
  project scopes a query; it is not required identity. `create` is the one
  command that needs one, and says so.
- `show` prints a one-element array, because Beads clients and `soil-marbles`
  parse `show --json` as a list.
- `--json` is accepted anywhere; every answer is already JSON.
- `--url`, `--token`, `--token-file` are refused by name. This build talks to
  the store the host opened, not to an endpoint of the caller's choosing.
- `MARBLES_PROJECT` / `MARBLES_PREFIX` / `MARBLES_ACTOR` are the only
  environment it reads. An empty value means unset.

The native binary keeps the rest: `init`, `import`, `plan`, `portfolio`,
`burndown`, `folder`, `goal`/`epic`/`task`, `remote`/`remotes`, `store`, `backup`,
`doctor`, `serve`, `login`. Those write to a different store, so a project must
not be split across both.

## Consolidating machines

`soil-marbles-import` copies one SQLite store into the records book. Running it
twice into one book consolidates two machines, but only if the second run is
neither destructive nor order-dependent:

```sh
mb-records-import --from neo.db    --root ~/.local/share/fungos/records \
  --book personal/avery/marbles --newest-wins
mb-records-import --from studio.db --root ~/.local/share/fungos/records \
  --book personal/avery/marbles --newest-wins
```

- `--newest-wins` keeps the revision with the later `updated_at`. Without it the
  last import overwrites the first, so which machine is "right" depends on the
  order you happened to run them.
- Dependencies are skipped when already present (an edge cannot change), so a
  re-run writes nothing new.
- Event ids continue from the highest one in the book, so two imports cannot
  share a `seq`.
- **History is opt-in** (`--history`). A fleet store carries hundreds of
  thousands of audit rows, and `mb` seeds its event counter by scanning the
  collection on every invocation — loading the archive would make every command
  pay for it. The SQLite file remains the archive.

## The record model

The engine in [src/records.rs](../src/records.rs) is append-only: schemas,
revisions, tombstones, and a live index. The wasm backend
([src/soil_engine.rs](../src/soil_engine.rs)) maps one Marbles record type to one
id inside the `marbles` collection of the host's book, and uses
`compare-and-swap` for claims so two invocations cannot both win one.


## 1. Concrete mapping: tables → record schemas

Marbles is expressed as a set of record types. Each record type has a **schema** (name, key
columns, typed data columns, a data column) and an **append-only trail of revisions**. Current
state is the *latest* revision for a key; a **tombstone** revision (deleted=true) makes the key
disappear from the live index while retaining the history trail — this is how Marbles' "deletes"
become non-destructive.

| SQLite table            | Record type       | Key                         | Notes |
|-------------------------|-------------------|-----------------------------|-------|
| `issue`                 | `issue`           | `["id"]`                    | Status, labels, assignee, evidence, metadata, timestamps all live in the JSON data column. |
| `project`               | `project`         | `["slug"]`                  | root/prefix/created_at. |
| `folder`                | `folder`          | `["slug"]`                  | |
| `project_folder`        | `project_folder`  | `["project"]`               | |
| `dep`                   | `dependency`      | `["child","parent"]`        | Directed edge child → depends_on parent. |
| `remote_dep`            | `remote_dep`      | `["child","canonical_ref"]` | Cross-store dependency; keyed by canonical ref. |
| `history` + `outbound_event` | `marbles_event` / `outbound_event` | `["seq"]` | One `seq` feeds both, mirroring SQLite's `last_insert_rowid()` feeding both inserts in `log_event`. |
| `store_meta`            | `store_meta`      | `["key"]`                   | Stores the store id under key `store_id`. |

### Revisions, tombstones, and the "current" view

- `Engine::append(name, key, data, deleted, at)` pushes a `Revision { seq, key, data, deleted }`
  onto a per-type `VecDeque`, bumps a single global `seq`, and maintains a `BTreeSet<Key>` live
  index.
- `Engine::current(name, key)` returns the **latest revision for that exact key**; if that latest
  revision is a tombstone the key is reported absent (the older revisions remain as history). This
  is the crux: a tombstone is only "gone" if it is the *most recent* revision of the key — the
  naive "last non-deleted anywhere" reading reintroduces deleted rows.
- `Engine::history_of(name, key)` returns the full trail including tombstones, in `seq` order —
  this is Marbles' append-only `history`.
- `Engine::keys(name)` returns only live keys; `keys(ISSUE)` is what `list`/`ready`/`stats`/
  `prometheus_metrics` iterate, exactly as the SQLite queries scan the `issue` table's live rows.

### Indexes

The SQLite secondary indexes (`ready` eligibility, `dep` reachability, group-by status) are not
materialised as separate structures. They are computed on read over the live key sets:

- **ready**: filter live `issue` keys by the eligibility predicate (open, past `available_at`, no
  non-closed local parent, lease expired/unclaimed) — the same predicate as the SQLite `ready`
  SQL, evaluated in Rust.
- **dep reachability**: BFS over `dependency` keys (`reachable`).
- **stats / prometheus**: single pass over live `issue` keys grouping by project/status.

This is the honest records model: there is no out-of-band index to keep consistent; the trail is
the source of truth and derived views are recomputed. For Marbles' scale this is fine and it is
exactly what the contract test exercises.

## 2. The hard parts

### Ready/claim lease races (single-writer CAS)

The SQLite `claim` is a compare-and-set guarded by the single connection. The adapter reproduces
this with a `Mutex<Inner>` — the whole engine is behind one lock, so every claim/touch/release/sweep
serializes exactly like the one SQLite connection. `claim` re-reads the current revision, and a
**live foreign claim returns `AlreadyClaimed(id, holder)`** — it never silently steals. An expired
lease (latest revision's `expires_at <= at`) is claimable by the next agent. The same holder
renewing is not a conflict. `touch` renews only for the recorded holder; a different assignee gets
`Error::Bad`.

### Human leases and the business-hours calendar

Human TTLs are business-hours deadlines, not wall-clock. The adapter delegates to
`crate::time::WorkWeek` (`human_expiry`, `deadline`) rather than re-implementing the calendar, so
the `with_week(...)` test hook behaves identically to `Db::with_policy`.

### Dependencies and readiness

`add_dep` rejects self-cycles and reachability cycles (BFS over `dependency` keys), matching
`Db::add_dep`. `ready` materialises the same eligibility predicate: a child with an open or
`review` parent is not ready; closing the parent unblocks it. Cross-project parents are allowed as
dependency edges (but `create`'s *hierarchy* `parent` stays project-local, with the exact SQLite
error text preserved).

### Events / outbox / history

`log_event` appends one `marbles_event` and, when the issue exists, one `outbound_event` sharing
the same `seq`. `pending_events`, `mark_event_delivered`, and `mark_event_failed` mutate the
outbox revision state (attempts, next_attempt_at, last_error, delivered_at) exactly as the SQLite
outbox does, so webhook delivery/retry semantics are preserved.

### Search

There is no dedicated search index. Listing/filtering (`list`, `ready`, `stats`) is a linear scan
over live issue keys with the SQLite predicates applied in Rust. This is the natural records model;
a real records host would offer query predicates over the same trail.

### Company / project partitioning

The SQLite `CompanyStores` gives each company a physically separate DB file. The records model
expresses the same isolation via **projects** (keyed namespaces) and can be partitioned per-company
by giving each company its own `Adapter`/`Engine` — the per-company split is an ownership boundary
around the same engine, not a new schema. `list(Some(project))` and `ready(Some(project))` reproduce
the project-scoped queries; `ready(None, …)` merges across projects (cross-repo coordination).

### SQLite import

Because the adapter speaks the `Store` trait, an import path is: open the SQLite `Db`, read its
tables, and replay them as `append` calls (issue rows → issue revisions, dep rows → dependency
revisions, history/outbox rows → the two event trails with a shared `seq`). The append-only shape
maps cleanly; tombstones are only needed for rows the source deleted before import (Marbles rarely
deletes). This is a follow-up; see the list at the end.

### API compatibility

The adapter implements the full `Store` trait (`ensure_project`, `projects`, `ensure_folder`,
`set_project_folder`, `folder_view`, `pending_events`, `mark_event_delivered/failed`, `create`,
`get`, `list`, `ready`, `update`, `add_label`, `remove_label`, `add_dep`, `remove_dep`, `claim`,
`touch`, `release`, `sweep`, `close`, `supersede`, `history`, `stats`, `prometheus_metrics`).
`drop_project` is not part of the trait (neither is it on `Db`'s trait) and is exposed as an
inherent method that cascade-tombstones every row touching the project. Error variants and the key
error texts (`parent … hierarchy is project-local`, `refusing to close … done means delivered`,
`AlreadyClaimed`) are preserved.

## 3. What the contract test protects

`tests/adapter_contract.rs` drives the adapter through the `Store` trait and asserts, on the
records backend, the same invariants the SQLite `tests/storage.rs` protects:

- create/get/list round-trip and project isolation
- grace window + edit-deferral hiding fresh work from the swarm
- dependency-driven readiness and cycle refusal at creation
- live-claim CAS (`AlreadyClaimed` names the holder), expired-lease re-claim
- sweep requeues expired agents but only escalates expired humans (and never re-steals human work)
- evidence-required close, review as the correct intermediate move, ack for non-code outcomes
- supersede closes with a pointer, not a claim
- append-only history + outbox delivery
- Prometheus metrics derive latency/capacity ages **without ever labelling by issue id**

## The wasm build

The `soil` Cargo feature is optional and wasm-only:

```sh
cargo build --release --target wasm32-wasip2 --no-default-features --features soil --bin mb
```

It adds `soil-sdk-records` from the `fungOS-linux/soil` repository and switches
`records::Engine` to `SoilEngine`, which maps one Marbles record type to one
id in the `marbles` collection and uses `compare-and-swap` for claims.
Native/default builds are unchanged.

The wasm library surface is `adapter`, `records`, `records_cli`, `store`, `time`,
`types` and `soil_engine`. The native CLI and server modules remain behind
`cfg(not(target_arch = "wasm32"))`. `bin/mb.rs` is the only wasm binary: it parses
with `records_cli` and translates the result into `Store` calls.

## Sync

The book syncs as an ordinary `fungos-records` book over `soil-sync-host`. Two
facts decide whether it works, and neither is obvious from the config:

- **The permit fixes the roles.** A retained Mycelium permit names an initiator
  and a responder; a responder that also dials out is not covered by it, and its
  sessions fail authentication. Configure the peer list on the *initiator* only
  and leave the responder's `peers` empty — it serves, and one signed direction
  carries both pull and push.
- **The first exchange sends the whole book in one frame.** `records.sync.ops.v1`
  answers a pull with every op the caller lacks, and the permit bounds the frame
  (16 MiB by default). A book of a few thousand issues fits; a book carrying the
  event archive does not, and the failure is a send timeout rather than a clear
  size error. This is why history is not imported (above) and why the first sync
  of a large book is a real limitation, not a slow path.

Verify a sync with a write rather than a log line: `mb --project probe create ...`
on one machine, then look for `issue/["probe-..."]` in the other machine's
`records.sqlite3`.

## Follow-ups

1. **Chunk `records.sync.ops.v1`.** The ops response is unbounded; a page plus a
   `more` flag would let any book bootstrap under any permit bound.
2. **Parity for the remaining commands.** `plan`, `portfolio`, `burndown`,
   `folder`, `goal`/`epic`/`task` are reads over the same records and could move
   to the spore; today they require the native binary and therefore a second
   store.
3. **Event head pointer.** `SoilEngine::max_event_seq` scans the whole collection
   on every invocation to avoid reusing an id. A `store_meta` head written under
   the same CAS as the event would make history importable again.
4. **`outbound_event`, `remote_dep`, `folder` import.** The importer covers
   projects, issues, dependencies and (opt-in) history. The rest of the SQLite
   tables are unported.
5. **Fetch a record type in one call.** Measured medians against a
   5,959-issue / 1,638-edge book: `show` 0.08 s, `list --project` 0.20 s,
   `list --all` 0.25 s, `ready` 0.20 s — where the native SQLite path does the
   same `list --all` in 0.04 s. The round trips are gone; what is left is the
   pages. A scan is capped at 512 records per call (`MAX_PAGE` in the host), so
   reading a type costs `ceil(n/512)` boundary crossings and `list --all` makes
   19 of them. Raising the cap, or keeping an index collection so a
   project-scoped query reads only its own issues, is the next order of
   magnitude.
6. **Query push-down.** `ready`/`list`/`stats` filter in Rust over that full key
   scan; a host that offers indexed predicates would make a fleet-sized book
   cheaper.


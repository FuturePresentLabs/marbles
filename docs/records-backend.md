# Porting Marbles to a records backend — spike design

This document records a **spike**: an investigation of whether Marbles (`mb`) can run on a
`fungos-records` / `soil:records` backend instead of (or alongside) the SQLite `Db`. It is not a
migration and does not replace production SQLite behaviour.

The working code is in:

- `src/records.rs` — a small **append-only records engine** (schemas, revisions,
  tombstones, live index) that can also write every revision through to a JSONL trail file and
  replay it on open, standing in for a records host's durable store.
- `src/adapter.rs` — an `Adapter` that implements the existing `Store` trait on the engine. It can
  be opened in memory (`open`/`in_memory`) or durable (`open_file(path)`), the latter replaying the
  trail so a store survives a drop/reopen — the records analogue of `CompanyStores` reopening a
  company DB file.
- `tests/adapter_contract.rs` — a contract test that drives the adapter through the same `Store`
  API the SQLite `Db` exposes and asserts the Marbles invariants survive.
- `tests/records_persistence.rs` — proves the durable adapter round-trips issues, labels, history,
  and the outbox across a drop/reopen.

Run them with:

```
cargo test --test adapter_contract
cargo test --test records_persistence
```

All tests pass and the existing SQLite suite is unaffected.

## Why an in-memory engine, not a live `soil:records` client

There is no confirmed `fungos-records` / `soil:records` host API available in this tree:
`fungos-records` is absent from crates.io and the local `fpl` registry, and Soil is documented as
an early design target (B3 depends on a future Soil host). Rather than fake an integration, the
spike builds a faithful **records model** in `records.rs` that mirrors the semantics a records host
would provide (append-only revisions, keys, tombstones, no destructive deletes) and proves that
Marbles can be expressed on top of it while preserving every invariant the SQLite store protects.
The `Adapter` sits behind the existing `Store` trait, so swapping in a real records host later is a
matter of re-pointing `Adapter`'s storage, not rewriting the domain logic.

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
deletes). This is a follow-up, not part of the spike.

### API compatibility

The adapter implements the full `Store` trait (`ensure_project`, `projects`, `ensure_folder`,
`set_project_folder`, `folder_view`, `pending_events`, `mark_event_delivered/failed`, `create`,
`get`, `list`, `ready`, `update`, `add_label`, `remove_label`, `add_dep`, `remove_dep`, `claim`,
`touch`, `release`, `sweep`, `close`, `supersede`, `history`, `stats`, `prometheus_metrics`).
`drop_project` is not part of the trait (neither is it on `Db`'s trait) and is exposed as an
inherent method that cascade-tombstones every row touching the project. Error variants and the key
error texts (`parent … hierarchy is project-local`, `refusing to close … done means delivered`,
`AlreadyClaimed`) are preserved.

## 3. What the spike proves

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

## Follow-ups (not done in this spike)

1. **Real host wiring.** Replace the in-memory `Engine` storage inside `Adapter` with a
   `soil:records`/`fungos-records` client once a confirmed host API exists; keep the `Store`
   delegation unchanged.
2. **Persistence.** The engine now writes every revision through to a JSONL trail and replays it on
   `open_file`, so a store survives a drop/reopen locally (`tests/records_persistence.rs`). A real
   records host provides the durable, append-only store directly; the follow-up is re-pointing the
   trail at that host rather than a local file.
3. **Import/replay.** Build the SQLite → records replay (see §"SQLite import") and a `Db`→`Adapter`
   swap behind a feature flag, without removing the SQLite path.
4. **Query push-down.** Where a host offers indexed predicates, push `ready`/`list`/`stats` filters
   down instead of scanning, for large stores.
5. **`CompanyStores` parity.** The durable `open_file` + reopen mechanism is now in place; the
   follow-up is wiring per-company `Adapter`/`Engine` instances to separate storage
   roots and add reopen/persistence tests mirroring the SQLite company-store tests.

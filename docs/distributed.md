# Distributed clients (and why there is no gossip yet)

Marbles is intentionally a **single-writer** work graph: one authoritative store, and every human
or agent talks to it over HTTP. That is a design choice, not an unfinished replication feature.
The critique of per-checkout replication is in the README: last-writer-wins rows resurrect closed
issues and drop claims.

This note records what "better in a distributed context" means in practice, what is implemented,
and what would justify the much larger multi-writer design.

## What already works across hosts

| Concern | Current behavior |
| --- | --- |
| Slow/hung server | Every API request is bounded by `--timeout-ms` / `MARBLES_TIMEOUT_MS` (default 8000 ms), with a separate connect cap. |
| Read outage | `list`, `ready`, `show`, `history`, `stats`, and `projects` replay the newest cached response with a loud `offline (cached … old, from …)` banner. The data is old, not invented. |
| Write outage | Writes are never cached or queued. They exit non-zero with `offline: <op> not applied (no server)`. An outage cannot produce a phantom claim. |
| OIDC refresh | Refresh is bounded by the same timeout. `invalid_grant` is terminal and prints `re-login required: … run marbles login`, not a hang or a raw IdP error. A refresh response that omits a rotated refresh token keeps the existing one. |
| Identity | Credentials assert the actor; request bodies only request. A client cannot claim as another principal. |

The cache is a cache, not a replica. One server still owns the truth; clients only remember what it
last told them.

## What is deliberately not built

A signed event log gossiped over Mycelium, with Unibus for live fan-out, would give local-first
writes and cross-issuer convergence. It is also an L-sized protocol: causal ordering, signed
device keys, quorum-committed lease terms, compaction, conflict UX, and divergence debugging.

The design review reached a clear verdict:

- Today's failures are **availability and auth**, not multi-writer contention.
- The S-sized fix is: a resilient client (cache + bounded requests + OIDC refresh), plus a
  PostgreSQL primary/replica and the existing outbox. Build that first.
- The gossip event log is justified only when a peer must **mutate** marbles while disconnected
  **and** peers under different OIDC issuers must converge without a shared control plane. Absent
  that requirement, it adds divergence risk for no current benefit.

So the current direction is: **keep one writer, make clients degrade honestly, and do not build a
second source of truth.**

## What is still on the S list

- **PostgreSQL primary + synchronous replica.** Infrastructure work, and it is part of the held
  rollout decision (SQLite vs managed PostgreSQL); not something the client code can decide.
- **Backup/restore and integrity drills.** `mb backup` / `mb doctor` cover the local store; hosted
  PostgreSQL should use its own PITR/backup mechanism.
- **More distributed read surfaces** if needed: a hosted read relay or snapshot service can be
  added without changing write ownership.

## When to revisit gossip

Revisit the L design only if all of these are true:

1. A real workflow requires status/close/review mutations on a disconnected peer.
2. The peers use different identity issuers and cannot share a control plane.
3. Read-only degradation plus a central writer has been tried and is insufficient.

Until then, the correct distributed improvement is the boring one: bounded calls, honest stale
reads, terminal re-login errors, and a durable authoritative store.

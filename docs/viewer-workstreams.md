# Marbles views and Canvas viewer

Status: workstreams A, B0/B1, and the first B2 Canvas adapter are implemented; B3 (Soil) remains. This
note records the shared contract and the staged plan. It does not change the work model: every view is a read-only projection over the same
issue graph. No view owns state, invents values, or creates a second source of truth.

## Implemented now

- `POST /v1/views.dashboard`, `/v1/views.scrum`, and `/v1/views.gantt` are read-only, share the
  `ViewScope` filters, and are in `client::READ_OPS`.
- The earlier `/view` HTML shell has been retired. The viewer is Canvas: a thin host fetches the
  read-only view API and runs a no-import wasm projection; Marbles serves data, not a second UI.
- The Gantt view reuses the plan scheduler, reports `estimate_source: "metadata" | "default"`,
  excludes goals and closed work by default, and fails closed on a cycle.
- `marbles-canvas-sync` (Rust host, `src/bin/marbles-canvas-sync.rs`) fetches `views.dashboard`, runs the no-import `viewer/marbles-view` wasm projection, and applies the result to the `marbles-live` Canvas workspace. It
  refreshes it on an interval. It reads the owner token afresh each cycle, refuses redirects,
  keeps the last good workspace on a failed fetch, and never writes tracker state.

## What exists today

- The server exposes one read operation per store operation (`/v1/issues.list`, `/v1/stats`,
  `/v1/plans.gantt`, …). A browser client can already read everything, but it needs several
  round trips and client-side aggregation to render a dashboard.
- `report.rs` already computes portfolio rollups, goal progress, and burndown. `plan.rs` already
  computes a plan DAG, critical path, slack, and concurrency. Both are local-only report
  functions today; only the plan view is exposed over HTTP.
- `marbles.fpl.dev` is configured as the Agora One service on port `7878`, with application
  bearer/OIDC auth. From this workstation the name resolves but HTTPS timed out; the saved local
  client URL (`http://100.80.85.86:7878`) also timed out. Treat deployment reachability as an
  infrastructure check, not evidence that a viewer can or cannot be added.
- Mycelium DNS can publish a service name such as `marbles.<host>.<site>.myc` when a fresh
  application-service lease exists. DNS does not supply a port, certificate, ingress, or auth.
  A DNS name alone does not make the viewer reachable.
- Canvas already has a browser renderer (`canvas-web`) and a durable `WidgetSpec` vocabulary.
  Its browser mode is currently a view-only mirror/demo, not a general remote-data client.
- Soil is an early design record, not a runtime. Its target matrix is native/kiosk/TV, not the
  browser. A browser viewer must not block on Soil.

## Deployment decision

Marbles serves the authenticated view APIs only. The viewer is a separate Canvas host: it fetches
the API with the owner credential, runs the no-import wasm projection, and applies the workspace
through the owner control socket. There is no `/view` ingress and no second UI in the Marbles
server.

The viewer is read-only. Claims, close/review, and other mutations stay in the CLI.

## Workstream A — view APIs

Add read-only operations to `src/api.rs`, backed by reusable functions in `src/report.rs` and
`src/plan.rs`. Every operation is added to `client::READ_OPS` so offline replay keeps working.

### `POST /v1/views.dashboard`

One bounded aggregate for the landing screen. Filters:

```json
{ "project": "marbles", "folder": "engineering", "plan": "checkout-v2", "days": 14, "stale_days": 7 }
```

Response:

```json
{
  "schema": "marbles.view.dashboard.v1",
  "generated_at": 0,
  "scope": { "project": null, "folder": null, "plan": null },
  "totals": { "total": 0, "open": 0, "in_progress": 0, "review": 0, "blocked": 0, "closed": 0,
              "ready": 0, "stale": 0, "percent_done": 0.0 },
  "portfolio": { },
  "burndown": { },
  "scrum": { "columns": [], "swimlanes": [] },
  "gantt": { "total_minutes": 0, "critical_path": [], "nodes": [] }
}
```

The aggregate may reuse the same `Issue` page and call the same pure report functions. It must not
query each child view again.

### `POST /v1/views.scrum`

Board projection:

```json
{
  "schema": "marbles.view.scrum.v1",
  "generated_at": 0,
  "scope": { "project": null, "folder": null, "plan": null },
  "columns": [
    { "status": "open", "title": "Ready", "count": 0, "wip_limit": null,
      "cards": [ { "id": "mb-1", "title": "…", "project": "marbles", "priority": 2,
                   "issue_type": "task", "assignee": null, "labels": [], "epic": null,
                   "blocked_by": 1, "age_seconds": 3600, "estimate": 30,
                   "plan": "checkout-v2" } ] }
  ],
  "swimlanes": [ { "key": "marbles", "title": "marbles", "columns": [] } ]
}
```

Rules:

- Columns are the existing `STATUSES` order, not a new vocabulary.
- Goals are containers and are excluded from board cards, matching portfolio.
- `blocked_by` is the count of open dependencies. A dependency missing from the store is not
  invented as closed for this view; reuse the ready-rule semantics and document the difference.
- `wip_limit` is null until the store has an explicit policy. Do not hard-code a limit.
- A card is a bounded projection, not the full `Issue` row.

### `POST /v1/views.gantt`

Generalize the existing plan scheduler so it can schedule either a plan or a scope of ordinary
issues:

```json
{ "project": "marbles", "plan": null, "include_closed": false, "default_estimate_min": 60 }
```

Response reuses the shape of `plan::Gantt` and `GanttNode`, with scope fields added. Nodes are
scheduled from `estimate` metadata, then dependency edges. `start_at`/`finish_at` are display
projections from the request time; they are not commitments.

Rules:

- A node without an estimate gets `default_estimate_min` and reports
  `"estimate_source": "default"`; estimates from metadata report `"metadata"`.
- A cycle is a named view error, never a partial chart. The store refuses cycles at write time,
  but a read must still fail closed.
- Closed issues are excluded by default; `include_closed` is explicit.
- Critical path/slack/concurrency reuse the existing plan implementation rather than a second
  scheduler.

### Tests

- Pure report/plan unit tests for scope, default estimates, cycles, empty store, and missing
  dependencies.
- API tests for each operation in local and HTTP modes with the same fixture.
- `READ_OPS`/offline cache test includes all three view operations.
- A golden JSON shape test so the browser adapter is not coupled to an accidental field rename.

## Workstream B — browser viewer and Canvas widgets

### B0 — view contract, no UI lock-in

Define `marbles.view.v1` JSON as the stable boundary. The browser and Canvas adapters consume the
same payloads. A Canvas `WidgetSpec` is a rendering artifact, never the API response itself.

### B1 — retired browser shell

The temporary same-origin `/view` HTML shell was removed once the Canvas host and wasm projection
landed. It duplicated the view rendering and kept the bearer token in the browser. The retained
contract is the read-only `marbles.view.v1` API.

### B2 — Canvas widget adapter

Implemented as the Rust host `marbles-canvas-sync` plus the `viewer/marbles-view` wasm module:

- the wasm module owns the dashboard -> `WidgetSpec` projection: `metric_board` for totals,
  `table` for ready/in-flight work, and `table` for the critical path
- the host fetches `views.dashboard` with the owner credential, runs the module under wasmtime,
  and applies the document through the owner control socket; the Marbles bearer never enters the
  wasm module or the browser
- the same core wasm ABI is run by the browser/V8 host in `viewer/marbles-view/web`; it has no
  imports, so there is no ambient network, filesystem, clock, or token access
- keep provenance/freshness in `WidgetData`; the footer carries `observed_at`, and a failed fetch
  leaves the last good workspace on screen instead of drawing a healthy-looking zero
- bump the widget spec id with the document revision. Canvas deliberately retains a live widget
  when the spec id and family are unchanged (for stream-bound values), so this is what makes a
  snapshot refresh decode instead of leaving stale data on screen.

Canvas Web can host the renderer. If Canvas Web cannot fetch Marbles directly, add a bounded
same-origin bridge rather than exposing the Marbles token to arbitrary surfaces.

### B3 — Soil (native/kiosk/TV follows, not a browser dependency)

Soil is currently a design record. When a Soil host exists, a Marbles Spore can declare
`soil:surface` and consume the same `marbles.view.v1` contract. Soil’s capability boundary also
means the Spore gets no ambient network: the host supplies fetch/stream capability explicitly.
Do not put OIDC client secrets, bearer tokens, or database access in a Spore manifest or module.

## Authentication

The browser must never hold the OIDC client secret. Two viable paths:

1. **OIDC PKCE public client** (Canvas browser host): exact redirect URIs for the Canvas host
   callback and local development; id/access token kept in memory or session storage; company
   claim still enforced server-side.
2. **Same-origin BFF**: server-side session cookie, refresh handled server-side, API proxied or
   cookie-authenticated. More code, but the better default for a product viewer.

Recommendation: BFF for hosted production, PKCE for the internal first cut. Local `marbles serve`
uses a pasted static token or a local-only token exchange; it must not weaken hosted auth.

## Suggested order

1. Workstream A: dashboard + scrum + generalized Gantt APIs, tests, docs. Independent of Canvas.
2. Workstream B0/B1: same-origin shell against the real API, then auth decision.
3. Workstream B2: Canvas widget adapter in `dock`.
4. Workstream B3: Soil only after a host runtime exists.

Two worktrees can run A and B0/B1 in parallel because B depends only on the `marbles.view.v1`
contract, not on the final Canvas widgets.

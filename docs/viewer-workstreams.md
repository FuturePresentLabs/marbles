# Marbles views and Canvas viewer

Status: workstreams A and B0/B1 are implemented; B2 (Canvas adapter) and B3 (Soil) remain. This
note records the shared contract and the staged plan. It does not change the work model: every view is a read-only projection over the same
issue graph. No view owns state, invents values, or creates a second source of truth.

## Implemented now

- `POST /v1/views.dashboard`, `/v1/views.scrum`, and `/v1/views.gantt` are read-only, share the
  `ViewScope` filters, and are in `client::READ_OPS`.
- `GET /view` serves an embedded, same-origin viewer shell. The shell is public static HTML/JS/CSS;
  every `/v1` data call still requires a bearer token. It includes a clearly labelled Demo-data
  mode so the layout can be reviewed without a tracker.
- The Gantt view reuses the plan scheduler, reports `estimate_source: "metadata" | "default"`,
  excludes goals and closed work by default, and fails closed on a cycle.

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

Serve the viewer from the Marbles server itself, under the same origin:

- hosted: `https://marbles.fpl.dev/view`
- local/Agora: `http://<marbles-host>:7878/view`
- optional later Mycelium name: `https://marbles.<host>.<site>.myc:<advertised-port>/view`

Same-origin removes CORS from the critical path and lets the viewer use the existing API without
a second ingress. A Mycelium DNS alias is a naming convenience, not a deployment prerequisite.

The viewer is read-only for the first cut. Claims, close/review, and other mutations stay in the
CLI until the view contract and auth path are proven.

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

### B1 — same-origin browser viewer

Add a read-only `/view` route served by Marbles. First cut:

- project/folder/plan selector
- dashboard totals, ready queue, WIP/review/blocked
- scrum columns
- Gantt with critical path
- explicit stale/generated-at labels

The first cut may use a small static HTML/JS shell to validate the API contract. It is a test
harness, not a third design system; replace it with the Canvas renderer when B2 lands.

### B2 — Canvas widget adapter

Add a Marbles data adapter to the Canvas host (repository directory `dock`, crate boundary to be
decided):

- fetch `views.dashboard`, `views.scrum`, `views.gantt` with the browser/owner credential
- map to durable `WidgetSpec`s: `metric_board`/`status_list` for totals, `table`/`rich_card` for
  cards, `dag_graph` or a new generic `gantt` widget for scheduling
- prefer a new generic Canvas widget kind over a Marbles-specific renderer if the board/timeline
  semantics are reusable
- keep provenance/freshness in `WidgetData`; a failed fetch must render stale/error, never a
  healthy-looking zero

Canvas Web can host the renderer. If Canvas Web cannot fetch Marbles directly, add a bounded
same-origin bridge rather than exposing the Marbles token to arbitrary surfaces.

### B3 — Soil (native/kiosk/TV follows, not a browser dependency)

Soil is currently a design record. When a Soil host exists, a Marbles Spore can declare
`soil:surface` and consume the same `marbles.view.v1` contract. Soil’s capability boundary also
means the Spore gets no ambient network: the host supplies fetch/stream capability explicitly.
Do not put OIDC client secrets, bearer tokens, or database access in a Spore manifest or module.

## Authentication

The browser must never hold the OIDC client secret. Two viable paths:

1. **OIDC PKCE public client** (`marbles-web`): exact redirect URIs for `/view/callback` and local
   development; id/access token kept in memory or session storage; company claim still enforced
   server-side.
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

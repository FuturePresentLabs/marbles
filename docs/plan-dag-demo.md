# Plans as DAGs — local-mode demo transcript

Executed against a scratch store (`MARBLES_HOME=/tmp/plan-dag-demo-final/home`, no server; the
hosted instance was 503 during this work). Plan: **6 nodes, 2 projects (repos), 3 conflict
groups**. Issue ids carry a random suffix, so they differ per run; the shape does not.

Raw artifacts: [`plan-dag-demo.transcript.txt`](plan-dag-demo.transcript.txt) (verbatim run) and
[`plan-dag-demo.plan.toml`](plan-dag-demo.plan.toml) (exact spec). Reproduce from the repo root:

```bash
cargo build
MARBLES_HOME=$(mktemp -d) docs/plan-dag-demo.sh
```

## The spec (`plan.toml`, written by the script)

```toml
name = "checkout-v2"
project = "marbles"

[[nodes]]
key = "schema"
title = "Add plan schema and metadata keys"
repo = "marbles"
conflict_group = "db"
risk = "high"
estimate = 30
done_means = "cargo test -p marbles plan_schema"
acceptance = "create/show/gantt/ready pass on an empty store"

[[nodes]]
key = "scaffold"
title = "Scaffold the checkout client"
project = "checkout"
repo = "checkout"
conflict_group = "api"
risk = "low"
estimate = 20
done_means = "cargo test -p checkout scaffold"
acceptance = "client builds with the generated types"

[[nodes]]
key = "api"
title = "Expose plan endpoints"
repo = "marbles"
conflict_group = "api"
risk = "medium"
estimate = 45
done_means = "cargo test --test server plans"
acceptance = "/v1/plans.* round-trips the same DAG"
deps = ["schema"]

[[nodes]]
key = "wiring"
title = "Wire the client to plan ready"
project = "checkout"
repo = "checkout"
conflict_group = "db"
risk = "medium"
estimate = 40
done_means = "cargo test -p checkout wiring"
acceptance = "client consumes the frontier without same-group races"
deps = ["scaffold"]

[[nodes]]
key = "docs"
title = "Document plans as DAGs"
repo = "marbles"
conflict_group = "docs"
risk = "low"
estimate = 15
done_means = "grep -q 'Plans are DAGs' README.md"
acceptance = "README section renders and commands match the CLI"
deps = ["api"]

[[nodes]]
key = "migration"
title = "Backfill metadata on existing issues"
repo = "marbles"
conflict_group = "db"
risk = "high"
estimate = 25
done_means = "cargo test -p marbles backfill"
acceptance = "legacy issues gain plan metadata without loss"
```

## Transcript

```text

$ mb init --prefix mb --no-hooks --quiet

$ mb init --prefix co --no-hooks --quiet
=== 1. review the draft (no writes) ===

$ mb plan create /tmp/plan-dag-demo-final/plan.toml --dry-run
draft plan "checkout-v2" (project marbles, 6 node(s), 3 edge(s)) — not written
  schema  Add plan schema and metadata keys
      repo: marbles
      conflict_group: db
      risk: high
      done_means: cargo test -p marbles plan_schema
      acceptance: create/show/gantt/ready pass on an empty store
      estimate: 30
  scaffold  Scaffold the checkout client
      repo: checkout
      conflict_group: api
      risk: low
      done_means: cargo test -p checkout scaffold
      acceptance: client builds with the generated types
      estimate: 20
  api  Expose plan endpoints
      repo: marbles
      conflict_group: api
      risk: medium
      done_means: cargo test --test server plans
      acceptance: /v1/plans.* round-trips the same DAG
      estimate: 45
      deps: schema
  wiring  Wire the client to plan ready
      repo: checkout
      conflict_group: db
      risk: medium
      done_means: cargo test -p checkout wiring
      acceptance: client consumes the frontier without same-group races
      estimate: 40
      deps: scaffold
  docs  Document plans as DAGs
      repo: marbles
      conflict_group: docs
      risk: low
      done_means: grep -q 'Plans are DAGs' README.md
      acceptance: README section renders and commands match the CLI
      estimate: 15
      deps: api
  migration  Backfill metadata on existing issues
      repo: marbles
      conflict_group: db
      risk: high
      done_means: cargo test -p marbles backfill
      acceptance: legacy issues gain plan metadata without loss
      estimate: 25
=== 2. write the plan (nodes + edges) ===

$ mb plan create /tmp/plan-dag-demo-final/plan.toml
plan "checkout-v2" — 6 node(s) in project marbles, 3 edge(s)
  mb-p8g2  schema  Add plan schema and metadata keys
  co-i6pr  scaffold  Scaffold the checkout client
  mb-s8u3  api  Expose plan endpoints
      waits on schema
  co-r77e  wiring  Wire the client to plan ready
      waits on scaffold
  mb-euvz  docs  Document plans as DAGs
      waits on api
  mb-mz7y  migration  Backfill metadata on existing issues
=== 3. plan show (DAG, topological order, metadata) ===

$ mb plan show checkout-v2
plan "checkout-v2" — 6 node(s), 2 repo(s), 3 conflict group(s)
repos: checkout, marbles
groups: api, db, docs

migration (mb-mz7y)  [open]  Backfill metadata on existing issues
    group:db  repo:marbles  risk:high  est:25m
    done-means: cargo test -p marbles backfill
    acceptance: legacy issues gain plan metadata without loss
scaffold (co-i6pr)  [open]  Scaffold the checkout client
    group:api  repo:checkout  risk:low  est:20m
    done-means: cargo test -p checkout scaffold
    acceptance: client builds with the generated types
schema (mb-p8g2)  [open]  Add plan schema and metadata keys
    group:db  repo:marbles  risk:high  est:30m
    done-means: cargo test -p marbles plan_schema
    acceptance: create/show/gantt/ready pass on an empty store
api (mb-s8u3)  [open]  Expose plan endpoints
    group:api  repo:marbles  risk:medium  est:45m
    waits on: schema
    done-means: cargo test --test server plans
    acceptance: /v1/plans.* round-trips the same DAG
docs (mb-euvz)  [open]  Document plans as DAGs
    group:docs  repo:marbles  risk:low  est:15m
    waits on: api
    done-means: grep -q 'Plans are DAGs' README.md
    acceptance: README section renders and commands match the CLI
wiring (co-r77e)  [open]  Wire the client to plan ready
    group:db  repo:checkout  risk:medium  est:40m
    waits on: scaffold
    done-means: cargo test -p checkout wiring
    acceptance: client consumes the frontier without same-group races
=== 4. plan gantt (estimates, critical path, concurrency) ===

$ mb plan gantt checkout-v2
plan "checkout-v2" — makespan 90m, 6 node(s), peak concurrency 3, running now 0
critical path (90m): schema → api → docs

NODE                 STATUS       GROUP         EST   START  FINISH  SLACK CRIT
api                  open         api           45m     30m     75m     0m yes
docs                 open         docs          15m     75m     90m     0m yes
migration            open         db            25m      0m     25m    65m 
scaffold             open         api           20m      0m     20m    30m 
schema               open         db            30m      0m     30m     0m yes
wiring               open         db            40m     20m     60m    30m 
=== 5. plan ready (parallel frontier, conflict groups) ===

$ mb plan ready checkout-v2
plan "checkout-v2" — 2 ready / 6 node(s)
  mb-mz7y [migration]  [db] Backfill metadata on existing issues  — done-means: cargo test -p marbles backfill
  co-i6pr [scaffold]  [api] Scaffold the checkout client  — done-means: cargo test -p checkout scaffold

waiting:
  mb-s8u3 [api]  Expose plan endpoints — deps: waits on mb-p8g2
  mb-euvz [docs]  Document plans as DAGs — deps: waits on mb-s8u3
  co-r77e [wiring]  Wire the client to plan ready — deps: waits on co-i6pr
  mb-p8g2 [schema]  Add plan schema and metadata keys — conflict_group: group "db" already has a node in this frontier

schema=mb-p8g2  migration=mb-mz7y
=== 6. a running node blocks its conflict group ===

$ mb update mb-p8g2 --status in_progress
mb-p8g2 updated (in_progress)

$ mb claim mb-p8g2 --as worker
claimed mb-p8g2 by worker (agent) until 1792681389

$ mb plan ready checkout-v2
plan "checkout-v2" — 1 ready / 6 node(s)
busy conflict groups: db → mb-p8g2
  co-i6pr [scaffold]  [api] Scaffold the checkout client  — done-means: cargo test -p checkout scaffold

waiting:
  mb-s8u3 [api]  Expose plan endpoints — deps: waits on mb-p8g2
  mb-euvz [docs]  Document plans as DAGs — deps: waits on mb-s8u3
  co-r77e [wiring]  Wire the client to plan ready — deps: waits on co-i6pr
  mb-mz7y [migration]  Backfill metadata on existing issues — conflict_group: group "db" busy with running node mb-p8g2
=== 7. closing the blocker advances dependents and frees the group ===

$ mb close mb-p8g2 --commit deadbeef --reason plan schema merged
mb-p8g2 closed

$ mb plan ready checkout-v2
plan "checkout-v2" — 2 ready / 6 node(s)
  mb-s8u3 [api]  [api] Expose plan endpoints  — done-means: cargo test --test server plans
  mb-mz7y [migration]  [db] Backfill metadata on existing issues  — done-means: cargo test -p marbles backfill

waiting:
  mb-euvz [docs]  Document plans as DAGs — deps: waits on mb-s8u3
  co-r77e [wiring]  Wire the client to plan ready — deps: waits on co-i6pr
  co-i6pr [scaffold]  Scaffold the checkout client — conflict_group: group "api" already has a node in this frontier
=== 8. machine-readable frontier (json) ===

$ mb plan ready checkout-v2 --json
{
  "plan": "checkout-v2",
  "ready": [
    {
      "id": "mb-s8u3",
      "key": "api",
      "title": "Expose plan endpoints",
      "status": "open",
      "repo": "marbles",
      "conflict_group": "api",
      "risk": "medium",
      "estimate": 45,
      "done_means": "cargo test --test server plans",
      "acceptance": "/v1/plans.* round-trips the same DAG",
      "deps": [
        "mb-p8g2"
      ],
      "external_deps": [],
      "priority": 2
    },
    {
      "id": "mb-mz7y",
      "key": "migration",
      "title": "Backfill metadata on existing issues",
      "status": "open",
      "repo": "marbles",
      "conflict_group": "db",
      "risk": "high",
      "estimate": 25,
      "done_means": "cargo test -p marbles backfill",
      "acceptance": "legacy issues gain plan metadata without loss",
      "deps": [],
      "external_deps": [],
      "priority": 2
    }
  ],
  "blocked": [
    {
      "id": "mb-p8g2",
      "key": "schema",
      "title": "Add plan schema and metadata keys",
      "reason": "not_open",
      "detail": "status is closed"
    },
    {
      "id": "mb-euvz",
      "key": "docs",
      "title": "Document plans as DAGs",
      "reason": "deps",
      "detail": "waits on mb-s8u3"
    },
    {
      "id": "co-r77e",
      "key": "wiring",
      "title": "Wire the client to plan ready",
      "reason": "deps",
      "detail": "waits on co-i6pr"
    },
    {
      "id": "co-i6pr",
      "key": "scaffold",
      "title": "Scaffold the checkout client",
      "reason": "conflict_group",
      "detail": "group \"api\" already has a node in this frontier"
    }
  ],
  "busy_groups": {},
  "total_nodes": 6
}
=== 9. hand-authored node with the same metadata fields ===

$ mb create Fix flaky plan gantt test --plan checkout-v2 --repo marbles --conflict-group docs --risk low --estimate 10 --done-means cargo test plan::gantt --acceptance gantt is deterministic
mb-n9zz created

$ mb plan show checkout-v2 --json
{
  "plan": "checkout-v2",
  "nodes": [
    {
      "id": "mb-mz7y",
      "key": "migration",
      "title": "Backfill metadata on existing issues",
      "status": "open",
      "repo": "marbles",
      "conflict_group": "db",
      "risk": "high",
      "estimate": 25,
      "done_means": "cargo test -p marbles backfill",
      "acceptance": "legacy issues gain plan metadata without loss",
      "deps": [],
      "external_deps": [],
      "priority": 2
    },
    {
      "id": "co-i6pr",
      "key": "scaffold",
      "title": "Scaffold the checkout client",
      "status": "open",
      "repo": "checkout",
      "conflict_group": "api",
      "risk": "low",
      "estimate": 20,
      "done_means": "cargo test -p checkout scaffold",
      "acceptance": "client builds with the generated types",
      "deps": [],
      "external_deps": [],
      "priority": 2
    },
    {
      "id": "mb-p8g2",
      "key": "schema",
      "title": "Add plan schema and metadata keys",
      "status": "closed",
      "repo": "marbles",
      "conflict_group": "db",
      "risk": "high",
      "estimate": 30,
      "done_means": "cargo test -p marbles plan_schema",
      "acceptance": "create/show/gantt/ready pass on an empty store",
      "deps": [],
      "external_deps": [],
      "priority": 2
    },
    {
      "id": "mb-s8u3",
      "key": "api",
      "title": "Expose plan endpoints",
      "status": "open",
      "repo": "marbles",
      "conflict_group": "api",
      "risk": "medium",
      "estimate": 45,
      "done_means": "cargo test --test server plans",
      "acceptance": "/v1/plans.* round-trips the same DAG",
      "deps": [
        "mb-p8g2"
      ],
      "external_deps": [],
      "priority": 2
    },
    {
      "id": "mb-euvz",
      "key": "docs",
      "title": "Document plans as DAGs",
      "status": "open",
      "repo": "marbles",
      "conflict_group": "docs",
      "risk": "low",
      "estimate": 15,
      "done_means": "grep -q 'Plans are DAGs' README.md",
      "acceptance": "README section renders and commands match the CLI",
      "deps": [
        "mb-s8u3"
      ],
      "external_deps": [],
      "priority": 2
    },
    {
      "id": "co-r77e",
      "key": "wiring",
      "title": "Wire the client to plan ready",
      "status": "open",
      "repo": "checkout",
      "conflict_group": "db",
      "risk": "medium",
      "estimate": 40,
      "done_means": "cargo test -p checkout wiring",
      "acceptance": "client consumes the frontier without same-group races",
      "deps": [
        "co-i6pr"
      ],
      "external_deps": [],
      "priority": 2
    },
    {
      "id": "mb-n9zz",
      "key": "mb-n9zz",
      "title": "Fix flaky plan gantt test",
      "status": "open",
      "repo": "marbles",
      "conflict_group": "docs",
      "risk": "low",
      "estimate": 10,
      "done_means": "cargo test plan::gantt",
      "acceptance": "gantt is deterministic",
      "deps": [],
      "external_deps": [],
      "priority": 2
    }
  ],
  "edges": [
    {
      "child": "mb-s8u3",
      "parent": "mb-p8g2"
    },
    {
      "child": "mb-euvz",
      "parent": "mb-s8u3"
    },
    {
      "child": "co-r77e",
      "parent": "co-i6pr"
    }
  ],
  "repos": [
    "checkout",
    "marbles"
  ],
  "conflict_groups": [
    "api",
    "db",
    "docs"
  ]
}
=== 10. store-level query: plans are just company-scoped metadata ===

$ mb list --all --json
[{"id":"mb-euvz","title":"Document plans as DAGs","description":"","status":"open","priority":2,"issue_type":"task","labels":[],"metadata":{"acceptance":"README section renders and commands match the CLI","conflict_group":"docs","done_means":"grep -q 'Plans are DAGs' README.md","estimate":15,"plan":"checkout-v2","plan_key":"docs","repo":"marbles","risk":"low"},"dependency_count":1,"dependent_count":0,"dependencies":[{"depends_on_id":"mb-s8u3"}],"evidence":[],"parent":null,"assignee":null,"actor_kind":null,"expires_at":null,"available_at":1792679589,"closed_at":null,"created_at":1792679589,"updated_at":1792679589,"project":"marbles"},{"id":"mb-mz7y","title":"Backfill metadata on existing issues","description":"","status":"open","priority":2,"issue_type":"task","labels":[],"metadata":{"acceptance":"legacy issues gain plan metadata without loss","conflict_group":"db","done_means":"cargo test -p marbles backfill","estimate":25,"plan":"checkout-v2","plan_key":"migration","repo":"marbles","risk":"high"},"dependency_count":0,"dependent_count":0,"dependencies":[],"evidence":[],"parent":null,"assignee":null,"actor_kind":null,"expires_at":null,"available_at":1792679589,"closed_at":null,"created_at":1792679589,"updated_at":1792679589,"project":"marbles"},{"id":"mb-p8g2","title":"Add plan schema and metadata keys","description":"","status":"closed","priority":2,"issue_type":"task","labels":[],"metadata":{"acceptance":"create/show/gantt/ready pass on an empty store","conflict_group":"db","done_means":"cargo test -p marbles plan_schema","estimate":30,"plan":"checkout-v2","plan_key":"schema","repo":"marbles","risk":"high"},"dependency_count":0,"dependent_count":1,"dependencies":[],"evidence":[{"kind":"commit","value":"deadbeef"}],"parent":null,"assignee":null,"actor_kind":null,"expires_at":null,"available_at":1792679709,"closed_at":1792679589,"created_at":1792679589,"updated_at":1792679589,"project":"marbles"},{"id":"mb-s8u3","title":"Expose plan endpoints","description":"","status":"open","priority":2,"issue_type":"task","labels":[],"metadata":{"acceptance":"/v1/plans.* round-trips the same DAG","conflict_group":"api","done_means":"cargo test --test server plans","estimate":45,"plan":"checkout-v2","plan_key":"api","repo":"marbles","risk":"medium"},"dependency_count":0,"dependent_count":1,"dependencies":[{"depends_on_id":"mb-p8g2"}],"evidence":[],"parent":null,"assignee":null,"actor_kind":null,"expires_at":null,"available_at":1792679589,"closed_at":null,"created_at":1792679589,"updated_at":1792679589,"project":"marbles"},{"id":"mb-n9zz","title":"Fix flaky plan gantt test","description":"","status":"open","priority":2,"issue_type":"task","labels":[],"metadata":{"acceptance":"gantt is deterministic","conflict_group":"docs","done_means":"cargo test plan::gantt","estimate":10,"plan":"checkout-v2","repo":"marbles","risk":"low"},"dependency_count":0,"dependent_count":0,"dependencies":[],"evidence":[],"parent":null,"assignee":null,"actor_kind":null,"expires_at":null,"available_at":1792680190,"closed_at":null,"created_at":1792679590,"updated_at":1792679590,"project":"marbles"}]

```

## Reproducer script (`docs/plan-dag-demo.sh`)

```bash
#!/bin/bash
# Local-mode demo for `mb plan show|gantt|ready|create`.
# Usage: MARBLES_HOME=$(mktemp -d) docs/plan-dag-demo.sh
# Requires: cargo build; MB=target/debug/marbles (override with MB=...).
set -euo pipefail

MB=${MB:-target/debug/marbles}
case "$MB" in
  /*) ;;
  *) MB=$(cd "$(dirname "$MB")" && pwd)/$(basename "$MB") ;;
esac
DEMO=${DEMO_DIR:-$(mktemp -d /tmp/plan-dag-demo.XXXXXX)}
export MARBLES_HOME=${MARBLES_HOME:-$DEMO/home}
unset MARBLES_URL
mkdir -p "$DEMO/marbles" "$DEMO/checkout"

cat > "$DEMO/plan.toml" <<'TOML'
name = "checkout-v2"
project = "marbles"

[[nodes]]
key = "schema"
title = "Add plan schema and metadata keys"
repo = "marbles"
conflict_group = "db"
risk = "high"
estimate = 30
done_means = "cargo test -p marbles plan_schema"
acceptance = "create/show/gantt/ready pass on an empty store"

[[nodes]]
key = "scaffold"
title = "Scaffold the checkout client"
project = "checkout"
repo = "checkout"
conflict_group = "api"
risk = "low"
estimate = 20
done_means = "cargo test -p checkout scaffold"
acceptance = "client builds with the generated types"

[[nodes]]
key = "api"
title = "Expose plan endpoints"
repo = "marbles"
conflict_group = "api"
risk = "medium"
estimate = 45
done_means = "cargo test --test server plans"
acceptance = "/v1/plans.* round-trips the same DAG"
deps = ["schema"]

[[nodes]]
key = "wiring"
title = "Wire the client to plan ready"
project = "checkout"
repo = "checkout"
conflict_group = "db"
risk = "medium"
estimate = 40
done_means = "cargo test -p checkout wiring"
acceptance = "client consumes the frontier without same-group races"
deps = ["scaffold"]

[[nodes]]
key = "docs"
title = "Document plans as DAGs"
repo = "marbles"
conflict_group = "docs"
risk = "low"
estimate = 15
done_means = "grep -q 'Plans are DAGs' README.md"
acceptance = "README section renders and commands match the CLI"
deps = ["api"]

[[nodes]]
key = "migration"
title = "Backfill metadata on existing issues"
repo = "marbles"
conflict_group = "db"
risk = "high"
estimate = 25
done_means = "cargo test -p marbles backfill"
acceptance = "legacy issues gain plan metadata without loss"
TOML

run() { printf '\n$ %s\n' "$*"; "$@"; }

cd "$DEMO/marbles"
run "$MB" init --prefix mb --no-hooks --quiet
cd "$DEMO/checkout"
run "$MB" init --prefix co --no-hooks --quiet
cd "$DEMO/marbles"

echo '=== 1. review the draft (no writes) ==='
run "$MB" plan create "$DEMO/plan.toml" --dry-run

echo '=== 2. write the plan (nodes + edges) ==='
run "$MB" plan create "$DEMO/plan.toml"

echo '=== 3. plan show (DAG, topological order, metadata) ==='
run "$MB" plan show checkout-v2

echo '=== 4. plan gantt (estimates, critical path, concurrency) ==='
run "$MB" plan gantt checkout-v2

echo '=== 5. plan ready (parallel frontier, conflict groups) ==='
run "$MB" plan ready checkout-v2

A=$("$MB" plan show checkout-v2 --json | jq -r '.nodes[] | select(.key=="schema") | .id')
F=$("$MB" plan show checkout-v2 --json | jq -r '.nodes[] | select(.key=="migration") | .id')
echo
echo "schema=$A  migration=$F"

echo '=== 6. a running node blocks its conflict group ==='
run "$MB" update "$A" --status in_progress
run "$MB" claim "$A" --as worker
run "$MB" plan ready checkout-v2

echo '=== 7. closing the blocker advances dependents and frees the group ==='
run "$MB" close "$A" --commit deadbeef --reason "plan schema merged"
run "$MB" plan ready checkout-v2

echo '=== 8. machine-readable frontier (json) ==='
run "$MB" plan ready checkout-v2 --json

echo '=== 9. hand-authored node with the same metadata fields ==='
run "$MB" create "Fix flaky plan gantt test" --plan checkout-v2 --repo marbles \
  --conflict-group docs --risk low --estimate 10 \
  --done-means "cargo test plan::gantt" --acceptance "gantt is deterministic"
run "$MB" plan show checkout-v2 --json

echo '=== 10. store-level query: plans are just company-scoped metadata ==='
run "$MB" list --all --json
```

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

---
title: Marbles agent work interface
description: Supported work tracking interfaces, identity, and evidence requirements.
type: reference
status: active
visibility: company
tags: [marbles, agents, issues, coordination]
---

# Marbles agent work interface

Marbles owns the work graph and its mutation history. Agents use its CLI or
authenticated HTTP API to discover work, claim a lease, record progress, and
provide delivery evidence. Wiki supplies documentation; it does not replace
Marbles' live issue state.

## CLI and repository setup

See the [project README](../../README.md) for installation and configuration.
`mb init` registers the current repository and installs managed agent
instructions. `marbles setup` updates those instructions with an explicit
profile. Read the installed instructions before changing project work.

For an already configured checkout, `marbles ready` returns eligible work and
`marbles list --json` returns machine-readable issue state. Command definitions
in [`src/main.rs`](../../src/main.rs) are the source of truth for flags; use the
installed binary's `--help` for the version actually running.

## HTTP interface

The routes are defined in [`src/api.rs`](../../src/api.rs). Operational probes
are `GET /healthz` and `GET /metrics`; the work API uses POST routes under `/v1/`:

| Area | Operations |
|---|---|
| Projects | `projects.ensure`, `projects.list` |
| Issues | `issues.create`, `issues.get`, `issues.list`, `issues.ready`, `issues.update`, `issues.close`, `issues.supersede`, `issues.import` |
| Dependencies | `deps.add`, `deps.remove` |
| Labels | `labels.add`, `labels.remove` |
| Claims | `claims.claim`, `claims.touch`, `claims.release`, `claims.sweep` |
| Evidence | `history.get`, `stats` |

Request and response definitions live in [`src/types.rs`](../../src/types.rs)
and the route handlers. Do not assume MCP or DCP availability from this HTTP
surface: those protocols are not exposed by the current Marbles router.

An optional `POST /v1/mattermost/slash` route serves Mattermost's
form-encoded slash commands from the same server and store. It authenticates a
separate shared command token, not a Marbles bearer; see
[`docs/mattermost.md`](../mattermost.md). It is an adapter over the same store
operations, not an additional work API.

## Identity and completion

Credentials establish the principal. OIDC verifies issuer, audience and company
scope; local static token filenames identify their owner. Request bodies cannot
assert another identity. Hosted PostgreSQL state is company scoped.

An agent claim is a renewable lease. Expiry returns interrupted work to the
ready pool. Human claims have distinct hold and escalation semantics. A
`review` issue is not delivered; closing requires a receipt or an explicit
acknowledgement that no delivery was expected. Consult the README for the
full lifecycle and [`deploy/PROMOTION.md`](../../deploy/PROMOTION.md) for
deployment promotion evidence.

## Documentation evidence

This reference was checked against the repository route table and README.
No live server mutation or deployment was performed for this documentation.

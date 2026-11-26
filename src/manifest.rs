//! The machine-readable command surface exported for Spores and connectors.
//!
//! This is a manifest, not a second registry: it describes the commands this
//! binary already implements, the scope they act on, and the write authority
//! they require. A host or MCP gateway can cache it, but `mb` remains the
//! authority; a manifest that disagrees with the binary is a bug, not a grant.

use serde_json::{Value, json};

pub const SCHEMA: &str = "marbles.command-manifest/1";

pub fn export() -> Value {
    json!({
        "schema": SCHEMA,
        "manifest_version": 1,
        "command": "mb",
        "version": env!("CARGO_PKG_VERSION"),
        "lineage": "records",
        "scope": {
            "project": "optional; absent means every project in the book",
            "store_wide_default": true,
            "project_is_identity": false,
            "actor": "MARBLES_ACTOR or --actor",
            "book": "selected by the host policy, never by the caller",
            "transport": "soil:records through the host command shim"
        },
        "authority": {
            "read": ["mcp:read"],
            "routine_write": ["mcp:tasks:write"],
            "sensitive": ["mcp:tasks:sensitive"],
            "note": "the gateway and connector enforce these; the manifest describes them and cannot grant them"
        },
        "concurrency": {
            "writer": "single-writer book",
            "revision": "append-only per record; `revision ID` returns the current revision",
            "cas": "`update --expect-revision N` fails with a revision conflict instead of overwriting",
            "claim": "claim is compare-and-set on (assignee, expires_at); a live foreign claim is refused"
        },
        "idempotency": {
            "create": "`create --id ID` refuses a duplicate id; a connector may derive a stable id",
            "reads": "idempotent",
            "update": "conditional on `--expect-revision` when supplied"
        },
        "operations": [
            {
                "name": "list",
                "mode": "read",
                "args": ["--all", "--id", "--limit", "--project"],
                "result": "array of issues"
            },
            {
                "name": "ready",
                "mode": "read",
                "args": ["--project", "--limit"],
                "result": "array of ready issues"
            },
            {
                "name": "search",
                "mode": "read",
                "args": ["--query", "--project", "--status", "--type", "--assignee", "--label", "--ready", "--blocked", "--all", "--limit"],
                "result": "array of matching issues, bounded by --limit"
            },
            {
                "name": "show",
                "mode": "read",
                "args": ["ID"],
                "result": "one-element array of issues, including dependencies and evidence"
            },
            {
                "name": "revision",
                "mode": "read",
                "args": ["ID"],
                "result": "{id, revision, origin}"
            },
            {
                "name": "history",
                "mode": "read",
                "args": ["ID"],
                "result": "append-only audit history"
            },
            {
                "name": "goal",
                "mode": "read",
                "args": ["list", "show ID"],
                "result": "goal tree and progress; the objective view"
            },
            {
                "name": "capability",
                "mode": "read",
                "args": ["list", "show", "verified", "gaps", "stale", "pilot-blockers", "baseline"],
                "result": "capability views with revision-bound evidence"
            },
            {
                "name": "create",
                "mode": "routine_write",
                "args": ["TITLE", "--id", "--project", "--type", "--priority", "--parent", "--label", "--external-ref", "--metadata"],
                "validation": "project required; title non-empty; priority integer; parent must be in the same project",
                "idempotent": "stable --id"
            },
            {
                "name": "update",
                "mode": "routine_write",
                "args": ["ID", "--title", "--description", "--status", "--priority", "--append-notes", "--metadata", "--set-metadata", "--expect-revision"],
                "validation": "status is one of open|in_progress|review|blocked|deferred|closed; priority is an integer",
                "cas": "--expect-revision"
            },
            {
                "name": "dep",
                "mode": "routine_write",
                "args": ["add|rm CHILD PARENT"],
                "validation": "both issues exist; self and reachability cycles are refused"
            },
            {
                "name": "claim",
                "mode": "routine_write",
                "args": ["ID", "--as", "--human", "--ttl"],
                "validation": "a live foreign claim is refused"
            },
            {
                "name": "touch",
                "mode": "routine_write",
                "args": ["ID", "--as"],
                "validation": "only the recorded holder may renew"
            },
            {
                "name": "release",
                "mode": "routine_write",
                "args": ["ID", "--as"],
                "validation": "releases the caller's claim"
            },
            {
                "name": "review",
                "mode": "routine_write",
                "args": ["ID", "--pr", "--commit"],
                "validation": "moves to review; does not close"
            },
            {
                "name": "label",
                "mode": "routine_write",
                "args": ["add|rm ID LABEL"],
                "result": "label membership"
            },
            {
                "name": "close",
                "mode": "sensitive",
                "args": ["ID", "--pr", "--commit", "--ack", "--reason"],
                "validation": "requires merge evidence or an explicit acknowledgement; never a silent close"
            },
            {
                "name": "supersede",
                "mode": "sensitive",
                "args": ["ID", "--with ID"],
                "validation": "retains the superseded record and names the replacement"
            },
            {
                "name": "project rm",
                "mode": "sensitive",
                "args": ["SLUG"],
                "validation": "cascades tombstones; requires explicit approval"
            }
        ]
    })
}

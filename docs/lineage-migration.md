# Lineage migration proposal

Status: **proposed** (DEC-018), not executed. No row has been copied or migrated.

## Current state

Two single-writer lineages exist and are not synchronized:

| lineage | reader | contents |
|---|---|---|
| `soil:records` book `personal/avery/marbles` | `mb` | 6,335 issue records; capability evidence seeded 2026-11-25 |
| native SQLite `~/.marbles/marbles.db` | `marbles` | historical per-machine issues, including `fpl-twenty-jwfh` in `review` |

The split is real: `fpl-twenty-jwfh` exists only in the native store. The MCP connector reads the
selected lineage and labels a read-only fallback (`split_lineage: true`) rather than merging.

## Proposed decision

1. **The `soil:records` book is authoritative for new fleet work.** It is the interface the
   connector, the Spore, and the capability evidence use.
2. **The native store becomes read-only historical.** No new task is created there. It stays
   available for the bounded read-only fallback and for audit.
3. **Migration is per open issue and explicit.** For each still-relevant native issue, an
   operator creates a record in the book through `mb`, carrying the native id in
   `external_ref`, and closes the native issue with an `ack` that names the new record. This is a
   deliberate recreate-with-reference, not a copy of the row.
4. **No bulk import, no automatic merge, no dual write.** A project must not be writable in both
   lineages. A bulk importer is a separate decision, not implied by this proposal.
5. **Verification is by count and spot-check.** Before and after: native open-issue count,
   book issue count, and `mb show` for each migrated `external_ref`.

## Stop conditions

Do not start until:

- an owner is named for the native-store freeze;
- the list of native open issues is reviewed (not every historical issue is worth migrating);
- `fpl-twenty-jwfh` and any other in-flight sales work is named explicitly, because closing it in
  the native store changes what outreach sees.

## Rollback

None is needed for the proposal. If the decision is rejected, the native store keeps writing as
before; the book already has the capability evidence and new fleet work. The only cost is the
existing split-lineage diagnostic.

## What this is not

- It is not a request to delete the native database.
- It is not a second store, a sync service, or an event bridge.
- It does not change the schema of either lineage.

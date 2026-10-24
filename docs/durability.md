# Durability

Marbles is a tracker for work that has receipts. A close is a promise that the merge happened;
the database should not lose the last acknowledged claim or close because the machine died in the
same second.

## Local store

- **SQLite, WAL mode.** Readers do not block the single writer.
- **`synchronous = FULL`.** A committed mutation is flushed before the client gets its receipt.
  This is deliberately more conservative than `NORMAL`; tracker writes are small and an
  acknowledged transition matters more than raw write throughput.
- **One writer.** Every mutation runs inside `BEGIN IMMEDIATE`; contention waits up to five
  seconds and then fails instead of interleaving.
- **Transactional history + outbox.** A mutation and its outbound event commit together. A webhook
  cannot be delivered for a transition that rolled back, and a transition cannot commit without
  its outbox row.

## Snapshot before risky work

`mb backup <path>` creates an atomic, compact snapshot with SQLite's `VACUUM INTO`. The target
must not already exist, and the source is checked with `PRAGMA integrity_check` before the copy.

```bash
mb backup ~/backups/marbles-$(date +%Y%m%d-%H%M%S).db
```

The command prints (or `--json` returns) the path, byte size, timestamp, row counts, and the
integrity result. To restore, stop the server, keep the current database as a rollback copy, then
replace it:

```bash
# server stopped
cp ~/.marbles/marbles.db ~/.marbles/marbles.db.pre-restore
cp ~/backups/marbles-20261024-010000.db ~/.marbles/marbles.db
rm -f ~/.marbles/marbles.db-wal ~/.marbles/marbles.db-shm
marbles serve
```

`backup` and `doctor` are local-mode commands: they read the database file, so run them on the
host that owns the store, not through a remote `--url`.

## Check the store

```bash
mb doctor
mb doctor --json
```

`doctor` reports:

| field | meaning |
| --- | --- |
| `integrity`, `quick_check` | SQLite integrity results (`ok` is healthy) |
| `foreign_key_violations` | dangling project/dependency references, if any |
| `journal_mode`, `synchronous` | expected `wal` / `FULL` |
| `db_bytes`, `wal_bytes` | current on-disk and WAL size |
| `projects`, `issues`, `deps`, `history` | row counts |
| `outbox_pending` | webhook events not yet delivered |

## Backups are not replication

A snapshot is a recovery artifact, not a second writer. Do not point two servers at copies of one
database, and do not restore over a live server. The design is one writer per lineage; a backup
respects that by being taken out of band and restored while the writer is stopped.

For hosted PostgreSQL deployments, use the database's own backup/PITR mechanism. `mb backup` is
for the local SQLite store.

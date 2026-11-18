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
- **Transactional history.** A mutation and its history row commit together, so the audit trail
  cannot describe a transition that rolled back.

## Snapshot before risky work

`marbles backup <path>` creates an atomic, compact snapshot with SQLite's `VACUUM INTO`. The target
must not already exist, and the source is checked with `PRAGMA integrity_check` before the copy.

```bash
marbles backup ~/backups/marbles-$(date +%Y%m%d-%H%M%S).db
```

The command prints (or `--json` returns) the path, byte size, timestamp, row counts, and the
integrity result. To restore, make sure no `marbles` command is running, keep the current database
as a rollback copy, then replace it:

```bash
cp ~/.marbles/marbles.db ~/.marbles/marbles.db.pre-restore
cp ~/backups/marbles-20261024-010000.db ~/.marbles/marbles.db
rm -f ~/.marbles/marbles.db-wal ~/.marbles/marbles.db-shm
```

`backup` and `doctor` read the database file, so run them on the host that owns the store.

## Check the store

```bash
marbles doctor
marbles doctor --json
```

`doctor` reports:

| field | meaning |
| --- | --- |
| `integrity`, `quick_check` | SQLite integrity results (`ok` is healthy) |
| `foreign_key_violations` | dangling project/dependency references, if any |
| `journal_mode`, `synchronous` | expected `wal` / `FULL` |
| `db_bytes`, `wal_bytes` | current on-disk and WAL size |
| `projects`, `issues`, `deps`, `history` | row counts |
| `outbox_pending` | history events recorded but never delivered; nothing delivers them now |

## Backups are not replication

A snapshot is a recovery artifact, not a second writer. Do not open two copies of one database, and
do not restore over a store a command is using. The design is one writer per lineage; a backup
respects that by being taken out of band and restored while nothing is writing.

A fleet book is not backed up this way: it is a `soil:records` book, and sync is its story, not a
SQLite snapshot.

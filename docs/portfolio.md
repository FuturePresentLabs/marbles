# Portfolio view

`mb portfolio` is the read-only rollup above folders, projects, goals, and the ready queue. It
answers "what needs attention across the whole store?" without mutating anything.

```bash
mb portfolio
mb portfolio --folder business --days 30 --stale-days 14
mb portfolio --json
```

## What it counts

For every project:

| field | meaning |
| --- | --- |
| `progress` | total/open/in-progress/review/blocked/deferred/closed counts and percent done |
| `ready` | open work past grace, not under a live claim, with every existing dependency closed |
| `oldest_ready_age_secs` | age of the oldest ready issue |
| `stale` | open/in-progress work whose `updated_at` is older than `--stale-days` (default 7) |
| `oldest_review_age_secs` | age of the oldest issue sitting in `review` |
| `burn_per_day` | closes per day over the `--days` window (default 14); absent with no closes |
| `goals` | open goals with descendant progress rollups |

Folders aggregate their projects. Unfiled projects are returned separately. Totals cover every
project in the query.

**Goals are containers, not work.** They are reported separately and excluded from project
`progress`, `ready`, `stale`, and burn counts, so a project with an open goal and two open tasks
shows two units of work, not three.

## Ready semantics

The portfolio reuses the same readiness rules as `mb ready`:

- status is `open`;
- `available_at` has passed (creation grace/edit deferral);
- no live claim (`assignee` is set and `expires_at` is missing or in the future);
- every dependency that exists in the store is `closed`.

Dependencies that point at an issue not present in the store are ignored, which matches the SQL
ready query. Goal issues are never counted as ready work.

## Notes

- One `issues.list` page is fetched and the report is computed locally, so local mode and the
  hosted API return the same view.
- `--folder` filters to projects assigned directly to that folder slug. Nested child folders are
  separate filters.
- This is a read-only command: no claims, no writes, no deployment surface.

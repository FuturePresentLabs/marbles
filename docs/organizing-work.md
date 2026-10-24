# Organizing work: folders, goals, epics, tasks

Marbles has one work graph per store. Folders and goals are ways to *view* that graph, not
separate databases: folder membership is an assignment, and a goal is an issue whose children are
epics and tasks.

## Folders group projects

A folder is a named group of projects. Folders may nest; a project belongs to at most one folder.
Projects with no assignment are **unfiled**.

```bash
mb folder create "Personal" --slug personal
mb folder create "Business" --slug business
mb folder create "Acme" --slug acme --parent business

mb folder list                 # table: folder, name, projects
mb folder tree                 # nested tree with projects in brackets
mb folder move acme business   # put a project in a folder
mb folder move acme none       # unfile it
```

`mb init --folder business` creates the folder if needed and assigns the new project in one step.
`mb projects` includes each project's `folder` so a dashboard can group without a second query.

Folders are organizational only. They do not scope claims, readiness, or dependencies: a blocker
in one folder can gate work in another.

## Goals → epics → tasks

A **goal** is a project-local root ("ship v1", "cut cloud spend by 30%"). An **epic** is a body of
work under a goal. A **task** is an executable unit under an epic.

```bash
mb goal create "Ship v1" -p 1
# → acme-goal1

mb epic create "Storage" --goal acme-goal1
# → acme-epic1

mb task create "Write WAL durability test" --epic acme-epic1
# → acme-task1
```

All three are ordinary issues. They can be claimed, labelled, blocked, moved to `review`, and
closed with evidence exactly like any other marble. Hierarchy uses the existing `parent` field, so
history, claims, and the ready queue already understand it.

Hierarchy is deliberately **project-local**: a child's parent must be in the same project. Use a
dependency edge for cross-project blockers:

```bash
mb dep add acme-task1 infra-task7    # acme waits for infra; no hierarchy implied
```

Cycles are refused, and the tree is rendered from the same rows the queue uses.

### Progress and trees

```bash
mb goal list                       # every open goal with descendant rollup
mb goal list --all --json          # include closed goals; machine-readable
mb goal show acme-goal1            # tree + progress bar
```

Progress counts the goal's descendants, not the goal container itself. A goal with one closed
task and one open task is 50%, even though the goal issue is still open.

`mb goal show` prints the full tree with status and priority:

```text
acme-goal1 — Ship v1 [open] p1 (goal)
############------------  50.0%
acme-goal1 [open] Ship v1 (goal, p1)
  acme-epic1 [open] Storage (epic, p2)
    acme-task1 [closed] Write the WAL test (task, p2)
    acme-task2 [in_progress] Add backup command (task, p2)
```

## Burndown

`mb burndown` computes a daily remaining-work series from issue creation and close timestamps. It
works for a project, a goal, or an epic, and returns JSON with `--json`.

```bash
mb burndown --project acme --days 30
mb burndown --goal acme-goal1 --days 14
mb burndown --epic acme-epic1 --days 7 --json
```

Each point reports:

| field | meaning |
| --- | --- |
| `date` | UTC day |
| `scope` | issues created by the end of that day |
| `done` | of those, issues closed by then |
| `remaining` | `scope - done` |
| `ideal_remaining` | even burn of the current scope across the window |

The summary adds `burn_per_day` (closes per day over the observed window) and
`projected_days_to_zero` (remaining ÷ burn rate). Both are absent rather than guessed when there
are no closes in the window.

## Notes on scale

- `goal list` and `burndown` fetch the project's issues once and compute locally, so they work
  identically in local mode and against the hosted API.
- Folder and goal metadata are ordinary rows; there is no second scheduler and no plan cache to
  invalidate.
- Cross-project dependencies remain allowed because blockers are about order, not ownership.

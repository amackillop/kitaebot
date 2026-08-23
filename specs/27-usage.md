# Spec 27: Usage Ledger

## Motivation

The usage ledger answers what the bot's work costs. The original
per-turn rows (build, model, tokens, cost) could say what a *deploy*
or a *model* costs, but not what a *task* costs — a GitHub issue, a
PR review, a duty run — which is the unit the operator actually
budgets by. Three gaps made per-task accounting impossible: duty
turns were not attributable to a duty (the name never crossed the
envelope), turn wall time was measured and discarded, and sub-agent
turns recorded orphaned under `session = "subagent"`, hiding the
largest cost component of review tasks.

The ledger stays per-turn: rows are the append-only ground truth and
a task is a **key column plus derived reporting**, not a lifecycle
entity. Nothing marks a task "done" in the database; completion is
readable from the outcome of its latest turn.

An alternative was considered and rejected: grouping the report by
the existing `(session, source)` columns. It saves only the task-key
newtype — the timing columns, duty-name plumbing, and sub-agent
threading are required by the metrics either way — and it promotes
`ChannelSource`'s Display strings (also the model-visible input tag,
journal lines, and alert text) into accounting identity, so any
wording change would silently split ledger history.

## Behavior

### Schema

The `turns` table in `state/kitaebot.db` (spec 05), migrations
`0001_baseline.sql` through `0003_turn_timing.sql`:

| Column | Added | Meaning |
|--------|-------|---------|
| `id` | 0001 | Insert order; recency proxy for grouping |
| `recorded_at` | 0001 | End-of-turn timestamp (SQLite default) |
| `git_sha` | 0001 | Build that ran the turn (flake-injected) |
| `session` | 0001 | Session the turn ran in; `subagent` for children |
| `source` | 0001 | `ChannelSource` Display or sub-agent label — display vocabulary, never a grouping key |
| `model` | 0001 | Provider model id |
| `calls` | 0001 | Provider calls in the turn |
| `prompt_tokens`, `completion_tokens` | 0001 | Summed over calls |
| `cost` | 0001 | USD from the wire (OpenRouter); NULL when unmetered |
| `task` | 0002 | Task key (below); NULL on legacy rows |
| `started_at` | 0003 | Turn start, epoch seconds; NULL on legacy rows |
| `duration_ms` | 0003 | Wall time of the turn; NULL on legacy rows |
| `outcome` | 0003 | Turn outcome label; NULL on legacy rows |

All post-baseline columns are nullable so both eras of rows parse.
No index on `task`: the report reads the whole ledger by design and
nothing filters by key. Writes are fire-and-forget (`record_turn`
warns and drops on failure — telemetry, not core state).

### Task keys

`TaskKey` (usage.rs) is derived purely from the dispatch's
`ChannelSource`:

| Source | Key |
|--------|-----|
| `Duty { duty }` | `duty:<name>` |
| `GitHub { repo, pr_number, .. }` | `pr:owner/repo#N` (role folded — feedback, contributor, and reviewer turns on one PR are one task) |
| `GitHubIssue { issue }` | `issue:owner/repo#N` |
| `Linear { issue }` | `linear:MDK-123` |
| `Socket` | `chat:socket` |
| `Telegram` | `chat:telegram` |

The duty name rides the envelope (`ChannelSource::Duty { duty }`);
Display and the journal topic remain `"Duty"`/`"duty"` — the key
carries the name so the four Display consumers (input tag, journal,
alerts, log spans) stay stable. Interactive channels aggregate under
one key each: sessions are the unit of conversation and spec 14's
`/stats` covers them.

### Sub-agent rollup

The actor derives the key once per envelope and places it in both
the root `TurnRecord` and the turn's `ToolCtx`, so the two cannot
disagree. The `task` tool copies the key into the child context and
the child's ledger row: sub-agent turns (`session = "subagent"`,
`source = explore|worker|reviewer`) carry the parent's task key and
fold into the task's cost and turn count. This resolves spec 19's
deferred cost-tracking item. A `ToolCtx` with no task (tests, the
distiller's ephemeral engine) records NULL.

Distillation turns are attributed through the command path: a
scheduled `/duty distill` arrives as `duty:distill`; an operator
`/distill` from the socket is `chat:socket`.

### Timing

`run_turn_metered` returns a `TurnMeter { usage, started_at,
duration, outcome }`; the record sites persist all of it. Two wall
times are derivable per task and both are reported:

- **Active** — Σ `duration_ms`: what the bot actually ground.
- **Span** — max(`started_at`·1000 + `duration_ms`) −
  min(`started_at`·1000): how long the task was in flight, including
  waits on humans.

`started_at` is integer epoch seconds: span arithmetic needs no date
parsing and the crate deliberately has no ISO-8601 parser.

### Outcome

The label the turn summary already logs, persisted:
`cancelled | error | max_iterations | no_progress | policy_halt |
text | tool_halt`. One derivation (`outcome_label`) feeds both log
and ledger, so they agree by construction. "Tasks the bot completes"
is read from here — a task whose last turn is `text` ended in a
delivered reply; `max_iterations` rows mark the expensive failures a
future duty budget (spec 24) must distinguish from productive spend.
Recorded from day one, surfaced in reports only when a view needs it.

### The /usage report

`By Task` is the headline table — Task, Turns, Cost, Active, Span —
ordered by most recent activity (max `id` per group), capped at 20
groups with a `(+N more tasks)` trailer. Rows with a NULL key render
as one `(untracked)` bucket, always last: legacy rows are not
backfilled (their `source` could reconstruct keys but nothing can
recover their timing). Aggregates sum only what exists — a group
mixing eras shows cost from all rows and timing from the timed ones;
`-` marks absent data, never rendered as zero.

`By Build` ($/turn per deploy — the cost-regression view) and
`By Model` follow unchanged. Tokens stay in those tables; the task
table is cost and time.

## Boundaries

### Owns

- The `turns` schema and its migrations
- `TaskKey` derivation and the key grammar
- `TurnRecord`/`TurnMeter` recording and the fire-and-forget policy
- The `/usage` report rendering

### Does Not Own

- Cost figures — the provider wire reports them (spec 02)
- Turn execution and outcome computation — the agent loop (spec 01)
- The duty schedule/budget that may one day consume this data
  (spec 24)
- The review ledger sharing the database (spec 23)

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Ledger insert fails | `warn!`, turn unaffected (telemetry, not core state) |
| Ledger absent (tests) | `/usage` reports tracking disabled |
| Legacy rows | NULL task/timing/outcome; `(untracked)` bucket, `-` timing |

## Constraints

- Rows are append-only; no updates, no lifecycle state
- The task key is the only grouping identity; `source` and `session`
  are display vocabulary
- New columns must be nullable — both row eras must always parse

## Open Questions

None currently. Pruning old rows is deliberate future work: the
report is bounded by the group cap, so ledger size is a disk
question, not a rendering one.

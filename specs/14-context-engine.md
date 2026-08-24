# Spec 14: Context Engine

## Motivation

The agent needs durable, project-scoped memory that accumulates over time.
Today, compaction crushes the entire conversation into a single summary and
throws away the originals. The agent forgets everything it explored yesterday
and starts from scratch.

The context engine replaces the current session + context window modules with a
pluggable system that owns the full message lifecycle: storage, compaction,
context assembly, and retrieval. The default implementation uses LCM (Lossless
Context Management) — a hierarchical DAG of summaries that preserves all
original messages and lets the agent drill back into them on demand.

The engine is a swappable trait so alternative strategies can be tested. A flat
session implementation preserves the previous behavior behind the same
interface (it absorbed the former specs 04 "Session" and 12 "Context Window
Management" — see the Flat Session Implementation section). A third
implementation, the ephemeral in-memory engine for sub-agent turns, is
described in spec 19.

## Behavior

### The Trait

All context management flows through a single trait. The agent loop, actor, and
channels interact exclusively with this interface.

```
trait ContextEngine: Send + Sync {
    // -- Turn operations (act on the active session) --
    push_message(msg: Message) -> Result<(), EngineError>
    assemble(system_prompt: &str) -> Result<AssembledContext, EngineError>
    compact_if_needed(summarize: &SummarizeFn) -> Result<Option<CompactionEvent>, EngineError>
    force_compact(summarize: &SummarizeFn) -> Result<CompactionEvent, EngineError>
    clear() -> Result<(), EngineError>
    save() -> Result<(), EngineError>
    stats() -> ContextStats                    // sync, infallible
    observe_tokens(prompt_tokens: usize)       // sync, infallible

    // -- Tools contributed by this engine --
    tools(scope: ToolScope) -> Vec<Arc<dyn Tool>>

    // -- Reporting --
    report() -> Result<String, EngineError>    // rendered /stats report

    // -- Session management --
    active_session() -> &str
    switch_session(name: &str) -> Result<(), EngineError>
    list_sessions() -> Result<Vec<SessionInfo>, EngineError>
}
```

`ToolScope` is `Root | SubAgent` (see spec 19); tool instances are `Arc`
so the same tool can appear in multiple filtered sets.

All methods are async via Rust return-position-impl-trait
(`impl Future<Output = ...> + Send`). The agent loop is generic over
`E: ContextEngine` so each engine is monomorphized at the call site;
no `Box<dyn ContextEngine>` indirection.

`SummarizeFn` is a borrowed callback for one summarization round-trip. Its
signature is `Fn(&str, &[Message]) -> Future<Result<String, ProviderError>>`.
The first argument is the per-call **instruction block** (placed in the
user turn alongside the formatted conversation); the second is the
messages to summarize. The engine does not own the provider — it borrows
the callback constructed once at startup via `make_summarize_fn`. The
system turn is fixed inside the closure (see §"Three-Level Summarization
Escalation"), so the engine only varies instructions per call.

The provider captured by the closure uses `provider.model_overrides.summarizer`
when set (see [spec 02](02-provider.md)), falling back to the root
model. Summaries are high-volume and low-stakes, so they typically run
on a cheaper model.

### Assembled Context

`assemble()` returns everything the agent loop needs for a provider call:

```
AssembledContext {
    messages: Vec<Message>,   // system prompt + ordered conversation
}
```

The system prompt is the first `Message::System` in the list; the
engine may augment it. LCM appends recall guidance instructing the
model how to use retrieval tools. The flat session passes the prompt
through unchanged.

### Compaction Events

`compact_if_needed()` and `force_compact()` return a `CompactionEvent` for
activity reporting:

```
CompactionEvent {
    before: usize,    // estimated tokens before compaction
    after: usize,     // estimated tokens after compaction
}
```

Engine-specific details (which DAG layers were created, how many summaries) are
internal. The event carries only what the activity system needs.

### Observed Tokens

Char-based estimates (`chars / 4`) undercount: they never see the system
prompt or tool schemas the provider actually tokenizes. When the provider
reports usage, the agent loop feeds the request's `prompt_tokens` back
into the engine via `observe_tokens()` after every response.

Engines keep the last observation for the active session and take
`max(estimate, observed)` for compaction triggers and `stats()`. Both
values are lower bounds on the next request's true size — the estimate
misses fixed overhead, the observation lags one turn — so the larger
one is the tighter bound.

The observation is dropped whenever the context shrinks (compaction,
`clear()`, `switch_session()`): it describes a request that no longer
reflects the session, and a stale high-water mark would win the `max()`
forever and re-trigger compaction on every turn. `EphemeralSession`
ignores observations since it never compacts.

### Session Info

`list_sessions()` returns metadata about each available session:

```
SessionInfo {
    name: String,
    message_count: usize,
    estimated_tokens: usize,
}
```

---

### Project-Scoped Sessions

Sessions are scoped to projects. Each session maintains its own conversation
history and memory. The agent builds dedicated knowledge per project over time.

**Naming**: sessions are identified by name. The default session is `general`
(hardcoded, not configurable). Names are sanitized before touching storage:
`/` becomes `--`, `..` and NUL bytes are stripped. The sanitized form is used
for filenames (flat) and conversation rows (LCM); display output desanitizes
(`--` back to `/`), so a session named `owner/repo` lists as `owner/repo` but
lives in `owner--repo.json`.

**Switching**: `switch_session(name)` is idempotent — creates the session if it
does not exist, loads it if it does. The active session name is persisted to
disk so it survives daemon restarts.

**Slash commands**:

| Command | Behavior |
|---------|----------|
| `/project` | List sessions, indicate which is active |
| `/project <name>` | Switch to named session (creates if new) |

**Channel routing**:

- **Socket / Telegram**: use the active session. The user switches explicitly
  via `/project`.
- **GitHub**: routes per-envelope based on the repository name. The GitHub
  channel extracts the repo from the PR context and includes it as a session
  hint on the envelope. The actor routes to that session for the turn without
  mutating the active session. If no session exists for the repo, one is
  created automatically.
- **Linear**: routes per-envelope based on the issue's `owner/repo` label —
  the same session key as GitHub, so a repo's PRs and tickets share one
  session. Message tagging stays per-issue via `ChannelSource::Linear`.
- **Heartbeat**: uses the active session. Per-project heartbeats are a future
  enhancement.

**Envelope routing**: the `Envelope` sent through `AgentHandle` carries an
optional `session_hint: Option<String>`. The actor resolves the target session
as `session_hint.unwrap_or(active_session)`. If the target differs from the
currently loaded session, the actor saves the current session, switches to the
target, processes the turn, saves, and switches back to the active session.

For LCM, switching sessions is a cheap metadata change (active
`conversation_id`). For the flat session, it is a JSON load/save.

---

### Flat Session Implementation

The baseline implementation. Absorbed the former specs 04 (Session) and 12
(Context Window Management). It exists so the system works without SQLite and
so the previous behavior is always available as a fallback.

#### Storage

One JSON file per session at `sessions/<sanitized-name>.json`, wrapping the
`Session` struct from `src/context/flat/session.rs`:

```
Session {
    messages: Vec<Message>,
    created_at: Timestamp,    // seconds since Unix epoch
    updated_at: Timestamp,
}
```

`Timestamp` is a newtype over `u64`. Messages do not carry individual
timestamps. On disk, messages use serde's default externally-tagged enum
format (e.g. `{"User": {"content": "..."}}`) — **not** the OpenAI wire
format; wire conversion happens in the provider module. No version field:
unknown fields are silently ignored for forward compatibility.

`Session` operations:

| Method | Behavior |
|--------|----------|
| `new()` | Create empty session with current timestamp |
| `load(path)` | Load from disk. Create new if file doesn't exist. `SessionError::Parse` if corrupt. |
| `save(path)` | Update `updated_at`, then atomic write (tmp + rename) |
| `add_message(msg)` | Append message, update `updated_at` |
| `clear()` | Wipe messages, preserve `created_at`, update `updated_at` |
| `compact(summary)` | Replace all messages with a single summary message |

Writes are atomic: write to `<name>.json.tmp`, then rename. A crash during
write leaves the original file intact.

#### Compaction

Effective budget is `(max_tokens - provider.max_tokens) * budget_percent /
100` (integer arithmetic; the output reserve is subtracted at startup, see
§"Dual Thresholds").
Tokens are estimated as `chars / 4`; `Message::char_count()` sums content
length, plus function names and argument strings for `ToolCalls`.

When `max(estimate, observed)` (see §"Observed Tokens") exceeds the
budget and the session has >= 2 messages:
format all messages as `[role] content` text, send through `SummarizeFn`
(no tools), and replace the whole conversation with a single
`Message::System` containing the summary. No partial windowing.
`force_compact()` skips the budget check but keeps the >= 2 message guard.

#### Tool output truncation

The flat engine cannot externalize, so at `push_message` it truncates
`Message::Tool` content above `context.tool_output_tokens` tail-biased:
keep the head half and the tail half, with a `... [~N tokens truncated] ...`
marker in between. Other roles pass through untouched.

#### Everything else

- **Context assembly**: prepend system prompt, return all messages. No prompt
  augmentation.
- **Tools**: none. Returns empty vec for both scopes.
- **`report()`**: loads every `sessions/*.json` (in-memory messages for the
  active session) and feeds them through the shared `stats::render` core.

---

### LCM Implementation

The primary implementation. Uses a SQLite database with a hierarchical DAG of
summaries that preserves all original messages while compressing older context
into navigable summary layers.

#### Data Model

**Messages**: raw conversation messages stored in SQLite.

| Column | Type | Description |
|--------|------|-------------|
| `message_id` | INTEGER PK | Auto-incrementing ID |
| `conversation_id` | INTEGER | Session foreign key |
| `seq` | INTEGER | Ordering within conversation |
| `role` | TEXT | `user`, `assistant`, `tool`, `system` |
| `content` | TEXT | Message content |
| `token_count` | INTEGER | Estimated tokens (`chars / 4`) |
| `created_at` | TEXT | ISO 8601 timestamp |

**Message parts**: decomposed message content for granular search.

| Column | Type | Description |
|--------|------|-------------|
| `part_id` | TEXT PK | Unique part identifier |
| `message_id` | INTEGER FK | Parent message |
| `part_type` | TEXT | `text`, `tool_call`, `tool_output` |
| `ordinal` | INTEGER | Part ordering within message |
| `text_content` | TEXT | Text content (for text parts) |
| `tool_call_id` | TEXT | Call ID (for tool parts) |
| `tool_name` | TEXT | Function name (for tool_call parts) |
| `tool_input` | TEXT | Function arguments JSON (for tool_call parts) |

The kitaebot `Message` enum is decomposed on write by the LCM engine:

| Message variant | Row | Parts |
|-----------------|-----|-------|
| `User` | role=user | 1 text part |
| `Assistant` | role=assistant | 1 text part |
| `ToolCalls` | role=assistant | 1 text part (content) + N tool_call parts |
| `Tool` | role=tool | 1 tool_output part (linked by call_id) |
| `System` | role=system | 1 text part |

This decomposition is internal to the LCM engine. The trait boundary uses
kitaebot's `Message` enum unchanged.

**Summaries**: DAG nodes representing compressed history.

| Column | Type | Description |
|--------|------|-------------|
| `summary_id` | TEXT PK | Deterministic ID (see Summary IDs below) |
| `conversation_id` | INTEGER | Session foreign key |
| `kind` | TEXT | `leaf` or `condensed` |
| `depth` | INTEGER | 0 for leaf, max(parent depths) + 1 for condensed |
| `content` | TEXT | Summary text |
| `token_count` | INTEGER | Estimated tokens in content |
| `earliest_at` | TEXT | Min timestamp of source messages |
| `latest_at` | TEXT | Max timestamp of source messages |
| `descendant_count` | INTEGER | Total nodes in subtree |
| `descendant_token_count` | INTEGER | Total tokens in subtree |
| `source_message_token_count` | INTEGER | Raw message tokens represented |
| `model` | TEXT | LLM that produced this summary |
| `created_at` | TEXT | When this summary was created |

**Summary IDs**: deterministic, computed as `SHA-256(content || sorted source
IDs)` truncated to 16 hex chars, prefixed with `sum_`. For leaf summaries, the
source IDs are the `message_id` values. For condensed summaries, the source IDs
are the child `summary_id` values. This ensures identical compaction inputs
produce identical IDs, making compaction idempotent. On collision (vanishingly
unlikely with 16 hex chars within a single conversation), append a monotonic
suffix.

**DAG edges**:

| Table | Columns | Description |
|-------|---------|-------------|
| `summary_messages` | `summary_id`, `message_id` | Leaf summary -> source messages |
| `summary_parents` | `summary_id`, `parent_summary_id` | Condensed -> child summaries it condensed |

**Large files**: references to files too large for inline context.

| Column | Type | Description |
|--------|------|-------------|
| `file_id` | TEXT PK | `file_` + SHA-256(path)[:16] |
| `conversation_id` | INTEGER | Session foreign key |
| `path` | TEXT | Filesystem path at time of encounter |
| `mime_type` | TEXT | Detected MIME type |
| `byte_size` | INTEGER | File size in bytes |
| `token_count` | INTEGER | Estimated tokens if loaded (`chars / 4`) |
| `exploration_summary` | TEXT | Type-aware summary (see Large File Handling) |
| `first_seen_message_id` | INTEGER FK | Message that first referenced this file |
| `created_at` | TEXT | ISO 8601 |

**File references in summaries**: junction table propagating file awareness
through the DAG.

| Column | Type | Description |
|--------|------|-------------|
| `summary_id` | TEXT FK | Summary that references this file |
| `file_id` | TEXT FK | The referenced file |

**Context items**: the active context ordering for a conversation.

| Column | Type | Description |
|--------|------|-------------|
| `conversation_id` | INTEGER | Session foreign key |
| `ordinal` | INTEGER | Chronological position |
| `item_type` | TEXT | `message` or `summary` |
| `message_id` | INTEGER | Set when item_type = message |
| `summary_id` | TEXT | Set when item_type = summary |

**Conversations**: session metadata.

| Column | Type | Description |
|--------|------|-------------|
| `conversation_id` | INTEGER PK | Auto-incrementing |
| `name` | TEXT UNIQUE | Session name (e.g. `general`, `kitaebot`) |
| `created_at` | TEXT | ISO 8601 |
| `updated_at` | TEXT | ISO 8601 |

**FTS**: virtual tables for full-text search.

```sql
CREATE VIRTUAL TABLE messages_fts USING fts5(content, content=messages, content_rowid=message_id);
CREATE VIRTUAL TABLE summaries_fts USING fts5(content, summary_id UNINDEXED);
```

`messages_fts` uses the external content pattern with `message_id` as the
rowid, so the FTS index is a thin overlay on the `messages` table. The
`summaries` table has a TEXT primary key (`summary_id`), which FTS5 cannot
use as a rowid, so `summaries_fts` is standalone and carries `summary_id`
as an UNINDEXED column for retrieval. Both tables are kept in sync with
the source via AFTER INSERT/UPDATE/DELETE triggers.

All tables are in a single database file. One `conversation_id` per project
session.

#### Connection Setup

Every connection opens with the following PRAGMAs:

```
PRAGMA journal_mode = WAL;       -- crash safety + concurrent readers
PRAGMA busy_timeout = 30000;     -- 30s wait before SQLITE_BUSY
PRAGMA foreign_keys = ON;        -- enforce DAG referential integrity
PRAGMA cache_size = -65536;      -- 64 MiB; default 2 MiB thrashes
PRAGMA synchronous = NORMAL;     -- safe with WAL, faster than FULL
PRAGMA temp_store = MEMORY;
```

The `REGEXP(pattern, text)` SQL function is registered as a user-defined
scalar via `rusqlite::functions::create_scalar_function`, backed by the
Rust `regex` crate. It is marked `SQLITE_DETERMINISTIC`. The pattern is
recompiled per call; caching with rusqlite `auxdata` is a follow-up once
`lcm_grep` is in heavy use.

#### Schema Migrations

Schema versioning is tracked via `PRAGMA user_version`. The engine holds
an ordered slice of migration SQL strings; entry `i` brings the database
from version `i` to version `i + 1`. The v1 baseline is the initial
schema DDL.

On open, the engine reads the current `user_version` and runs each
migration whose index is `>= user_version`. Each migration runs inside its
own `BEGIN EXCLUSIVE; ...; PRAGMA user_version = N; COMMIT;` block, so
two processes cannot interleave migrations and a partial failure rolls
back atomically (both the DDL and the version bump). On a statement-level
failure mid-transaction the engine issues an explicit `ROLLBACK` because
SQLite does not implicitly rewind the open transaction.

Adding a migration is append-only: push the new SQL string onto the slice.
Existing entries are never reordered, edited, or removed — that would
break every database that has already advanced past them. Within a single
migration, prefer `IF NOT EXISTS` clauses so a partially applied prior
attempt remains recoverable.

#### The DAG

The DAG has four layers:

1. **Raw messages (layer 0)**: original messages in the `messages` table.
2. **Leaf summaries (depth 0)**: compress chunks of raw messages. Each leaf
   points to its source messages via `summary_messages`.
3. **Condensed summaries (depth 1+)**: compress groups of same-depth
   summaries (run length >= `min_condensed_fanout`, default 2). Each
   condensed node points to its children via `summary_parents`. Depth =
   parent depth + 1 (children of a condensed pass share a depth by
   construction).
4. **Active context**: the `context_items` table — an ordered sequence of raw
   messages (the protected tail) and summary nodes (older compressed history).

Edges are bidirectional by convention: summaries point to their sources
(messages or child summaries), enabling traversal from any summary back to the
original raw content.

#### Large File Handling

Tool results frequently include file contents that individually approach or
exceed the context budget. A single large log file or codebase dump can consume
the entire window in one turn.

**Thresholds**: two, by message role. User content above
`large_file_threshold` tokens (default: 25,000) is stored externally rather
than inlined into the active context. Tool results threshold on the much
lower `context.tool_output_tokens` (default: 4,096) because they arrive on
every turn; instead of an LLM exploration summary they get a free
**mechanical excerpt** — first and last ~30 lines (byte-capped per side)
with an omission marker in the middle. The tail matters: build and test
logs put the failure at the end.

**On push_message**: when the engine receives content above the applicable
threshold, it:

1. Stores the file path, size, and metadata in the `large_files` table.
2. Generates an **exploration summary**: a type-aware dispatcher for user
   payloads, the mechanical excerpt for tool results.
3. Replaces the file content in the active context with a compact reference:

```
<file id="file_abc123" path="data/output.json" tokens="142000">
Exploration summary text here...
</file>
```

The `path` attribute comes from an in-band lookup: for an oversized tool
result, the engine queries the already-persisted `tool_call` part of the
originating `file_read` call (linked by `call_id`) and reads its `path`
argument. No state is carried between pushes and the `ContextEngine` trait is
untouched. When no hint exists (e.g. an oversized raw user message), the
attribute carries the payload's stored location instead —
workspace-relative (`context/lcm/payloads/<file_id>`), because both the
reference and `lcm_describe` hand this path to the model and the file
tools reject absolute paths. `large_files.path` records the same value
the reference carries, in both cases.

**Externalization at ingest**: the oversized raw payload is written to disk
under `context/lcm/payloads/<file_id>` and the `messages.content` row stores
the `<file>` reference, not the raw bytes. `lcm_expand` reads from disk
when recovering originals.

This is a deliberate departure from a strict "messages are stored verbatim"
invariant. Storing tens-of-megabytes tool outputs inline ballooned the
SQLite database and slowed FTS in the reference implementation; it
externalizes at ingest for the same reason (see `large-files.ts`,
`formatRawPayloadReference`). The `<file>` reference plus on-disk payload
together remain the source of truth.

**Type-aware exploration dispatchers**:

| File type | Strategy |
|-----------|----------|
| JSON | Schema extraction: top-level keys, array lengths, value types. Sample first/last elements of large arrays. |
| CSV | Column names, row count, sample rows (first 3 + last 3). |
| Code (`.rs`, `.py`, `.ts`, etc.) | Structural analysis: function/method signatures, struct/class definitions, import list. Uses tree-sitter or regex, not LLM. |
| SQL / database dumps | Table names, schema (CREATE TABLE statements), row counts if available. |
| Plain text / logs | LLM-generated summary via `SummarizeFn`. Bounded to `large_file_summary_tokens`. |
| Binary / unknown | Size, MIME type, first 256 bytes hex dump. No LLM call. |

Only the plain text dispatcher requires an LLM call. All others are
deterministic.

**File ID propagation through the DAG**: when messages referencing a file are
compacted into a summary, the engine copies the `file_id` associations to the
new summary via the `summary_files` junction table. This ensures that even
after multiple compaction rounds, the model retains awareness of files
encountered earlier in the session. The `<file>` reference tags are included in
summary content by the summarization prompt.

**File re-reads**: when the agent uses `file_read` on a path that matches an
existing `large_files` entry, the tool operates normally (reads from disk). The
engine does not intercept reads — it only intercepts storage of tool output
into the active context.

#### Compaction

Compaction uses a dual-threshold system with three-level summarization
escalation.

##### Dual Thresholds

Two token thresholds govern when compaction fires. Both compare against
`max(estimate, observed)` (see §"Observed Tokens"):

| Threshold | Default | Config | Behavior |
|-----------|---------|--------|----------|
| Soft (`tau_soft`) | `effective * 0.70` | `context.lcm.soft_budget_percent` (70) | Synchronous compaction at the turn boundary, after the reply is delivered — and only when the actor's mailbox is empty, so a burst of queued turns keeps its warm cache and compaction lands at the end of the burst. |
| Hard (`tau_hard`) | `effective * 0.90` | `context.lcm.hard_budget_percent` (90) | Emergency: synchronous compaction before the next completion, mid-turn if necessary. |

`effective` is `context.max_tokens - provider.max_tokens`: the provider can
generate up to its output budget on top of the prompt, so thresholds apply to
the window minus that reserve (computed once at startup via
`Config::effective_context`; config validation requires `context.max_tokens >
provider.max_tokens`). The same reserve applies to the flat engine's
`budget_percent`.

The control loop:

```
1. before each completion:
       if tokens(context) > tau_hard: compact synchronously (emergency)
2. push_message(msg) -> persist to store, append to context_items
3. after the reply is delivered (turn boundary):
       if tokens(context) > tau_soft: compact synchronously
```

**Why the turn boundary, and not a background task.** Compaction rewrites
`context_items`, and `assemble()` reads them fresh on every call, so any
mid-turn rewrite changes the prompt prefix and cold-starts the provider's
prompt cache for every remaining completion in the turn. An earlier design
spawned soft-threshold compaction as a background task; its writes landed
whenever it finished, which was mid-turn in practice, and one observed turn
paid for three full cold re-reads of a ~90k-token session because of it
(2026-08-10, the first `git_rebase` turn). At the turn boundary the
damage is bounded at one call: only the next turn's first completion can
lose a cache hit, and in this deployment it rarely had one — turns on a
session are typically separated by longer than the implicit cache's TTL,
and the prompt prefix mutates between turns anyway (the memory index is
re-read fresh each root turn, and role segments differ per dispatch). The
deployed model's window (1M) dwarfs `context.max_tokens` (200k), so letting
a turn run past `tau_soft` costs cached-rate tokens only; the hard check
before each completion remains as the emergency for a pathological turn —
it pays the cache cold-start deliberately, because the alternative is an
oversized request. Removing the background task also removed its
concurrency: no half-finished pass to drain, no double-spawn guard, no
task racing `force_compact` for the same chunks.

**Overhead regimes**:

| Context size | Overhead |
|-------------|----------|
| `< tau_soft` | None. Store acts as passive logger. |
| `tau_soft <= size < tau_hard` | Synchronous compaction at the turn boundary; delays the next dispatch on the session, never the reply. |
| `>= tau_hard` | Emergency synchronous compaction before the next completion; cold-starts the cache mid-turn. |

Below the soft threshold the engine adds zero latency; in the soft band the
cost is between turns where the cache is already cold; only the hard band —
reachable only by a single turn growing ~20% of the window past soft —
touches a live turn.

**Protected tail**: the most recent N messages (configurable, default 32) are
never compacted. They remain as raw messages in the active context.

**Pinned user request**: the newest `user` message is additionally exempt
from compaction wherever it sits. A long working turn pushes hundreds of
tool messages through the tail, so without the pin the task statement is
among the first things summarized — exactly when the turn still needs its
verbatim wording. Leaf-pass chunk selection skips the pinned row; chunks
split around it. A newer user message moves the pin and the superseded
message becomes ordinary compactable history. Oversized user content is
already externalized at ingest, so the pinned row is small by
construction.

##### Two-Phase Compaction

1. **Leaf pass**: select the oldest contiguous raw messages outside the
   protected tail. Chunk them (up to `leaf_chunk_tokens`). Summarize each
   chunk via the three-level escalation protocol. Create leaf summary nodes,
   link to source messages via `summary_messages`, replace the message range
   in `context_items` with the new summary. Propagate any `file_id`
   associations from source messages to the new summary.

2. **Condensed pass**: iterate from shallowest depth upward. Select oldest
   contiguous summaries at the same depth. Require minimum fanout
   (`min_condensed_fanout`, default 2). Summarize via the three-level
   escalation protocol. Create condensed node, link to children via
   `summary_parents`, replace the summary range in `context_items` with
   the new condensed node. Propagate `file_id` associations from child
   summaries.

##### Three-Level Summarization Escalation

When a chunk needs summarizing, the engine attempts three levels in order. If a
level fails to reduce token count (output >= input), it escalates to the next.
Output under 500 characters is rejected as degenerate and escalates the same
way — a few-line summary of a chunk worth compacting is a model failure, not
compression. The floor applies only to inputs of at least four times its size
(~2,000 estimated characters): a small residual chunk can honestly summarize
to under the floor, and rejecting that wastes two LLM calls to end at level-3
passthrough. The escalation ladder is the retry mechanism — no per-level
retries — and for floor-gated inputs level 3's 512-token truncation yields
strictly more content than any degenerate summary it replaces.

| Level | Strategy | Target tokens | LLM? |
|-------|----------|---------------|------|
| 1. Normal | `preserve_details` mode — retain specifics, decisions, file paths, commands | Target = chunk token count / compression_ratio | Yes |
| 2. Aggressive | `bullet_points` mode — key decisions and outcomes only | Target = level 1 target / 2 | Yes |
| 3. Deterministic | Truncate to first 512 tokens, append `[Truncated from N tokens]` | 512 | No |

Level 3 guarantees convergence. It requires no LLM call and always reduces.

**Summarization prompts**: every summarization call uses the same fixed,
minimal system prompt (`SUMMARIZER_ROLE_PROMPT`) that establishes the
model's role as a context-compaction engine. Per-call instructions go in
the user turn alongside the formatted conversation, wrapped in
`<conversation_segment>` tags. This split (role in system, instructions
in user) mirrors the reference implementation and makes per-call prompt
variation a one-string change in the engine.

Level 1, Level 2, and the flat session each carry a distinct instruction
block:

- **Level 1 (LCM, normal)**: prose summary preserving specifics —
  decisions, file paths, commands, tool outcomes.
- **Level 2 (LCM, aggressive)**: terse bullets, decisions and outcomes
  only. Opens by acknowledging that level 1 was rejected for being too
  long.
- **Flat session**: one block, similar in spirit to level 1 but without
  LCM-specific read-back conventions (no DAG to expand into).

LCM blocks (L1 and L2) enforce two read-back conventions:

- **`Files:` tracking line** — the summary lists files touched, or
  includes `Files: none`. Each entry carries a short clause on why the
  file matters (`src/exec.rs — added retry wrapper`), so a path that
  survives into deep condensed layers still says why to reopen it. Lets
  future read-back scan for file activity without parsing prose.
- **`Expand for details about: <...>` trailer** — every LCM summary ends
  with this hook, naming the region the next model should `lcm_grep` or
  `lcm_expand` into if it needs the dropped details.

The flat session keeps `Files:` tracking for parity but skips the trailer:
it has no DAG, so there is nothing to expand.

Both LCM blocks also instruct the model to preserve file IDs and `<file>`
reference tags, include timestamps for key decisions, note tool usage and
outcomes, and omit verbose tool output (already in the immutable store).

The `model` column on the summary records which LLM produced it. If a
dedicated summarizer model is configured (`provider.model_overrides.summarizer`),
it is used instead of the agent's primary model. This allows using a
cheaper/faster model for summarization. Level 3 records
`level3-truncate` (no LLM).

**Deferred**:

- **Depth-aware condensed prompts** (D1, D2, D3+ in the reference) — until
  we observe real DAG depth in production, we ship one condensed prompt
  per level (L1, L2). Splitting by depth is a one-prompt-per-depth change
  to the escalator.
- **`previous_summary` chaining** for differential summarization — adds a
  `previous_summary` parameter to the escalator and a query against the
  latest same-depth summary. Lands once we see drift across compactions.
- **Operator/custom instructions block** — easy to bolt on once there is
  a user-facing surface for custom summarization instructions.

**Metadata propagation**: on each summary creation, propagate
`descendant_count`, `descendant_token_count`, `source_message_token_count`,
and time ranges up through the DAG.

#### Context Assembly

`assemble()` builds the message list sent to the provider:

1. Fetch `context_items` for the active conversation, ordered by `ordinal`.
2. Resolve each item:
   - Message items: reconstruct `Message` from the `messages` table.
   - Summary items: format as a `Message::System` with structured metadata
     (id, kind, depth, time range, content).
3. The protected tail (recent raw messages) is always included.
4. Augment system prompt with LCM recall guidance when summaries are present
   in the assembled context.

There is no budget-aware truncation at assembly time — compaction is the
size control. Assembly renders whatever `context_items` currently holds.

**Summary format in context** (injected as system message content):

```
<summary id="sum_xxx" kind="leaf" depth="0"
         earliest_at="2025-01-15T10:30:00" latest_at="2025-01-15T12:45:00">
Summary text here...
</summary>
```

**Written-files recall**: when the assembled context contains summaries,
the engine appends a mechanical segment listing the distinct paths passed
to `file_write`/`file_edit` since the pinned user message, newest first,
capped at 30:

```
## Files Written For This Request
src/agent/mod.rs
src/tools/direnv.rs
```

Sessions are long-lived, so the scope is the current request, not the
session: a session-wide list degrades into "files ever written in this
repo", going stale across tickets and workspace cleans. The request
scope targets the one window where recall is actually lost — a long
turn hard-compacting away its own earlier tool calls. Cross-turn recall
within a task is git's job (`git status`, `git log`), not context's.

Derived at `assemble()` by querying stored `tool_call` parts for those
tool names at `seq` greater than the pinned user message, reading the
`path` argument — the store already holds every call, so no new state
and no bookkeeping. When nothing after the pin has been compacted the
raw calls are still visible in context and the segment is omitted.
Writes made by worker sub-agents (their transcripts never enter the
store) or via exec redirects are invisible to the query; that is the
accepted trade for a purely derived list.

**Recall guidance** (appended to system prompt when summaries are present):

```
## Compacted History

Summaries above are compressed context — maps to details, not details
themselves. Use retrieval tools before asserting specifics from summaries.

Tool escalation:
1. lcm_grep — search by keyword or regex
2. lcm_describe — inspect a specific summary's metadata and lineage
3. lcm_expand — drill into a summary to retrieve children or source messages
   (sub-agent only — see below)

Do not guess exact values (commands, paths, SHAs, config) from condensed
summaries. Use lcm_grep to search, or delegate expansion to a sub-agent.
```

#### Retrieval Tools

The LCM engine contributes three tools via `tools()`:

**`lcm_grep`** — search compacted history.

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `pattern` | String | yes | Search pattern (FTS5 query syntax or regex) |
| `mode` | String | no | `regex` or `fts` (default: `fts`) |
| `scope` | String | no | `messages`, `summaries`, or `both` (default: `both`) |
| `limit` | u32 | no | Max results (default: 50) |

`fts` mode uses SQLite FTS5 — token-based matching with boolean operators
(`AND`, `OR`, `NOT`, phrase queries). This is the fast path for keyword
searches. Patterns that fail FTS5 parsing (punctuation-heavy literals like
`isl-0.20`, which models pass routinely) are retried once as a quoted
phrase instead of surfacing the syntax error.

`regex` mode uses a `REGEXP` user function registered on the SQLite connection.
It scans the `content` column directly (no index), so it is slower than FTS but
supports arbitrary patterns. The engine registers a Rust regex implementation
via `rusqlite::functions::create_scalar_function`.

Returns matching snippets with IDs for follow-up via `lcm_describe` or
`lcm_expand`. Results are grouped by the summary node that currently covers
them in the active context, so the model knows which region of the conversation
each match belongs to.

**`lcm_describe`** — inspect a summary or file node.

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | String | yes | Summary ID (`sum_xxx`) or file ID (`file_xxx`) |

For summary IDs: returns the summary's full content, depth, time range,
parent/child relationships, subtree statistics (descendant count, total
tokens), and associated file IDs.

For file IDs: returns the file path, MIME type, byte size, token count, and
the exploration summary. Provides enough context to decide whether to re-read
the file from disk.

Provides cost annotations for expansion decisions (e.g. "expanding this
summary would yield ~12,000 tokens across 3 children").

**`lcm_expand`** — drill into a summary.

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `summary_id` | String | yes | Summary to expand |
| `depth` | u32 | no | How many levels to expand (default: 1) |
| `include_messages` | bool | no | Include raw source messages for leaf nodes (default: false) |
| `token_cap` | u32 | no | Max tokens in response (default: 5000) |

Recursively expands a summary. For condensed nodes, fetches children. For leaf
nodes, optionally fetches source messages. Stops when `token_cap` is reached.

**Sub-agent restriction**: `lcm_expand` is restricted to sub-agents only. The
main agent loop cannot call it directly. This prevents context flooding in the
primary interaction loop — expanding a deep summary can recover arbitrarily
large volumes of earlier conversation, which would defeat the purpose of
compaction. When the main agent needs to inspect compacted history, it
delegates the expansion to a sub-agent (see spec 19), which processes the
expanded content in its own context window and returns only the relevant
findings. The restriction is enforced via `tools(ToolScope::Root)` omitting
`lcm_expand`.

All three tools operate on the active conversation only. Cross-session search
is a future enhancement.

#### Token Estimation

Same heuristic as the current system: `chars / 4`. No tokenizer dependency.
Used for budget checks, compaction triggers, and `token_cap` enforcement in
retrieval tools.

---

### Agent Loop Integration

The agent loop changes are minimal. The current flow:

```
1. session.load()
2. context.compact_if_needed(session, provider)
3. session.add_message(user_msg)
4. loop { provider.chat(system_prompt + session.messages, tools) }
5. session.save()
```

Becomes:

```
1. engine.push_message(user_msg)
2. engine.compact_if_needed(&summarize_fn)  // may spawn (soft) or block (hard)
3. loop {
       ctx = engine.assemble(system_prompt)
       provider.chat(ctx.messages, base_tools + engine.tools())
   }
4. engine.save()
```

`AssembledContext` carries a single `messages: Vec<Message>` field
with the system prompt prepended. The provider receives the message
list directly; there is no separate `system_prompt` parameter on the
chat call.

The `engine.load()` call happens once at startup (or on session switch), not
per-turn. For LCM, the SQLite connection stays open. For flat session, the JSON
file is loaded into memory.

The actor resolves the target session per-envelope:

```
target = envelope.session_hint.unwrap_or(engine.active_session())
if target != engine.active_session():
    engine.save()
    engine.switch_session(target)
```

After the turn, if the actor switched away from the user's active session for
a routed envelope (e.g. GitHub), it switches back.

---

### Slash Commands

| Command | Behavior |
|---------|----------|
| `/context` | Token usage, message count, budget percentage from `engine.stats()` |
| `/compact` | Delegates to `engine.force_compact()` |
| `/new` | Delegates to `engine.clear()` |
| `/stats` | Per-engine usage report from `engine.report()` |
| `/project` | List sessions, show active |
| `/project <name>` | Switch session |

#### Usage report

`report()` returns a rendered string. All engines feed their stored messages
through the shared analysis core in `stats.rs`: per-tool call counts and
output bytes, exec command breakdown, failure classification by content
prefix, and blocked-command / repeated-call tables. The report is
cross-session — every session or conversation the engine knows about, not
just the active one.

The LCM engine reads the raw `messages` table (not the live context), so
history that compaction folded away still counts. It appends a health
section: summary counts by depth, `level3-truncate` count (failed
summarizations), raw-vs-context token totals per conversation, and
externalized large-file count/bytes.

---

### Configuration

```toml
[context]
engine = "lcm"              # "lcm" or "flat"
max_tokens = 200000
budget_percent = 80         # flat session compaction trigger
tool_output_tokens = 4096   # engine-level tool result size policy

[context.lcm]
fresh_tail_count = 32
leaf_chunk_tokens = 20000
min_condensed_fanout = 2
soft_budget_percent = 70
hard_budget_percent = 90
large_file_threshold = 25000
large_file_summary_tokens = 400
```

| Config key | Default | Description |
|------------|---------|-------------|
| `context.engine` | `flat` | Which engine implementation to use |
| `context.max_tokens` | `200000` | Model context window size. Must be > `provider.max_tokens`; engines see the window minus that output reserve. |
| `context.budget_percent` | `80` | Flat-session compaction trigger (1-100). Ignored by LCM, which uses the dual thresholds below. |
| `context.tool_output_tokens` | `4096` | Tool result content above this many estimated tokens is size-limited by the engine: LCM externalizes it with a mechanical excerpt; the flat engine truncates it tail-biased at push. Must be > 0. |
| `context.lcm.fresh_tail_count` | `32` | Protected tail size (raw messages exempt from compaction). Must be > 0. |
| `context.lcm.leaf_chunk_tokens` | `20000` | Max tokens per leaf or condensed chunk |
| `context.lcm.min_condensed_fanout` | `2` | Minimum children to form a condensed summary. Must be >= 2. |
| `context.lcm.soft_budget_percent` | `70` | Soft compaction threshold (async, non-blocking). 1..=100, must be < `hard_budget_percent`. |
| `context.lcm.hard_budget_percent` | `90` | Hard compaction threshold (sync, blocking). 1..=100. |
| `context.lcm.large_file_threshold` | `25000` | Message content above this many estimated tokens is externalized to disk at ingest. Must be > 0. |
| `context.lcm.large_file_summary_tokens` | `400` | Token bound on LLM exploration summaries for externalized plain-text payloads. Must be > 0. |

Backend selection happens at startup. Changing `context.engine` requires a
restart. The old backend's data remains on disk but is not used.

Every engine owns a namespaced subdirectory of
`<workspace>/context/` and lays it out as it pleases:
`context/flat/sessions/<name>.json` for the flat engine,
`context/lcm/lcm.db` plus `context/lcm/payloads/` for LCM, each with
its own `active_session` cursor. The workspace hands the engine one
directory and looks no deeper; the per-engine namespace makes the
"old backend's data remains on disk" guarantee structural — switching
engines cannot clobber another backend's files, including the cursor
both would otherwise share.

The cheaper summarization model called for by phase 4 (large file
handling) is configured via `provider.model_overrides.summarizer`
(see [spec 02](02-provider.md)).

### Implementing a new engine

The trait is the whole integration surface, but four contracts are
not visible in the signatures:

- **Dense distillation positions.** `transcript_since` positions must
  be dense per session: the distiller advances its watermark by
  `after + returned.len()`, and `latest_positions` must report the
  tip the full transcript would advance to. Sparse IDs or a
  reordering store silently corrupt watermarks (spec 21).
- **Observed tokens drop on shrink.** `observe_tokens` records the
  provider's ground-truth prompt size; the engine must discard it on
  compaction, clear, and session switch, or `max(estimate, observed)`
  re-triggers compaction forever on a stale high-water mark.
- **Maintenance need not summarize.** `compact_if_needed` receives a
  summarizer, not an obligation: an engine whose pressure response is
  something else (dropping stale tool results, cache-aware pruning)
  ignores the callback. `force_compact` currently demands an event
  even from engines with nothing to do (see Open Questions).
- **Backup assumes local files.** `ContextEngine::backup` stages the
  engine's namespaced `context/<name>/` subdirectory; an engine whose
  state is not on the local filesystem breaks this assumption and the
  spec 05 backup contract with it.

Mechanically: the constructor takes the shared `context/` directory
and must namespace its own subdirectory inside it; wiring means one
new `EngineKind` arm in `main.rs`'s spawn match and one in
`backup.rs`'s stage match — both exhaustive, so the compiler walks
you to every site.

### Active Session Persistence

The active session name is written to `state/active_session` (plain text,
atomic write). On startup, the engine reads this file to restore the last
active session. If the file is missing the engine falls back to `general`.
For LCM, an unknown name simply creates a new conversation row.

## Boundaries

### Owns

- Message storage and retrieval
- Context assembly (what gets sent to the provider)
- Compaction trigger logic and execution (dual-threshold, three-level escalation)
- Large file detection, exploration summary generation, and reference tracking
- Summarization prompts and LLM calls (via borrowed `CompleteFn`)
- Session lifecycle: create, switch, clear, persist
- Active session tracking across restarts
- Retrieval tools (LCM: grep, describe, expand)
- The `ContextEngine` trait definition and shared types
- Token estimation
- `/context`, `/compact`, `/new`, `/project`, `/stats` command implementations

### Does Not Own

- The provider — completion is borrowed via callback
- The agent loop — orchestration stays in the actor
- Channel transport — channels provide session hints, the actor routes
- System prompt content — sourced from the workspace, engine may augment
- Tool dispatch — engine-contributed tools are merged into the existing
  registry
- Safety/leak detection — tool output still passes through the safety layer
- Filesystem confinement — Landlock handles that
- Sub-agent spawning — spec 19 owns that; this spec defines what tools are
  restricted to sub-agents

### Interactions

- **Actor**: calls `compact_if_needed`, `push_message`, `assemble`, `save` per
  turn. Handles session routing per-envelope using `switch_session`.
- **Channels**: GitHub channel provides `session_hint` on envelopes.
  Socket/Telegram use the active session.
- **Tool registry**: `engine.tools()` returns additional tools at startup.
  These are merged into `Tools::new()` alongside the base tools.
- **Workspace**: the engine reads the system prompt via `assemble()`. The
  workspace module still owns prompt file concatenation; the engine receives
  the result and may augment it.
- **Activity system**: compaction events are reported via `Activity::Compaction`.
- **Sub-agents (spec 19)**: sub-agents receive `lcm_expand` in their tool set.
  The main agent does not. This is enforced by the engine's `tools()` method
  taking a `ToolScope` argument (`Root` | `SubAgent`). Sub-agent tool sets
  hold the same tool instances (shared `Arc`s over the engine's connection
  and active conversation id), so children query the parent's store directly.

## Failure Modes

| Failure | Error | Behavior |
|---------|-------|----------|
| SQLite open/init fails | `EngineError::Storage` | Fatal at startup |
| Compaction LLM call fails (level 1) | — | Escalate to level 2 (aggressive) |
| Compaction LLM call fails (level 2) | — | Escalate to level 3 (deterministic truncation) |
| Compaction LLM output exceeds input or degenerate (level 1) | — | Escalate to level 2 |
| Compaction LLM output exceeds input or degenerate (level 2) | — | Escalate to level 3 |
| Level 3 deterministic truncation | — | Always succeeds. Guaranteed convergence. |
| Async compaction fails | — | Logged. Next turn retries via `compact_if_needed`. Context unchanged. |
| Session not found on switch | — | Created automatically (idempotent) |
| Active session file missing | — | Fall back to `general` |
| Active session file corrupt | — | Fall back to `general` |
| FTS query fails | `EngineError::Storage` | Error text returned to LLM via tool error |
| Regex query timeout / invalid pattern | `ToolError::ExecutionFailed` | Error text returned to LLM |
| Token cap exceeded during expand | — | Partial result with `truncated: true` |
| Large file exploration summary fails | — | Fall back to size + MIME metadata only |
| JSON session file corrupt (flat) | `EngineError::Session` | Propagated to caller |
| Filesystem I/O error | `EngineError::Session` | Propagated to caller |

## Constraints

- One engine active at a time, selected at startup via config
- One session active at a time per the actor (sequential envelope processing)
- Token estimation is `chars / 4` — no tokenizer library
- LCM database is a single SQLite file in the workspace
- No cross-session search (tools query the active conversation only)
- No session deletion
- `lcm_expand` restricted to sub-agents (enforced via `ToolScope`)
- Routed envelopes (GitHub/Linear session hints) rewrite the
  `state/active_session` file on switch: a crash mid-turn restores the
  routed session, not the one the user last selected via `/project`
- Async compaction requires the actor to check for pending results before each
  `assemble()` call
- Async compaction uses `tokio::spawn`; the spawned task opens its own
  writer `Connection` rather than sharing the engine's
  `Arc<Mutex<Connection>>`. WAL allows one writer plus concurrent
  readers, so the actor's reads on the main connection proceed
  unimpeded while the background task writes. The engine holds the
  `JoinHandle` and drains it at the start of every compaction call.
- Large file detection operates on tool results only — user messages are never
  replaced with file references
- Exploration summary dispatchers for structured formats (JSON, CSV, code, SQL)
  are deterministic — no LLM calls. Only plain text/logs use the `SummarizeFn`.

## Dependencies

### Spec 19 (Sub-Agents)

The paper's full architecture (LCM paper §3, "From Symbolic to
Operator-Level Recursion") includes operator-level recursion primitives
and a scope-reduction invariant for sub-agent delegation. **None of that
lives in this spec.** Spec 19 owns it. Spec 14 ships a complete and usable
LCM engine on its own.

Owned by spec 19:

- **The `task` tool** for sub-agent spawning (paper Appendix C.3 describes
  `Task`/`Tasks`; spec 19 ships a single tool — parallelism comes from the
  model emitting multiple `task` calls in one response).
- **`llm_map` and `agentic_map`** for operator-level recursion over
  unbounded datasets (paper §3.1, Appendix C.2). These offload data
  parallelism from model-generated loops to deterministic engine
  primitives. `llm_reduce` and `agentic_reduce` are explicit non-goals
  per paper §3.1; reduction is better served by code the agent writes
  against schema-validated `*_map` outputs.
- **Scope-reduction invariant** for nested delegation (paper §3.2):
  sub-agents that spawn further sub-agents must declare `delegated_scope`
  and `kept_work`. Calls that delegate the entire responsibility are
  rejected. Root agents and read-only exploration agents are exempt.
- **Enforced `lcm_expand` restriction**. Spec 19 owns the sub-agent-only
  access; this spec only defines the `ToolScope` split that carries it.

Interfaces between this spec and spec 19:

- `engine.tools(scope)` with `ToolScope::Root | ToolScope::SubAgent`.
  Root agents do not receive `lcm_expand`; sub-agents do.
- `SummarizeFn` is reused for `llm_map` per-item calls. The operator's
  worker pool, retry, and schema validation live in spec 19.
- The immutable store provides the registration surface for `*_map`
  output JSONL files (paper §3.1, "Database-Backed Execution").

## Open Questions

1. Should the flat session implementation be maintained long-term, or
   deprecated once LCM is stable?
2. Cross-session search — should `lcm_grep` optionally search across all
   sessions? If so, how should results from other sessions be presented?
3. Per-project heartbeats — should `HEARTBEAT.md` be per-session so each
   project can have its own recurring tasks?
4. Large file detection heuristic — should the engine intercept all tool
   results, or only `file_read` output? Intercepting all results catches
   cases like `exec` returning large output, but may have false positives.
5. Pin depth — the pin covers only the newest user message. If work
   sessions show older-but-live requests being summarized away (e.g. a
   follow-up instruction arriving mid-task), extend to the last N user
   messages under a token cap.

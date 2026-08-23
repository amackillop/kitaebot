# Spec 01: Agent Loop

## Motivation

The agent loop is the core execution engine. It orchestrates the conversation
between the user, the LLM, and the tools. Each "turn" sends context to the LLM
and either receives a final text response or executes tool calls in a loop until
the LLM produces one.

## Behavior

### Turn Lifecycle

A turn proceeds in this order:

1. Compact the session if the token budget is exceeded (see [spec 14](14-context-engine.md))
2. Push the user message onto the session
3. Enter the tool loop (up to `max_iterations`):
   a. Prepend the system prompt to the session messages (not persisted)
   b. Call the provider
   c. Feed the response's `prompt_tokens` (when the API reports usage)
      into the engine via `observe_tokens` — ground truth for the next
      compaction check (see [spec 14](14-context-engine.md))
   d. If `Response::Text` — store assistant message, then exit with the
      text — except once per turn under `ReplyPolicy::Confirm` (see
      Reply Confirmation), where the loop continues instead
   e. If `Response::ToolCalls` — store assistant message, run safety gates,
      execute calls in parallel, record results, continue loop

Compaction runs **before** the user message is added, so the current input is
never summarized away.

The system prompt is the startup concatenation of `SOUL.md`, `AGENTS.md`,
and `USER.md` (all optional), read once at workspace init (see
[spec 06](06-system-prompt.md)).

### Tool Result Recording

Every successful tool output is passed through the safety layer (see
[spec 11](11-safety.md)). Clean outputs are wrapped in XML tags:

```
<tool_output name="tool_name">
output
</tool_output>
```

If the safety layer detects a leaked secret, the output is replaced with an
error message and never enters the session.

### Repetition Detection

The loop fingerprints each iteration's tool calls as a set of `(name, args)`
pairs (compared as `serde_json::Value` so key order is irrelevant).

| Consecutive identical count | Behavior |
|-----------------------------|----------|
| 1-2 | Execute normally |
| 3   | Skip execution, inject error as each call's result |
| 4   | Skip, inject, then abandon the turn with `NoProgress` |

The counter resets when the fingerprint changes, and a round that
executes anything clears the strike count — the gate is about lack of
progress, and running a different tool is progress.

Skipping stops the tool; it does nothing about a model that keeps
asking. A live turn spent 76 of its 100 iterations re-sending one
refused call, each a full provider call, so refusal needs a limit behind
it. `NoProgress` is distinct from `MaxIterationsReached` because the
budget was not the problem: the turn had rounds left and was spending
them on a result it already had. It stays an error rather than the
final-answer squeeze so unattended turns still raise an alert, which a
successful-looking degraded reply would not.

The refusal is injected even on the round that ends the turn: every
`tool_call` needs a matching result before the next completion, so
skipping that would leave the stored context malformed.

### Policy Violation Gate

A blocked tool call is just an error string to the model: nothing stops
it from ignoring the message and trying creative workarounds, and the
repeat detector cannot catch that because the arguments change on every
attempt. The gate puts a limit behind refusals: escalate to a human
instead of letting the model negotiate with the guardrails.

When a tool returns `ToolError::Blocked`, a strike counter increments for
the rule that fired, identified by its guidance string (the convention:
`operation` carries the variable content, guidance is static per rule).

| Strike (per rule) | Behavior |
|--------|----------|
| 1 | Inject a system message directing the LLM to stop attempting the blocked operation. Continue the turn. |
| 2 | Halt the turn immediately. The turn returns a distinct `PolicyHalt` outcome carrying the blocked operations and their guidance; channels render it as a synthetic response. |

Distinct rules strike independently — the gate targets workarounds of a
refusal the model has already seen, so a halt only ever fires after that
rule's guidance reached the model in an earlier round. A long turn's
unrelated first offenses each get their own directive and the turn
continues (a live execution turn was once halted for an absolute
`working_dir` plus a deny-listed `git fetch` three minutes apart,
killing finished work at the pre-push check). Parallel calls blocked in
the same round count once: they were issued before the directive could
land.

A total cap backstops cross-rule evasion: four blocked rounds in one
turn halt it regardless of rule diversity. Below the cap distinct rules
learn independently; a turn that keeps finding new walls is probing the
guardrails, not learning them.

Strike counters reset per turn. The turn's success type is an ADT
(`Text`, `PolicyHalt`, or `ToolHalt`), so callers can tell a halted turn
from a normal reply without string-sniffing — the hook for notifying on
unattended failures.

### Tool Strike Escalation

The repeat detector only fires on *consecutive* identical calls. A
model that interleaves other tool calls between retries of the same
failing call escapes it, and a deterministic environment failure
(egress-blocked URL, missing binary) can burn most of the iteration
budget before the turn grinds to `MaxIterationsReached` with no
deliverable. A live duty turn spent 30 calls on `github_ci_status`
across eight iterations, every one failing on the same egress-blocked
/logs redirect.

The tool strike tracker counts failures keyed on
`(tool name, canonical args, error class)` across the whole turn,
surviving interleaving. `ToolError::Blocked` is excluded — the
policy strike system already handles it.

| Identical failures | Behavior |
|-------------------|----------|
| 1-2 | Error text returned to LLM as normal. |
| 3   | Error text augmented: "this exact call has failed N times this turn; the failure is deterministic — stop retrying and adapt or report." |
| 5   | Turn halts with `ToolHalt { tool, args, error_class, count }`. |

Timeouts are a distinct retry class: build artifacts persist in the
target dir / nix store, so a retry resumes further along. The timeout
error text says so: "Partial progress persists — a retry resumes where
this stopped." At the notice threshold it adds a caveat: if the
store/target mtimes have not advanced, the failure is deterministic.

The strike signature uses `error_class`, a coarse classification of
the `ToolError` variant (e.g. `http_status:502`, `command_failed:1`,
`timeout`). Two calls with the same tool and args but different error
classes strike independently — only an identical failure mode is
deterministic.

### Reply Confirmation

A text response with no tool calls ends the turn — the right contract
for attended chat, where a human reads the text as conversation. Some
channels post the turn's text verbatim to an external medium (GitHub
issue and PR comments, Linear), and there the model occasionally ends a
work turn with mid-reasoning narration instead of a deliberate report;
a live issue turn published its internal monologue about a borrow-checker
fight as a public comment.

`ReplyPolicy` names the two contracts:

| Policy | On `Response::Text` |
|--------|---------------------|
| `Accept` | First text ends the turn (attended chat, duties, sub-agents). |
| `Confirm` | First text is held: the assistant message is stored, a system directive states that the next text reply publishes verbatim — continue with tool calls if unfinished, otherwise reply with the comment to publish. The next text ends the turn. |

The nudge fires at most once per turn and never on the last iteration:
under `BudgetPolicy::Fail` a nudge into the cap loses the turn, which is
worse than publishing possible narration. The turn summary logs `nudged`
so leak frequency is measurable.

### Cancellation

The turn accepts a cancellation token. Cancellation is checked:

- Before compaction
- Around the provider call
- Around tool execution (`join_all`)
- At the top of each loop iteration

When cancelled, the turn emits `Activity::Cancelled` and returns
`Error::Cancelled`. Partial session state from the current turn is still saved.

Racing tool execution against the token means losing futures are **dropped**.
Sub-agents ([spec 19](19-sub-agents.md)) rely on this: a `task` tool call is
itself a full child turn, and cancelling the parent drops it mid-await. Drop
stops the loop, not necessarily side effects already in flight (spawned
processes rely on kill-on-drop).

### Activity Events

The turn accepts an optional activity sender. Events are emitted at:

1. **Compaction** — after successful compaction, with before/after token counts
2. **Tool start** — before execution, one per call
3. **Tool end** — after execution, with error if failed/blocked
4. **Max iterations** — when the loop is exhausted
5. **Cancelled** — when the cancellation token fires

When repetition detection skips execution, no tool events are emitted for that
iteration. Events use non-blocking `try_send`; they are silently dropped if the
channel is full.

## Boundaries

### Owns

- The tool loop: iteration, repetition detection, policy gate, cancellation
- Turn-level orchestration: compaction trigger, context assembly, provider call
- Tool result recording and safety checking

The loop is generic over the context engine and provider, and is exposed
crate-internally so sub-agents ([spec 19](19-sub-agents.md)) run the exact
same turn function against an ephemeral child context — repetition detection
and the policy gate come along for free.

### Does Not Own

- Session persistence — the actor handles load/save around each envelope
- System prompt content — sourced from the workspace
- Tool execution — delegated to the tool registry
- Context compaction logic — delegated to the context module
- Safety/leak detection — delegated to the safety module

### Actor

The loop runs inside an actor that processes envelopes sequentially. The actor
owns the session path and handles load/save.

**Session save semantics**: the session is saved after every envelope,
regardless of whether the turn succeeded or failed. This means partial state
(e.g., tool calls executed before a provider error) is persisted. If the save
itself fails, the save error takes precedence and the turn result is lost.

**Input classification**: the actor delegates to `Input::parse()`. Text
starting with `/` must match a known slash command or an error is returned.
Everything else is a free-text message routed through `run_turn`.

Known commands: `/compact`, `/context`, `/heartbeat`, `/new`, `/stats`.

Commands handle their own session load/save independently from the message
path. This means commands like `/compact` and `/new` load and save the session
directly, while `/heartbeat` delegates to `process_message` which does its own
load/save.

### AgentHandle

A cloneable `mpsc::Sender<Envelope>` wrapper. Channels call
`send_message(source, input, activity_tx, cancel)` and await a `Reply` over a
oneshot channel. `Reply` carries a `content: String` and a `preformatted: bool`
hint for display formatting.

If the actor has shut down, `send_message` returns a synthetic error string.

### ChannelSource

Messages are tagged with their origin before entering the session:

- `Heartbeat`
- `GitHub { pr_number: u32 }`
- `Socket`
- `Telegram`

The actor prepends `[ChannelSource]` to each user message.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Provider API error | Return error to caller. Session (including partial state) is saved. |
| Tool execution error | Error text returned to LLM as tool result. Turn continues. |
| Tool blocked (policy) | Strike counter incremented. At 2 strikes, turn halts with guidance message. |
| Identical tool failure | Strike counter incremented. At 3, error text names the repetition. At 5, turn halts with `ToolHalt`. |
| Safety violation | Tool output replaced with error. Original output never stored. Turn continues. |
| Max iterations | Return `Error::MaxIterationsReached`. Session saved. |
| Cancellation | Return `Error::Cancelled`. Session saved. |
| Session save failure | Save error propagated to caller. Turn result is lost. |

## Constraints

All values configurable via `config.toml`:

| Constraint | Default | Config key |
|------------|---------|------------|
| Max iterations per turn | 100 | `agent.max_iterations` |
| Exec tool timeout | 60s | `tools.exec.timeout_secs` |
| Provider response max tokens | 32768 | `provider.max_tokens` |
| Context window budget | 200,000 tokens at 80% | `context.max_tokens`, `context.budget_percent` |

Note: `provider.max_tokens` caps the LLM's response length. `context.max_tokens`
caps the conversation window and triggers compaction. These are independent.

## Open Questions

None currently.

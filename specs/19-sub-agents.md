# Spec 19: Sub-Agents

## Motivation

The agent needs to delegate work to isolated child contexts. Two immediate
drivers:

1. **Context isolation**: tool-heavy tasks (test runs, large file analysis,
   deep code search) produce verbose intermediate output that pollutes the
   parent's context window. A sub-agent absorbs that output and returns only
   a summary.

2. **LCM `lcm_expand` restriction**: the paper restricts `lcm_expand` to
   sub-agents to prevent context flooding in the main loop. Without sub-agent
   infrastructure, `lcm_expand` is either unrestricted (dangerous) or
   unusable.

The design follows the universal pattern observed across OpenCode, Claude
Code, and Goose: parent sends a prompt string to a fresh child context, child
runs independently, parent gets back a result string.

## Behavior

### The Tool

Sub-agents are exposed as a single tool called `task`. The LLM calls it like
any other tool.

**Parameters:**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `prompt` | String | yes | The task for the sub-agent to perform |
| `agent_type` | String | no | `explore` or `worker` (default: `explore`) |

**Returns:** the sub-agent's final assistant text response as a plain string.
All intermediate tool calls and reasoning remain inside the sub-agent's
context and are invisible to the parent.

### Agent Types

Two built-in types. Each defines a system prompt and an explicit tool
allowlist. Tool sets are built once at startup by filtering the parent's
registry (see [Tool Sets](#tool-sets)). Names with no matching tool are
skipped, not rejected: a tool may be legitimately absent because the operator
disabled it via `tools.disabled` (the operator's intent propagates to
sub-agents) or because it was compiled out. Typos in the hardcoded allowlists
are caught by a unit test validating them against the tool catalog.

**`explore`** — read-only research agent. Default type.

| Property | Value |
|----------|-------|
| Max iterations | `sub_agents.max_iterations` (default: 30) |
| Tools | `file_read`, `glob_search`, `grep`, `web_fetch`, `web_search` |
| LCM tools | `lcm_grep`, `lcm_describe`, `lcm_expand` |
| Cannot | Write files, execute commands, spawn sub-agents |

**`worker`** — read-write agent for self-contained tasks.

| Property | Value |
|----------|-------|
| Max iterations | `sub_agents.max_iterations` (default: 30) |
| Tools | explore's tools plus `file_write`, `file_edit`, `exec` |
| LCM tools | `lcm_grep`, `lcm_describe`, `lcm_expand` |
| Cannot | Spawn sub-agents, use git/GitHub tools |

Both allowlists are explicit — the worker is **not** "everything except
`task`". Outward-visible actions (pushing commits, creating PRs, replying to
review comments) stay with the parent, which has the conversation context and
the accountability. A sub-agent acting on a delegated one-line prompt should
not be able to publish anything.

Neither type receives the `task` tool. **No recursive spawning.** This is the
structural termination guarantee — no depth limits needed because recursion is
impossible.

Both sub-agent loops run with the same repetition detection and policy strike
gate as the parent (free, since the loop is shared).

### Context Isolation

The sub-agent gets a **fresh, empty context**. The only input from the parent
is the `prompt` string. The parent's conversation history, system prompt, and
prior tool results are not accessible to the child.

The sub-agent receives its own system prompt (type-specific, see below) and
the `prompt` as a user message.

**Child context**: an `EphemeralSession` — an in-memory `ContextEngine`
implementation holding a `Vec<Message>`. `compact_if_needed` is a no-op,
`assemble` concatenates, nothing touches disk. Created per `task` call,
dropped when the call returns. There is deliberately no compaction: a
sub-agent that outgrows the provider's context window gets a provider error
back (see Failure Modes), which the parent sees as tool error text. Compacting
a child that is supposed to return a summary would be treating the symptom.

The one size policy it applies: tool results above 20,000 estimated tokens
(`SUB_AGENT_TOOL_OUTPUT_TOKENS`) are truncated tail-biased at push. The cap
sits far above the root's `context.tool_output_tokens` because sub-agents
exist to absorb verbose output, and `lcm_expand` may legitimately return up
to `MAX_EXPAND_TOKEN_CAP` (20k) in a single tool result.

**LCM integration**: when the parent runs the LCM engine, the sub-agent's
tool set includes the engine's retrieval tools (`lcm_grep`, `lcm_describe`,
`lcm_expand`) as shared instances. These tools already carry
`Arc<Mutex<Connection>>` and the active conversation id, so the child
searches and drills into the **parent's** compacted history with no extra
plumbing. All three are read-only against the store. The sub-agent's own
messages live only in its `EphemeralSession` and are never persisted.

The parent is blocked in the `task` tool call while the child runs, so there
is no reader contention on the shared connection beyond parallel sub-agents,
which serialize on the mutex per query.

### Tool Sets

Two registry changes make filtered tool sets possible:

1. The registry holds `Arc<dyn Tool>` instead of `Box<dyn Tool>`, so the same
   tool instance can appear in multiple sets ([spec 03](03-tools.md)).
2. `ContextEngine::tools()` takes a scope argument: `ToolScope::Root` returns
   the engine tools for the main agent (`lcm_grep`, `lcm_describe`),
   `ToolScope::SubAgent` additionally includes `lcm_expand`. The flat engine
   returns nothing for either scope. This closes spec 14's interim hatch: the
   main agent loses `lcm_expand` when this spec lands.

At startup the actor builds three tool sets from the same instances: the
parent's (base tools + root engine tools + `task`), explore's, and worker's.
The `task` tool is constructed with the two child sets prebuilt — no runtime
filtering.

### Execution

The `task` tool's `execute()`:

1. Parse `agent_type`, pick the prebuilt tool set, system prompt, and
   provider.
2. Create a fresh `EphemeralSession`.
3. Run the same `run_turn` the parent uses (exposed `pub(crate)` from the
   agent module) with the child engine, the type's system prompt, `prompt` as
   the user message, the type's provider, and `sub_agents.max_iterations`.
4. Return the final assistant text as the tool result.

The sub-agent runs **synchronously** from the parent's perspective. It is a
tool call that blocks until completion, like any other tool. The parent's
`join_all` over parallel tool calls means the LLM can launch multiple
sub-agents concurrently by emitting multiple `task` calls in a single
response.

**Parallel sub-agents**: `execute()` takes `&self` and holds only shared
immutable state; each invocation creates its own `EphemeralSession`, so
concurrent children share nothing mutable. LCM queries serialize on the
shared connection mutex; concurrent provider calls are ordinary HTTP
concurrency; one parent cancellation drops every child at once. The only
unguarded interaction is two parallel workers mutating the same workspace
files — the same hazard as two parallel `exec` calls today. It is the
model's responsibility not to issue conflicting parallel writes, and
`file_edit`'s exact-match precondition makes lost updates fail loudly.

### Cancellation

The child's `run_turn` receives the parent's real `CancellationToken`,
delivered through the `ToolCtx` the `task` tool gets on `execute()`
([spec 03](03-tools.md)).

The **primary** cancel path is still drop-based: the parent's loop races
tool execution against its own token (`cancellable(join_all(...))`), so
cancelling the parent drops the sub-agent's future mid-await and its
in-memory context with it. The threaded token is correctness on top —
a child that reaches an iteration boundary before the drop lands observes
the cancellation itself and can emit a (labeled) `Cancelled` event.

The caveat is inherited from the parent's own cancellation semantics:
drop-based cancellation stops the loop, not necessarily side effects already
in flight (a spawned process under `exec` relies on kill-on-drop). This is
the same contract the parent has today.

### Activity and Observability

The child runs with a private activity channel; the `task` tool forwards
each child event to the parent's sink wrapped in
`Nested { agent, event }` ([spec 16](16-activity.md)), labeled with the
agent type. A user watching a verbose channel sees the delegation
bracketed and the child's work inside it:

```
Running tool: task
[worker] Running tool: exec
[worker] Tool finished: exec
Tool finished: task
```

When the parent ctx has no activity sink, the child gets none either and no
forwarding machinery is created. The child's iterations also remain visible
in the daemon logs via `tracing`, same as the parent's.

### System Prompts

**Explore:**
```
You are a research agent. Your job is to find information and report back.

Be concise and specific. Include file paths, line numbers, and code snippets
when relevant. Do not speculate — only report what you find.

Return your findings as a direct answer to the task. Your response will be
read by another agent, not a human.
```

**Worker:**
```
You are a task agent. Complete the assigned task and report what you did.

Be concise. Describe what you changed and why. Include file paths and
relevant details. Your response will be read by another agent, not a human.
```

Both prompts are appended with environment info (working directory, available
tools) following the same pattern as the parent's system prompt assembly.

### Tool Description

The `task` tool's description tells the parent LLM when and how to use it:

```
Launch a sub-agent to perform a task in an isolated context. The sub-agent
runs independently and returns its findings as text. Use this for:
- Searching the codebase for specific patterns or information
- Reading and analyzing files without polluting your context
- Performing self-contained tasks that produce verbose intermediate output
- Expanding compacted history via lcm_expand (only available to sub-agents)

The sub-agent cannot see your conversation history. Pack all necessary
context into the prompt.

agent_type "explore" (default): read-only research. Cannot modify files.
agent_type "worker": can read, write, and execute commands. For
self-contained tasks. Cannot use git or GitHub.
```

## Agent Loop Integration

The `task` tool holds `Arc` references to shared state and implements the
`Tool` trait like any other tool (the actor constructs it at startup and adds
it to the parent's registry — the agent loop stays generic and knows nothing
about sub-agents):

```rust
struct TaskTool<P: Provider> {
    explore_provider: Arc<P>,        // provider.model_overrides.explore, or the root's
    worker_provider: Arc<P>,         // provider.model_overrides.worker, or the root's
    summarize: SummarizeFn,          // run_turn signature; child never compacts
    explore: AgentType,              // system prompt + prebuilt Tools
    worker: AgentType,
    max_iterations: usize,
}
```

The alternative — the actor intercepting `task` calls like a built-in
command — was rejected: it special-cases the loop and breaks the parallel
`join_all` dispatch that makes concurrent sub-agents free.

`TaskTool` is generic over the provider (the registry stores it as
`Arc<dyn Tool>`, so the generic never escapes). The prebuilt `Tools` sets
exclude `task` by construction, which is the recursion guard.

## Configuration

```toml
[sub_agents]
max_iterations = 30
```

| Config key | Default | Description |
|------------|---------|-------------|
| `sub_agents.max_iterations` | `30` | Max tool loop iterations per sub-agent |

Each agent type runs on its own model when `provider.model_overrides.explore` or
`provider.model_overrides.worker` is set (see [spec 02](02-provider.md)); unset types
use the parent's model. Model selection is static config only — there is
deliberately no per-call model argument on the `task` tool, so the parent
model never controls spend. The delegation itself is the difficulty
classification: what the root keeps runs on the root model, what it hands
off runs on the type's model.

## Boundaries

### Owns

- The `task` tool definition and execution
- Agent type definitions (system prompts, tool allowlists)
- `EphemeralSession` and the sub-agent context lifecycle (create, run, discard)
- The prebuilt per-type tool sets

### Does Not Own

- The agent loop — reuses `run_turn` from spec 01
- The provider — borrows via `Arc`
- Tool definitions — reuses shared instances from the parent's registry
- Engine tool scoping — `ContextEngine::tools(scope)` lives in spec 14
- Cancellation — primary path is future drop; the token arrives via `ToolCtx`
  (spec 03)

### Interactions

- **Agent loop (spec 01)**: `run_turn` becomes `pub(crate)` and is called by
  the `task` tool with a child engine. Not recursion in the language sense —
  the child context has no way to call `task`.
- **Context engine (spec 14)**: `ContextEngine::tools()` gains the
  `ToolScope` parameter. `lcm_expand` moves from interim (main agent,
  conservative cap) to sub-agent-only. Sub-agent LCM tools are shared
  instances bound to the parent's store and active conversation.
- **Tool registry (spec 03)**: registry holds `Arc<dyn Tool>`; per-type sets
  are built at startup from shared instances. The `task` tool registers like
  any other tool.
- **Activity (spec 16)**: child events are forwarded to the parent's sink
  wrapped in `Nested { agent, event }`.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Sub-agent hits max_iterations | Error text returned as tool result. Parent continues. |
| Sub-agent provider error (incl. context overflow) | Error text returned as tool result. Parent continues. |
| Sub-agent tool error | Handled inside the child loop (same as parent) |
| Parent cancelled | Child future dropped mid-await, context discarded |
| Invalid `agent_type` | `ToolError::InvalidArguments` |
| Allowlisted tool absent from registry | Skipped (disabled or compiled out); typos caught by tests |

Sub-agent errors do **not** crash the parent. They are returned as tool error
text, and the parent LLM decides how to proceed.

## Constraints

- No recursive spawning — `task` is absent from both child tool sets by
  construction
- Child context is ephemeral and in-memory — discarded after completion,
  never persisted to the parent's store
- Explicit tool allowlists per type — no "everything except" sets
- No git/GitHub tools in any sub-agent — outward-visible actions belong to
  the parent
- One model per agent type, fixed in config (`provider.model_overrides.*`); the
  parent cannot pick a model per call
- Synchronous execution only (blocking tool call); concurrency comes from the
  parent emitting parallel `task` calls, bounded by the provider's rate
  limits like any other parallel tool execution
- No sub-agent compaction — context overflow surfaces as a provider error in
  the tool result
- Child token budget is the provider's context window; no separate
  configurable budget

## Future Extensions

These are **not** in this spec but the architecture accommodates them:

- **Custom agent types**: user-defined types via config or markdown files
  with system prompts, tool lists, and model selection.
- **Background execution**: non-blocking sub-agents that run concurrently
  while the parent continues. Requires changing the tool result delivery
  mechanism.
- **Resumable sub-agents**: persist sub-agent transcripts, allow the parent
  to resume a previous sub-agent by ID.
- **`llm_map` / `agentic_map`**: operator-level recursion tools that spawn
  N sub-agents in parallel over a JSONL dataset. Builds on top of the `task`
  tool's execution machinery.
- **Scope-reduction invariant**: when recursive spawning is enabled, require
  sub-agents to declare `delegated_scope` and `kept_work` to prevent
  infinite delegation.
- **Cost tracking**: roll up sub-agent token usage to the parent session.
- **Concurrency limit**: cap parallel sub-agents if provider rate limits
  become a problem in practice.

## Open Questions

None currently.

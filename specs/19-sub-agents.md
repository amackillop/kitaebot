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

Two built-in types. Each defines a system prompt and a tool allowlist.

**`explore`** — read-only research agent. Default type. Cheap model.

| Property | Value |
|----------|-------|
| Model | `config.sub_agents.explore_model` (default: same as parent) |
| Max iterations | `config.sub_agents.max_iterations` (default: 30) |
| Tools | `file_read`, `glob_search`, `grep`, `web_fetch`, `web_search` |
| LCM tools | `lcm_grep`, `lcm_describe`, `lcm_expand` |
| Cannot | Write files, execute commands, spawn sub-agents |

**`worker`** — read-write agent for self-contained tasks.

| Property | Value |
|----------|-------|
| Model | Same as parent |
| Max iterations | `config.sub_agents.max_iterations` (default: 30) |
| Tools | All parent tools **except** `task` |
| LCM tools | `lcm_grep`, `lcm_describe`, `lcm_expand` |
| Cannot | Spawn sub-agents |

Neither type receives the `task` tool. **No recursive spawning.** This is the
structural termination guarantee — no depth limits needed because recursion is
impossible.

### Context Isolation

The sub-agent gets a **fresh, empty context**. The only input from the parent
is the `prompt` string. The parent's conversation history, system prompt, and
prior tool results are not accessible to the child.

The sub-agent receives its own system prompt (type-specific, see below) and
the `prompt` as a user message.

**LCM integration**: when the parent uses the LCM engine, the sub-agent
shares the parent's **immutable store** (read-only SQLite connection) but has
its own throwaway active context. This means `lcm_grep`, `lcm_describe`, and
`lcm_expand` can search and drill into the parent's compacted history. The
sub-agent's own messages are not persisted to the parent's store.

**Flat session**: the sub-agent gets an in-memory `Session` that is discarded
after the task completes. No disk I/O.

### Execution

The `task` tool's `execute()` method:

1. Build a tool set based on `agent_type`.
2. Create a fresh context (in-memory session or throwaway LCM conversation).
3. Derive a child `CancellationToken` from the parent's token.
4. Run `run_turn(engine, system_prompt, prompt, provider, tools,
   max_iterations, cancel)` — the same function the parent uses.
5. Return the final assistant text as the tool result.

The sub-agent runs **synchronously** from the parent's perspective. It is a
tool call that blocks until completion, like any other tool. The parent's
`join_all` over parallel tool calls means the LLM can launch multiple
sub-agents concurrently by emitting multiple `task` tool calls in a single
response.

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

### Cancellation

The sub-agent's `CancellationToken` is a child of the parent's. When the user
cancels the parent turn, all sub-agents are cancelled automatically via
Tokio's token hierarchy. The sub-agent's `run_turn` checks cancellation at the
same points as the parent (before compaction, around provider calls, around
tool execution, at loop top).

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
agent_type "worker": can read, write, and execute. For self-contained tasks.
```

## Agent Loop Integration

The `task` tool needs access to:
- The `Provider` (to make LLM calls for the sub-agent)
- The `Tools` registry (to build filtered tool sets)
- The `Workspace` (for system prompt assembly)
- The `ContextEngine` (for LCM read-only access, if applicable)

This is more state than a typical tool holds. Two options:

**Option A**: the `task` tool is constructed by the actor and holds `Arc`
references to shared state. It implements the `Tool` trait like any other
tool.

**Option B**: the actor intercepts `task` tool calls specially (like a
built-in command) rather than dispatching through the tool registry.

**Decision: Option A.** The tool holds what it needs. The actor constructs it
at startup and includes it in the tool registry. This keeps the agent loop
generic — it doesn't need to know about sub-agents.

```rust
struct TaskTool {
    provider: Arc<P>,
    workspace: Arc<Workspace>,
    base_tools: Arc<Tools>,
    lcm_db_path: Option<PathBuf>,  // for LCM read-only connection
    config: SubAgentConfig,
}
```

The `task` tool is added to the parent's tool set but **not** to any
sub-agent's tool set. This is enforced by building the sub-agent's `Tools`
from a filtered subset that excludes `task`.

## Configuration

```toml
[sub_agents]
max_iterations = 30
explore_model = ""    # empty = use parent's model
```

| Config key | Default | Description |
|------------|---------|-------------|
| `sub_agents.max_iterations` | `30` | Max tool loop iterations per sub-agent |
| `sub_agents.explore_model` | `""` | Model for explore agents (empty = parent's model) |

## Boundaries

### Owns

- The `task` tool definition and execution
- Agent type definitions (system prompts, tool allowlists)
- Sub-agent context lifecycle (create, run, discard)
- Tool filtering per agent type

### Does Not Own

- The agent loop — reuses `run_turn` from spec 01
- The provider — borrows via `Arc`
- Tool definitions — filters the parent's registry
- Context engine — borrows read-only access for LCM tools
- Cancellation — inherits from parent via token hierarchy

### Interactions

- **Agent loop (spec 01)**: `run_turn` is called recursively (not in the
  language sense — the sub-agent calls the same function, but in a separate
  context with no way to call `task`).
- **Context engine (spec 14)**: LCM sub-agents share the parent's immutable
  store for retrieval tools. `lcm_expand` is moved from interim (available to
  main agent) to sub-agent-only when this spec lands.
- **Tool registry (spec 03)**: the `task` tool is registered like any other
  tool. Sub-agent tool sets are built by filtering the parent's registry.
- **Activity (spec 16)**: sub-agent tool events are emitted through the
  parent's activity channel (the sub-agent receives the same
  `activity_tx`). Events are tagged to distinguish parent from child.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Sub-agent hits max_iterations | Returns `Error::MaxIterationsReached` text as tool result |
| Sub-agent provider error | Returns error text as tool result. Parent continues. |
| Sub-agent tool error | Handled internally by sub-agent's loop (same as parent) |
| Sub-agent cancelled | Returns `Error::Cancelled` — propagates to parent |
| Invalid agent_type | Returns `ToolError::InvalidArguments` |
| LCM DB not available | LCM tools excluded from sub-agent's tool set |

Sub-agent errors do **not** crash the parent. They are returned as tool error
text, and the parent LLM decides how to proceed.

## Constraints

- No recursive spawning — `task` tool excluded from sub-agent tool sets
- Sub-agent context is ephemeral — discarded after completion
- Sub-agent messages are not persisted to the parent's store
- One model per agent type (no per-invocation model selection initially)
- Synchronous execution only (blocking tool call)
- No sub-agent-specific compaction — sub-agents should finish within their
  context budget. If they exceed it, they hit max_iterations.

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
- **Per-invocation model selection**: let the parent choose the model per
  task call.
- **Cost tracking**: roll up sub-agent token usage to the parent session.
- **Persistent sub-agent memory**: cross-session learning per agent type.

## Open Questions

1. **Activity events**: should sub-agent tool events appear in the parent's
   activity stream (tagged as sub-agent), or should they be suppressed? The
   former gives visibility; the latter reduces noise.
2. **Token budget**: should sub-agents have their own configurable context
   budget, or just inherit the parent's `max_tokens`? For explore agents
   that should be short-lived, a smaller budget would enforce conciseness.
3. **Parallel sub-agent concurrency**: the parent can emit multiple `task`
   calls which `join_all` executes concurrently. Should there be a
   concurrency limit to avoid hammering the provider API?

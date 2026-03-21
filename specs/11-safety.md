# Spec 11: Safety

## Motivation

A lightweight safety layer providing two cheap, high-value defenses: leak
detection and output wrapping. Applied to every tool output before it enters
the LLM conversation. Once a secret enters the context window, it can be
exfiltrated in subsequent responses — block before injection, not after.

This is one layer in a defense-in-depth stack. Secrets should never reach tool
output in the first place — they're loaded from credential files (not env
vars) and the exec tool scrubs the child environment. See
[spec 13](13-credentials.md) for the full stack.

## Behavior

### Leak Detection

`check_tool_output(tool_name, output)` scans tool output against a compiled
`RegexSet` (built once via `LazyLock`, matched in a single pass).

**Patterns:**

| Pattern | Matches |
|---------|---------|
| `sk-ant-[a-zA-Z0-9_-]{20,}` | Anthropic API keys |
| `sk-[a-zA-Z0-9_-]{20,}` | OpenAI API keys |
| `ghp_[a-zA-Z0-9]{30,}` | GitHub personal access tokens |
| `gho_[a-zA-Z0-9]{30,}` | GitHub OAuth tokens |
| `ghs_[a-zA-Z0-9]{30,}` | GitHub server tokens |
| `AKIA[0-9A-Z]{16}` | AWS access key IDs |
| `-----BEGIN [A-Z ]+PRIVATE KEY-----` | Private key headers |
| `postgres://\S+:\S+@` | PostgreSQL connection strings |
| `mysql://\S+:\S+@` | MySQL connection strings |
| `mongodb(\+srv)?://\S+:\S+@` | MongoDB connection strings |
| `redis://\S+:\S+@` | Redis connection strings |

Each pattern requires enough structure beyond the bare prefix to avoid false
positives when the agent reads its own source code.

**On match**: returns `Err(SafetyError::LeakDetected { pattern_name })`. The
agent loop substitutes a sanitized error message:
`"Tool output blocked: Potential secret detected (pattern: {name}). Do not retry."`

The original output is **discarded** — it never enters the session.

**On no match**: wraps the output in XML tags and returns it.

Patterns are hardcoded, not configurable.

### Output Wrapping

Clean tool output is wrapped in XML-style tags:

```
<tool_output name="exec">
$ ls -la
total 24
...
Exit code: 0
</tool_output>
```

This tells the LLM to treat the content as data, not instructions. Cheap
defense against prompt injection from command output.

### Agent Loop Integration

In the agent loop's result recording step, every successful tool result passes
through `check_tool_output`. On leak detection:

1. Original output dropped
2. Sanitized error stored as the `Message::Tool` content
3. `Activity::ToolEnd` emitted with error for observability
4. Turn continues — the LLM sees the error and can respond accordingly

Failed tool calls (execution errors) skip the safety check — there's no
output to leak.

## Boundaries

### Owns

- Leak pattern definitions and compilation
- Single-pass regex scanning
- XML output wrapping
- The `SafetyError` type

### Does Not Own

- Decision of what to do on leak detection — the agent loop substitutes the
  error message and continues
- Exec deny-list / policy violations — separate concern (see
  [spec 03](03-tools.md))
- Credential isolation — see [spec 13](13-credentials.md)

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Leak detected | Output blocked, sanitized error to LLM, turn continues |

There are no failure modes for the safety module itself — regex compilation
is infallible (`LazyLock` panics on invalid regex, caught at startup).

## Constraints

- No configurability — patterns are hardcoded
- No severity levels — all leaks are hard blocks
- Single pass scanning via `RegexSet`

## Open Questions

None currently.

# Spec 11: Safety

## Motivation

A lightweight safety layer providing two cheap, high-value defenses: leak
redaction and output wrapping. Applied to every tool output before it enters
the LLM conversation. Once a secret enters the context window, it can be
exfiltrated in subsequent responses — redact before injection, not after.

Redaction, not blocking, because blocking taxes false positives without
stopping a determined read: withholding a whole file over one key-shaped
span costs the model everything around it and invites reconstructing the
content through other commands (an agent whose own source embeds
key-shaped test fixtures burned most of an iteration budget slice-reading
a blocked file three lines at a time). Redacting the span keeps the
secret out of the context either way, keeps the rest of the output
usable, and removes the incentive to route around the layer.

This is one layer in a defense-in-depth stack. Secrets should never reach tool
output in the first place — they're loaded from credential files (not env
vars) and the exec tool scrubs the child environment. See
[spec 13](13-credentials.md) for the full stack.

## Behavior

### Leak Redaction

`check_tool_output(tool_name, output)` scans tool output against a compiled
`RegexSet` (built once via `LazyLock`, matched in a single pass); on a hit,
per-pattern regexes replace each matching span.

**Patterns:**

| Pattern | Matches |
|---------|---------|
| `sk-ant-[a-zA-Z0-9_-]{20,}` | Anthropic API keys |
| `sk-[a-zA-Z0-9_-]{20,}` | OpenAI API keys |
| `ghp_[a-zA-Z0-9]{30,}` | GitHub personal access tokens |
| `gho_[a-zA-Z0-9]{30,}` | GitHub OAuth tokens |
| `ghs_[a-zA-Z0-9]{30,}` | GitHub server tokens |
| `AKIA[0-9A-Z]{16}` | AWS access key IDs |
| `(?s)-----BEGIN [A-Z ]+PRIVATE KEY-----.*?(?:-----END [A-Z ]+PRIVATE KEY-----\|\z)` | Private keys, header through END marker (or end of output) — redacting only the header would leave the body readable |
| `postgres://\S+:\S+@` | PostgreSQL connection strings |
| `mysql://\S+:\S+@` | MySQL connection strings |
| `mongodb(\+srv)?://\S+:\S+@` | MongoDB connection strings |
| `redis://\S+:\S+@` | Redis connection strings |

Each pattern requires enough structure beyond the bare prefix to avoid false
positives when the agent reads its own source code.

**On match**: every matching span is replaced in place with
`[REDACTED: {pattern name}]` and the redacted output is wrapped and
returned as `CheckedOutput { wrapped, redactions }`. The secret never
enters the session; everything around it does. A pattern is reported in
`redactions` only if its replacement changed the output — an Anthropic
key is also OpenAI-shaped, and the more specific pattern (listed first)
consumes the span.

**On no match**: wraps the output in XML tags and returns it with an
empty `redactions`.

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
through `check_tool_output`. On redaction:

1. The redacted, wrapped output is stored as the `Message::Tool` content
2. A `WARN` naming the tool and pattern lands in the error tee — the
   self-analysis duty's symptom source (spec 24), so recurring
   redactions surface to the operator as either a real leak or a false
   positive worth fixing
3. The turn continues; a redaction is a successful call, not a failure
   (`Activity::ToolEnd` carries no error, and `/stats` classifies the
   stored message as success — `FailureKind::SafetyBlock` survives only
   to classify pre-redaction session history)

Failed tool calls (execution errors) skip the safety check — there's no
output to leak.

`AGENTS.md` (spec 06) carries the model-side norm: withheld content is
policy, never reconstructed through other commands; a rail that blocks
the task warrants a `notify`, not a workaround.

## Boundaries

### Owns

- Leak pattern definitions and compilation
- Single-pass regex scanning and span redaction
- XML output wrapping
- The `CheckedOutput` type

### Does Not Own

- Decision of what to do on redaction — the agent loop logs the WARN and
  continues
- Exec deny-list / policy violations — separate concern (see
  [spec 03](03-tools.md))
- Credential isolation — see [spec 13](13-credentials.md)

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Leak detected | Span redacted in place, WARN to the error tee, turn continues |

There are no failure modes for the safety module itself — regex compilation
is infallible (`LazyLock` panics on invalid regex, caught at startup).

## Constraints

- No configurability — patterns are hardcoded
- No severity levels — every match is redacted the same way
- Single pass scanning via `RegexSet`; per-pattern replacement only on a hit

## Open Questions

None currently.

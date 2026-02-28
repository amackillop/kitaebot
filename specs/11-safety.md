# Safety

## Purpose

A lightweight safety layer providing two cheap, high-value defenses: leak detection and output wrapping. Applied to tool output before it enters the LLM conversation.

## Why This Design?

1. **Leaked secrets are unrecoverable** — Once a secret enters LLM context, it can be exfiltrated in subsequent responses. Block before injection, not after.
2. **Prompt injection is cheap to mitigate** — Wrapping tool output in tags tells the LLM to treat content as data, not instructions. Near-zero cost.
3. **No policy engine** — No severity levels, no sanitizer, no configurability. Just two hard rules.

## Architecture

A `Safety` struct with one method:

- `check_tool_output(tool_name: &str, output: &str) -> Result<String, LeakDetected>`

On success, returns the output wrapped in tags. On failure, returns `LeakDetected` with the matched pattern name (not the secret itself).

## Leak Detection

Scan tool output for known secret patterns before sending to the LLM. Match against fixed prefixes and literal strings — no regex engine needed.

### Patterns

| Pattern | Matches |
|---------|---------|
| `sk-` | OpenAI API keys |
| `sk-ant-` | Anthropic API keys |
| `ghp_` | GitHub personal access tokens |
| `gho_` | GitHub OAuth tokens |
| `ghs_` | GitHub server tokens |
| `AKIA` | AWS access key IDs |
| `-----BEGIN` | Private key headers (RSA, EC, etc.) |
| `postgres://` | PostgreSQL connection strings |
| `mysql://` | MySQL connection strings |
| `mongodb://` | MongoDB connection strings |
| `redis://` | Redis connection strings |

### Behavior

- **Action: block** — Return `Err(LeakDetected)` to the caller. The agent loop converts this to a sanitized error message for the LLM: `"Tool output blocked: potential secret detected (pattern: {name}). Do not retry."`.
- This is a hard failure. The original output never enters the conversation.
- Patterns are hardcoded, not configurable. They're well-known prefixes — no reason to make them dynamic.

### Scanning Strategy

Simple substring/prefix matching. Iterate patterns, check `output.contains(pattern)`. Short-circuit on first match.

## Output Wrapping

Wrap tool output in XML-style tags before injecting into the conversation:

```
<tool_output name="exec">
$ ls -la
total 24
drwxr-xr-x  3 kitaebot kitaebot 4096 Feb 21 12:00 .
-rw-r--r--  1 kitaebot kitaebot  512 Feb 21 12:00 SOUL.md

Exit code: 0
</tool_output>
```

This tells the LLM to treat the content as data, not instructions. Cheap defense against prompt injection from command output (e.g., a file containing "ignore previous instructions").

## Error Handling

`LeakDetected` is a domain error, not a system error. The agent loop handles it by substituting a safe error message. The LLM sees the error and can inform the user or try a different approach.

```rust
enum SafetyError {
    LeakDetected { pattern_name: String },
}
```

## Future Considerations

- **Scan LLM responses** — Also check model output before delivering to the user. Adds latency but catches reflection attacks.
- **Custom patterns** — Allow users to add workspace-specific patterns via `config.toml`.
- **Allowlisting** — Let specific tool invocations bypass leak detection (e.g., a secrets-management tool).

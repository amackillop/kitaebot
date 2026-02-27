# Tool System

## Purpose

Tools are capabilities the agent can invoke to interact with the environment. The tool system provides a registry for managing tools and dispatch for executing them.

## Why This Design?

1. **Extensibility** — New tools are added as enum variants; the core loop doesn't change
2. **Discoverability** — Tools describe themselves via JSON Schema for the LLM
3. **Safety** — Tools can validate arguments and enforce restrictions

## Architecture

Tools are modeled as a `Tool` enum with explicit match dispatch. Each variant wraps a concrete struct (e.g., `Exec`). A `Stub` variant exists under `#[cfg(test)]` for unit testing the agent loop.

The `Tools` struct is a `Vec<Tool>` with linear scan for lookup. For small tool counts (<50), this outperforms `HashMap` due to cache locality and no hashing overhead. Tool execution involves HTTP calls to an LLM (100ms+), so lookup time is noise.

Each tool provides:
- `NAME` / `DESCRIPTION` — constants for the LLM
- `parameters()` — JSON Schema derived via `schemars`
- `execute(args)` — async execution returning `Result<String, ToolError>`

## MVP Tool: `exec`

The only tool for MVP. Executes shell commands in the workspace via `sh -c`.

### Parameters

A single required `command` field (string). Schema is generated from a `schemars::JsonSchema`-derived `Args` struct.

### Behavior

1. Check for path traversal (`../`)
2. Check command against deny patterns (regex set)
3. Execute with `tokio::process::Command`, cwd = workspace
4. Capture stdout/stderr
5. Return formatted output with exit code, truncated if over 10KB

### Safety Guards

Commands are checked against deny patterns before execution:

- `rm -r`, `rm -rf` — recursive deletion
- `mkfs` — filesystem creation
- `dd if=` — disk operations
- `> /dev/` — device writes
- `shutdown`, `reboot` — system power
- Fork bomb pattern

**Note**: These are defense-in-depth heuristics, not a sandbox. The VM is the real isolation.

### Restrictions

Timeout and output size are configurable via `config.toml` (see `src/config.rs`). Defaults shown below.

| Restriction | Default | Config key |
|-------------|---------|------------|
| Working directory | Workspace root | — |
| Timeout | 60 seconds | `tools.exec.timeout_secs` |
| Output size | 10KB (UTF-8 aware) | `tools.exec.max_output_bytes` |
| Path traversal | Rejects `../` | — |

### Output Format

```
$ ls -la
total 24
drwxr-xr-x  3 kitaebot kitaebot 4096 Feb 21 12:00 .
-rw-r--r--  1 kitaebot kitaebot  512 Feb 21 12:00 SOUL.md

Exit code: 0
```

Stderr is prefixed with `STDERR:` and separated from stdout.

## Error Handling

Errors are returned to the LLM as text (via `unwrap_or_else` in the agent loop), not propagated as exceptions. The LLM decides how to proceed.

Error variants: `NotFound`, `InvalidArguments`, `ExecutionFailed`, `Timeout`, `Blocked`.

## Future Tools

Not in MVP, but the registry makes these easy to add:

| Tool | Purpose |
|------|---------|
| `read_file` | Read file contents (avoid shell for clarity) |
| `write_file` | Write file (atomic, with backup) |
| `edit_file` | Line-based editing |
| `web_fetch` | Fetch URL content |
| `web_search` | Search via SearXNG |

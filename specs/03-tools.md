# Spec 03: Tool System

## Motivation

Tools are capabilities the agent invokes to interact with the environment. A
dedicated tool replaces the unpredictable `exec`-everything path with something
deterministic and token-efficient. The LLM declares intent via typed parameters
(`file_read { path: "src/main.rs" }`) instead of reasoning about shell syntax.
If the LLM would repeatedly use `exec` for a task, that task should be a tool.

## Behavior

### Trait and Registry

Tools implement a `Tool` trait with async execution. Each tool is a struct that
owns its configuration. The registry holds `Box<dyn Tool>` in a `Vec` with
linear scan for lookup (fast enough for <50 tools, better cache locality than a
map).

```
trait Tool: Send + Sync {
    name()        -> &'static str
    description() -> &'static str
    parameters()  -> serde_json::Value    // JSON Schema
    execute(args) -> Result<String, ToolError>
}
```

Dispatch: find tool by name, parse arguments from JSON string to `Value`,
call `execute`. Unknown tool name returns `ToolError::NotFound`. Malformed
arguments return `ToolError::InvalidArguments`.

Tool definitions are converted to `ToolDefinition` (OpenAI function-calling
format) and passed to the provider on each call.

### Disabling Tools

Individual tools can be excluded by name via `tools.disabled` in config.
Unknown names in the disabled list are rejected at startup.

### Shared Utilities

- **`truncate_output`** — UTF-8 aware string truncation with byte count
  reporting. Used by `exec`, `grep`, `web_fetch`, and any tool with large
  output.
- **`PathGuard`** — workspace-confined path resolution. Rejects null bytes,
  `../`, and absolute paths. Canonicalizes and verifies the result is under the
  workspace root. Provides `resolve()` for existing files and `resolve_new()`
  for files that don't exist yet. Used by all file tools.

---

## Tool Catalog

### `exec` — Shell Command Execution

Executes commands via `bash -c` within the workspace.

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `command` | String | yes | Shell command to execute |
| `working_dir` | String | no | Subdirectory within workspace (default: workspace root) |

**Safety guards — two-layer deny system:**

1. **Regex layer** — a compiled `RegexSet` of ~70+ patterns covering:
   destructive file ops (`rm -rf`, `shred`, `find -delete`), disk/filesystem
   (`mkfs`, `dd`, `fdisk`, `mount`), system power (`shutdown`, `reboot`,
   `systemctl`), privilege escalation (`sudo`, `su`, `chmod`, `chown`),
   network exfiltration (`curl -T`, `nc -l`, `socat`), pipe-to-shell
   (`curl|sh`, `wget|sh`), reverse shells (`/dev/tcp/`, python/ruby/perl
   socket), port scanning (`nmap`, `masscan`), secret harvesting
   (`~/.ssh/id_*`, `~/.aws/`), GPG keyring access, process control
   (`kill -9`), cron persistence, kernel modules, firewall manipulation,
   injection/escape (`LD_PRELOAD`, `nsenter`), and git/gh operations that
   must use dedicated tools.

2. **Shell-aware structural layer** — tokenizes with `shlex`, strips env var
   prefixes and path prefixes, matches binary+subcommand. Catches bypass
   patterns like `VAR=x git commit`, `/usr/bin/git clone`, and
   piped/chained commands. Includes a full Nix deny list (`nixos-rebuild`,
   `nix-env`, `nix profile`, `nix store delete/gc`, `nix-collect-garbage`,
   remote flake refs).

These are defense-in-depth heuristics with friendly error messages. The real
filesystem boundary is the Landlock sandbox (see [spec 15](15-sandbox.md)).

**Environment scrubbing:**

Child processes run with only allowlisted env vars forwarded:

- **Execution**: `PATH`, `HOME`, `USER`, `SHELL`
- **Locale**: `LANG`, `LC_ALL`, `LC_CTYPE`
- **Terminal**: `TERM`, `COLORTERM`
- **Temp**: `TMPDIR`, `TMP`, `TEMP`
- **Nix**: `NIX_PATH`, `NIX_PROFILES`, `NIX_SSL_CERT_FILE`
- **TLS**: `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`
- **Workspace**: `KITAEBOT_WORKSPACE`
- **GPG**: `GNUPGHOME`
- **Misc**: `TZ`, `EDITOR`, `VISUAL`
- **XDG**: `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`

Notably absent: `CREDENTIALS_DIRECTORY`. See [spec 13](13-credentials.md).

**Direnv integration:**

When the working directory contains a `.envrc`, cached devshell environment
variables are injected into the subprocess. On cache miss, the first exec call
blocks until evaluation completes; subsequent calls are instant. On failure,
the command runs without the devshell and a warning is logged. See
[Direnv Cache](#direnv-cache).

**Output format:**

```
$ ls -la
total 24
drwxr-xr-x  3 kitaebot kitaebot 4096 Feb 21 12:00 .
-rw-r--r--  1 kitaebot kitaebot  512 Feb 21 12:00 SOUL.md

Exit code: 0
```

Stderr is prefixed with `STDERR:` and separated from stdout.

**Restrictions:**

| Restriction | Default | Config key |
|-------------|---------|------------|
| Timeout | 600 seconds | `tools.exec.timeout_secs` |
| Output size | 10KB (UTF-8 aware) | `tools.exec.max_output_bytes` |

---

### `file_read` — Read File Contents

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | String | yes | Relative to workspace |
| `offset` | u32 | no | Start line, 1-based |
| `limit` | u32 | no | Max lines to return, default 2000 |

Resolves via `PathGuard`. Rejects files >10MB. Formats with line numbers
(`{line_number}\t{content}`). Appends summary (lines shown, total lines,
bytes). UTF-8 only.

---

### `file_write` — Write File Contents

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | String | yes | Relative to workspace |
| `content` | String | yes | File content |

Resolves via `PathGuard::resolve_new`. Creates parent directories. Returns
byte count written.

---

### `file_edit` — Find-and-Replace Edit

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | String | yes | Relative to workspace |
| `old_string` | String | yes | Must be non-empty, must match exactly once |
| `new_string` | String | yes | Replacement (empty = delete) |

**Two-tier matching:**

1. **Exact**: `match_indices(old_string)`. If 1 match, use it. If >1, error
   with count. If 0, fallback.
2. **Whitespace-flexible**: Normalize both file and search string (collapse
   whitespace runs, trim trailing), sliding window comparison. Must match
   exactly once.

Splices replacement into the original file at the matched position.

---

### `glob_search` — Find Files by Pattern

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `pattern` | String | yes | Glob pattern, e.g. `"**/*.rs"` |

Rejects traversal patterns. Collects up to 1000 results. Returns sorted
relative paths.

---

### `grep` — Search File Contents

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `pattern` | String | yes | Regex pattern |
| `path` | String | no | Directory, default `"."` |
| `include` | String | no | File glob filter |

Uses the `ignore` crate's `WalkBuilder` (respects `.gitignore`) and
`grep-searcher`/`grep-regex` (ripgrep as a library). Accumulates up to 200
matches. Output truncated to configured max bytes.

---

### `web_fetch` — Fetch URL Content

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `url` | String | yes | Must be http or https |

GET with timeout. Strips HTML tags via regex. Collapses whitespace. Truncates
to max bytes.

| Restriction | Default | Config key |
|-------------|---------|------------|
| Timeout | 30 seconds | `tools.web_fetch.timeout_secs` |
| Max response | 50KB | `tools.web_fetch.max_response_bytes` |

---

### `web_search` — Web Search via Perplexity

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `query` | String | yes | Search query |

Sends a chat completion to OpenRouter with `perplexity/sonar` and returns the
synthesized answer. Uses a `CompletionsClient` (same HTTP client type as the
provider, separate instance) — not the `Provider` trait, to avoid circular
dependency.

| Restriction | Default | Config key |
|-------------|---------|------------|
| Model | `perplexity/sonar` | `tools.web_search.model` |
| Max tokens | 1024 | `tools.web_search.max_tokens` |
| Timeout | 30 seconds | `tools.web_search.timeout_secs` |

---

### Git Tools

Three tools wrapping the `git` binary. Gated behind `git.enabled` in config.

`GitCli<R>` holds the GitHub PAT, workspace root, co-authors, and an optional
direnv cache. The token is injected via a temporary `GIT_ASKPASS` helper script
(0o700 permissions, deleted on drop) for authenticated operations. Commits do
not need authentication; clone and push do.

| Tool | Description |
|------|-------------|
| `git_clone` | Clone a repository into the workspace. Runs `direnv allow` synchronously then warms the direnv cache in the background. |
| `git_commit` | Commit staged changes with co-author trailers. |
| `git_push` | Push commits to a remote. |

All tools take `repo_dir` (relative to workspace root) and validate it via
`resolve_repo_dir` — rejects traversal, absolute paths, and directories
without `.git`.

---

### GitHub Tools

Seven tools wrapping the `gh` CLI. Gated behind `github.enabled` in config
(separate from `git.enabled`).

`GhCli<R>` holds the GitHub PAT (injected as `GH_TOKEN` via process env) and
workspace root.

| Tool | Description |
|------|-------------|
| `github_ci_status` | Check CI status for a git ref. |
| `github_gh` | Generic `gh` CLI escape hatch — runs arbitrary `gh` subcommands. |
| `github_pr_create` | Create a pull request. |
| `github_pr_list` | List pull requests (open/closed/all). |
| `github_pr_reviews` | Fetch reviews for a pull request. |
| `github_pr_diff_comments` | Fetch inline diff comments on a PR. |
| `github_pr_diff_reply` | Reply to an inline diff comment. |

---

### Testing

`CliRunner` is the subprocess boundary trait. `RealCliRunner` spawns real
processes; `StubCliRunner` yields pre-enqueued responses for tests. Both
`GitCli<R>` and `GhCli<R>` are generic over `R: CliRunner`.

Network tools (`web_fetch`, `web_search`) are excluded entirely under the
`mock-network` feature flag.

---

## Direnv Cache

### Problem

Projects cloned into the workspace use Nix flake devshells. The exec tool runs
commands inside these devshells so the project's toolchain is available. A naive
approach — hooking direnv into every `bash -c` — causes a thundering herd when
parallel tool calls each trigger a full `nix print-dev-env` evaluation.

### Requirements

1. **Evaluate once** — `direnv export json` runs at most once per directory,
   regardless of concurrent exec calls
2. **Invalidate on change** — modified `.envrc` or `flake.lock` triggers
   re-evaluation on the next exec call
3. **Don't cache failures** — transient direnv errors don't poison the cache
4. **Graceful degradation** — if direnv fails, exec runs without the devshell
5. **Warm on clone** — `git_clone` pre-populates the cache in the background
6. **Trust before evaluate** — `git_clone` runs `direnv allow` synchronously
   before returning
7. **Shared across tools** — single cache instance shared between exec and
   git_clone

### Invalidation

Cache keys are directories. Staleness is determined by the mtime of `.envrc`
and `flake.lock` — two `stat` calls per lookup.

## Boundaries

### Owns

- Tool trait definition and registry
- Tool dispatch (name lookup, argument parsing)
- Per-tool execution logic
- Path guarding and output truncation
- Exec deny-list (both regex and structural layers)
- Environment scrubbing
- Direnv cache

### Does Not Own

- Decision of what to do with tool results — the agent loop handles that
- Safety/leak detection on tool output — the safety module handles that
- XML wrapping of tool output — the safety module handles that
- Filesystem confinement — Landlock handles that
- Tool definition wire format — the types module handles that

## Failure Modes

| Failure | Error Variant | Behavior |
|---------|---------------|----------|
| Unknown tool name | `NotFound` | Error text returned to LLM |
| Malformed arguments | `InvalidArguments` | Error text returned to LLM |
| Command on deny list | `Blocked { operation, guidance }` | Friendly message to LLM explaining what to use instead |
| Exec timeout | `Timeout` | Process killed, error returned to LLM |
| Tool runtime error | `ExecutionFailed` | Error text returned to LLM |

All errors are surfaced to the LLM as text. The LLM decides how to proceed.
The agent loop's policy gate escalates repeated `Blocked` errors (see
[spec 01](01-agent-loop.md)).

## Constraints

| Config key | Default | Description |
|------------|---------|-------------|
| `tools.exec.timeout_secs` | 600 | Exec command timeout |
| `tools.exec.max_output_bytes` | 10240 | Exec output cap (UTF-8 aware) |
| `tools.web_fetch.timeout_secs` | 30 | HTTP GET timeout |
| `tools.web_fetch.max_response_bytes` | 51200 | HTTP response cap |
| `tools.web_search.model` | `perplexity/sonar` | Search model |
| `tools.web_search.max_tokens` | 1024 | Search response cap |
| `tools.web_search.timeout_secs` | 30 | Search timeout |
| `tools.disabled` | `[]` | Tool names to exclude |

Git/GitHub tools require their respective `git.enabled`/`github.enabled` flags
and a valid GitHub PAT loaded from credentials.

## Open Questions

None currently.

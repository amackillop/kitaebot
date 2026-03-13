# Tool System

## Purpose

Tools are capabilities the agent can invoke to interact with the environment. The tool system provides a registry for managing tools and dispatch for executing them.

## Why This Design?

Every tool could be the LLM generating a shell command through `exec`. We add dedicated tools to replace that unpredictable path with something deterministic and token-efficient. The LLM declares intent via typed parameters (`file_read { path: "src/main.rs" }`) instead of reasoning about shell syntax (`exec { command: "cat src/main.rs" }`). This eliminates failure modes (wrong flags, injection risks, unparsed output) and keeps context lean. If the LLM would repeatedly use `exec` for a task, that task should be a dedicated tool.

1. **Extensibility** — New tools implement the `Tool` trait; the core loop doesn't change
2. **Discoverability** — Tools describe themselves via JSON Schema for the LLM
3. **Safety** — Tools can validate arguments and enforce restrictions

## Architecture

Tools implement a `Tool` trait with async execution. Each tool is a struct that owns its configuration. The registry holds `Box<dyn Tool>` for dynamic dispatch.

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> serde_json::Value;
    fn execute(&self, args: serde_json::Value)
        -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>>;
}

pub struct Tools(Vec<Box<dyn Tool>>);
```

Native `async fn` in traits is not dyn-compatible, so `execute` returns a pinned boxed future instead.

The `Tools` struct uses `Vec` with linear scan for lookup. For small tool counts (<50), this outperforms `HashMap` due to cache locality and no hashing overhead.

### Shared Utilities

- **`truncate_output`** — UTF-8 aware string truncation with byte count reporting. Defined in `tools/mod.rs`. Used by `exec`, `grep`, `web_fetch`, and any tool producing large output.
- **`PathGuard`** — Workspace-confined path resolution. Rejects null bytes, `../`, and absolute paths. Canonicalizes and verifies the result is under the workspace root. Provides `resolve()` for existing files and `resolve_new()` for files that don't exist yet. Used by all file tools.

## Tools

### `exec` — Shell Command Execution

Executes commands via `sh -c` within the workspace directory.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `command` | `String` | yes | Shell command to execute |

#### Behavior

1. Check for path traversal (`../`)
2. Check command against deny patterns (regex set)
3. Execute with `tokio::process::Command`, cwd = workspace
4. Capture stdout/stderr
5. Return formatted output with exit code, truncated if over limit

#### Safety Guards

Commands are checked against deny patterns before execution:

- `rm -r`, `rm -rf` — recursive deletion
- `mkfs` — filesystem creation
- `dd if=` — disk operations
- `> /dev/` — device writes
- `shutdown`, `reboot` — system power
- Fork bomb pattern
- `git clone`, `git push`, `git commit` — must use dedicated GitHub tools (`github_clone`, `github_push`, `github_commit`)
- `git reset --hard` — destructive git operations
- `gpg --export-secret`, `.gnupg/` — GPG keyring access
- `gpgsign=false` — cannot override commit signing config
- Secret harvesting (`~/.ssh/id_*`, `~/.aws/`, `.config/gh/`)
- Network exfiltration (`curl --upload-file`, `nc -l`, `socat`)
- Privilege escalation (`sudo`, `chmod`, `chown`)
- Process control, cron persistence, kernel modules, reverse shells

**Note**: These are defense-in-depth heuristics providing friendly error messages. The real filesystem boundary is the Landlock sandbox applied at startup (see [spec 15](15-sandbox.md)), which confines all child processes — including `sh -c` — to the workspace, `/nix/store`, `/tmp`, and read-only system paths.

#### Environment Scrubbing

Child processes run with a scrubbed environment. Only a known-safe allowlist of variables is forwarded:

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

Notably absent: `CREDENTIALS_DIRECTORY`. The agent's shell commands cannot discover or read the credential files path. See [spec 13](13-credentials.md) for the credential isolation design.

#### Restrictions

| Restriction | Default | Config key |
|-------------|---------|------------|
| Working directory | Workspace root | — |
| Timeout | 60 seconds | `tools.exec.timeout_secs` |
| Output size | 10KB (UTF-8 aware) | `tools.exec.max_output_bytes` |
| Path traversal | Rejects `../` | — |

#### Output Format

```
$ ls -la
total 24
drwxr-xr-x  3 kitaebot kitaebot 4096 Feb 21 12:00 .
-rw-r--r--  1 kitaebot kitaebot  512 Feb 21 12:00 SOUL.md

Exit code: 0
```

Stderr is prefixed with `STDERR:` and separated from stdout.

---

### `file_read` — Read File Contents

Read a file from the workspace with optional offset and line limit.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | `String` | yes | Relative to workspace |
| `offset` | `u32` | no | Start line, 1-based |
| `limit` | `u32` | no | Max lines to return, default 2000 |

#### Behavior

1. Resolve path via `PathGuard`
2. Read file (reject >10MB)
3. Apply offset/limit
4. Format with line numbers (`{line_number}\t{content}`)
5. Append summary (lines shown, total lines, bytes)

#### Restrictions

| Restriction | Default |
|-------------|---------|
| Max file size | 10MB |
| Default line limit | 2000 |
| Encoding | UTF-8 only |

---

### `file_write` — Write File Contents

Write content to a file in the workspace. Creates parent directories as needed.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | `String` | yes | Relative to workspace |
| `content` | `String` | yes | File content to write |

#### Behavior

1. Resolve path via `PathGuard::resolve_new`
2. Create parent directories (`create_dir_all`)
3. Write content
4. Return byte count written

---

### `file_edit` — Edit File Contents

Find-and-replace editing. Requires the old string to match exactly once in the file.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | `String` | yes | Relative to workspace |
| `old_string` | `String` | yes | Must be non-empty, must match exactly once |
| `new_string` | `String` | yes | Replacement (empty = delete) |

#### Behavior (Two-Tier Matching)

1. **Exact match**: `match_indices(old_string)` — if 1 match, use it. If >1, error with count. If 0, fallback.
2. **Whitespace-flexible**: Normalize both file and search string (collapse whitespace runs, trim trailing), sliding window comparison. Must match exactly once.
3. Splice replacement into original file at matched position, write file.

---

### `glob_search` — Find Files by Pattern

Find files matching a glob pattern within the workspace.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `pattern` | `String` | yes | Glob pattern, e.g. `"**/*.rs"` |

#### Behavior

1. Reject traversal patterns
2. `glob::glob(workspace.join(pattern))`
3. Collect up to 1000 results
4. Return sorted relative paths

#### Restrictions

| Restriction | Default |
|-------------|---------|
| Max results | 1000 |
| Traversal | Rejected |

---

### `grep` — Search File Contents

Search for a regex pattern in files within the workspace.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `pattern` | `String` | yes | Regex pattern |
| `path` | `String` | no | Directory, default `"."` |
| `include` | `String` | no | File glob filter |

#### Behavior

1. Resolve directory via `PathGuard`
2. Walk files using the `ignore` crate's `WalkBuilder` (respects `.gitignore`)
3. Search each file with `grep-searcher` + `grep-regex` (ripgrep as a library)
4. Accumulate up to 200 matches, truncate output

No external binary required — uses the `grep` facade crate (v0.4) which bundles the same regex engine as `rg`.

---

### `web_fetch` — Fetch URL Content

Fetch content from a URL and return it as text.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `url` | `String` | yes | Must be http or https |

#### Behavior

1. Validate URL scheme (http/https only)
2. GET with timeout
3. Strip HTML tags via regex `<[^>]*>`
4. Collapse whitespace
5. Truncate to configured max bytes

#### Restrictions

| Restriction | Default | Config key |
|-------------|---------|------------|
| Timeout | 30 seconds | `tools.web_fetch.timeout_secs` |
| Max response | 50KB | `tools.web_fetch.max_response_bytes` |
| Schemes | http, https | — |

---

### `web_search` — Web Search via Perplexity

Search the web using Perplexity (via OpenRouter) and return a synthesized answer.

#### Parameters

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `query` | `String` | yes | Search query |

#### Behavior

1. POST to OpenRouter with `perplexity/sonar` model
2. Return synthesized answer text

Direct HTTP POST (not via `Provider` trait — avoids circular dependency). Owns its own `reqwest::Client` and reuses the `openrouter-api-key`.

#### Restrictions

| Restriction | Default | Config key |
|-------------|---------|------------|
| Model | `perplexity/sonar` | `tools.web_search.model` |
| Max tokens | 1024 | `tools.web_search.max_tokens` |
| Timeout | 30 seconds | `tools.web_search.timeout_secs` |

### GitHub Tools — Authenticated Git & PR Operations

Ten standalone tools for git and GitHub CLI operations. Each tool holds
`Arc<GitHubClient<A>>` and owns only its business logic. The client
provides plumbing (`run_gh`, `run_git`, `resolve_repo_dir`,
`current_branch`).

Gated behind `github.enabled` in config. When disabled (or no token),
none of the tools are registered.

#### Token Injection

The GitHub PAT never enters environment variables or the context window.
For `gh` commands it is passed as `GH_TOKEN`. For `git clone`/`push`, a
temporary `GIT_ASKPASS` helper script is written, used for one
subprocess, then deleted.

#### Architecture

`GitHubApi` is the subprocess boundary trait. `RealGitHubApi` spawns
real processes; `StubGitHubApi` yields pre-enqueued responses for tests.
`GitHubClient<A>` carries workspace root, co-authors, and the API
handle. Individual tools are zero-config structs wrapping
`Arc<GitHubClient<A>>`.

#### Tools

| Tool | Description |
|------|-------------|
| `github_ci_status` | Check CI status for a git ref |
| `github_clone` | Clone a repository into the workspace |
| `github_commit` | Stage and commit changes with co-author trailers |
| `github_push` | Push commits to a remote |
| `github_pr_create` | Create a pull request |
| `github_pr_list` | List pull requests (open/closed/all) |
| `github_pr_reviews` | Fetch reviews for a pull request |
| `github_pr_comment` | Post a comment on a pull request |
| `github_pr_diff_comments` | Fetch inline diff comments on a PR |
| `github_pr_diff_reply` | Reply to an inline diff comment |

All tools take `repo_dir` (relative to workspace root) and validate it
via `GitHubClient::resolve_repo_dir` — rejects traversal, absolute
paths, and directories without `.git`.

---

## Error Handling

Errors are returned to the LLM as text (via `unwrap_or_else` in the agent loop), not propagated as exceptions. The LLM decides how to proceed.

Error variants: `NotFound`, `InvalidArguments`, `ExecutionFailed`, `Timeout`, `Blocked`.

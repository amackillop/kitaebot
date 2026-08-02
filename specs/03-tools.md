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
owns its configuration. The registry holds `Arc<dyn Tool>` in a `Vec` with
linear scan for lookup (fast enough for <50 tools, better cache locality than a
map). `Arc` rather than `Box` so the same instance can appear in multiple
tool sets — sub-agent sets ([spec 19](19-sub-agents.md)) are built by
filtering the parent's registry without reconstructing tools.

```
trait Tool: Send + Sync {
    name()        -> &'static str
    description() -> &'static str
    parameters()  -> serde_json::Value    // JSON Schema
    execute(args, ctx) -> Result<String, ToolError>
}
```

Dispatch: find tool by name, parse arguments from JSON string to `Value`,
call `execute` with a clone of the turn's `ToolCtx`. Unknown tool name
returns `ToolError::NotFound`. Malformed arguments return
`ToolError::InvalidArguments`.

### Per-Turn Context

`execute` receives a `ToolCtx` — the per-turn context the agent loop threads
into every dispatch:

```rust
pub struct ToolCtx {
    activity: Option<mpsc::Sender<Activity>>,  // event sink (spec 16)
    cancel: CancellationToken,                 // fires on client disconnect
}
```

`ToolCtx` is owned and cheaply cloneable (both fields are Arc-backed); the
loop clones it once per tool call, and the clone moves into the tool's boxed
future. `run_turn` itself carries the same struct in place of separate
activity/cancel parameters — one source of truth per turn.

Most tools ignore the ctx. The `task` tool ([spec 19](19-sub-agents.md))
uses both fields: it forwards child activity events (labeled) to the
parent's sink and passes the real cancellation token into the child loop.
Primary cancellation remains drop-based — the loop races `join_all` against
the token — the ctx token exists for tools that can react more gracefully
than being dropped.

Tool definitions are converted to `ToolDefinition` (OpenAI function-calling
format) and passed to the provider on each call.

### Disabling Tools

Individual tools can be excluded by name via `tools.disabled` in config.
Unknown names in the disabled list are rejected at startup.

### Filtered Tool Sets

A registry can produce a second registry containing only allowlisted names
(shared `Arc` instances, no reconstruction). Used by spec 19 to build
per-agent-type tool sets at startup. Names with no matching tool are skipped:
the tool may be disabled via `tools.disabled` or compiled out. Unlike
`tools.disabled` (operator input, validated at startup), the allowlists are
hardcoded, so typos are caught by tests instead.

### Shared Utilities

- **`truncate_output`** — UTF-8 aware string truncation with byte count
  reporting. Used by `exec`, `grep`, `web_fetch`, and any tool with large
  output. Most callers pass `TOOL_OUTPUT_CEILING_BYTES` (5 MiB, not
  configurable): a memory-protection ceiling, not context policy.
  Context-size limits live in the engines (`context.tool_output_tokens`,
  spec 14), which externalize or truncate tool results long before the
  ceiling matters.
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
   injection/escape (`LD_PRELOAD`, `nsenter`), credential probing
   (`~/.git-credentials`, `credential.helper=` injection), and git
   operations that must use dedicated tools.

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
| Output size | 5 MiB memory ceiling (UTF-8 aware) | none — engine policy applies first (spec 14) |

**Process lifetime:**

The child is spawned with `kill_on_drop`. Both timeout and turn cancellation
work by dropping the wait future, which kills the direct `bash` child instead
of orphaning it. Grandchildren that detach from the bash process may still
survive — process-group kill is out of scope.

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

A failed edit is the most expensive tool error in the loop: the model
re-reads, re-guesses, and often re-fails. The contract below spends its
complexity on making failures recoverable in the same turn.

**Parameters:**

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | String | yes | Relative to workspace |
| `old_string` | String | yes | Must be non-empty |
| `new_string` | String | yes | Replacement (empty = delete) |
| `replace_all` | Bool | no | Replace every exact match (default false) |

**Match ladder** — four rungs tried in order; the first rung that
produces at least one match decides the outcome:

1. **Exact**: byte-for-byte `match_indices(old_string)`.
2. **Trailing-whitespace-insensitive**: line-by-line comparison with
   trailing whitespace stripped from both sides.
3. **Whitespace-flexible**: collapse whitespace runs, trim trailing,
   sliding-window comparison.
4. **Unicode-folded**: rung 3's normalization plus folding typographic
   confusables to ASCII on both sides — curly quotes to straight,
   en/em dashes to hyphen, non-breaking space to plain space.

Rungs are ordered least- to most-aggressive so the most faithful match
wins. Matching runs against normalized views, but the splice always
replaces the corresponding span of the *original* bytes, and
`new_string` is inserted verbatim — normalization never rewrites file
content it wasn't asked to touch.

**Match-count semantics** on the deciding rung:

- Exactly one match: edit applies.
- Multiple matches, rung 1, `replace_all: true`: all replaced.
- Multiple matches otherwise: error naming the rung, the count, and
  the line number of each match, suggesting more surrounding context
  (or `replace_all` when the matches were exact). `replace_all` never
  applies to fuzzy rungs — fuzzy matching exists to absorb copy noise
  around one target, not to bulk-edit text the model didn't write
  precisely.
- Zero matches on all four rungs: stale-read failure, below.

**Success result**: the edited region echoed with line numbers and
three lines of context on each side — the model sees the applied state
without a follow-up read and gets line anchors for subsequent edits.
For `replace_all`, the match count plus the first edited region.

**Stale-read failure**: when no rung matches, the error carries a hint
that the file may have changed since it was read, followed by the
file's current content. Recovery becomes a same-turn re-issue of the
edit against fresh content instead of a read round-trip. The snapshot
is not truncated at the tool layer; it rides the standard tool-output
pipeline, where the engines own size policy.

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
| Max response | 512KB | `tools.web_fetch.max_response_bytes` |

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

Four tools wrapping the `git` binary. Gated behind `git.enabled` in config.
`git.repositories` keys repos by exact `owner/repo` (case-insensitive);
listing a repo trusts its `.envrc` on clone — see
[Direnv Cache](#direnv-cache).

`GitCli<R>` holds the GitHub PAT, workspace root, co-authors, and an optional
direnv cache. The token is injected via a temporary `GIT_ASKPASS` helper script
(0o700 permissions, deleted on drop) for authenticated operations. Commits do
not need authentication; clone, fetch, and push do.

| Tool | Description |
|------|-------------|
| `git_clone` | Clone a repository into the workspace. For repos listed in `git.repositories`, runs `direnv allow` synchronously then warms the direnv cache in the background. |
| `git_commit` | Commit staged changes with co-author trailers. |
| `git_fetch` | Fetch refs from a remote. Fetch a base branch before rebasing onto it. |
| `git_push` | Push commits to a remote. |

All tools take `repo_dir` (relative to workspace root) and validate it via
`resolve_repo_dir` — rejects traversal, absolute paths, and directories
without `.git`.

---

### GitHub Tools

Eight tools on the in-process REST client. Gated behind `github.enabled`
in config (separate from `git.enabled`).

`GithubApi` pairs the client (which holds the PAT inside its IO closure)
with workspace-relative repo-dir resolution: the `owner/repo` a tool acts
on comes from the checkout's origin remote, so the model names a
directory, never a repo.

| Tool | Description |
|------|-------------|
| `github_api` | Generic REST escape hatch. Paths are forced under `repos/<owner>/<repo>/` and the first segment must be one of `issues`, `labels`, `milestones`, `pulls`, `releases`. |
| `github_ci_status` | Check CI status for a git ref. |
| `github_pr_create` | Create a pull request. |
| `github_pr_list` | List pull requests (open/closed/all). |
| `github_pr_review_submit` | Submit a formal PR review (APPROVE or COMMENT) with inline comments. REQUEST_CHANGES is unrepresentable in the args. |
| `github_pr_reviews` | Fetch reviews for a pull request. |
| `github_pr_diff_comments` | Fetch inline diff comments on a PR. |
| `github_pr_diff_reply` | Reply to an inline diff comment. |

---

### `task` — Sub-Agent Delegation

Defined and owned by [spec 19](19-sub-agents.md). Registered in the parent's
registry like any other tool; excluded from all sub-agent tool sets.

---

### `notify` — Push Notification

Defined and owned by [spec 17](17-notify.md). Registered only when
`telegram.enabled`; excluded from all sub-agent tool sets.

| Param | Type | Required | Notes |
|-------|------|----------|-------|
| `message` | String | yes | Content to send |
| `urgency` | String | no | `low` (default, batched) or `high` (immediate) |

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
   before returning, and only for repos listed in `git.repositories`. An
   `.envrc` is arbitrary shell executing at clone time, before anyone has
   read the repo; the allowlist is the only gate on that. Unlisted repos
   clone normally and degrade to no-devshell (requirement 4).
7. **Shared across tools** — single cache instance shared between exec and
   git_clone

### Invalidation

Cache keys are directories. Staleness is determined by the mtime of `.envrc`
and `flake.lock` — two `stat` calls per lookup.

## Build Warm

### Problem

A repository's pre-commit hook runs that repository's checks. On a cold
Nix store those checks are a full build (~40 min at 4 cores, vs seconds
warm) — far past the 900s `git_commit` allows for hook-running
subcommands, so the first commit to any repository cannot land. Raising
the timeout doesn't fix it: the wait is a build, and belongs somewhere
visible rather than inside a tool call.

### Requirements

1. **Warm after the devshell** — the check command resolves from the
   devshell, so it runs after [Direnv Cache](#direnv-cache) requirement 5
   completes for that checkout
2. **Declared, not guessed** — command configured per repo (the repo's
   `AGENTS.md` states it in prose, not machine-readably). Unconfigured
   repos warm the devshell only
3. **Two triggers** — the self-maintenance duty
   ([spec 24](24-self-directed-work.md)) for configured repos, and
   checkout preparation for repos cloned on demand
4. **Unconditional** — a warm-store run costs seconds; no bookkeeping
   about whether it already happened
5. **Background** — never blocks the turn that triggered it
6. **Readers wait, one runner** — a `Notify` per repo, as the direnv
   cache does. Nix serialises same-derivation builds anyway, but a
   waiter blocked on the derivation lock dies on the tool timeout with
   no indication why
7. **Only `git_commit` waits** — reading, editing, and delegating need
   nothing from the build cache
8. **Failure doesn't block** — a failed warm is logged; `git_commit`
   proceeds and fails on the hook honestly
9. **Its own timeout** — generous, separate from tool timeouts; the
   thing waited on is a build, not a command
10. **Visible** — emit activity while waiting, so a long wait reads as
    progress rather than a hang

### Configuration

`check` on the repo's `git.repositories` entry is the repo's check
command — the same one its pre-commit hook runs — and the warm runs
it ahead of need. Hanging it off the trust entry means it cannot name
a repo the trust list does not: listing the repo is the trust grant,
and the command rides on it. No new trust surface — `warm_devshell`
already executes the repo's `.envrc` on clone behind the same entry.
Repos without one warm the devshell and nothing else.

```toml
[git.repositories."amackillop/kitaebot"]
check = "just check"
```

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
| `file_edit` no match | `ExecutionFailed` | Error carries stale-read hint + current file content (see tool section) |
| `file_edit` ambiguous match | `ExecutionFailed` | Error lists rung, count, and match line numbers |

All errors are surfaced to the LLM as text. The LLM decides how to proceed.
The agent loop's policy gate escalates repeated `Blocked` errors (see
[spec 01](01-agent-loop.md)).

## Constraints

| Config key | Default | Description |
|------------|---------|-------------|
| `tools.exec.timeout_secs` | 600 | Exec command timeout |
| `tools.web_fetch.timeout_secs` | 30 | HTTP GET timeout |
| `tools.web_fetch.max_response_bytes` | 524288 | HTTP response cap (also bounds network transfer) |
| `tools.web_search.model` | `perplexity/sonar` | Search model |
| `tools.web_search.max_tokens` | 1024 | Search response cap |
| `tools.web_search.timeout_secs` | 30 | Search timeout |
| `tools.disabled` | `[]` | Tool names to exclude |

Git/GitHub tools require their respective `git.enabled`/`github.enabled` flags
and a valid GitHub PAT loaded from credentials.

## Open Questions

- **Declared vs discovered warm command**: per-repo config goes stale
  silently when a repo renames its check; a machine-readable convention
  in the repo would be self-service but needs every repo to adopt it.
  Configured while there are two repos.
- **Rooting the warm**: nothing holds a gcroot on what a warm builds —
  the command is opaque (`just check`, a package script), not a
  `nix build` with an out-link — so GC can evict it. GC is
  pressure-triggered only ([spec 09](09-vm.md#garbage-collection)), so
  eviction takes disk pressure, not a timer. Whether rooting needs
  solving beyond that needs the [spec 24](24-self-directed-work.md)
  duties running first.
- **Read-before-edit precondition**: opencode-style enforcement
  (reject edits to files not yet read this session) would prevent
  blind edits but needs per-session read tracking in the harness. The
  stale-read failure snapshot already covers the recovery path;
  deferred until blind edits show up in practice.

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

The LLM sometimes passes a field as a JSON string instead of a
JSON value (e.g. `"review": "{\"repo\":\"...\"}"` instead of
`"review": {"repo":"..."}`, or `"limit": "30"` instead of
`"limit": 30`). Fields prone to this use the `string_or_value`
deserializer, which parses the inner JSON string before deserializing into
the target type. Applied to `task`'s `review` parameter,
`github_api`'s `body` parameter, `file_read`'s `offset`/`limit`
parameters, `lcm_grep`'s `limit` parameter, and `lcm_expand`'s
`depth`/`include_messages`/`token_cap` parameters.

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
- **`truncate_head`** — Head-truncating variant for log-shaped output where
  the diagnosis concentrates at the end. Drops leading bytes and keeps
  the last `max_bytes`, prepending `[truncated N leading bytes]`. Used
  by `job_logs` (CI logs); `github_api`'s raw path uses head-keeping
  `truncate_output` (generic API responses are not tail-weighted).
- **`PathGuard`** — workspace-confined path resolution. Rejects null bytes,
  `../`, and absolute paths outside the workspace; an absolute path under
  the root is accepted as the relative spelling it names (the prompt
  advertises the root, so models echo it — refusing the unambiguous form
  only burned iterations). Normalization happens before the daemon-owned
  write fence, which compares components and must never see the absolute
  spelling. Canonicalizes and verifies the result is under the
  workspace root. Provides `resolve()` for existing files and `resolve_new()`
  for files that don't exist yet; the `resolve_writable*` variants add the
  daemon-owned fence: `config.toml`, `context/`, and `state/` are readable but
  not writable through the file tools, with `state/review-checklist.md` as the
  one model-maintained exception. Used by all file tools.

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

1. **Regex layer** — a compiled `RegexSet` of ~60+ patterns covering:
   destructive file ops (`rm -rf`, `find -delete`), internal state
   (workspace-root `context/` references except reads of
   `context/lcm/payloads/<file_id>`, which `<file>` references hand to the
   model and the sandbox grants; redirection into `state/`),
   disk/filesystem (`mkfs`, `dd if=`, `fdisk`), system power (`init 0-6`,
   `systemctl`), privilege escalation (`sudo`, `chmod`, `chown`),
   network exfiltration (`curl -T`, `nc -l`, `socat`), pipe-to-shell
   (`curl|sh`, `wget|sh`), reverse shells (`/dev/tcp/`, python/ruby/perl
   socket), port scanning (`nmap`, `masscan`), secret harvesting
   (`~/.ssh/id_*`, `~/.aws/`), GPG keyring access, process control
   (`kill -9`), cron persistence (`crontab`), kernel modules, firewall
   manipulation, injection/escape (`LD_PRELOAD`, `nsenter`), credential
   probing (`~/.git-credentials`, `credential.helper=` injection), git
   operations that must use dedicated tools, and the Nix fences
   (`nixos-rebuild`, `nix-env`, `nix store delete/gc/optimise`,
   `nix-channel`, `nix copy --to`, remote flake refs).

2. **Shell-aware structural layer** — splits the raw string into
   simple-command segments on unquoted `|`, `;`, `&`, and newline
   (a separator inside quotes or after a backslash is an argument),
   tokenizes each segment with `shlex`, strips env var and path
   prefixes, and matches binary+subcommand. Catches bypass patterns
   like `VAR=x git commit`, `/usr/bin/git clone`, and piped/chained
   commands. Owns every deny rule that is just a binary name in
   command position (`shred`, `wipe`, `truncate`, `mount`, `umount`,
   `shutdown`, `reboot`, `poweroff`, `halt`, `su`, `at`): those names
   double as English prose and grep-pattern text, so command position
   must be decided with real quoting rules — a regex over the raw
   string blocked `grep "a\|truncate"` (#135). Also blocks `gh auth`
   and `nix profile`.

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

The child is spawned with `kill_on_drop` as the leader of its own process
group. Both timeout and turn cancellation work by dropping the wait future,
which kills the direct child and sweeps the rest of the group with `SIGKILL`
via a drop guard, so grandchildren (`bash → just → cargo → test binaries`)
cannot outlive the turn. A normal exit disarms the sweep: whatever a
finished command deliberately left running in the background is its own
business. A timeout is never a bare kill: the child's output is read
incrementally into buffers that survive the sweep, and the timeout
error carries a bounded tail of it plus a cgroup pressure snapshot
(memory.current, memory.events, PSI) taken at kill time — a stalled
build's last words and the pressure it died under are the diagnosis
(issue #74). The same group semantics apply to every subprocess spawned
through `cli_runner` (git, warm). Descendants that call `setsid` escape
the group and the sweep; nothing short of cgroups catches those.

**Test-evidence trailer** (issue #145): exec output recognized as a
failing test run gets a mechanical trailer appended at dispatch —
summed pass/fail counts, failed test names (bounded), and the first
panic block with its assertion values. The model reads results through
self-authored filters and discards the decisive lines (a 200-iteration
turn piped seven test runs through `grep -A2 panicked | head -5` while
debugging a wrong expectation the left/right values would have
refuted); the trailer preserves the verdict regardless. Recognition is
a per-format registry — libtest and pytest today — that degrades to no
trailer on unrecognized output, never to an error, and passing runs get
none: it exists to preserve failure evidence, not restate green
summaries. The remaining half of #145, guidance against piping test
commands at all, waits on the #136 deny-guidance work it builds on.

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
| `new_string` | String | yes | Replacement (empty = delete); must differ from `old_string` |
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
that the file may have changed since it was read, followed by a
bounded excerpt around the nearest candidate line — the line with the
highest normalized-token overlap with `old_string` (ties to the
earliest; zero overlap anchors at the top) — in `file_read`'s numbered
format with three lines of context each side and a `[showing lines
X-Y of Z]` header when the window is partial. The excerpt is capped at
2 KiB by the tool itself (`truncate_output`), not delegated to the
engine: the same string is the log line and error-tee entry, and the
tee has no per-entry cap (spec 24, "entry size is correctness"). An
excerpt around the nearest match also beats a full dump for
re-synchronization — the model gets line anchors near where it was
editing instead of a wall of source to re-scan. Recovery remains a
same-turn re-issue of the edit against the excerpt, or a targeted
`file_read` when the excerpt shows the file moved further than one
window.

**No-op loop guard**: the tool remembers recent futile payloads per
path, in memory, for the daemon's lifetime. A payload is futile when
it fails to match (or matches ambiguously), or when it matches but
leaves the file byte-identical — reachable through a fuzzy rung even
with `old_string != new_string`, when the replacement is the file's
existing spelling. The third identical futile payload in a path's
recent window returns `EditLoop` — a hard error instructing a re-read
— instead of a third copy of the same result. Identical `old_string`
and `new_string` never gets that far: it is rejected upfront as
invalid arguments. A no-change success is labeled `(no change)` in the
result and skips the write, so identical bytes never churn the mtime;
a content-changing edit clears the path's history. The guard
classifies outcomes rather than pre-blocking payloads: if the file
changed since the failures and the same payload now lands, it succeeds
normally. The window is a short per-path ring (not a consecutive
counter), so alternating two failing payloads trips the guard for
each.

The history is one map shared across tasks and sub-agents — the tool
is a daemon-lifetime singleton — so concurrent writers to the same
path can race it: one agent's successful write clears counts another
was accumulating, and interleaved attempts can trip the guard on a
payload that would have succeeded after the next write. Both races
are benign by the outcome-classifying design above: a cleared ring
only delays the hard stop, and a spurious `EditLoop` costs exactly
one re-read. The guard needs no serialization to stay correct, only
to stay cheap — which a spurious trip does not threaten.

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

Requests carry no credentials. A 403/404 from a GitHub host (`github.com`,
`*.github.com`, `*.githubusercontent.com`) is indistinguishable from a private
resource, so it returns a blocked error steering the model to the
authenticated `github_*` tools instead of a bare status.

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
| `git_push` | Push commits to a remote. Fast-forward only — no force option: published bot branches are append-only outside `git_fixup`. The flag existed for rebase/squash workflows (fc1041c), which the keyring isolation (spec 15) later made impossible via exec; its only remaining use was silently squashing a branch under review. When `branch` is absent, the tool resolves the current branch via `git symbolic-ref --short HEAD` rather than relying on the upstream tracking config (which may point at `origin/HEAD` after `git switch -c <branch> origin/HEAD`). |
| `git_fixup` | The sanctioned history rewrite: meld the staged changes into an earlier commit of the current branch (same-base autosquash) and force-push with lease. Safety: refuses base-branch targets, dirty worktrees, and detached HEAD; verifies the final tree is byte-identical to the pre-rebase tree (melding never changes the tree, so any conflict resolution or rebase bug that alters content is rejected mechanically); on conflict or violation it aborts, restores the branch, and leaves the tweak staged for a normal commit. Runs in the git tier, so rewritten commits are signed. Moved-base rebases are `git_rebase`'s job. |
| `git_rebase` | The second sanctioned history rewrite: replay the current branch onto the updated default branch (`action: start`), pausing on conflicts for the model to resolve (`file_edit` + `git add` via exec, then `action: continue`; `abort` restores). No tree invariant can exist here — a conflict resolution changes content by definition — so the mechanical bounds are two: the push is `--force-with-lease` pinned to the remote branch position from `start`'s fetch (a concurrent push fails the lease instead of being overwritten), and `start` refuses a branch that diverged from its remote-tracking ref, so the push publishes only the tool's own replay plus in-window resolutions — never pre-cooked local history. Resolution *correctness* is deliberately unguarded in-tool; the PR review reads the resolved diff. Refuses the default branch, dirty worktrees, and detached HEAD. Runs in the git tier, so replayed commits are signed. |

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
| `github_api` | Generic REST escape hatch. Paths are forced under `repos/<owner>/<repo>/` and the first segment must be one of `actions` (GET only — writing dispatches, re-runs, or cancels workflows, a human decision), `dependabot` (GET only — writing dismisses alerts, a human decision), `issues` (comment paths GET only — the channels post the turn's reply as the thread comment, and a tool-written comment duplicates it and steals the plan anchor, spec 25), `labels`, `milestones`, `pulls`, `releases`. |
| `github_ci_status` | Report the latest CI run on a branch (pending, green, or red), with failure logs when it failed. |
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
3. **Don't cache fast failures** — transient direnv errors (blocked,
   parse error) don't poison the cache; the next caller retries.
   Timeouts are the exception: a 900s evaluation timeout is cached
   with a short TTL (60s) so repeated operations during a hang degrade
   to no-devshell immediately instead of each blocking for the full
   timeout
4. **Detect silent flake failures** — `direnv export json` exits 0
   even when `use flake` fails, printing the nix error to stderr and
   exporting a bare environment. The cache classifies by the export
   first: an export carrying a nix devshell signature (`IN_NIX_SHELL`
   or `NIX_STORE`) proves the flake evaluated and succeeds regardless
   of stderr noise, since shellHook steps (just, pnpm, cargo) also
   print `error:`-prefixed lines without breaking the devshell. A
   signature-less export falls back to checking stderr for nix's
   `error:` marker (after stripping ANSI color codes); a match is a
   failure carrying the stderr, so the real error surfaces in the
   duty outcome rather than as a `command not found` from bare PATH.
5. **Graceful degradation** — if direnv fails, exec runs without the devshell
6. **Warm on clone** — `git_clone` pre-populates the cache in the background
7. **Trust before evaluate** — `git_clone` runs `direnv allow` synchronously
   before returning, and only for repos listed in `git.repositories`. An
   `.envrc` is arbitrary shell executing at clone time, before anyone has
   read the repo; the allowlist is the only gate on that. Unlisted repos
   clone normally and degrade to no-devshell (requirement 5).
8. **Shared across tools** — single cache instance shared between exec and
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
   devshell, so it runs after [Direnv Cache](#direnv-cache) "Warm on
   clone" completes for that checkout
2. **Declared, not guessed** — command configured per repo (the repo's
   `AGENTS.md` states it in prose, not machine-readably). Unconfigured
   repos warm the devshell only
3. **Two triggers** — the self-maintenance duty
   ([spec 24](24-self-directed-work.md)) for configured repos, and
   checkout preparation for repos cloned on demand
4. **Unconditional per invocation** — a warm-store run itself costs
   seconds and needs no bookkeeping about whether it already happened
   (the `Warmer`'s `Notify` dedup handles concurrent callers). The
   duty's *scheduling* is gated per-repo on new commits
   ([spec 24](24-self-directed-work.md) warm duty), but each warm
   invocation that the gate lets through runs unconditionally
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

### Cargo Target Dir

A shared `CARGO_TARGET_DIR` at `workspace/projects/target` replaces
per-repo `target/` dirs. Set in the daemon's systemd environment
(`vm/configuration.nix`) and added to `SAFE_ENV_VARS` so `safe_env()`
lets it through to exec children, warm commands, and git hooks.
Placed under `projects/` so the exec Landlock tier (which grants `all`
on `projects/`) can write it; the workspace root is list-only.
Survives `git clean -fdx` by construction — the clean runs inside
individual repo dirs, and `projects/target` is a sibling, not a
child. Per-repo `target/` is no longer in `KEPT_CACHES` and is swept
on clean.

The devShell does not set `CARGO_TARGET_DIR`, so the systemd
environment is the sole source — direnv does not override it.

The warm command (`just warm`: `cargo build --tests --features
mock-network && cargo sweep --time 7 && cargo sweep --maxsize 12GB
&& nix build .#deps --out-link .gcroots/deps`) populates the shared
dir, sweeps it, and roots the crane dep closure (below). Chained
after `just check` in the repo's warm config
(`deploy/configuration.nix`), so the daily warm covers the cycle.
`cargo-sweep` runs on the shared dir; cross-repo collisions are a
known cargo hazard (upstream #14135) accepted because the agent
serializes turns.

The sweep is two-stage because a pure time sweep cannot bound the
dir: incremental caches accrete one universe per crate × feature set
× profile × RUSTFLAGS combination, each up to ~650 MB, and steady
build activity keeps them all inside any freshness window (measured
2026-08-24: 18 GB, 11 GB of it incremental universes, against an
original ~1-2 GB estimate). `--time 7` drops artifacts unused for a
week; `--maxsize 12GB` then evicts oldest-first to a hard ceiling.
12 GB is ~1.5× the measured healthy live set (~8 GB: the mock-network
test profile, two clippy profiles, and the other repos' occasional
builds), so eviction reaches hot artifacts only if the live set
itself outgrows the cap; the cost of a miss is one leaf rebuild.

### Nix Store Roots

`nix flake check` roots nothing it builds, and the VM's automatic
GC (weekly `nix.gc` timer plus `min-free`/`max-free` pressure
collection) deletes any unrooted path. Without a root the crane dep
closure (`kitaebot-deps`, the vendored registry) is garbage by
construction, and every GC pass costs the next nix operation a
~10-minute rebuild of all vendored crates — long enough to blow the
600s exec timeout twice over while looking like a hung cargo (the
2026-08-23 incident behind this section).

`just warm` therefore pins the closure: `nix build .#deps --out-link
.gcroots/deps`, where `packages.deps` exposes crane's
`cargoArtifacts`. The out-link lives in the checkout, so the
per-turn clean must keep it — `.gcroots` is in `KEPT_CACHES` beside
`.direnv`, which survives for the same reason (nix-direnv's devshell
and flake-input roots live there).

Cleanup is by unrooting, not deletion: the out-link path is fixed,
each warm atomically repoints it, and the superseded closure becomes
garbage for the next GC pass. No code deletes old closures — the GC
already does, and it runs often. The deps derivation is built from
crane's dummy source (Cargo.toml/Cargo.lock only), so the root stays
valid across source edits and the link rewrite is a no-op between
lock changes; a stale closure outlives a lock bump by at most one GC
period. Rooting stops at deps deliberately: check outputs
(clippy/test) are ~40s leaf rebuilds with deps alive, and
`packages.default` churns per commit.

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
| Command exited non-zero | `CommandFailed` | Error text (command, output, exit code) returned to LLM |
| `file_edit` no match | `Precondition` | Error carries stale-read hint + bounded excerpt around the nearest candidate line (see tool section) |
| `file_edit` ambiguous match | `Precondition` | Error lists rung, count, and match line numbers |
| `file_edit` repeated futile payload | `EditLoop` | Third identical payload in a path's recent window that produced no change or failed identically; instructs a re-read |

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
- **Read-before-edit precondition**: opencode-style enforcement
  (reject edits to files not yet read this session) would prevent
  blind edits but needs per-session read tracking in the harness. The
  stale-read failure snapshot already covers the recovery path;
  deferred until blind edits show up in practice.

# Kitaebot

Autonomous programming agent in Rust. Runs in a NixOS VM with Landlock sandboxing, proxy-based egress filtering, credential isolation, and leak detection.

## Overview

Kitaebot is a long-running daemon that accepts messages via Telegram, Unix socket, GitHub PR comments, GitHub issues, or Linear issues, routes them through an LLM agent loop with tool use, and persists conversation state through a pluggable context engine. A duty scheduler runs recurring work on its own schedule. The agent keeps durable cross-session knowledge in a memory subsystem, reviews its own work through gated reviewer sub-agents, and can consume external MCP tool servers.

Two binaries:

| Binary | Purpose | Lifecycle |
|--------|---------|-----------|
| `kitaebot run` | Daemon (Telegram + socket + duties + GitHub PRs/issues + Linear) | systemd service |
| `kitaebot backup <dir>` | Stage durable workspace state into `<dir>` for archiving | On-demand |
| `kchat <socket>` | Socket client REPL | On-demand |

## Architecture

```
Channels (Telegram, Unix socket, GitHub PRs, GitHub issues, Linear, Duties)
        │
        ├─ Messages ──► AgentHandle ──► Agent actor (sequential)
        │                                 ├─ process_message ──► LLM loop
        │                                 └─ commands::execute ──► local ops
        │
        └─ Context engine (flat JSON sessions or LCM SQLite DAG,
           messages tagged by source)
```

The agent is an actor (Ryhl pattern) — a spawned tokio task that processes one envelope at a time. Channels hold cloneable `AgentHandle`s and send messages via `send_message()`, awaiting a reply over a oneshot channel. This eliminates session locking: the actor owns the session and processes requests sequentially.

The agent loop calls the LLM, dispatches tool calls in parallel, checks outputs for leaked secrets, and repeats until the model produces a final response or hits `max_iterations`.

### Context engines

Conversation state lives behind the `ContextEngine` trait, selected via `context.engine`:

- **flat** (default) — per-name JSON session files under `workspace/context/sessions/`; compacts by summarizing the whole history when the token budget is exceeded.
- **lcm** — hierarchical compaction over a SQLite DAG at `context/lcm.db`. Old messages collapse into summary nodes (between turns at a soft threshold, blocking mid-turn only at the hard emergency threshold); the `lcm_*` tools let the agent search and re-expand compacted history.

Sub-agents run on an ephemeral in-memory engine that never compacts.

### Memory

Durable cross-session knowledge lives in `memory/` (spec 21). The index file `memory/MEMORY.md` is read fresh each root turn and appended to the system prompt, so the agent always sees what it knows without a tool call; detail lives in `memory/topics/*.md`, reached with the ordinary file tools. A distillation duty folds recent session history into memory on a schedule, gated by a mechanical token counter (per-session watermarks track what has already been distilled) so a pass only spends an LLM turn when enough new material has accumulated.

### Sub-agents and self-review

The `task` tool delegates to sub-agents: `explore` (read-only research), `worker` (can write files and execute commands), and `reviewer` (read-only judge). Each can run on its own model override.

The review pipeline (spec 23) prompts the agent to have a reviewer sub-agent judge its work at four gates: `plan`, `commit`, `series`, and `pr`. The reviewer ends its response with a fenced `findings` block; the task tool parses it and records verdicts and findings in a review ledger (`/findings` reads it back). Gates are prompted, not enforced — `review.enabled` only controls the recording.

### Duties

The duty scheduler (spec 24) dispatches named units of scheduled work through the agent actor. Schedules are wall-clock with persisted `last_run`, so cadence survives restarts and an overdue duty fires once at startup instead of bursting. Built-ins: `distill` (memory distillation, token-gated), `warm` (re-warms build caches for repos whose HEAD moved), and `self_analysis` (mines the bot's own error tee and journal, proposes fixes as GitHub issues). Operators add `prompt` duties — arbitrary watch-tasks against a trusted repo, optionally gated on `new-commits` so they only fire when the remote head moved.

WARN and ERROR tracing events are mirrored as JSON lines under `state/errors/` (daily rolling files, bounded retention) so the self-analysis duty can read back symptoms that otherwise exist only in journald.

### Tools

Typed tools replace a generic shell. The LLM declares intent via parameters instead of reasoning about shell syntax.

| Tool | Description |
|------|-------------|
| `exec` | Run a shell command (timeout, output cap, deny-list, env scrubbing) |
| `file_read` | Read a file |
| `file_write` | Write a file |
| `file_edit` | Patch a file |
| `glob_search` | Find files by pattern |
| `grep` | Search file contents (ripgrep backend) |
| `web_fetch` | HTTP GET (timeout, response size limit) |
| `web_search` | LLM-powered web search (Perplexity) |
| `git_clone` | Clone a repository (auto-warms direnv cache) |
| `git_commit` | Commit staged changes |
| `git_fetch` | Fetch refs from a remote |
| `git_push` | Push commits to a remote (fast-forward only) |
| `git_fixup` | Meld staged changes into an earlier branch commit (tree-invariant force push) |
| `git_rebase` | Rebase the branch onto the moved default branch, conflict-aware (lease-pinned force push) |
| `github_pr_create` | Create a pull request |
| `github_issue_create` | Open an issue in a configured repo (unassigned; assignment is the human gate) |
| `github_pr_list` | List pull requests |
| `github_pr_reviews` | Fetch PR reviews |
| `github_pr_diff_comments` | Fetch PR diff comments |
| `github_pr_diff_reply` | Reply to a PR diff comment |
| `github_pr_review_submit` | Submit a PR review (approve / request changes / comment) |
| `github_comment_update` | Edit a previously posted comment |
| `github_ci_status` | Check CI status for a ref |
| `github_api` | GitHub REST escape hatch, scoped to the repo |
| `linear_set_state` | Move a Linear issue to a named workflow state |
| `task` | Delegate to a sub-agent (`explore` research, `worker` implementation, `reviewer` judge) |
| `notify` | Push a message to the user via Telegram (batched by priority) |
| `lcm_grep` | Search compacted history (LCM engine) |
| `lcm_describe` | Inspect a compacted node (LCM engine) |
| `lcm_expand` | Re-expand compacted history (LCM engine, sub-agents only) |

Git and GitHub tools are gated on `git.enabled` and `github.enabled` respectively; `linear_set_state` on `linear.enabled`; `notify` on `telegram.enabled`; the `lcm_*` tools on `context.engine = "lcm"`. Tools can be individually disabled via `tools.disabled`.

All tool outputs pass through `safety::check_tool_output` and execute inside the Landlock sandbox.

### MCP servers

External tool servers speaking MCP (JSON-RPC 2.0 over stdio) can be attached via `mcp.servers` (spec 22). Each server is spawned as a child process; its advertised tools register namespaced as `<server>_<tool>` alongside the built-ins. Per server: an optional `tools` allowlist bounds schema size, `env_credentials` maps environment variables to systemd credentials so secrets never appear in `config.toml`, and `explore = true` admits the server's tools to the read-only sub-agent sets (the operator asserting it has no side effects). The implemented protocol subset is `initialize`, `tools/list`, and `tools/call`.

### Security model

1. **Landlock sandbox** — Filesystem access restricted to workspace, `/nix/store` (ro), `/tmp`, `/etc` (ro), `/dev`. Applied at startup, inherited by child processes. Every same-uid child that runs repo-influenced code (the exec tool, build-warm commands, and git) additionally re-enforces a tighter tier on itself via the hidden `confine` subcommand: `projects/` stays fully writable, but `state/`, `context/`, `memory/` are neither readable nor writable (one carve-out: `context/lcm/payloads/` is readable, since `<file>` references hand those paths to the model), and the signing keyring lives outside the workspace so no exec child names it at all. The `git` tier additionally grants the keyring and a private askpass helper for signing and authenticated fetches. `tools.exec.sandbox` selects the per-child mechanism for exec: `landlock` (default), `bwrap` (bubblewrap namespace view masking daemon-owned paths; needs mount/userns syscalls loosened on the unit), or `none` (children inherit the daemon's Landlock).
2. **Proxy-based egress filter** — nftables restricts the kitaebot uid to loopback; all outbound HTTP(S) goes through a local tinyproxy that allows CONNECT only to allowlisted hostnames. Prevents prompt-injection-driven exfiltration.
3. **Leak detection** — Regex scan on tool outputs before they enter the context window.
4. **Credential isolation** — Secrets loaded via systemd `LoadCredential` before Landlock enforcement. Inaccessible to child processes.
5. **Environment scrubbing** — `exec` runs with a safe allowlist of environment variables.
6. **Path confinement** — `PathGuard` rejects path traversal in file tools.
7. **systemd hardening** — `ProtectSystem=strict`, `ProtectHome`, `NoNewPrivileges`, empty `CapabilityBoundingSet`, `MemoryDenyWriteExecute`, syscall filter.

### Provider

Any OpenAI-compatible chat completions API. Supported endpoints:

- OpenRouter (default; per-endpoint pricing fetched live for `/usage`)
- OpenAI
- Groq
- Together
- Mistral
- Any URL (`provider.api = "https://..."` hits a custom OpenAI-compatible endpoint)

Per-role model overrides route sub-agents, summarization, memory distillation, review, and planning to different models (see `model_overrides` below). Every turn is metered into a per-task usage ledger (spec 27): cost, turns, and wall time, with sub-agent usage rolled up into the parent task; `/usage` reports it.

## Development

Requires [Nix](https://nixos.org/) with flakes enabled.

```bash
nix develop              # Enter dev shell
just check               # Full validation: nix flake check, nix lint/fmt, clippy, tests, audit
just rust-check          # Fast inner loop: cargo fmt-check + clippy + tests (not the commit gate)
just audit               # Supply-chain audit: RustSec advisories, sources, bans, licenses (deny.toml)
just bench-build [rev]   # Benchmark cold/link-heavy builds; optional rev to compare against
just build               # Compile
just warm                # Warm the shared cargo target dir, sweep stale artifacts, gcroot the dep closure
just test                # Run tests (mock-network feature)
just test-e2e            # E2e suite: real daemon against a loopback fixture server
just test-one NAME       # Run tests matching a name
just test-nixos          # Run all NixOS VM integration tests
just test-nixos-one NAME # Run a single NixOS VM test (e.g. egress)
just lint                # Clippy with --deny warnings
just fmt                 # Format Rust + Nix
just fix                 # Auto-fix clippy issues
just wt BRANCH [BASE]    # Worktree at .worktrees/BRANCH: branch, secrets/FUTURE.md symlinks, direnv allow
just wt-list             # List worktrees
just wt-rm BRANCH        # Remove worktree, delete branch if merged
```

### VM workflow

```bash
just vm-build           # Build NixOS VM
just vm-run             # Start VM, wait for SSH
just vm-run --fresh     # Wipe state and restart
just vm-run --rebuild   # Rebuild and restart
just chat               # Connect to daemon via SSH socket forwarding
just ask "message"      # Send one message, print the reply, exit
just findings           # Show the review ledger (/findings)
just vm-ssh             # SSH into running VM
just vm-shell           # Shell as kitaebot daemon user (debugging)
just vm-logs            # Tail daemon, tinyproxy (refused CONNECTs), and kernel (egress drops) logs
just vm-logs-dump [n]   # Dump the last n log lines and exit (non-interactive)
just vm-journal [topic] # Show the bot's work journal (state/JOURNAL.md), optionally one topic
just vm-backup          # Stage durable state via `kitaebot backup`, tar to backups/ on the host
just vm-restore FILE    # Restore durable state from a vm-backup artifact
just vm-stop            # Shut down VM gracefully (pkill fallback)
```

## Configuration

Configuration is done through the NixOS module. The module serializes `kitaebot.settings` to `config.toml` via `pkgs.formats.toml` and symlinks it into the workspace. The daemon reads the TOML at startup; you never edit it by hand.

```nix
kitaebot = {
  package = kitaebot;                            # The kitaebot package (required)
  secretsDir = "/path/to/secrets";               # One file per credential
  logLevel = "kitaebot=debug";                   # RUST_LOG filter
  vm = {
    memorySize = 4096;
    cores = 4;
    diskSize = 20480;
  };  # QEMU resources (MB)

  tools = with pkgs; [                           # Extra packages on the exec tool's PATH
    curl                                         # (bash, coreutils, direnv, git, and nix
    findutils                                    #  are always present: the daemon spawns
    gnugrep                                      #  them itself)
    gnused
    which
  ];

  gitConfig = {                                  # Git identity via programs.git
    name = "kitaebot";
    email = "kitaebot@pm.me";
    signingKey = "D90B07BF61863EA1";             # Optional, enables GPG commit signing
  };

  settings = {                                   # Becomes config.toml
    agent = {
      max_iterations = 100;
    };
    sub_agents = {
      max_iterations = 30;                       # Tool-loop cap per sub-agent turn
    };
    usage = {
      rates = {                                  # Optional overrides, USD per 1M tokens (spec 27).
        "z-ai/glm-5.2" = {                       # On OpenRouter, /usage prices from the live per-endpoint
          input_per_mtok = 0.4046;               # rates automatically; an entry here overrides them.
          cache_read_per_mtok = 0.07514;
        };
      };
    };
    context = {
      engine = "flat";                           # flat | lcm
      max_tokens = 200000;
      budget_percent = 80;                       # Flat engine compaction trigger
      lcm = {                                    # LCM tuning, ignored when engine = "flat"
        fresh_tail_count = 32;                   # Newest N items never compacted
        leaf_chunk_tokens = 20000;               # Max tokens per summary chunk
        min_condensed_fanout = 2;                # Min children per condensed summary
        soft_budget_percent = 70;                # Background compaction starts
        hard_budget_percent = 90;                # Compaction blocks the actor
        large_file_threshold = 25000;            # Externalize message content above this
        large_file_summary_tokens = 400;         # Summary budget for externalized files
      };
    };
    git = {
      enabled = true;                            # Enables git tools (clone, commit, push)
      co_authors = [ "Name <email>" ];
      repositories = {                           # Listing a repo trusts its .envrc (direnv allow)
        "owner/repo" = {
          check = "just check; just warm";       # Check command; warms nix store + cargo target dir (spec 03)
          proposals = "github";                  # Discovery duties may file issues here (spec 24)
        };
        "owner/other" = { };                     # Trust-only entry (no check command, no proposals)
      };
    };
    github = {
      enabled = true;
      poll_interval_secs = 300;            # 5 minutes between PR polls
      owner = "amackillop";                # Required when enabled
      trusted_users = [];                  # Additional allowed users
      trusted_bots = [];                   # Bot apps whose PR feedback to act on
      issues = {
        enabled = true;                    # Poll assigned issues (work) + unassigned bot-authored/mentioning issues (discussion)
        poll_interval_secs = 300;
        plan_label = "needs-plan";         # Labeled issues get plan-first; unlabeled execute directly
      };
    };
    linear = {
      enabled = true;
      poll_interval_secs = 120;
      trusted_users = [ "user@example.com" ];    # Emails allowed to drive issues
      plan_label = "needs-plan";                 # Labeled issues get plan-first; unlabeled execute directly
    };
    mcp = {                                      # External MCP tool servers (spec 22)
      startup_timeout_secs = 30;                 # Spawn + handshake + tools/list budget
      call_timeout_secs = 60;
      servers = {
        bkb = {                                  # Tools register as bkb_*
          command = "bkb-mcp";                   # Resolved via PATH (add the package to `tools`)
          args = [ ];
          env = { };                             # Literal env vars
          env_credentials = {                    # Env var -> credential file in secretsDir
            BKB_API_KEY = "bkb-api-key";
          };
          tools = [ "search" "lookup_bip" ];     # Allowlist; unset registers all advertised tools
          explore = true;                        # Admit to read-only sub-agents (asserts no side effects)
        };
      };
    };
    review = {
      enabled = true;                            # Record reviewer findings in the review ledger (spec 23)
    };
    duties = {                                   # Duty scheduler (spec 24)
      distill = { every = "1h"; };                # Token gate still applies
      warm = { every = "24h"; };                  # Build-warm duty; warms repos whose HEAD moved (spec 24)
      self_analysis = {                           # Mine own errors/journal, propose fixes as issues
        every = "24h";
        repo = "owner/repo";                      # Must be trusted and proposal-enabled
      };
      prompt = [                                  # Operator-defined watch-tasks
        {
          name = "ci-watch";
          every = "6h";                           # or daily = "06:00" (UTC)
          repo = "owner/repo";                    # Must be listed in git.repositories
          gate = "new-commits";                   # Optional; needs github.enabled
          prompt = "Check CI on the default branch.";
        }
      ];
    };
    memory = {
      index_cap_bytes = 8192;                    # Byte cap on the injected MEMORY.md index
      distill_threshold_tokens = 40000;          # Undistilled tokens across all sessions before the distill duty runs
      distill_slice_tokens = 10000;              # Per-pass token budget; unset = threshold capped at 10k. Backlogs drain slice by slice
    };
    provider = {
      api = "openrouter";                        # openrouter | openai | groq | together | mistral
      model = "arcee-ai/trinity-large-preview:free";
      max_tokens = 32768;
      temperature = 0.7;                         # 0.0–2.0; optional, unset omits the parameter (endpoint default applies)
      reasoning.effort = "high";                 # Optional bound for the root model: effort or max_tokens (OpenRouter only)
      model_overrides = {                        # Per-role: model, reasoning bound, or both; unset parts fall back to provider
        explore.model = "cheap/model";
        worker.model = "mid/model";
        summarizer.model = "cheap/model";          # Fresh-path summaries only; live compaction rides the main model's cache (spec 14)
        reviewer.model = "strong/model";           # Judges the bot's own work and others' PRs
        planner.model = "strong/model";            # Plan turns think here; keep distinct from reviewer (the judge must not be the author)
        memory = {                                 # Distilled facts persist and inject every turn; don't skimp
          model = "strong/model";
          reasoning.effort = "low";                # Reasoning spirals starve content room; bound the thinking
        };
      };
    };
    socket = {
      path = "/run/kitaebot/chat.sock";
    };
    telegram = {
      enabled = true;
      chat_id = 123456789;
      poll_timeout_secs = 30;                    # getUpdates long-poll timeout
    };
    tools = {
      disabled = [ "web_search" ];               # Disable specific tools by name
      exec = {
        timeout_secs = 600;
        sandbox = "landlock";                    # landlock | bwrap | none (per-child confinement)
      };
      web_fetch = {
        timeout_secs = 30;
        max_response_bytes = 524288;
      };
      web_search = {
        model = "perplexity/sonar";
        max_tokens = 1024;
        timeout_secs = 30;
      };
    };
  };

  egressAllowlist = [                            # Hostnames kitaebot uid may CONNECT to
    "openrouter.ai"                              # via tinyproxy (all direct egress is
    "api.telegram.org"                           # dropped by nftables)
    "github.com"
    "api.github.com"
    "githubusercontent.com"
    "api.perplexity.ai"
  ];
};
```

All fields in `settings` have sane defaults; an empty attrset produces a valid config. Unknown fields are rejected at daemon startup.

### Secrets

Secrets are loaded via systemd `LoadCredential` from `kitaebot.secretsDir`. One file per credential, not environment variables.

| File | Required |
|------|----------|
| `provider-api-key` | Always |
| `telegram-bot-token` | When `telegram.enabled = true` |
| `github-token` | When `git.enabled = true` or `github.enabled = true` |
| `linear-api-key` | When `linear.enabled = true` |
| `gpg-signing-key` | When `gitConfig.signingKey` is set |
| `<name>` | Per `mcp.servers.*.env_credentials` value |

## Project layout

```
src/
├── main.rs              Entry point, subcommand routing
├── bin/kchat.rs          Socket client REPL
├── agent/               Agent actor module
│   ├── mod.rs           Core agent loop (process_message, run_turn)
│   ├── actor.rs         Agent struct, sequential envelope processing
│   ├── handle.rs        AgentHandle (cloneable actor interface)
│   ├── task.rs          task tool (explore/worker sub-agents)
│   └── envelope.rs      Envelope, ChannelSource types
├── clients/             HTTP client abstractions
│   ├── chat_completion.rs  OpenAI-compatible API
│   ├── github.rs           GitHub REST API
│   ├── telegram.rs         Telegram Bot API
│   ├── linear.rs           Linear GraphQL API
│   └── openrouter_pricing.rs  Live per-endpoint rates for /usage
├── context/             Context engines (ContextEngine trait)
│   ├── flat/            Per-name JSON sessions, whole-history compaction
│   ├── ephemeral.rs     In-memory engine for sub-agents (never compacts)
│   ├── stats.rs         Pure /stats report core (ContextEngine::report)
│   └── lcm/             Hierarchical compaction over SQLite (lcm_* tools)
├── provider/            LLM abstraction (completions, wire format, mock)
├── tools/               Tool trait + implementations
│   ├── exec.rs          Shell command (timeout, deny-list, env scrubbing)
│   ├── file_*.rs        File read/write/edit with PathGuard
│   ├── glob_search.rs   File pattern matching
│   ├── grep.rs          Content search (ripgrep backend)
│   ├── git/             Clone, commit, push, fixup, rebase, URL validation
│   ├── github/          PR ops, reviews, CI status, REST escape hatch
│   ├── linear.rs        linear_set_state
│   ├── mcp.rs           MCP client (JSON-RPC over stdio) + dynamic tool registration
│   ├── network/         web_fetch, web_search (Perplexity)
│   ├── bwrap.rs         Bubblewrap per-child view for exec (sandbox = "bwrap")
│   ├── warm.rs          Build-cache warmer (spec 03)
│   ├── cli_runner.rs    Subprocess boundary for git
│   ├── direnv.rs        Dev environment cache
│   └── path.rs          PathGuard (traversal rejection)
├── memory/              Memory subsystem (spec 21): MEMORY.md index + distillation
├── sandbox.rs           Landlock policy
├── confine.rs           Hidden subcommand: per-child Landlock tier, then exec
├── safety.rs            Leak detection
├── secrets.rs           systemd credential loading
├── config.rs            TOML config with validation
├── channel/             Channels the agent talks through (spec 10)
│   ├── socket.rs        Unix socket NDJSON channel
│   ├── telegram.rs      Telegram Bot API channel
│   ├── github/          GitHub channels: prs.rs, issues.rs (+ review_checkout)
│   ├── linear.rs        Linear issue polling channel
│   └── execution_checkout.rs  Fresh-base checkout prep shared by ticket channels
├── notify.rs            notify tool + Telegram push batching
├── daemon.rs            Event loop (select over enabled channels)
├── dispatch.rs          Input classification and Reply type
├── commands.rs          Slash commands (/new, /project, /context, /compact, /duties, /duty, /distill, /stats, /usage, /findings)
├── usage.rs             Usage ledger: per-task cost, turns, wall time (spec 27)
├── review.rs            Review ledger: verdicts and findings (spec 23)
├── state_db.rs          Operational DB handle (ledgers, cursor docs)
├── state_db/migrations  Numbered SQL migrations for the state DB
├── sqlite.rs            Shared migration ladder mechanics
├── duty/                Duty scheduler (schedule, state, self-analysis)
├── errlog.rs            Error tee: WARN/ERROR events as JSON lines under state/errors/
├── backup.rs            Durable-state staging for `kitaebot backup` (spec 05)
├── conventions.rs       Worked repo's AGENTS.md appended to the system prompt
├── retry.rs             Generic async retry combinator
├── runtime.rs           Provider/tools/channels assembly
├── activity.rs          Structured turn events for observability
├── workspace.rs         Workspace init + system prompt assembly
├── time.rs              ISO 8601 timestamps (Hinnant algorithm)
├── types.rs             Domain types (Message, ToolCall, Response)
├── error.rs             Algebraic error types
└── prompts/             Compiled in via include_str!:
                         SOUL.md, AGENTS.md (root system prompt)
                         developer-workflow.md, plan-format.md (segments)
                         explore.md, worker.md, reviewer.md (sub-agents)
                         review-gates.md, review-protocol.md (segments)
                         distill.md
vm/
├── configuration.nix    NixOS module (systemd service, egress filter, hardening)
├── backup.sh            In-VM script: `kitaebot backup` + tar to stdout (fed over ssh)
├── restore.sh           In-VM script: untar a backup into the workspace
├── test-egress.nix      NixOS VM integration test for egress filter
├── test-backup.nix      NixOS VM integration test for backup/restore
├── test-flakes.nix      NixOS VM integration test for the daemon's nix toolchain
├── test-fixtures/       Test fixture data
└── prompts/             USER.md (operator file, provisioned)
nix/
├── bkb-mcp.nix          Bitcoin knowledge base MCP server package
└── lightpanda.nix       Headless browser package
deploy/
├── configuration.nix    Host-specific settings (SSH keys, secrets, tools)
└── flake.nix            Deployment flake
specs/                   Design specifications
```

## License

MIT

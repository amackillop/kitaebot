# Kitaebot

Autonomous programming agent in Rust. Runs in a NixOS VM with Landlock sandboxing, proxy-based egress filtering, credential isolation, and leak detection.

## Overview

Kitaebot is a long-running daemon that accepts messages via Telegram, Unix socket, GitHub PR comments, or Linear issues, routes them through an LLM agent loop with tool use, and persists conversation state through a pluggable context engine. A duty scheduler runs recurring work on its own schedule.

Two binaries:

| Binary | Purpose | Lifecycle |
|--------|---------|-----------|
| `kitaebot run` | Daemon (Telegram + socket + duties + GitHub + Linear) | systemd service |
| `kchat <socket>` | Socket client REPL | On-demand |

## Architecture

```
Channels (Telegram, Unix socket, GitHub PR, Linear, Duties)
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

- **flat** (default) — per-name JSON session files under `workspace/sessions/`; compacts by summarizing the whole history when the token budget is exceeded.
- **lcm** — hierarchical compaction over a SQLite DAG at `state/lcm.db`. Old messages collapse into summary nodes (background at a soft threshold, blocking at a hard threshold); the `lcm_*` tools let the agent search and re-expand compacted history.

Sub-agents run on an ephemeral in-memory engine that never compacts.

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
| `git_push` | Push commits to a remote |
| `github_pr_create` | Create a pull request |
| `github_pr_list` | List pull requests |
| `github_pr_reviews` | Fetch PR reviews |
| `github_pr_diff_comments` | Fetch PR diff comments |
| `github_pr_diff_reply` | Reply to a PR diff comment |
| `github_ci_status` | Check CI status for a ref |
| `github_gh` | General-purpose `gh` CLI wrapper |
| `task` | Delegate to a sub-agent (`explore` read-only research, `worker` implementation) |
| `notify` | Push a message to the user via Telegram (batched by priority) |
| `lcm_grep` | Search compacted history (LCM engine) |
| `lcm_describe` | Inspect a compacted node (LCM engine) |
| `lcm_expand` | Re-expand compacted history (LCM engine, sub-agents only) |

Git and GitHub tools are gated on `git.enabled` and `github.enabled` respectively; `notify` on `telegram.enabled`; the `lcm_*` tools on `context.engine = "lcm"`. Tools can be individually disabled via `tools.disabled`.

All tool outputs pass through `safety::check_tool_output` and execute inside the Landlock sandbox.

### Security model

1. **Landlock sandbox** — Filesystem access restricted to workspace, `/nix/store` (ro), `/tmp`, `/etc` (ro), `/dev`. Applied at startup, inherited by child processes.
2. **Proxy-based egress filter** — nftables restricts the kitaebot uid to loopback; all outbound HTTP(S) goes through a local tinyproxy that allows CONNECT only to allowlisted hostnames. Prevents prompt-injection-driven exfiltration.
3. **Leak detection** — Regex scan on tool outputs before they enter the context window.
4. **Credential isolation** — Secrets loaded via systemd `LoadCredential` before Landlock enforcement. Inaccessible to child processes.
5. **Environment scrubbing** — `exec` runs with a safe allowlist of environment variables.
6. **Path confinement** — `PathGuard` rejects path traversal in file tools.
7. **systemd hardening** — `ProtectSystem=strict`, `ProtectHome`, `NoNewPrivileges`, empty `CapabilityBoundingSet`, `MemoryDenyWriteExecute`, syscall filter.

### Provider

Any OpenAI-compatible chat completions API. Supported endpoints:

- OpenRouter (default)
- OpenAI
- Groq
- Together
- Mistral

## Development

Requires [Nix](https://nixos.org/) with flakes enabled.

```bash
nix develop              # Enter dev shell
just check               # Full validation: nix flake check, nix lint/fmt, clippy, tests
just rust-check          # Fast inner loop: cargo fmt-check + clippy + tests (not the commit gate)
just build               # Compile
just test                # Run tests (mock-network feature)
just test-one NAME       # Run tests matching a name
just test-nixos          # Run all NixOS VM integration tests
just test-nixos-one NAME # Run a single NixOS VM test (e.g. egress)
just lint                # Clippy with --deny warnings
just fmt                 # Format Rust + Nix
just fix                 # Auto-fix clippy issues
```

### VM workflow

```bash
just vm-build           # Build NixOS VM
just vm-run             # Start VM, wait for SSH
just vm-run --fresh     # Wipe state and restart
just vm-run --rebuild   # Rebuild and restart
just chat               # Connect to daemon via SSH socket forwarding
just ask "message"      # Send one message, print the reply, exit
just vm-ssh             # SSH into running VM
just vm-shell           # Shell as kitaebot daemon user (debugging)
just vm-logs            # Tail daemon, tinyproxy (refused CONNECTs), and kernel (egress drops) logs
just vm-notifications   # Show the notification mirror (state/NOTIFICATIONS.md)
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

  tools = with pkgs; [                           # Packages on the exec tool's PATH
    coreutils
    curl
    findutils
    gh
    git
    gnugrep
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
        "owner/repo".check = "just check";       # The repo's check command; warms its build cache (spec 03)
        "owner/other" = { };                     # Trust-only entry (no check command)
      };
    };
    github = {
      enabled = true;
      poll_interval_secs = 300;            # 5 minutes between PR polls
      owner = "amackillop";                # Required when enabled
      trusted_users = [];                  # Additional allowed users
      trusted_bots = [];                   # Bot apps whose PR feedback to act on
    };
    linear = {
      enabled = true;
      poll_interval_secs = 120;
      trusted_users = [ "user@example.com" ];    # Emails allowed to drive issues
    };
    duties = {                                   # Duty scheduler (spec 24)
      distill = { every = "1h"; };                # Token gate still applies
      warm = { every = "24h"; };                  # Build-warm duty; runs only when some repo sets check
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
    };
    provider = {
      api = "openrouter";                        # openrouter | openai | groq | together | mistral
      model = "arcee-ai/trinity-large-preview:free";
      max_tokens = 32768;
      temperature = 0.7;                         # 0.0–2.0
      model_overrides = {                        # Per-role models, fall back to model
        explore = "cheap/model";
        worker = "mid/model";
        summarizer = "cheap/model";
        reviewer = "strong/model";                 # Judges the bot's own work and others' PRs
        memory = "strong/model";                   # Distilled facts persist and inject every turn; don't skimp
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
    "flakehub.com"
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
│   ├── telegram.rs         Telegram Bot API
│   └── linear.rs           Linear GraphQL API
├── context/             Context engines (ContextEngine trait)
│   ├── flat.rs          Per-name JSON sessions, whole-history compaction
│   ├── ephemeral.rs     In-memory engine for sub-agents (never compacts)
│   ├── stats.rs         Pure /stats report core (ContextEngine::report)
│   └── lcm/             Hierarchical compaction over SQLite (lcm_* tools)
├── provider/            LLM abstraction (completions, wire format, mock)
├── tools/               Tool trait + implementations
│   ├── exec.rs          Shell command (timeout, deny-list, env scrubbing)
│   ├── file_*.rs        File read/write/edit with PathGuard
│   ├── glob_search.rs   File pattern matching
│   ├── grep.rs          Content search (ripgrep backend)
│   ├── git/             Clone, commit, push, URL validation
│   ├── github/          PR ops, CI status, generic gh CLI
│   ├── network/         web_fetch, web_search (Perplexity)
│   ├── cli_runner.rs    Subprocess boundary for git/gh
│   ├── direnv.rs        Dev environment cache
│   └── path.rs          PathGuard (traversal rejection)
├── sandbox.rs           Landlock policy
├── safety.rs            Leak detection
├── secrets.rs           systemd credential loading
├── session.rs           Atomic JSON persistence
├── config.rs            TOML config with validation
├── channel/             Channels the agent talks through (spec 10)
│   ├── socket.rs        Unix socket NDJSON channel
│   ├── telegram.rs      Telegram Bot API channel
│   ├── github.rs        GitHub PR polling channel (+ review_checkout)
│   └── linear.rs        Linear issue polling channel (+ execution_checkout)
├── notify.rs            notify tool + Telegram push batching
├── daemon.rs            Event loop (select over enabled channels)
├── dispatch.rs          Input classification and Reply type
├── commands.rs          Slash commands (/new, /context, /compact, /duties, /distill, /stats)
├── duty/                Duty scheduler (mod, schedule, state)
├── runtime.rs           Provider/tools/channels assembly
├── activity.rs          Structured turn events for observability
├── workspace.rs         Workspace init + system prompt assembly
├── time.rs              ISO 8601 timestamps (Hinnant algorithm)
├── types.rs             Domain types (Message, ToolCall, Response)
├── error.rs             Algebraic error types
└── prompts/             Compiled in via include_str!:
                         SOUL.md, AGENTS.md (root system prompt)
                         explore.md, worker.md, reviewer.md (sub-agents)
                         review-gates.md, review-protocol.md (segments)
                         distill.md
vm/
├── configuration.nix    NixOS module (systemd service, egress filter, hardening)
├── test-egress.nix      NixOS VM integration test for egress filter
├── test-fixtures/       Test fixture data
└── prompts/             USER.md (operator file, provisioned)
nix/
└── lightpanda.nix       Headless browser package
deploy/
├── configuration.nix    Host-specific settings (SSH keys, secrets, tools)
└── flake.nix            Deployment flake
specs/                   Design specifications
```

## License

MIT

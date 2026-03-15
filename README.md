# Kitaebot

Autonomous programming agent in Rust. Runs in a NixOS VM with Landlock sandboxing, credential isolation, and leak detection.

## Overview

Kitaebot is a long-running daemon that accepts messages via Telegram, Unix socket, or GitHub PR comments, routes them through an LLM agent loop with tool use, and persists conversation state in a unified session. A periodic heartbeat triggers autonomous task review.

Two binaries:

| Binary | Purpose | Lifecycle |
|--------|---------|-----------|
| `kitaebot run` | Daemon (Telegram + socket + heartbeat + GitHub) | systemd service |
| `kchat <socket>` | Socket client REPL | On-demand |

## Architecture

```
Channels (Telegram, Unix socket, GitHub PR, Heartbeat)
        │
        ├─ Messages ──► AgentHandle ──► Agent actor (sequential)
        │                                 ├─ process_message ──► LLM loop
        │                                 └─ commands::execute ──► local ops
        │
        └─ Unified session (single session.json, messages tagged by source)
```

The agent is an actor (Ryhl pattern) — a spawned tokio task that processes one envelope at a time. Channels hold cloneable `AgentHandle`s and send messages via `send_message()`, awaiting a reply over a oneshot channel. This eliminates session locking: the actor owns the session and processes requests sequentially.

The agent loop calls the LLM, dispatches tool calls in parallel, checks outputs for leaked secrets, and repeats until the model produces a final response or hits `max_iterations`.

### Tools

Typed tools replace a generic shell. The LLM declares intent via parameters instead of reasoning about shell syntax.

| Tool | Description |
|------|-------------|
| `exec` | Run a shell command (timeout, output cap, env scrubbing) |
| `file_read` | Read a file |
| `file_write` | Write a file |
| `file_edit` | Patch a file |
| `glob_search` | Find files by pattern |
| `grep` | Search file contents (Ripgrep backend) |
| `web_fetch` | HTTP GET (timeout, response size limit) |
| `web_search` | LLM-powered web search (Perplexity) |
| `git_clone` | Clone a repository |
| `git_commit` | Commit staged changes |
| `git_push` | Push commits to a remote |
| `github_pr_create` | Create a pull request |
| `github_pr_list` | List pull requests |
| `github_pr_reviews` | Fetch PR reviews |
| `github_pr_comment` | Post a PR comment |
| `github_pr_diff_comments` | Fetch PR diff comments |
| `github_pr_diff_reply` | Reply to a PR diff comment |
| `github_ci_status` | Check CI status for a ref |
| `gh_cli` | General-purpose `gh` CLI wrapper |

All tool outputs pass through `safety::check_tool_output` and execute inside the Landlock sandbox.

### Security model

1. **Landlock sandbox** — Filesystem access restricted to workspace, `/nix/store` (ro), `/tmp`, `/etc` (ro), `/dev`. Applied at startup, inherited by child processes.
2. **Leak detection** — Regex scan on tool outputs before they enter the context window.
3. **Credential isolation** — Secrets loaded via systemd `LoadCredential` before Landlock enforcement. Inaccessible to child processes.
4. **Environment scrubbing** — `exec` runs with a safe allowlist of environment variables.
5. **Path confinement** — `PathGuard` rejects path traversal in file tools.

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
nix develop          # Enter dev shell
just check           # Full validation: nix flake check, clippy, fmt, tests
just build           # Compile
just test            # Run tests (mock-network feature)
just lint            # Clippy with --deny warnings
just fmt             # Format Rust + Nix
```

### VM workflow

```bash
just vm-build        # Build NixOS VM
just vm-run          # Start VM, wait for SSH
just vm-run --fresh  # Wipe state and restart
just chat            # Connect to daemon via SSH socket forwarding
just vm-ssh          # SSH into running VM
just vm-stop         # Kill VM
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
    context = {
      max_tokens = 200000;
      budget_percent = 80;
    };
    git = {
      co_authors = [ "Name <email>" ];
    };
    github = {
      enabled = true;
      poll_interval_secs = 300;            # 5 minutes between PR polls
    };
    heartbeat = {
      interval_secs = 1800;
    };
    provider = {
      api = "openrouter";                        # openrouter | openai | groq | together | mistral
      model = "arcee-ai/trinity-large-preview:free";
      max_tokens = 4096;
      temperature = 0.7;                         # 0.0–2.0
    };
    socket = {
      path = "/run/kitaebot/chat.sock";
    };
    telegram = {
      enabled = true;
      chat_id = 123456789;
    };
    tools = {
      exec = {
        timeout_secs = 60;
        max_output_bytes = 10240;
      };
      web_fetch = {
        timeout_secs = 30;
        max_response_bytes = 51200;
      };
      web_search = {
        model = "perplexity/sonar";
        max_tokens = 1024;
        timeout_secs = 30;
      };
    };
  };
};
```

All fields in `settings` have sane defaults; an empty attrset produces a valid config. Unknown fields are rejected at daemon startup.

### Secrets

Secrets are loaded via systemd `LoadCredential` from `kitaebot.secretsDir`. One file per credential, not environment variables.

| File | Required |
|------|----------|
| `provider-api-key` | Always |
| `telegram-bot-token` | When `telegram.enabled = true` |
| `github-token` | When `git.enabled` or `github.enabled` |
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
│   └── envelope.rs      Envelope, ChannelSource types
├── clients/             HTTP client abstractions
│   ├── chat_completion.rs  OpenRouter/OpenAI-compatible API
│   └── telegram.rs         Telegram Bot API
├── provider/            LLM abstraction (completions, mock)
├── tools/               Tool trait + implementations (exec, files, grep, web, git, github)
├── sandbox.rs           Landlock policy
├── safety.rs            Leak detection
├── secrets.rs           systemd credential loading
├── session.rs           Atomic JSON persistence
├── config.rs            TOML config with validation
├── context.rs           Token budget management
├── telegram.rs          Telegram Bot API channel
├── socket.rs            Unix socket NDJSON channel
├── github_channel.rs    GitHub PR polling channel
├── daemon.rs            Event loop (select over 4 channels)
├── dispatch.rs          Input classification and Reply type
├── commands.rs          Slash command dispatch
├── heartbeat.rs         Periodic heartbeat channel (timer + prepare/finish)
├── runtime.rs           Provider/tools/channels assembly
├── activity.rs          Structured turn events for observability
├── workspace.rs         Workspace init + system prompt assembly
├── time.rs              ISO 8601 timestamps (Hinnant algorithm)
├── stats.rs             Conversation statistics
├── types.rs             Domain types (Message, ToolCall, Response)
└── error.rs             Algebraic error types
vm/
├── configuration.nix    NixOS module (systemd service, options, hardening)
└── prompts/             SOUL.md and AGENTS.md templates
deploy/
├── configuration.nix    Host-specific settings (SSH keys, secrets, tools)
└── flake.nix            Deployment flake
specs/                   Design specifications (00–16)
```

## License

MIT

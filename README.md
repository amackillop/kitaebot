# Kitaebot

Autonomous programming agent in Rust. Runs in a NixOS VM with Landlock sandboxing, credential isolation, and leak detection.

## Overview

Kitaebot is a long-running daemon that accepts messages via Telegram or a Unix socket, routes them through an LLM agent loop with tool use, and persists conversation state per channel. A periodic heartbeat triggers autonomous task review.

Two binaries:

| Binary | Purpose | Lifecycle |
|--------|---------|-----------|
| `kitaebot run` | Daemon (Telegram + socket + heartbeat) | systemd service |
| `kchat <socket>` | Socket client REPL | On-demand |

## Architecture

```
Channels (Telegram, Unix socket)
        │
        ├─ Messages ──► agent::process_message ──► LLM loop with tool use
        │
        └─ Slash commands ──► commands::execute ──► local ops (clear, compact, stats)
```

The agent loop calls the LLM, dispatches tool calls, checks outputs for leaked secrets, and repeats until the model produces a final response or hits `max_iterations`.

### Tools

8 typed tools replace a generic shell. The LLM declares intent via parameters instead of reasoning about shell syntax.

| Tool | Description |
|------|-------------|
| `exec` | Run a shell command (timeout, output cap, deny patterns) |
| `file_read` | Read a file |
| `file_write` | Write a file |
| `file_edit` | Patch a file |
| `glob_search` | Find files by pattern |
| `grep` | Search file contents |
| `web_fetch` | HTTP GET |
| `web_search` | LLM-powered web search (Perplexity) |

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

`config.toml` in the workspace root. In the VM this is `/var/lib/kitaebot/config.toml`. Missing file produces defaults. Unknown fields are rejected.

```toml
[provider]
api = "openrouter"                          # openrouter | openai | groq | together | mistral
model = "arcee-ai/trinity-large-preview:free"
max_tokens = 4096
temperature = 0.7                           # 0.0–2.0

[agent]
max_iterations = 100

[tools.exec]
timeout_secs = 60
max_output_bytes = 10240

[tools.web_fetch]
timeout_secs = 30
max_response_bytes = 51200

[tools.web_search]
model = "perplexity/sonar"
max_tokens = 1024
timeout_secs = 30

[heartbeat]
interval_secs = 1800

[telegram]
enabled = false
chat_id = 0                                 # Required when enabled
poll_timeout_secs = 30

[socket]
path = "/run/kitaebot/chat.sock"

[context]
max_tokens = 200000
budget_percent = 80                         # Compaction trigger (1–100)
```

### Secrets

Secrets are loaded from the systemd credentials directory (`$CREDENTIALS_DIRECTORY`), not environment variables. Required files:

- `provider-api-key` (or matching provider name)
- `telegram-bot-token` (when Telegram is enabled)

## Project layout

```
src/
├── main.rs              Entry point, subcommand routing
├── bin/kchat.rs          Socket client REPL
├── agent.rs              Core agent loop
├── provider/             LLM abstraction (completions, mock)
├── tools/                Tool trait + implementations
├── sandbox.rs            Landlock policy
├── safety.rs             Leak detection
├── secrets.rs            systemd credential loading
├── session.rs            Atomic JSON persistence
├── config.rs             TOML config with validation
├── context.rs            Token budget management
├── telegram.rs           Telegram Bot API channel
├── socket.rs             Unix socket NDJSON channel
├── daemon.rs             Event loop (select over channels + heartbeat)
├── commands.rs           Slash command dispatch
├── workspace.rs          Workspace init + system prompt assembly
├── types.rs              Domain types (Message, ToolCall, Response)
└── error.rs              Algebraic error types
vm/
└── configuration.nix     NixOS module (systemd service, options, hardening)
deploy/
├── configuration.nix     Host-specific settings (SSH keys, secrets, tools)
├── flake.nix             Deployment flake
specs/                    Design specifications
```

## License

MIT

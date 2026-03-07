# CLI Interface

## Purpose

The `kitaebot` CLI is the user-facing entry point. A single binary with subcommands for the daemon and the local REPL.

## Why Single Binary?

1. **Simple deployment** — One artifact to build, ship, and manage
2. **Shared code** — Both modes use the same agent core, provider, and tools
3. **No IPC** — Daemon and REPL are independent processes, coordinated via file locks
4. **Minimal protocol** — The daemon exposes a Unix socket for the `kchat` client; no gRPC, no REST

## Subcommands

```
$ kitaebot <command>

Commands:
  run        Start the daemon (Telegram poller + socket listener + heartbeat timer)
  heartbeat  One-shot heartbeat cycle
```

Bare invocation prints usage and exits with code 1.

The `chat` subcommand (interactive REPL) has been removed. Interactive access is through `kchat` over the Unix socket channel, which provides identical functionality with activity event support.

## Run Mode (Daemon)

```
$ kitaebot run
```

Long-lived process that runs until signaled (SIGTERM/SIGINT). Spawns two async tasks on the tokio runtime:

1. **Telegram poller** — Long-polls `getUpdates`, processes messages, sends responses
2. **Socket listener** — Accepts connections on `/run/kitaebot/chat.sock`, NDJSON protocol
3. **Heartbeat timer** — Fires every 30 minutes, runs awareness check

All three loops share a single `TurnConfig` (provider, tools, iteration limit, context config) constructed once at startup and passed by reference.

### Daemon Lifecycle

- **Startup**: Initialize workspace, load config, create provider, register tools, spawn channel tasks
- **Running**: Both tasks run concurrently on the tokio runtime
- **Shutdown**: On SIGTERM/SIGINT, cancel tasks gracefully, release locks, exit 0

### Systemd Integration

The daemon runs as a systemd service (`Type=simple`). See [09-vm.md](09-vm.md) for unit file details.

## Global Startup

Runs before subcommand dispatch:

1. Initialize workspace (exit 1 on failure)
2. Load `config.toml` from workspace (exit 1 on malformed file; missing file → defaults)
3. Load secrets via `LoadCredential` (API key, optionally Telegram token)
4. Apply Landlock sandbox (warn and continue if unsupported)
5. Initialize provider + tools from config and secrets

## Error Behavior

- Workspace init failure: print message, exit 1
- Config load failure (malformed TOML, invalid values): print message, exit 1
- Provider init failure: print message, suggest setting env var, exit 1
- Session load failure: print message, exit 1
- Turn error: print to stderr, continue (REPL) or log and continue (daemon)
- Session save failure: print to stderr, continue

## Future Considerations

- **clap integration** — CLI args (`--model`, `--config`, `-v`)
- **More slash commands** — `/help`, `/history`, `/config`, `/soul`
- **Non-interactive mode** — `kitaebot chat "message"` or `echo "message" | kitaebot chat`
- **Exit codes** — Distinguish config errors (2) from provider errors (3)
- **Readline support** — History, completion, editing
- **Colors** — Syntax highlighting for code blocks
- **Progress indicators** — Spinner while waiting for response
- **Streaming output** — Print tokens as they arrive
- **Multiline input** — For pasting code blocks
- **Status subcommand** — `kitaebot status` to check daemon state, session info

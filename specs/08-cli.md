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
  run  Start daemon (heartbeat + channels)
```

Bare invocation prints usage and exits with code 1.

Interactive access is through `kchat` over the Unix socket channel. The `chat` and `heartbeat` subcommands have been removed — interactive access uses the socket, and heartbeat runs as a channel inside the daemon.

## Run Mode (Daemon)

```
$ kitaebot run
```

Long-lived process that runs until signaled (SIGTERM/SIGINT). Spawns an agent actor and four concurrent channel loops inside a `tokio::select!`:

1. **Heartbeat timer** — Sends `/heartbeat` through the agent handle on a configurable interval
2. **Telegram poller** — Long-polls `getUpdates`, sends messages through the agent handle
3. **GitHub PR poller** — Polls for new reviews/comments on the bot's open PRs
4. **Socket listener** — Accepts connections on `/run/kitaebot/chat.sock`, NDJSON protocol

All channels hold a clone of `AgentHandle` and communicate with the agent actor via `send_message()`. The actor owns the provider, tools, and unified session.

### Daemon Lifecycle

- **Startup**: Initialize workspace, load config, load secrets, apply sandbox, assemble runtime, spawn agent actor
- **Running**: Four channel loops run concurrently via `tokio::select!`
- **Shutdown**: On SIGTERM/SIGINT, clean up socket file and exit

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

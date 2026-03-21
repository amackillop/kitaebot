# Spec 08: Binaries

## Motivation

The CLI is the user-facing entry point. Two binaries: `kitaebot` runs the
daemon, `kchat` connects to it over a Unix socket for interactive use.

## Behavior

### Binaries

| Binary | Role | Lifecycle |
|--------|------|-----------|
| `kitaebot run` | Daemon: agent actor + four channel loops | Long-lived (systemd service) |
| `kchat <socket-path>` | Thin NDJSON client for the Unix socket | Interactive, on-demand |

### `kitaebot`

One subcommand: `run`. Bare invocation prints usage and exits with code 1.
Unknown subcommands exit with code 1. No `clap` — raw `std::env::args()`
parsing.

### Daemon Startup Sequence

1. Initialize tracing subscriber
2. `Workspace::init()` — resolve path, create directories. Fatal on failure.
3. `Config::load()` — load `config.toml` from workspace. Fatal on malformed
   file; missing file uses defaults.
4. `runtime::build()` — load secrets via `LoadCredential`, construct provider
   and tools. Fatal on missing required secrets. This happens **before**
   sandboxing so credential files are still accessible.
5. `sandbox::apply()` — enforce Landlock. Warn and continue if unsupported.
6. `daemon::run()` — spawn agent actor, enter channel loops.

### Daemon Runtime

Four concurrent channel loops inside a `tokio::select!`:

1. **Heartbeat timer** — sends `/heartbeat` through the agent handle
2. **Telegram poller** — long-polls `getUpdates` (if enabled)
3. **GitHub PR poller** — polls for new reviews/comments (if enabled)
4. **Socket listener** — accepts connections on the configured socket path

Disabled channels resolve to `std::future::pending()` — they park forever
without consuming resources.

All channels hold a clone of `AgentHandle`. The actor owns the provider, tools,
and unified session.

### Shutdown

On SIGTERM or SIGINT, the daemon cleans up the socket file and exits. Both
signals are handled via `tokio::signal`.

### `kchat`

A thin synchronous REPL (blocking `UnixStream`, not async). Connects to the
daemon's socket, reads a greeting, then loops:

- Read a line from stdin
- `/exit` exits locally (never sent to server)
- Everything else is sent as `{"content": "..."}` — no client-side command
  discrimination; the server's `Input::parse()` handles `/`-prefixed lines
- Activity messages are printed to stderr with a `~ ` prefix
- Responses and greetings are printed to stdout
- Errors are printed to stderr

See [spec 10](10-channels.md) for the full socket protocol.

## Boundaries

### Owns

- Argument parsing and subcommand dispatch
- Startup sequence orchestration (workspace, config, secrets, sandbox, runtime)
- Shutdown signal handling and socket cleanup
- `kchat` display formatting

### Does Not Own

- Agent loop — the agent module handles that
- Channel implementations — each channel module handles its own loop
- Socket protocol — the socket module handles NDJSON framing
- Config parsing — the config module handles that

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Workspace init failure | Print message, exit 1 |
| Config load failure (malformed TOML) | Print message, exit 1 |
| Secret load failure | Print message, exit 1 |
| Sandbox unsupported | Warn, continue without sandbox |
| Socket bind failure | Log warning, park the socket loop (daemon continues without socket) |
| Turn error (during daemon operation) | Log and continue |

## Constraints

- `kitaebot run` is designed for systemd (`Type=simple`)
- `kchat` requires exactly one positional argument (socket path)
- No `clap` or argument parsing library — intentionally minimal

## Open Questions

None currently.

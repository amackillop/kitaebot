# Spec 22: MCP Client

## Motivation

The agent's knowledge tools end at web search. SOUL.md tells it to
verify LDK and protocol assumptions against the Bitcoin Knowledge Base
(bkb), and the mdk repos' AGENTS.md files lean on the same source —
but no bkb tool exists, so that value is aspirational. The same gap
covers Grafana (dashboards, Loki, incidents). The operator's own
Claude Code harness has both as MCP servers, and the bot should match
it. (The harness also runs Linear's hosted MCP server; the bot skips
it — native Linear tools already cover ticket access, and the hosted
server's OAuth flow is browser-interactive, useless to a headless
daemon.)

MCP (Model Context Protocol) is the standard way to attach such
servers: a JSON-RPC session over a child process's stdio, where the
server advertises tools and the client calls them. One client
implementation buys every current and future server. This spec adds
that client to the tool layer: configured servers are spawned as
long-lived children, their advertised tools registered alongside the
built-ins, and their calls dispatched through the same pipeline
(activity events, output ceilings, safety scan) as every other tool.

Tools only. Resources, prompts, sampling, and roots are out of scope
until a concrete server makes them worth having.

## Behavior

### Configuration

Servers are declared in `config.toml`, one table per server under
`[mcp.servers.<name>]`:

| Key | Default | Description |
|-----|---------|-------------|
| `command` | required | Executable to spawn (resolved via `PATH`) |
| `args` | `[]` | Argument vector |
| `env` | `{}` | Extra environment variables (literals) |
| `env_credentials` | `{}` | Env var → credential name, loaded via `LoadCredential` ([spec 13](13-credentials.md)) |
| `tools` | unset | Allowlist of advertised tool names to register; unset = all |
| `explore` | `false` | Admit this server's tools to the explore sub-agent's set |

No `enabled` flag: an empty `[mcp.servers]` (or its absence) means no
MCP anywhere — no children, no tools, no cost.

The `tools` allowlist is the schema-size control. A server like
Grafana advertises dozens of tools; registering all of them floods
every request's tool definitions. The operator picks the handful that
earn their tokens.

### Lifecycle

- Each configured server is one long-lived child process, spawned at
  startup during tool construction — the first long-lived child in the
  system (every other tool spawns per call). Environment: the same
  scrubbed base as exec children (`SAFE_ENV_VARS`, which carries the
  egress proxy vars) plus the server's `env` and resolved
  `env_credentials`.
- Startup performs the MCP `initialize` handshake and one `tools/list`,
  then registers the advertised tools (filtered by the allowlist).
  A server that fails to spawn, handshake, or list within the startup
  timeout is logged and skipped; its tools are simply absent and the
  daemon runs on.
- The toolset is fixed at startup. `tools/list_changed` notifications
  are ignored; a server that changes its toolset needs a daemon
  restart to be seen.
- If a server process dies, the next call to one of its tools attempts
  one respawn + handshake; if that fails the call returns a tool error
  and the server is marked dead until the respawn backoff elapses.
  Respawns never re-run `tools/list` — the registered toolset stays
  the startup snapshot, so a respawned server that no longer serves a
  registered tool fails per-call like any other server error.
- Children are killed on daemon shutdown (kill-on-drop, same as exec).

### Tool registration

- Registered names are namespaced `<server>_<tool>` (e.g.
  `bkb_search`, `grafana_query_loki_logs`) so servers cannot collide
  with each other or shadow built-ins.
- A namespaced name that still collides with an existing tool is
  skipped with a warning; built-ins always win.
- Name, description, and input schema come from the server's
  `tools/list` response, passed through to the provider unchanged.
  The trust judgment sits at configuration time: an operator who lists
  a server vouches for its tool descriptions, the same way they vouch
  for a binary on `PATH`.

### Scope

MCP tools join the root and worker tool sets. The explore set is
read-only by design ([spec 19](19-sub-agents.md)); a server's tools
enter it only when its config sets `explore = true`, which is the
operator asserting the server has no side effects. bkb (pure knowledge
lookup) sets it; Grafana (can create incidents and annotations) does
not. The reviewer set ([spec 23](23-self-review.md)) takes the same
opt-in subset: it is read-only for the same reason explore is, and a
knowledge server is exactly what lets it verify claims instead of
guessing.

### Calls

- Dispatch sends `tools/call` with the model's arguments and waits up
  to `mcp.call_timeout_secs`. Timeout, transport error, and a response
  with `isError: true` all surface as ordinary tool errors — the turn
  continues, the model sees the message.
- Text content items are concatenated into the tool result. Non-text
  content (images, embedded resources) is replaced by a one-line
  placeholder naming what was omitted.
- Results ride the standard pipeline: `TOOL_OUTPUT_CEILING_BYTES`
  truncation in the tool layer, `safety::check_tool_output` leak scan
  ([spec 11](11-safety.md)), engine tool-output policy
  ([spec 14](14-context-engine.md)).
- Calls emit the standard `ToolStart`/`ToolEnd` activity events
  ([spec 16](16-activity.md)); no MCP-specific activity variants.
- Cancellation: the per-turn token aborts the wait. The child is not
  killed (it serves later calls); an eventual late response to a
  cancelled call is discarded.

### Provenance

MCP results are external data, exactly like web_fetch bodies: subject
to the instructions-in-data rule ([spec 11](11-safety.md),
[spec 21](21-memory.md)). Nothing new to enforce here — the existing
prompt guidance already covers "content fetched by tools".

## Boundaries

### Owns

- The MCP client: stdio transport, JSON-RPC framing, `initialize` /
  `tools/list` / `tools/call`
- Server child lifecycle: spawn, handshake, respawn backoff, shutdown
- Registration of advertised tools into the `Tools` registry,
  namespacing, allowlist filtering, collision policy
- The `[mcp]` config section and its validation

### Does Not Own

- Tool registry, dispatch, output truncation — [spec 03](03-tools.md)
- Leak scanning and output wrapping — [spec 11](11-safety.md)
- Secret material: `env_credentials` values load through the existing
  secrets mechanism — [spec 13](13-credentials.md)
- Which tools reach which agent type: it feeds the same
  root/worker/explore set construction that built-ins use —
  [spec 19](19-sub-agents.md)
- Network reachability of remote-backed servers: their egress goes
  through the same proxy allowlist as everything else —
  [spec 18](18-egress-filter.md)
- Packaging server binaries into the VM image — [spec 09](09-vm.md)

### Interactions

- **Runtime build** constructs the client per configured server before
  sandbox enforcement (credential files must still be readable), and
  extends `Tools` with the survivors.
- **Egress filter**: servers proxying to remote APIs (bkb →
  bitcoinknowledge.dev, mcp-grafana → cumulo.grafana.net) need their
  domains on the allowlist; the proxy env vars already flow through
  `SAFE_ENV_VARS`.
- **VM image**: each `command` must exist on the daemon's `PATH`,
  nix-packaged so spawn is hermetic — no launcher that downloads at
  startup (`pnpx`, `uvx`).

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Server binary missing / spawn fails | Warn at startup, server skipped, daemon runs |
| Handshake or `tools/list` times out | Warn, server skipped |
| `tools` allowlist names an unadvertised tool | Startup config error (fail fast, same as `tools.disabled` validation) |
| Registered name collides | Warn, MCP tool skipped, built-in wins |
| Server dies between calls | Next call respawns once; on failure, tool error + backoff |
| Call times out / transport error / `isError` result | Ordinary tool error, turn continues |
| Oversized result | Truncated at the tool-layer ceiling, then engine policy |
| Credential named in `env_credentials` missing | Startup config error (fail fast, consistent with other secrets) |

A misbehaving server degrades to "its tools error"; it can never take
the daemon down or stall a turn past the call timeout.

## Constraints

| Config key | Default | Description |
|------------|---------|-------------|
| `mcp.startup_timeout_secs` | 30 | Spawn + handshake + `tools/list` budget per server |
| `mcp.call_timeout_secs` | 60 | Per-call budget |
| `mcp.servers.<name>.*` | — | Per-server table (see Behavior) |

- Hand-rolled client, no MCP SDK dependency: the subset used here is
  `initialize`, `tools/list`, and `tools/call` over newline-delimited
  JSON-RPC on stdio — too small to justify a framework crate.
- Stdio transport only. Both target servers are local binaries; if a
  remote-only server ever matters, the plan is native streamable-HTTP
  transport (POST + bearer header from `LoadCredential`), not a shim
  child.
- Tools only: no resources, prompts, sampling, roots, or elicitation.
  Server-initiated requests are answered with method-not-found /
  rejected per protocol.
- Secrets never appear in `config.toml`; anything sensitive goes
  through `env_credentials`.
- Server children inherit the systemd unit's hardening; nothing may
  weaken the unit for a server's sake. (Node-based servers work
  because `MemoryDenyWriteExecute` is already off for V8.)

## Open Questions

- Lazy spawn (first call) instead of startup spawn would save idle
  children, but startup spawn gives fail-fast config feedback and the
  child count is tiny. Revisit only if server count grows.
- Streamable-HTTP transport: deferred until a remote-only server is
  actually wanted. Decided against shim children (`mcp-remote` via
  `pnpx`): non-hermetic spawn, node supervision, and an OAuth dance a
  headless daemon cannot complete.

Decided: hand-rolled client over an SDK crate (subset is three
methods); worker gets all MCP tools — it already holds exec, nothing
a server advertises is riskier than that; explore stays per-server
opt-in.

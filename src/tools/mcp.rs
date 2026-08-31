//! MCP client: JSON-RPC 2.0 over newline-delimited stdio.
//!
//! See `specs/22-mcp.md`. This module is the protocol core, generic
//! over the byte streams so unit tests run against in-memory duplex
//! pairs; process spawn and lifecycle live in the layer above. The
//! subset implemented is exactly what the spec scopes: `initialize`,
//! `tools/list`, and `tools/call`. Server-initiated requests are
//! answered with method-not-found; notifications are ignored.
//!
//! Above the protocol core sits the lifecycle layer: one long-lived
//! child per configured server, spawned at startup with the scrubbed
//! exec environment plus configured literals and credentials, its
//! advertised tools registered as namespaced [`Tool`]s. A dead server
//! respawns once on the call that discovers it, then backs off.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{error, warn};

use crate::config::McpConfig;
use crate::error::ToolError;
use crate::secrets::{Secret, load_secret};
use crate::types::is_valid_tool_name;

use super::{Tool, ToolCtx, Tools, safe_env};

/// Protocol version sent in `initialize`. We use no version-specific
/// features, so whatever the server negotiates back is accepted.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// JSON-RPC "method not found", sent for any server-initiated request.
const METHOD_NOT_FOUND: i64 = -32601;

/// One tool advertised by a server's `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AdvertisedTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Client-side MCP failure. `Call` is a well-formed `isError` result —
/// the tool ran and reported failure; everything else is transport or
/// protocol trouble.
#[derive(Debug, thiserror::Error)]
pub(crate) enum McpError {
    /// Stream closed or I/O failed.
    #[error("transport: {0}")]
    Transport(String),
    /// The server answered with a JSON-RPC error object.
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    /// The server sent something outside the protocol shape.
    #[error("protocol: {0}")]
    Protocol(String),
    /// `tools/call` returned `isError: true` with this rendered content.
    #[error("tool error: {0}")]
    Call(String),
}

/// A JSON-RPC connection over a byte-stream pair.
///
/// One in-flight request at a time by construction (`&mut self`);
/// concurrent tool calls against the same server serialize in the
/// layer above.
pub(crate) struct Connection<R, W> {
    reader: Lines<BufReader<R>>,
    writer: W,
    next_id: u64,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Connection<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader).lines(),
            writer,
            next_id: 0,
        }
    }

    /// Perform the `initialize` handshake: request, then the
    /// `notifications/initialized` notification.
    pub async fn initialize(&mut self) -> Result<(), McpError> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "kitaebot",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
        .await?;
        self.notify("notifications/initialized", json!({})).await
    }

    /// Fetch the advertised toolset, following pagination cursors.
    pub async fn list_tools(&mut self) -> Result<Vec<AdvertisedTool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            let advertised = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol("tools/list result lacks tools".into()))?;
            for entry in advertised {
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| McpError::Protocol("advertised tool lacks name".into()))?;
                tools.push(AdvertisedTool {
                    name: name.to_string(),
                    description: entry
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: entry.get("inputSchema").cloned().unwrap_or(json!({})),
                });
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }

    /// Call one tool. Text content items are concatenated; non-text
    /// items are replaced by a one-line placeholder naming what was
    /// omitted. An `isError` result becomes [`McpError::Call`].
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String, McpError> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let rendered = render_content(&result);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(McpError::Call(rendered));
        }
        Ok(rendered)
    }

    /// Send a request and await its response, servicing whatever else
    /// arrives in the meantime: notifications are skipped,
    /// server-initiated requests are answered with method-not-found,
    /// and stale responses (a lower id, e.g. from a cancelled call)
    /// are discarded.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        loop {
            let line = self
                .reader
                .next_line()
                .await
                .map_err(|e| McpError::Transport(e.to_string()))?
                .ok_or_else(|| McpError::Transport("stream closed".into()))?;
            let message: Value = serde_json::from_str(&line)
                .map_err(|e| McpError::Protocol(format!("bad json: {e}")))?;

            if message.get("method").is_some() {
                // Server-initiated request: refuse per protocol. A
                // method without an id is a notification, ignored by
                // design (spec 22).
                if let Some(request_id) = message.get("id") {
                    self.send(&json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {
                            "code": METHOD_NOT_FOUND,
                            "message": "kitaebot serves no requests",
                        },
                    }))
                    .await?;
                }
                continue;
            }

            match message.get("id").and_then(Value::as_u64) {
                Some(got) if got == id => {
                    if let Some(error) = message.get("error") {
                        return Err(McpError::Rpc {
                            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                            message: error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_string(),
                        });
                    }
                    return message
                        .get("result")
                        .cloned()
                        .ok_or_else(|| McpError::Protocol("response lacks result".into()));
                }
                // Stale response to an abandoned request: discard.
                Some(_) => {}
                None => return Err(McpError::Protocol("response lacks id".into())),
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn send(&mut self, message: &Value) -> Result<(), McpError> {
        let mut line = message.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(e.to_string()))
    }
}

/// How long a server that failed to respawn stays marked dead before
/// the next call may try again.
const RESPAWN_BACKOFF: Duration = Duration::from_mins(1);

/// Everything needed to (re)spawn one server child.
struct SpawnSpec {
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    /// Resolved `env_credentials`; exposed only at spawn.
    secret_env: Vec<(String, Secret)>,
}

type StdioConnection = Connection<ChildStdout, ChildStdin>;

/// A live child: the process handle (kill-on-drop) and its connection.
struct Live {
    _child: Child,
    conn: StdioConnection,
}

struct ServerState {
    live: Option<Live>,
    dead_until: Option<Instant>,
}

/// One configured server, shared by all of its registered tools.
/// Concurrent calls serialize on the state mutex.
pub(crate) struct McpServer {
    spec: SpawnSpec,
    startup_timeout: Duration,
    call_timeout: Duration,
    state: Mutex<ServerState>,
}

impl McpServer {
    /// Spawn the child and complete the `initialize` handshake.
    /// Respawns never re-run `tools/list` — the registered toolset
    /// stays the startup snapshot (spec 22).
    async fn spawn(spec: &SpawnSpec, budget: Duration) -> Result<Live, McpError> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .env_clear()
            .envs(safe_env())
            .envs(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .envs(
                spec.secret_env
                    .iter()
                    .map(|(k, s)| (k.clone(), s.expose().to_string())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Server stderr flows to the daemon's, i.e. the journal.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| McpError::Transport(format!("spawn {}: {e}", spec.command)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child stdout missing".into()))?;
        let mut conn = Connection::new(stdout, stdin);
        timeout(budget, conn.initialize())
            .await
            .map_err(|_| McpError::Transport("initialize timed out".into()))??;
        Ok(Live {
            _child: child,
            conn,
        })
    }

    /// Call one tool, respawning a dead server once. A transport
    /// failure mid-call drops the child, respawns, and retries the
    /// call once; a failed respawn marks the server dead for
    /// [`RESPAWN_BACKOFF`]. Rpc/call errors leave the connection
    /// alone — the server answered, it just said no.
    async fn call(&self, tool: &str, args: Value) -> Result<String, McpError> {
        let mut state = self.state.lock().await;
        let ServerState { live, dead_until } = &mut *state;
        let live = if let Some(live) = live {
            live
        } else {
            if let Some(until) = *dead_until
                && Instant::now() < until
            {
                return Err(McpError::Transport(
                    "server down, respawn backoff active".into(),
                ));
            }
            match Self::spawn(&self.spec, self.startup_timeout).await {
                Ok(spawned) => {
                    *dead_until = None;
                    live.insert(spawned)
                }
                Err(e) => {
                    *dead_until = Some(Instant::now() + RESPAWN_BACKOFF);
                    return Err(e);
                }
            }
        };
        match timeout(self.call_timeout, live.conn.call_tool(tool, args.clone())).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(McpError::Transport(first))) => {
                state.live = None;
                match Self::spawn(&self.spec, self.startup_timeout).await {
                    Ok(mut live) => {
                        let retried = timeout(self.call_timeout, live.conn.call_tool(tool, args))
                            .await
                            .map_err(|_| McpError::Transport("call timed out".into()))
                            .and_then(|r| r);
                        state.live = Some(live);
                        retried
                    }
                    Err(respawn) => {
                        state.dead_until = Some(Instant::now() + RESPAWN_BACKOFF);
                        Err(McpError::Transport(format!(
                            "{first}; respawn failed: {respawn}"
                        )))
                    }
                }
            }
            Ok(Err(e)) => Err(e),
            // Timed out: keep the connection — the server may just be
            // slow, and the stale-id discard absorbs a late response.
            Err(_) => Err(McpError::Transport("call timed out".into())),
        }
    }
}

/// One advertised tool, registered as `<server>_<tool>`.
///
/// Name and description leak: [`Tool`] hands out `&'static str`, the
/// set is a handful fixed at startup, and it lives as long as the
/// daemon anyway.
struct McpTool {
    server: Arc<McpServer>,
    remote_name: String,
    name: &'static str,
    description: &'static str,
    parameters: Value,
}

impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn execute(
        &self,
        args: Value,
        ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            // Cancellation abandons the wait, not the child: the
            // server keeps serving later calls, and its late response
            // is discarded by the stale-id check.
            let result = tokio::select! {
                () = ctx.cancel.cancelled() => {
                    return Err(ToolError::Cancelled);
                }
                result = self.server.call(&self.remote_name, args) => result,
            };
            result.map_err(|e| ToolError::Mcp {
                tool: self.name.to_string(),
                source: e,
            })
        })
    }
}

/// The registered MCP tools, split by admission scope: `all` feeds the
/// root and worker sets, `explore` (servers whose config asserts no
/// side effects) additionally feeds the read-only sets.
#[derive(Default)]
pub(crate) struct McpTools {
    pub all: Vec<Arc<dyn Tool>>,
    pub explore: Vec<Arc<dyn Tool>>,
}

impl McpTools {
    /// Drop any tool whose namespaced name still collides with an
    /// existing registration. Built-ins always win (spec 22).
    pub fn without_collisions(self, existing: &Tools) -> Self {
        let keep = |tool: &Arc<dyn Tool>| {
            if existing.contains(tool.name()) {
                warn!(
                    tool = tool.name(),
                    "MCP tool collides with a built-in; skipped"
                );
                false
            } else {
                true
            }
        };
        Self {
            all: self.all.into_iter().filter(keep).collect(),
            explore: self.explore.into_iter().filter(keep).collect(),
        }
    }
}

/// Spawn every configured server, handshake, list tools, and build
/// the registrations. A server that fails to spawn, handshake, or
/// list within the startup budget is logged and skipped, as is one
/// advertising a name the API's tool-name grammar rejects; those tools
/// are simply absent and the daemon runs on. Config errors — a missing
/// credential, an allowlist naming an unadvertised tool, or a server
/// key that is not a legal tool-name prefix — fail fast like every
/// other startup config error.
///
/// # Call exactly once
///
/// [`Tool::name`] and [`Tool::description`] return `&'static str`,
/// which is free for the built-ins (literals) and requires promoting a
/// runtime `String` for MCP, so each registration leaks its name and
/// description. That is sound only because this function runs once per
/// process, from `main`, and the registrations then live until exit:
/// bounded allocation with the program's lifetime, which is what
/// [`Box::leak`] is for. Nothing in the type system enforces the
/// once-ness. Calling this a second time — a config reload, a SIGHUP
/// handler, a retry after a failed startup — leaks a full set of names
/// and descriptions per call, unbounded over the daemon's life.
///
/// Respawning a dead server is not a second call: the call path in
/// [`McpServer::call`] replaces the child process and reuses the
/// existing registration, so it never re-lists or re-registers.
///
/// If a caller ever needs this more than once, give [`McpTool`] owned
/// `String` fields first; do not add a second call site against the
/// leak.
pub(crate) async fn start(config: &McpConfig) -> McpTools {
    let startup_budget = Duration::from_secs(config.startup_timeout_secs);
    let call_timeout = Duration::from_secs(config.call_timeout_secs);
    let mut tools = McpTools::default();

    for (name, server_config) in &config.servers {
        // The key prefixes every tool this server registers, so a key
        // the tool-name grammar rejects makes all of them
        // untransmittable. Operator error, so fail fast.
        if !is_valid_tool_name(name) {
            error!("mcp.servers.{name}: server name must match ^[a-zA-Z0-9_-]+$");
            std::process::exit(1);
        }
        let secret_env: Vec<(String, Secret)> = server_config
            .env_credentials
            .iter()
            .map(|(var, credential)| match load_secret(credential) {
                Ok(secret) => (var.clone(), secret),
                Err(e) => {
                    error!("mcp.servers.{name}: credential {credential}: {e}");
                    std::process::exit(1);
                }
            })
            .collect();
        let spec = SpawnSpec {
            command: server_config.command.clone(),
            args: server_config.args.clone(),
            env: server_config.env.clone().into_iter().collect(),
            secret_env,
        };

        // Spawn + handshake + list share one startup budget.
        let started = timeout(startup_budget, async {
            let mut live = McpServer::spawn(&spec, startup_budget).await?;
            let advertised = live.conn.list_tools().await?;
            Ok::<_, McpError>((live, advertised))
        })
        .await
        .map_err(|_| McpError::Transport("startup timed out".into()))
        .and_then(|r| r);
        let (live, advertised) = match started {
            Ok(pair) => pair,
            Err(e) => {
                warn!("mcp server {name} skipped: {e}");
                continue;
            }
        };

        // An allowlist naming an unadvertised tool is a config typo:
        // fail fast, same as tools.disabled validation.
        if let Some(allow) = &server_config.tools {
            for wanted in allow {
                if !advertised.iter().any(|t| &t.name == wanted) {
                    error!("mcp.servers.{name}.tools: server does not advertise \"{wanted}\"");
                    std::process::exit(1);
                }
            }
        }

        let server = Arc::new(McpServer {
            spec,
            startup_timeout: startup_budget,
            call_timeout,
            state: Mutex::new(ServerState {
                live: Some(live),
                dead_until: None,
            }),
        });
        for tool in advertised {
            if let Some(allow) = &server_config.tools
                && !allow.iter().any(|a| a == &tool.name)
            {
                continue;
            }
            // A server whose advertised names already carry the
            // "<server>_" prefix is already namespaced; re-prefixing
            // would register bkb_bkb_search.
            let registered_name = if tool.name.starts_with(&format!("{name}_")) {
                tool.name.clone()
            } else {
                format!("{name}_{}", tool.name)
            };
            // Registered names ship in tools[] on every request, so one
            // the API's grammar rejects would 400 every call the daemon
            // makes, not just this tool's. The server chose the name, so
            // drop the tool and keep running, same as a collision.
            if !is_valid_tool_name(&registered_name) {
                error!(
                    tool = registered_name,
                    server = name,
                    "MCP tool name must match ^[a-zA-Z0-9_-]+$; skipped"
                );
                continue;
            }
            let registered: Arc<dyn Tool> = Arc::new(McpTool {
                server: Arc::clone(&server),
                remote_name: tool.name.clone(),
                // Bounded, not a leak: `start` runs once per process and
                // these live until exit. See "Call exactly once" above.
                name: Box::leak(registered_name.into_boxed_str()),
                description: Box::leak(tool.description.into_boxed_str()),
                parameters: tool.input_schema,
            });
            tools.all.push(registered.clone());
            if server_config.explore {
                tools.explore.push(registered);
            }
        }
    }
    tools
}

/// Render a `tools/call` result's content items to one string.
fn render_content(result: &Value) -> String {
    let Some(items) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    items
        .iter()
        .map(|item| {
            let kind = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match kind {
                "text" => item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                other => format!("[non-text content omitted: {other}]"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, duplex};

    /// A scripted server end: reads one line per expected request,
    /// asserts the method, writes the scripted raw lines.
    struct Script {
        server_in: BufReader<tokio::io::ReadHalf<DuplexStream>>,
        server_out: tokio::io::WriteHalf<DuplexStream>,
    }

    type TestConnection =
        Connection<tokio::io::ReadHalf<DuplexStream>, tokio::io::WriteHalf<DuplexStream>>;

    fn pair() -> (TestConnection, Script) {
        let (client_stream, server_stream) = duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_stream);
        let (server_read, server_write) = tokio::io::split(server_stream);
        (
            Connection::new(client_read, client_write),
            Script {
                server_in: BufReader::new(server_read),
                server_out: server_write,
            },
        )
    }

    impl Script {
        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            self.server_in.read_line(&mut line).await.unwrap();
            serde_json::from_str(&line).unwrap()
        }

        async fn send_raw(&mut self, raw: &str) {
            self.server_out
                .write_all(format!("{raw}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn respond(&mut self, id: u64, result: Value) {
            self.send_raw(&json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
                .await;
        }
    }

    #[tokio::test]
    async fn initialize_handshakes_and_notifies() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            let init = script.recv().await;
            assert_eq!(init["method"], "initialize");
            assert_eq!(init["params"]["clientInfo"]["name"], "kitaebot");
            script
                .respond(1, json!({"protocolVersion": "2025-06-18"}))
                .await;
            let notified = script.recv().await;
            assert_eq!(notified["method"], "notifications/initialized");
            assert!(notified.get("id").is_none());
        });
        conn.initialize().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn list_tools_follows_pagination() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            let first = script.recv().await;
            assert_eq!(first["method"], "tools/list");
            script
                .respond(
                    1,
                    json!({
                        "tools": [{"name": "search", "description": "find things",
                                   "inputSchema": {"type": "object"}}],
                        "nextCursor": "page2",
                    }),
                )
                .await;
            let second = script.recv().await;
            assert_eq!(second["params"]["cursor"], "page2");
            script
                .respond(2, json!({"tools": [{"name": "timeline"}]}))
                .await;
        });
        let tools = conn.list_tools().await.unwrap();
        server.await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "find things");
        assert_eq!(tools[1].name, "timeline");
        assert_eq!(tools[1].description, "");
    }

    #[tokio::test]
    async fn call_tool_concatenates_text_and_placeholders() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            let call = script.recv().await;
            assert_eq!(call["method"], "tools/call");
            assert_eq!(call["params"]["name"], "search");
            assert_eq!(call["params"]["arguments"]["q"], "bolt12");
            script
                .respond(
                    1,
                    json!({"content": [
                        {"type": "text", "text": "first"},
                        {"type": "image", "data": "..."},
                        {"type": "text", "text": "second"},
                    ]}),
                )
                .await;
        });
        let out = conn
            .call_tool("search", json!({"q": "bolt12"}))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(out, "first\n[non-text content omitted: image]\nsecond");
    }

    #[tokio::test]
    async fn is_error_result_surfaces_as_call_error() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            script.recv().await;
            script
                .respond(
                    1,
                    json!({"isError": true,
                           "content": [{"type": "text", "text": "backend down"}]}),
                )
                .await;
        });
        let err = conn.call_tool("search", json!({})).await.unwrap_err();
        server.await.unwrap();
        assert!(matches!(err, McpError::Call(msg) if msg == "backend down"));
    }

    #[tokio::test]
    async fn rpc_error_surfaces() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            script.recv().await;
            script
                .send_raw(
                    &json!({"jsonrpc": "2.0", "id": 1,
                            "error": {"code": -32602, "message": "bad params"}})
                    .to_string(),
                )
                .await;
        });
        let err = conn.call_tool("search", json!({})).await.unwrap_err();
        server.await.unwrap();
        assert!(matches!(err, McpError::Rpc { code: -32602, .. }));
    }

    #[tokio::test]
    async fn notifications_and_stale_responses_are_skipped() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            script.recv().await;
            // A notification, then a stale response (id 0), then the real one.
            script
                .send_raw(
                    &json!({"jsonrpc": "2.0", "method": "notifications/progress"}).to_string(),
                )
                .await;
            script
                .send_raw(&json!({"jsonrpc": "2.0", "id": 0, "result": {}}).to_string())
                .await;
            script.respond(1, json!({"content": []})).await;
        });
        let out = conn.call_tool("x", json!({})).await.unwrap();
        server.await.unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn server_request_gets_method_not_found() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            script.recv().await;
            // Server-initiated request while the client is waiting.
            script
                .send_raw(
                    &json!({"jsonrpc": "2.0", "id": 99, "method": "sampling/createMessage"})
                        .to_string(),
                )
                .await;
            let refusal = script.recv().await;
            assert_eq!(refusal["id"], 99);
            assert_eq!(refusal["error"]["code"], METHOD_NOT_FOUND);
            script.respond(1, json!({"content": []})).await;
        });
        conn.call_tool("x", json!({})).await.unwrap();
        server.await.unwrap();
    }

    // --- lifecycle tests: scripted bash servers ---

    use std::collections::BTreeMap;
    use std::path::Path;

    use crate::config::McpServerConfig;
    use crate::tools::MockTool;

    /// A minimal MCP server in bash: answers initialize, tools/list
    /// (one tool, "echo"), and tools/call ("pong"), with correct ids.
    const FULL_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"v"}}\n' "$id" ;;
    *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echoes back","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *tools/call*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
  esac
done
"#;

    /// First life: serves startup then exits. Later lives (marker
    /// present): the full server. Exercises the respawn path.
    const DIES_AFTER_STARTUP: &str = r#"
if [ ! -f "$MARKER" ]; then
  touch "$MARKER"
  while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$line" in
      *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"v"}}\n' "$id" ;;
      *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{}}]}}\n' "$id"; exit 0 ;;
    esac
  done
fi
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"v"}}\n' "$id" ;;
    *tools/call*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"reborn"}]}}\n' "$id" ;;
  esac
done
"#;

    fn write_script(dir: &Path, body: &str) -> String {
        let path = dir.join("server.sh");
        std::fs::write(&path, body).unwrap();
        path.display().to_string()
    }

    fn server_config(script: &str, env: BTreeMap<String, String>) -> McpServerConfig {
        McpServerConfig {
            command: "bash".to_string(),
            args: vec![script.to_string()],
            env,
            env_credentials: BTreeMap::new(),
            tools: None,
            explore: false,
        }
    }

    fn mcp_config(name: &str, server: McpServerConfig) -> McpConfig {
        McpConfig {
            startup_timeout_secs: 10,
            call_timeout_secs: 10,
            servers: [(name.to_string(), server)].into(),
        }
    }

    #[tokio::test]
    async fn start_registers_namespaced_tools() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), FULL_SERVER);
        let mut server = server_config(&script, BTreeMap::new());
        server.explore = true;
        let tools = start(&mcp_config("test", server)).await;
        assert_eq!(tools.all.len(), 1);
        assert_eq!(tools.all[0].name(), "test_echo");
        assert_eq!(tools.all[0].description(), "echoes back");
        // explore = true admits the server's tools to the read-only sets.
        assert_eq!(tools.explore.len(), 1);
    }

    /// Advertises names that already carry the server prefix, the way
    /// bkb-mcp advertises `bkb_search`.
    const PRE_PREFIXED_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"v"}}\n' "$id" ;;
    *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"bkb_search","inputSchema":{}}]}}\n' "$id" ;;
  esac
done
"#;

    /// Advertises one legal name and one carrying a dot, the way
    /// servers that namespace with `group.tool` do.
    const UNTRANSMITTABLE_NAME_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"v"}}\n' "$id" ;;
    *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"search","inputSchema":{}},{"name":"docs.config","inputSchema":{}}]}}\n' "$id" ;;
  esac
done
"#;

    /// The whole daemon 400s if an illegal name reaches tools[], so the
    /// offending tool is dropped and the rest of the server survives.
    #[tokio::test]
    async fn tools_with_untransmittable_names_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), UNTRANSMITTABLE_NAME_SERVER);
        let tools = start(&mcp_config("test", server_config(&script, BTreeMap::new()))).await;
        assert_eq!(tools.all.len(), 1);
        assert_eq!(tools.all[0].name(), "test_search");
    }

    #[tokio::test]
    async fn already_prefixed_names_are_not_doubled() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), PRE_PREFIXED_SERVER);
        let tools = start(&mcp_config("bkb", server_config(&script, BTreeMap::new()))).await;
        assert_eq!(tools.all.len(), 1);
        assert_eq!(tools.all[0].name(), "bkb_search");
    }

    #[tokio::test]
    async fn non_explore_server_stays_out_of_readonly_sets() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), FULL_SERVER);
        let tools = start(&mcp_config("test", server_config(&script, BTreeMap::new()))).await;
        assert_eq!(tools.all.len(), 1);
        assert!(tools.explore.is_empty());
    }

    #[tokio::test]
    async fn missing_binary_skips_server() {
        let server = McpServerConfig {
            command: "definitely-not-a-real-binary".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_credentials: BTreeMap::new(),
            tools: None,
            explore: false,
        };
        let tools = start(&mcp_config("ghost", server)).await;
        assert!(tools.all.is_empty());
    }

    #[tokio::test]
    async fn call_flows_through_registered_tool() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), FULL_SERVER);
        let tools = start(&mcp_config("test", server_config(&script, BTreeMap::new()))).await;
        let result = tools.all[0]
            .execute(json!({"q": "ping"}), ToolCtx::default())
            .await
            .unwrap();
        assert_eq!(result, "pong");
    }

    #[tokio::test]
    async fn dead_server_respawns_once_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), DIES_AFTER_STARTUP);
        let marker = dir.path().join("marker");
        let env: BTreeMap<String, String> =
            [("MARKER".to_string(), marker.display().to_string())].into();
        let tools = start(&mcp_config("test", server_config(&script, env))).await;
        assert_eq!(tools.all.len(), 1);
        // The first life exited after startup; this call discovers the
        // dead pipe, respawns, and retries against the second life.
        let result = tools.all[0]
            .execute(json!({}), ToolCtx::default())
            .await
            .unwrap();
        assert_eq!(result, "reborn");
    }

    #[tokio::test]
    async fn allowlist_filters_registration() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), FULL_SERVER);
        let mut server = server_config(&script, BTreeMap::new());
        server.tools = Some(vec![]);
        let tools = start(&mcp_config("test", server)).await;
        assert!(tools.all.is_empty());
    }

    #[test]
    fn collisions_drop_the_mcp_tool() {
        let existing = Tools::new(vec![Arc::new(MockTool::new("out")) as _], &[]).unwrap();
        let shadowed: Arc<dyn Tool> = Arc::new(MockTool::new("shadowed"));
        let mcp = McpTools {
            all: vec![shadowed.clone()],
            explore: vec![shadowed],
        };
        // MockTool's registered name is "mock", which collides.
        let survivors = mcp.without_collisions(&existing);
        assert!(survivors.all.is_empty());
        assert!(survivors.explore.is_empty());
    }

    #[tokio::test]
    async fn closed_stream_is_transport_error() {
        let (mut conn, script) = pair();
        drop(script);
        let err = conn.call_tool("x", json!({})).await.unwrap_err();
        assert!(matches!(err, McpError::Transport(_)));
    }

    #[tokio::test]
    async fn garbage_line_is_protocol_error() {
        let (mut conn, mut script) = pair();
        let server = tokio::spawn(async move {
            script.recv().await;
            script.send_raw("not json at all").await;
        });
        let err = conn.call_tool("x", json!({})).await.unwrap_err();
        server.await.unwrap();
        assert!(matches!(err, McpError::Protocol(_)));
    }
}

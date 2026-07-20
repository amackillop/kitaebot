//! MCP client: JSON-RPC 2.0 over newline-delimited stdio.
//!
//! See `specs/22-mcp.md`. This module is the protocol core, generic
//! over the byte streams so unit tests run against in-memory duplex
//! pairs; process spawn and lifecycle live in the layer above. The
//! subset implemented is exactly what the spec scopes: `initialize`,
//! `tools/list`, and `tools/call`. Server-initiated requests are
//! answered with method-not-found; notifications are ignored.
// Consumed by the server-lifecycle commit; the allow leaves with it.
#![allow(dead_code)]

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};

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
#[derive(Debug)]
pub(crate) enum McpError {
    /// Stream closed or I/O failed.
    Transport(String),
    /// The server answered with a JSON-RPC error object.
    Rpc { code: i64, message: String },
    /// The server sent something outside the protocol shape.
    Protocol(String),
    /// `tools/call` returned `isError: true` with this rendered content.
    Call(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(msg) => write!(f, "transport: {msg}"),
            Self::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            Self::Protocol(msg) => write!(f, "protocol: {msg}"),
            Self::Call(msg) => write!(f, "tool error: {msg}"),
        }
    }
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

//! Unix domain socket channel.
//!
//! Listens on `/run/kitaebot/chat.sock` for NDJSON clients. Clients send
//! `{"content": "..."}` — the server parses slash commands from content.
//!
//! Single client at a time: while one client is connected, new
//! connections are accepted only to send an error and close them.
//!
//! Peers are gated by `SO_PEERCRED` against a uid allowlist; Landlock
//! does not mediate unix-socket connects, so this is the only check.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent::AgentHandle;
use crate::agent::envelope::ChannelSource;

// ── Protocol types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct ClientMsg {
    content: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    Activity { content: String },
    Error { content: String },
    Greeting { content: String },
    Response { content: String },
}

// ── Public entry point ──────────────────────────────────────────────

/// Listen for socket clients until cancelled.
///
/// If the socket directory does not exist (no `RuntimeDirectory`),
/// logs an info message and parks forever so the daemon can still
/// run without the socket channel.
///
/// Only peers whose uid is in `allowed_uids` are served; any other
/// peer (including the daemon's own same-uid children) gets an error
/// and is closed.
pub async fn listen(socket_path: &Path, handle: &AgentHandle, allowed_uids: &[u32]) -> ! {
    let path = socket_path;

    // Unlink stale socket left by a previous run.
    let _ = std::fs::remove_file(path);

    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("Socket directory missing, socket channel disabled");
            std::future::pending().await
        }
        Err(e) => {
            error!("Socket bind failed: {e}, socket channel disabled");
            std::future::pending().await
        }
    };

    info!("Socket channel listening on {}", path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => match stream.peer_cred() {
                Ok(cred) if allowed_uids.contains(&cred.uid()) => {
                    serve(&listener, stream, handle).await;
                }
                cred => {
                    warn!(?cred, "Socket peer outside uid allowlist, rejecting");
                    reject(stream, "peer uid not allowed").await;
                }
            },
            Err(e) => error!("Socket accept error: {e}"),
        }
    }
}

// ── Connection handling ─────────────────────────────────────────────

/// Serve a single client, rejecting concurrent connections.
async fn serve(listener: &UnixListener, stream: UnixStream, handle: &AgentHandle) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Greeting
    let greeting = handle.greeting().await;
    if send(&mut writer, &ServerMsg::Greeting { content: greeting })
        .await
        .is_err()
    {
        return;
    }

    // Message loop: read from client, reject new connections concurrently.
    // Activity frames are on by default so one-shot clients see turn
    // internals without a toggle round trip; /verbose turns them off.
    let mut verbose = true;
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
            result = listener.accept() => {
                if let Ok((stream, _)) = result {
                    reject(stream, "Another client is connected").await;
                }
                continue;
            }
        }

        // We have a complete line from the client. Parse and dispatch.
        let Some(input) = parse_line(&line, &mut writer, &mut verbose).await else {
            continue;
        };

        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();

        let result = {
            let reply_fut =
                handle.send_message(ChannelSource::Socket, input, None, Some(tx), cancel.clone());
            tokio::pin!(reply_fut);

            // Drain activity events while dispatch runs. Monitor the
            // client reader so we can cancel on disconnect.
            let mut disconnect_line = String::new();
            loop {
                tokio::select! {
                    biased;
                    Some(event) = rx.recv() => {
                        if verbose {
                            let _ = send(&mut writer, &ServerMsg::Activity { content: event.to_string() }).await;
                        }
                    }
                    result = reader.read_line(&mut disconnect_line) => {
                        match result {
                            Ok(0) | Err(_) => {
                                warn!("Client disconnected during dispatch, cancelling turn");
                                cancel.cancel();
                                // The reader is now permanently ready with
                                // EOF; selecting on it again would starve
                                // the reply future and spin. Just wait for
                                // the cancelled turn to finish.
                                break (&mut reply_fut).await;
                            }
                            Ok(_) => {
                                // Client sent another line mid-dispatch; ignore it.
                                disconnect_line.clear();
                            }
                        }
                    }
                    result = &mut reply_fut => break result,
                }
            }
        };

        // Drain remaining buffered events.
        while let Ok(event) = rx.try_recv() {
            if verbose {
                let _ = send(
                    &mut writer,
                    &ServerMsg::Activity {
                        content: event.to_string(),
                    },
                )
                .await;
            }
        }

        if cancel.is_cancelled() {
            // Client is gone. No point sending a response.
            info!("Turn cancelled by client disconnect");
            return;
        }

        let response = match result {
            Ok(reply) => ServerMsg::Response {
                content: reply.content,
            },
            Err(content) => ServerMsg::Error { content },
        };
        let _ = send(&mut writer, &response)
            .await
            .inspect_err(|e| debug!("Failed to send response: {e}"));
    }
}

/// Send an error to a client and close the connection.
async fn reject(stream: UnixStream, message: &str) {
    let (_, mut writer) = stream.into_split();
    let _ = send(
        &mut writer,
        &ServerMsg::Error {
            content: message.into(),
        },
    )
    .await
    .inspect_err(|e| debug!("Failed to send rejection: {e}"));
}

// ── Message parsing ─────────────────────────────────────────────────

/// Parse a client line and handle protocol-level concerns (`/verbose`, bad JSON).
///
/// Returns `Some(input)` if the line should be dispatched to the agent,
/// `None` if it was handled locally (error response, toggle, etc.).
async fn parse_line(line: &str, writer: &mut OwnedWriteHalf, verbose: &mut bool) -> Option<String> {
    let msg: ClientMsg = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => {
            let _ = send(
                writer,
                &ServerMsg::Error {
                    content: format!("Invalid JSON: {e}"),
                },
            )
            .await
            .inspect_err(|e| debug!("Failed to send error response: {e}"));
            return None;
        }
    };

    let input = msg.content.trim().to_string();

    // /verbose is UI state, not a slash command — intercept before dispatch.
    if input == "/verbose" {
        *verbose = !*verbose;
        let label = if *verbose { "on" } else { "off" };
        let _ = send(
            writer,
            &ServerMsg::Response {
                content: format!("Verbose: {label}"),
            },
        )
        .await;
        return None;
    }

    Some(input)
}

// ── Wire helpers ────────────────────────────────────────────────────

/// Serialize a server message as a single NDJSON line.
async fn send(writer: &mut OwnedWriteHalf, msg: &ServerMsg) -> Result<(), std::io::Error> {
    let mut buf = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    buf.push('\n');
    writer.write_all(buf.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::test_support::{TestAgent, workspace};
    use crate::tools::Tools;
    use crate::types::Response;
    use std::sync::Arc;
    use tokio::io::BufReader as TokioBufReader;
    use tokio::net::unix::OwnedWriteHalf as ClientWriteHalf;

    // ── Test harness ────────────────────────────────────────────────

    /// Typed NDJSON client for tests.
    struct TestClient {
        reader: TokioBufReader<tokio::net::unix::OwnedReadHalf>,
        writer: ClientWriteHalf,
        buf: String,
    }

    impl TestClient {
        /// Connect to a socket path, retrying until the listener is ready.
        async fn connect(path: &std::path::Path) -> Self {
            let stream = loop {
                match tokio::net::UnixStream::connect(path).await {
                    Ok(s) => break s,
                    Err(_) => tokio::task::yield_now().await,
                }
            };
            let (reader, writer) = stream.into_split();
            Self {
                reader: TokioBufReader::new(reader),
                writer,
                buf: String::new(),
            }
        }

        /// Read and deserialize the next NDJSON line.
        async fn recv(&mut self) -> ServerMsg {
            self.buf.clear();
            self.reader.read_line(&mut self.buf).await.unwrap();
            serde_json::from_str(&self.buf).unwrap()
        }

        /// Serialize and send a client message.
        async fn send(&mut self, content: &str) {
            let msg = ClientMsg {
                content: content.into(),
            };
            let mut line = serde_json::to_string(&msg).unwrap();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        /// Send a raw line (for malformed-input tests).
        async fn send_raw(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }
    }

    /// The effective uid, from /proc — no libc dep just for tests.
    fn euid() -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").expect("proc self").uid()
    }

    /// Spawn `listen` in the background and return a connected client.
    ///
    /// The returned `JoinHandle` and tempdirs must be held alive for the
    /// duration of the test.
    async fn spawn_listener(
        responses: Vec<Result<Response, crate::error::ProviderError>>,
    ) -> (
        TestClient,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir, // workspace dir
        tempfile::TempDir, // socket dir
    ) {
        spawn_listener_with_tools(responses, Tools::default()).await
    }

    async fn spawn_listener_with_tools(
        responses: Vec<Result<Response, crate::error::ProviderError>>,
        tools: Tools,
    ) -> (
        TestClient,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        spawn_listener_with_uids(responses, tools, &[euid()]).await
    }

    async fn spawn_listener_with_uids(
        responses: Vec<Result<Response, crate::error::ProviderError>>,
        tools: Tools,
        allowed_uids: &[u32],
    ) -> (
        TestClient,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let (ws_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(responses));
        let handle = TestAgent::new(ws, provider)
            .tools(tools)
            .max_iterations(5)
            .spawn();

        let sock_dir = tempfile::tempdir().unwrap();
        let sock_path = sock_dir.path().join("test.sock");

        let path = sock_path.clone();
        let uids = allowed_uids.to_vec();
        let join = tokio::spawn(async move {
            listen(&path, &handle, &uids).await;
        });

        let client = TestClient::connect(&sock_path).await;
        (client, join, ws_dir, sock_dir)
    }

    // ── Integration tests ───────────────────────────────────────────

    #[tokio::test]
    async fn greeting_then_message_roundtrip() {
        let (mut client, join, _ws, _sock) =
            spawn_listener(vec![Ok(Response::Text("pong".into()))]).await;

        assert!(matches!(client.recv().await, ServerMsg::Greeting { .. }));

        client.send("ping").await;

        match client.recv().await {
            ServerMsg::Response { content } => assert_eq!(content, "pong"),
            other => panic!("expected Response, got {other:?}"),
        }

        join.abort();
    }

    #[tokio::test]
    async fn second_client_is_rejected() {
        let (mut client, join, _ws, sock_dir) =
            spawn_listener(vec![Ok(Response::Text("ok".into())); 5]).await;

        client.recv().await; // greeting

        // Send a message so serve() enters the select! that rejects.
        client.send("hold").await;
        client.recv().await; // response

        let mut client2 = TestClient::connect(&sock_dir.path().join("test.sock")).await;
        assert!(matches!(client2.recv().await, ServerMsg::Error { .. }));

        drop(client);
        join.abort();
    }

    /// A tool that returns immediately; used to make a turn emit
    /// activity events.
    struct EchoTool;

    impl crate::tools::Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "returns immediately"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: crate::tools::ToolCtx,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<String, crate::error::ToolError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { Ok("echoed".to_string()) })
        }
    }

    #[tokio::test]
    async fn activity_forwarded_by_default() {
        use crate::types::{ToolCall, ToolFunction};

        let calls = Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "c1".to_string(),
                ToolFunction {
                    name: "echo".parse().unwrap(),
                    arguments: "{}".to_string(),
                },
            )],
        };
        let tools = Tools::new(vec![Arc::new(EchoTool)], &[]).unwrap();
        let (mut client, join, _ws, _sock) =
            spawn_listener_with_tools(vec![Ok(calls), Ok(Response::Text("done".into()))], tools)
                .await;

        client.recv().await; // greeting
        client.send("go").await;

        // Without sending /verbose, tool activity must stream before
        // the final response.
        let mut saw_activity = false;
        loop {
            match client.recv().await {
                ServerMsg::Activity { .. } => saw_activity = true,
                ServerMsg::Response { content } => {
                    assert_eq!(content, "done");
                    break;
                }
                other => panic!("expected Activity or Response, got {other:?}"),
            }
        }
        assert!(saw_activity, "no activity frames forwarded by default");

        join.abort();
    }

    /// A tool that never finishes; keeps a dispatch in flight until the
    /// turn is cancelled and the tool future is dropped.
    struct SlowTool;

    impl crate::tools::Tool for SlowTool {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn description(&self) -> &'static str {
            "hangs forever"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: crate::tools::ToolCtx,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<String, crate::error::ToolError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn disconnect_mid_dispatch_frees_the_slot() {
        use crate::types::{ToolCall, ToolFunction};

        let calls = Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "c1".to_string(),
                ToolFunction {
                    name: "slow".parse().unwrap(),
                    arguments: "{}".to_string(),
                },
            )],
        };
        let tools = Tools::new(vec![Arc::new(SlowTool)], &[]).unwrap();
        let (mut client, join, _ws, sock_dir) =
            spawn_listener_with_tools(vec![Ok(calls)], tools).await;

        client.recv().await; // greeting
        client.send("go").await;
        // Let the dispatch reach the hanging tool, then vanish.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(client);

        // serve() must cancel the turn and return to the accept loop.
        // Before the fix it spun on the EOF reader forever, so a new
        // client never got a greeting.
        let mut client2 = TestClient::connect(&sock_dir.path().join("test.sock")).await;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), client2.recv())
            .await
            .expect("serve() did not free the client slot after disconnect");
        assert!(matches!(msg, ServerMsg::Greeting { .. }));

        join.abort();
    }

    #[tokio::test]
    async fn peer_outside_allowlist_is_rejected() {
        // No uid is allowed, so the test's own peer is refused.
        let (mut client, join, _ws, _sock) =
            spawn_listener_with_uids(vec![], Tools::default(), &[]).await;

        match client.recv().await {
            ServerMsg::Error { content } => assert!(content.contains("not allowed")),
            other => panic!("expected Error, got {other:?}"),
        }
        // The rejection closes the connection.
        client.buf.clear();
        let n = client.reader.read_line(&mut client.buf).await.unwrap();
        assert_eq!(n, 0, "rejected peer must be disconnected");

        join.abort();
    }

    #[tokio::test]
    async fn invalid_json_returns_error() {
        let (mut client, join, _ws, _sock) = spawn_listener(vec![]).await;

        client.recv().await; // greeting

        client.send_raw("not json\n").await;

        match client.recv().await {
            ServerMsg::Error { content } => assert!(content.contains("Invalid JSON")),
            other => panic!("expected Error, got {other:?}"),
        }

        join.abort();
    }

    // ── Unit tests ──────────────────────────────────────────────────

    #[test]
    fn deserialize_message() {
        let json = r#"{"content":"hello"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.content, "hello");
    }

    #[test]
    fn deserialize_command() {
        let json = r#"{"content":"/new"}"#;
        let msg: ClientMsg = serde_json::from_str(json).unwrap();
        assert_eq!(msg.content, "/new");
    }

    #[test]
    fn serialize_greeting() {
        let msg = ServerMsg::Greeting {
            content: "hello".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"greeting""#));
        assert!(json.contains(r#""content":"hello""#));
    }

    #[test]
    fn serialize_response() {
        let msg = ServerMsg::Response {
            content: "line1\nline2".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        // Embedded newlines must be JSON-escaped, not literal.
        assert!(!json.contains('\n'));
        assert!(json.contains(r"\n"));
    }

    #[test]
    fn serialize_error() {
        let msg = ServerMsg::Error {
            content: "bad".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"error""#));
    }
}

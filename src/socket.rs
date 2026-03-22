//! Unix domain socket channel.
//!
//! Listens on `/run/kitaebot/chat.sock` for NDJSON clients. Clients send
//! `{"content": "..."}` — the server parses slash commands from content.
//!
//! Single client at a time: while one client is connected, new
//! connections are accepted only to send an error and close them.

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
use crate::commands;

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
pub async fn listen(socket_path: &Path, session_path: &Path, handle: &AgentHandle) -> ! {
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
            Ok((stream, _)) => {
                serve(&listener, stream, session_path, handle).await;
            }
            Err(e) => error!("Socket accept error: {e}"),
        }
    }
}

// ── Connection handling ─────────────────────────────────────────────

/// Serve a single client, rejecting concurrent connections.
async fn serve(
    listener: &UnixListener,
    stream: UnixStream,
    session_path: &Path,
    handle: &AgentHandle,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Greeting
    let greeting = commands::greeting(session_path);
    if send(&mut writer, &ServerMsg::Greeting { content: greeting })
        .await
        .is_err()
    {
        return;
    }

    // Message loop: read from client, reject new connections concurrently.
    let mut verbose = false;
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
                    reject(stream).await;
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
                handle.send_message(ChannelSource::Socket, input, Some(tx), cancel.clone());
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

/// Send an error to a second client and close the connection.
async fn reject(stream: UnixStream) {
    let (_, mut writer) = stream.into_split();
    let _ = send(
        &mut writer,
        &ServerMsg::Error {
            content: "Another client is connected".into(),
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
    let mut buf = serde_json::to_string(msg).expect("ServerMsg is always serializable");
    buf.push('\n');
    writer.write_all(buf.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ContextConfig;
    use crate::provider::MockProvider;
    use crate::tools::Tools;
    use crate::types::Response;
    use crate::workspace::Workspace;
    use std::sync::Arc;
    use tokio::io::BufReader as TokioBufReader;
    use tokio::net::unix::OwnedWriteHalf as ClientWriteHalf;

    const CTX: ContextConfig = ContextConfig {
        max_tokens: 200_000,
        budget_percent: 80,
    };

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
        let ws_dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(ws_dir.path().to_path_buf()).unwrap();
        let session_path = ws.session_path();

        let ws = Arc::new(ws);
        let provider = Arc::new(MockProvider::new(responses));
        let engine = crate::engine::flat::FlatSession::new(ws.session_path(), CTX).unwrap();
        let summarize = crate::engine::make_summarize_fn(provider.clone());
        let handle = AgentHandle::spawn(
            ws.clone(),
            provider,
            Arc::new(Tools::default()),
            1,
            engine,
            summarize,
        );

        let sock_dir = tempfile::tempdir().unwrap();
        let sock_path = sock_dir.path().join("test.sock");

        let path = sock_path.clone();
        let join = tokio::spawn(async move {
            listen(&path, &session_path, &handle).await;
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

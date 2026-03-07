//! Unix domain socket channel.
//!
//! Listens on `/run/kitaebot/chat.sock` for NDJSON clients. Clients send
//! `{"content": "..."}` — the server parses slash commands from content,
//! same as the REPL.
//!
//! Single client at a time: while one client is connected, new
//! connections are accepted only to send an error and close them.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info};

use crate::agent::TurnConfig;
use crate::commands;
use crate::provider::Provider;
use crate::workspace::Workspace;

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
pub async fn listen<P: Provider>(
    socket_path: &Path,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
) -> ! {
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
                serve(&listener, stream, workspace, config).await;
            }
            Err(e) => error!("Socket accept error: {e}"),
        }
    }
}

// ── Connection handling ─────────────────────────────────────────────

/// Serve a single client, rejecting concurrent connections.
async fn serve<P: Provider>(
    listener: &UnixListener,
    stream: UnixStream,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Greeting
    let greeting = commands::greeting(&workspace.socket_session_path());
    if send(&mut writer, &ServerMsg::Greeting { content: greeting })
        .await
        .is_err()
    {
        return;
    }

    // Message loop: read from client, reject new connections concurrently.
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        handle_line(&line, &mut writer, workspace, config).await;
                    }
                }
            }
            result = listener.accept() => {
                if let Ok((stream, _)) = result {
                    reject(stream).await;
                }
            }
        }
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

// ── Message dispatch ────────────────────────────────────────────────

async fn handle_line<P: Provider>(
    line: &str,
    writer: &mut OwnedWriteHalf,
    workspace: &Workspace,
    config: &TurnConfig<'_, P>,
) {
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
            return;
        }
    };

    let input = msg.content.trim();
    let session_path = workspace.socket_session_path();

    let result = commands::dispatch(input, &session_path, workspace, config).await;

    let response = match result {
        Ok(content) => ServerMsg::Response { content },
        Err(content) => ServerMsg::Error { content },
    };
    let _ = send(writer, &response)
        .await
        .inspect_err(|e| debug!("Failed to send response: {e}"));
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
    use crate::types::Response;
    use crate::workspace::Workspace;
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
        let provider = MockProvider::new(responses);
        let tools = crate::tools::Tools::default();

        let sock_dir = tempfile::tempdir().unwrap();
        let sock_path = sock_dir.path().join("test.sock");

        let path = sock_path.clone();
        let handle = tokio::spawn(async move {
            let config = TurnConfig {
                provider: &provider,
                tools: &tools,
                max_iterations: 1,
                context: &CTX,
            };
            listen(&path, &ws, &config).await;
        });

        let client = TestClient::connect(&sock_path).await;
        (client, handle, ws_dir, sock_dir)
    }

    // ── Integration tests ───────────────────────────────────────────

    #[tokio::test]
    async fn greeting_then_message_roundtrip() {
        let (mut client, handle, _ws, _sock) =
            spawn_listener(vec![Ok(Response::Text("pong".into()))]).await;

        assert!(matches!(client.recv().await, ServerMsg::Greeting { .. }));

        client.send("ping").await;

        match client.recv().await {
            ServerMsg::Response { content } => assert_eq!(content, "pong"),
            other => panic!("expected Response, got {other:?}"),
        }

        handle.abort();
    }

    #[tokio::test]
    async fn second_client_is_rejected() {
        let (mut client, handle, _ws, sock_dir) =
            spawn_listener(vec![Ok(Response::Text("ok".into())); 5]).await;

        client.recv().await; // greeting

        // Send a message so serve() enters the select! that rejects.
        client.send("hold").await;
        client.recv().await; // response

        let mut client2 = TestClient::connect(&sock_dir.path().join("test.sock")).await;
        assert!(matches!(client2.recv().await, ServerMsg::Error { .. }));

        drop(client);
        handle.abort();
    }

    #[tokio::test]
    async fn invalid_json_returns_error() {
        let (mut client, handle, _ws, _sock) = spawn_listener(vec![]).await;

        client.recv().await; // greeting

        client.send_raw("not json\n").await;

        match client.recv().await {
            ServerMsg::Error { content } => assert!(content.contains("Invalid JSON")),
            other => panic!("expected Error, got {other:?}"),
        }

        handle.abort();
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

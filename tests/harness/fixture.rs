//! Loopback fixture server standing in for external APIs.
//!
//! Serves scripted chat-completion responses matched on request
//! content, so tests stay deterministic when the daemon's loops run
//! concurrently. Unmatched requests get a 400 — a missing rule fails
//! the test loudly instead of feeding the agent canned filler.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;

/// A scripted response: consumed by the first request whose body
/// contains `substr`, in insertion order.
struct Rule {
    substr: String,
    response: serde_json::Value,
}

#[derive(Default)]
struct FixtureState {
    completion_rules: Vec<Rule>,
}

pub struct FixtureServer {
    addr: SocketAddr,
    state: Arc<Mutex<FixtureState>>,
}

impl FixtureServer {
    /// Bind on an ephemeral loopback port and serve until the test
    /// process exits. The server runs on its own thread so tests stay
    /// synchronous.
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(FixtureState::default()));
        let router_state = state.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build fixture runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("failed to bind fixture server");
                tx.send(listener.local_addr().expect("no local addr"))
                    .expect("fixture receiver dropped");
                let app = Router::new()
                    .route("/chat/completions", post(completions))
                    .with_state(router_state);
                axum::serve(listener, app)
                    .await
                    .expect("fixture server died");
            });
        });
        let addr = rx.recv().expect("fixture server failed to start");
        Self { addr, state }
    }

    /// The chat completions endpoint, for `provider.api`.
    pub fn completions_url(&self) -> String {
        format!("http://{}/chat/completions", self.addr)
    }

    /// Serve `response` for the first completion request whose body
    /// contains `substr`. One-shot; add one rule per expected turn.
    pub fn on_completion(&self, substr: &str, response: serde_json::Value) {
        self.state.lock().unwrap().completion_rules.push(Rule {
            substr: substr.to_string(),
            response,
        });
    }
}

async fn completions(State(state): State<Arc<Mutex<FixtureState>>>, body: String) -> Response {
    let mut state = state.lock().unwrap();
    let matched = state
        .completion_rules
        .iter()
        .position(|rule| body.contains(&rule.substr));
    match matched {
        Some(i) => {
            let rule = state.completion_rules.remove(i);
            (StatusCode::OK, axum::Json(rule.response)).into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            "fixture server: no completion rule matches this request",
        )
            .into_response(),
    }
}

/// A completion body holding a plain text reply.
pub fn text(content: &str) -> serde_json::Value {
    json!({"choices": [{"message": {"content": content}}]})
}

/// A completion body holding a single tool call.
pub fn tool_call(name: &str, arguments: &serde_json::Value) -> serde_json::Value {
    json!({"choices": [{"message": {"content": null, "tool_calls": [
        {"id": "call-1", "function": {"name": name, "arguments": arguments.to_string()}}
    ]}, "finish_reason": "tool_calls"}]})
}

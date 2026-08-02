//! Loopback fixture server standing in for external APIs.
//!
//! Serves scripted chat-completion responses matched on request
//! content, so tests stay deterministic when the daemon's loops run
//! concurrently. Unmatched requests get a 400 — a missing rule fails
//! the test loudly instead of feeding the agent canned filler.
//!
//! The Telegram routes model the Bot API: tests push updates into an
//! inbox served by `getUpdates` (held briefly when empty so the
//! daemon's poll loop doesn't spin), and every `sendMessage` body is
//! recorded for assertions.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;

/// A scripted response matched against request bodies in insertion
/// order. One-shot rules are consumed by their first match.
struct Rule {
    substr: String,
    response: serde_json::Value,
    once: bool,
}

#[derive(Default)]
struct FixtureState {
    completion_rules: Vec<Rule>,
    telegram_updates: Vec<serde_json::Value>,
    next_update_id: i64,
    telegram_sends: Vec<serde_json::Value>,
    linear_issues: Vec<serde_json::Value>,
    linear_comments: Vec<serde_json::Value>,
}

type SharedState = Arc<Mutex<FixtureState>>;

pub struct FixtureServer {
    addr: SocketAddr,
    state: SharedState,
}

impl FixtureServer {
    /// Bind on an ephemeral loopback port and serve until the test
    /// process exits. The server runs on its own thread so tests stay
    /// synchronous.
    pub fn start() -> Self {
        let state = SharedState::default();
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
                // Bot token is empty in mock-network builds, so the
                // Telegram client posts to /bot/<method>.
                let app = Router::new()
                    .route("/chat/completions", post(completions))
                    .route("/bot/getUpdates", post(get_updates))
                    .route("/bot/sendMessage", post(send_message))
                    .route("/graphql", post(linear_graphql))
                    .with_state(router_state);
                axum::serve(listener, app)
                    .await
                    .expect("fixture server died");
            });
        });
        let addr = rx.recv().expect("fixture server failed to start");
        Self { addr, state }
    }

    /// Base URL, for the `*.api_base` config keys.
    pub fn api_base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The chat completions endpoint, for `provider.api`.
    pub fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.api_base())
    }

    /// Serve `response` for the first completion request whose body
    /// contains `substr`. One-shot; add one rule per expected turn.
    pub fn on_completion(&self, substr: &str, response: serde_json::Value) {
        self.push_rule(substr, response, true);
    }

    /// Like [`FixtureServer::on_completion`], but the rule survives
    /// its matches. For flows where the poll cadence may legitimately
    /// dispatch the same input more than once.
    pub fn on_completion_always(&self, substr: &str, response: serde_json::Value) {
        self.push_rule(substr, response, false);
    }

    fn push_rule(&self, substr: &str, response: serde_json::Value, once: bool) {
        self.state.lock().unwrap().completion_rules.push(Rule {
            substr: substr.to_string(),
            response,
            once,
        });
    }

    /// Queue an incoming Telegram text message for `getUpdates`.
    pub fn push_telegram_update(&self, chat_id: i64, text: &str) {
        let mut state = self.state.lock().unwrap();
        state.next_update_id += 1;
        let id = state.next_update_id;
        state.telegram_updates.push(json!({
            "update_id": id,
            "message": {"message_id": id, "chat": {"id": chat_id}, "text": text},
        }));
    }

    /// All recorded `sendMessage` bodies so far.
    pub fn telegram_sends(&self) -> Vec<serde_json::Value> {
        self.state.lock().unwrap().telegram_sends.clone()
    }

    /// Replace the issue set served by the `assignedIssues` query.
    pub fn set_linear_issues(&self, issues: Vec<serde_json::Value>) {
        self.state.lock().unwrap().linear_issues = issues;
    }

    /// All recorded `commentCreate` variable sets so far.
    pub fn linear_comments(&self) -> Vec<serde_json::Value> {
        self.state.lock().unwrap().linear_comments.clone()
    }

    /// Block until a `commentCreate` whose body contains `substr`
    /// arrives, and return its variables. Panics after 10s.
    pub fn wait_for_linear_comment(&self, substr: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(comment) = self.linear_comments().into_iter().find(|comment| {
                comment["body"]
                    .as_str()
                    .is_some_and(|body| body.contains(substr))
            }) {
                return comment;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no linear comment containing {substr:?} within 10s; \
                 comments so far: {:?}",
                self.linear_comments(),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Block until a `sendMessage` body whose text contains `substr`
    /// arrives, and return it. Panics after 10s.
    pub fn wait_for_telegram_send(&self, substr: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(send) = self.telegram_sends().into_iter().find(|send| {
                send["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(substr))
            }) {
                return send;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no telegram send containing {substr:?} within 10s; \
                 sends so far: {:?}",
                self.telegram_sends(),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

async fn completions(State(state): State<SharedState>, body: String) -> Response {
    let mut state = state.lock().unwrap();
    let matched = state
        .completion_rules
        .iter()
        .position(|rule| body.contains(&rule.substr));
    match matched {
        Some(i) => {
            let response = if state.completion_rules[i].once {
                state.completion_rules.remove(i).response
            } else {
                state.completion_rules[i].response.clone()
            };
            (StatusCode::OK, axum::Json(response)).into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            "fixture server: no completion rule matches this request",
        )
            .into_response(),
    }
}

/// Long-poll semantics, scaled down: hold the request up to ~1s
/// waiting for an update at or past `offset`, then return whatever is
/// there. Updates stay queued until the client acks them by offset.
async fn get_updates(State(state): State<SharedState>, body: String) -> Response {
    let offset = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|req| req["offset"].as_i64())
        .unwrap_or(0);
    for _ in 0..40 {
        let pending: Vec<serde_json::Value> = {
            let state = state.lock().unwrap();
            state
                .telegram_updates
                .iter()
                .filter(|u| u["update_id"].as_i64().unwrap_or(0) >= offset)
                .cloned()
                .collect()
        };
        if !pending.is_empty() {
            return axum::Json(json!({"ok": true, "result": pending})).into_response();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    axum::Json(json!({"ok": true, "result": []})).into_response()
}

async fn send_message(State(state): State<SharedState>, body: String) -> Response {
    let Ok(send) = serde_json::from_str::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "sendMessage body is not JSON").into_response();
    };
    let mut state = state.lock().unwrap();
    state.telegram_sends.push(send);
    let id = state.telegram_sends.len();
    axum::Json(json!({
        "ok": true,
        "result": {"message_id": id, "chat": {"id": 0}, "text": null},
    }))
    .into_response()
}

/// Discriminate Linear GraphQL operations on the query text. Order
/// matters: the assignedIssues query mentions `viewer` too.
async fn linear_graphql(State(state): State<SharedState>, body: String) -> Response {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "graphql body is not JSON").into_response();
    };
    let query = request["query"].as_str().unwrap_or_default();
    if query.contains("commentCreate") {
        let mut state = state.lock().unwrap();
        state.linear_comments.push(request["variables"].clone());
        return axum::Json(json!({"data": {"commentCreate": {"success": true}}})).into_response();
    }
    if query.contains("assignedIssues") {
        let issues = state.lock().unwrap().linear_issues.clone();
        return axum::Json(json!({
            "data": {"viewer": {"assignedIssues": {"nodes": issues}}},
        }))
        .into_response();
    }
    if query.contains("viewer") {
        return axum::Json(json!({
            "data": {"viewer": {
                "id": "bot-1", "name": "Kitaebot", "email": "bot@example.com",
            }},
        }))
        .into_response();
    }
    (
        StatusCode::BAD_REQUEST,
        "fixture server: unrecognized graphql operation",
    )
        .into_response()
}

/// A Linear issue as served by `assignedIssues`.
pub fn linear_issue(
    identifier: &str,
    title: &str,
    repo: &str,
    comments: &[serde_json::Value],
) -> serde_json::Value {
    json!({
        "id": format!("uuid-{identifier}"),
        "identifier": identifier,
        "title": title,
        "description": "e2e fixture issue",
        "labels": {"nodes": [{"name": repo}]},
        "comments": {"nodes": comments},
    })
}

/// A Linear issue comment stamped one second in the future, so it is
/// strictly newer than any poll cursor the daemon has already saved.
pub fn linear_comment(email: &str, body: &str) -> serde_json::Value {
    let out = std::process::Command::new("date")
        .args(["-u", "-d", "+1 second", "+%Y-%m-%dT%H:%M:%S.000Z"])
        .output()
        .expect("failed to run date");
    let created_at = String::from_utf8(out.stdout).unwrap().trim().to_string();
    json!({
        "body": body,
        "createdAt": created_at,
        "user": {"id": format!("user-{email}"), "name": email, "email": email},
    })
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

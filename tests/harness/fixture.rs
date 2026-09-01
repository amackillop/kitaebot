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
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
    completion_requests: Vec<String>,
    telegram_updates: Vec<serde_json::Value>,
    next_update_id: i64,
    telegram_sends: Vec<serde_json::Value>,
    linear_issues: Vec<serde_json::Value>,
    linear_comments: Vec<serde_json::Value>,
    github_prs: Vec<serde_json::Value>,
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
                    .route("/user", get(gh_user))
                    .route("/search/issues", get(gh_search))
                    .route("/repos/{owner}/{repo}/pulls/{number}", get(gh_pull))
                    .route(
                        "/repos/{owner}/{repo}/pulls/{number}/{resource}",
                        get(gh_pull_resource),
                    )
                    .route(
                        "/repos/{owner}/{repo}/issues/{number}/comments",
                        get(gh_issue_comments),
                    )
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

    /// Replace the PR set served by the GitHub routes. Build entries
    /// with [`github_pr`].
    pub fn set_github_prs(&self, prs: Vec<serde_json::Value>) {
        self.state.lock().unwrap().github_prs = prs;
    }

    /// All completion request bodies received so far.
    pub fn completion_requests(&self) -> Vec<String> {
        self.state.lock().unwrap().completion_requests.clone()
    }

    /// Block until a completion request whose body contains `substr`
    /// arrives. Panics after 30s.
    pub fn wait_for_completion_request(&self, substr: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !self
            .completion_requests()
            .iter()
            .any(|body| body.contains(substr))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "no completion request containing {substr:?} within 30s",
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// All recorded `commentCreate` variable sets so far.
    pub fn linear_comments(&self) -> Vec<serde_json::Value> {
        self.state.lock().unwrap().linear_comments.clone()
    }

    /// Block until a `commentCreate` whose body contains `substr`
    /// arrives, and return its variables. Panics after 30s.
    pub fn wait_for_linear_comment(&self, substr: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
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
                "no linear comment containing {substr:?} within 30s; \
                 comments so far: {:?}",
                self.linear_comments(),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Block until a `sendMessage` body whose text contains `substr`
    /// arrives, and return it. Panics after 30s.
    pub fn wait_for_telegram_send(&self, substr: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
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
                "no telegram send containing {substr:?} within 30s; \
                 sends so far: {:?}",
                self.telegram_sends(),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

async fn completions(State(state): State<SharedState>, body: String) -> Response {
    let mut state = state.lock().unwrap();
    state.completion_requests.push(body.clone());
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
        let id = format!("created-{}", state.linear_comments.len());
        return axum::Json(json!({
            "data": {"commentCreate": {"success": true, "comment": {"id": id}}},
        }))
        .into_response();
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
    json!({
        "id": format!("comment-{email}:{body}"),
        "body": body,
        "createdAt": future_timestamp(1),
        "user": {"id": format!("user-{email}"), "name": email, "email": email},
    })
}

// ── GitHub routes ───────────────────────────────────────────────────
//
// PRs live in FixtureState as one object each, with sub-resources
// (reviews, comments, commits, files) embedded; the handlers slice
// out the piece each REST endpoint serves. The bot login is fixed
// to "kitaebot".

async fn gh_user() -> Response {
    axum::Json(json!({"login": "kitaebot"})).into_response()
}

/// Serve the search q= qualifiers: `commenter:`, `review-requested:`,
/// and `author:` select the PRs marked with the matching `search`
/// field. `commenter:` is checked first because the contributed query
/// also carries `-author:` and `-review-requested:` negations.
async fn gh_search(State(state): State<SharedState>, RawQuery(query): RawQuery) -> Response {
    let query = query.unwrap_or_default();
    let wanted = if query.contains("commenter:") {
        "contributed"
    } else if query.contains("review-requested:") {
        "review-requested"
    } else if query.contains("author:") {
        "own"
    } else {
        return (StatusCode::BAD_REQUEST, "unrecognized search query").into_response();
    };
    let items: Vec<serde_json::Value> = state
        .lock()
        .unwrap()
        .github_prs
        .iter()
        .filter(|pr| pr["search"] == wanted)
        .map(|pr| {
            json!({
                "number": pr["number"],
                "title": pr["title"],
                "body": pr["body"],
                "user": pr["user"],
                "repository_url":
                    format!("https://api.github.com/repos/{}", pr["nwo"].as_str().unwrap()),
                "updated_at": "2026-01-01T00:00:00Z",
            })
        })
        .collect();
    axum::Json(json!({"total_count": items.len(), "items": items})).into_response()
}

fn find_pr(
    state: &SharedState,
    owner: &str,
    repo: &str,
    number: &str,
) -> Option<serde_json::Value> {
    let nwo = format!("{owner}/{repo}");
    let number: u64 = number.parse().ok()?;
    state
        .lock()
        .unwrap()
        .github_prs
        .iter()
        .find(|pr| pr["nwo"] == nwo.as_str() && pr["number"] == number)
        .cloned()
}

async fn gh_pull(
    State(state): State<SharedState>,
    UrlPath((owner, repo, number)): UrlPath<(String, String, String)>,
) -> Response {
    let Some(pr) = find_pr(&state, &owner, &repo, &number) else {
        return (StatusCode::NOT_FOUND, "no such fixture PR").into_response();
    };
    axum::Json(json!({
        "state": pr["state"],
        "title": pr["title"],
        "head": {"sha": pr["head_sha"], "ref": "pr-branch"},
        "base": {"sha": "0".repeat(40), "ref": pr["base_ref"]},
    }))
    .into_response()
}

async fn gh_pull_resource(
    State(state): State<SharedState>,
    UrlPath((owner, repo, number, resource)): UrlPath<(String, String, String, String)>,
) -> Response {
    let Some(pr) = find_pr(&state, &owner, &repo, &number) else {
        return (StatusCode::NOT_FOUND, "no such fixture PR").into_response();
    };
    let field = match resource.as_str() {
        "comments" => "diff_comments",
        "commits" => "commits",
        "files" => "files",
        "reviews" => "reviews",
        _ => return (StatusCode::NOT_FOUND, "unknown pull sub-resource").into_response(),
    };
    // Failing one PR's reviews fetch exercises the skip-and-log path.
    if pr["fail_reviews"].as_bool().unwrap_or(false) && resource == "reviews" {
        return (StatusCode::NOT_FOUND, "fixture-induced failure").into_response();
    }
    axum::Json(pr[field].clone()).into_response()
}

async fn gh_issue_comments(
    State(state): State<SharedState>,
    UrlPath((owner, repo, number)): UrlPath<(String, String, String)>,
) -> Response {
    let Some(pr) = find_pr(&state, &owner, &repo, &number) else {
        return (StatusCode::NOT_FOUND, "no such fixture PR").into_response();
    };
    axum::Json(pr["issue_comments"].clone()).into_response()
}

/// A GitHub PR with empty sub-resources. Tests set `search` to `own`,
/// `review-requested`, or `contributed` to surface it in the search
/// passes, and fill `reviews`/`commits`/`files`/`diff_comments`/
/// `issue_comments`.
pub fn github_pr(nwo: &str, number: u32, author: &str, title: &str) -> serde_json::Value {
    json!({
        "nwo": nwo,
        "number": number,
        "title": title,
        "body": "e2e fixture PR",
        "state": "open",
        "user": {"login": author},
        "head_sha": "0".repeat(40),
        "base_ref": "main",
        "search": serde_json::Value::Null,
        "reviews": [],
        "issue_comments": [],
        "diff_comments": [],
        "commits": [],
        "files": [],
    })
}

/// A submitted PR review stamped in the future, so it stays strictly
/// newer than the poll cursor the daemon initializes at boot. The
/// margin matches the harness's boot deadline: reviews exist before
/// the daemon starts, and a boot crossing a wall-second would
/// otherwise leave the review at-or-before the cursor forever.
pub fn github_review(author: &str, state: &str, body: &str) -> serde_json::Value {
    json!({
        "id": 1,
        "user": {"login": author},
        "state": state,
        "body": body,
        "submitted_at": future_timestamp(5),
    })
}

/// An inline review comment linked to a review. `created_at` is
/// stamped in the *past* — GitHub stamps a pending-review comment at
/// draft time, so this is the shape of a draft-then-submit review's
/// comment: older than the cursor while its review is newer.
pub fn github_diff_comment(id: u64, author: &str, body: &str, review_id: u64) -> serde_json::Value {
    json!({
        "id": id,
        "pull_request_review_id": review_id,
        "path": "src/main.rs",
        "line": 42,
        "body": body,
        "user": {"login": author},
        "created_at": past_timestamp(60),
    })
}

/// ISO 8601 timestamp `seconds` behind the wall clock.
fn past_timestamp(seconds: u64) -> String {
    future_timestamp_with_offset(-i64::try_from(seconds).unwrap_or(i64::MAX))
}

/// ISO 8601 timestamp `seconds` ahead of the wall clock. The daemon's
/// poll cursors move at tick granularity; a same-second timestamp can
/// compare at-or-before the cursor and never dispatch.
fn future_timestamp(seconds: u64) -> String {
    future_timestamp_with_offset(i64::try_from(seconds).unwrap_or(i64::MAX))
}

fn future_timestamp_with_offset(seconds: i64) -> String {
    let out = std::process::Command::new("date")
        .args([
            "-u",
            "-d",
            &format!("{seconds:+} seconds"),
            "+%Y-%m-%dT%H:%M:%S.000Z",
        ])
        .output()
        .expect("failed to run date");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A PR conversation comment stamped in the future, for the same
/// reason as [`github_review`]: it exists before the daemon boots and
/// must stay strictly newer than the poll cursor.
pub fn github_issue_comment(author: &str, body: &str) -> serde_json::Value {
    json!({
        "id": 1,
        "user": {"login": author},
        "body": body,
        "created_at": future_timestamp(5),
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

//! Linear GraphQL API client.
//!
//! Pure response parsing lives in [`interpret_response`]. The IO layer is a
//! stored closure inside [`LinearClient`] — swap it for tests without
//! traits or generics. The base URL is injected so e2e tests can point at
//! a local fixture server; `mock-network` builds refuse non-loopback hosts.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::RawResponse;
use crate::error::LinearError;
use crate::secrets::Secret;

// ---------------------------------------------------------------------------
// Closure type alias
// ---------------------------------------------------------------------------

type PostResult = Result<RawResponse, LinearError>;
type PostFuture = Pin<Box<dyn Future<Output = PostResult> + Send>>;
type PostFn = Arc<dyn Fn(Vec<u8>) -> PostFuture + Send + Sync>;

// ---------------------------------------------------------------------------
// GraphQL operations
// ---------------------------------------------------------------------------

const VIEWER_QUERY: &str = "{ viewer { id name email } }";

const ASSIGNED_ISSUES_QUERY: &str = "\
{ viewer { assignedIssues(\
first: 50, \
filter: { state: { type: { nin: [\"completed\", \"canceled\"] } } }\
) { nodes { \
id identifier title description \
labels { nodes { name } } \
comments(first: 100) { nodes { id body createdAt user { id name email } } } \
} } } }";

const COMMENT_CREATE_MUTATION: &str = "\
mutation($issueId: String!, $body: String!) { \
commentCreate(input: { issueId: $issueId, body: $body }) { success comment { id } } }";

/// Resolve an issue (by UUID or human identifier) to its internal id
/// and its team's workflow states.
const ISSUE_STATES_QUERY: &str = "\
query($id: String!) { issue(id: $id) { \
id team { states(first: 50) { nodes { id name } } } } }";

const ISSUE_UPDATE_STATE_MUTATION: &str = "\
mutation($id: String!, $stateId: String!) { \
issueUpdate(id: $id, input: { stateId: $stateId }) { success } }";

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client for the Linear GraphQL API.
///
/// Concrete struct — no generics. The IO strategy is a closure injected at
/// construction time. `Clone` is free (`Arc`).
#[derive(Clone)]
pub struct LinearClient {
    post: PostFn,
}

impl LinearClient {
    pub fn new(api_key: Secret, api_base: &str) -> Self {
        #[cfg(feature = "mock-network")]
        super::assert_loopback(api_base);
        let endpoint = format!("{}/graphql", api_base.trim_end_matches('/'));
        let client = super::http_client(
            reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)),
        );
        Self {
            post: Arc::new(move |body| {
                let client = client.clone();
                let api_key = api_key.clone();
                let endpoint = endpoint.clone();
                Box::pin(async move {
                    // Personal API keys go bare — no `Bearer` prefix.
                    let resp = client
                        .post(&endpoint)
                        .header("Authorization", api_key.expose())
                        .header("Content-Type", "application/json")
                        .body(body)
                        .send()
                        .await
                        .map_err(|e| LinearError::Network(e.to_string()))?;
                    let status = resp.status().as_u16();
                    let bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| LinearError::Network(e.to_string()))?;
                    Ok(RawResponse {
                        status,
                        body: bytes.to_vec(),
                        retry_after_secs: None,
                    })
                })
            }),
        }
    }

    /// Test constructor — inject an arbitrary closure.
    #[cfg(test)]
    pub fn from_fn<F, Fut>(f: F) -> Self
    where
        F: Fn(Vec<u8>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = PostResult> + Send + 'static,
    {
        Self {
            post: Arc::new(move |body| Box::pin(f(body))),
        }
    }

    async fn request<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Option<serde_json::Value>,
    ) -> Result<T, LinearError> {
        let body = serde_json::to_vec(&GraphqlRequest { query, variables })
            .map_err(|e| LinearError::Network(e.to_string()))?;
        let raw = (self.post)(body).await?;
        interpret_response(&raw)
    }

    /// The user the API key belongs to.
    pub async fn viewer(&self) -> Result<Viewer, LinearError> {
        let data: ViewerData = self.request(VIEWER_QUERY, None).await?;
        Ok(data.viewer)
    }

    /// Open issues assigned to the viewer, with labels and comments.
    pub async fn assigned_issues(&self) -> Result<Vec<Issue>, LinearError> {
        let data: AssignedIssuesData = self.request(ASSIGNED_ISSUES_QUERY, None).await?;
        Ok(data.viewer.assigned_issues.nodes)
    }

    /// Post a comment on an issue; returns the created comment's id.
    pub async fn create_comment(&self, issue_id: &str, body: &str) -> Result<String, LinearError> {
        let variables = serde_json::json!({ "issueId": issue_id, "body": body });
        let data: CommentCreateData = self
            .request(COMMENT_CREATE_MUTATION, Some(variables))
            .await?;
        match (data.comment_create.success, data.comment_create.comment) {
            (true, Some(comment)) => Ok(comment.id),
            (true, None) => Err(LinearError::Api(
                "commentCreate succeeded without returning the comment id".into(),
            )),
            (false, _) => Err(LinearError::Api(
                "commentCreate returned success=false".into(),
            )),
        }
    }

    /// Move an issue to the workflow state named `state_name`
    /// (case-insensitive).
    ///
    /// A workflow that has no such state is not an error: the outcome
    /// reports the available state names so the caller can adapt. Only
    /// transport and API-level failures surface as [`LinearError`].
    pub async fn set_state(
        &self,
        issue: &str,
        state_name: &str,
    ) -> Result<SetStateOutcome, LinearError> {
        let variables = serde_json::json!({ "id": issue });
        let data: IssueStatesData = self.request(ISSUE_STATES_QUERY, Some(variables)).await?;
        let states = data.issue.team.states.nodes;

        let matched = states
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(state_name))
            .map(|s| (s.id.clone(), s.name.clone()));

        let Some((state_id, name)) = matched else {
            return Ok(SetStateOutcome::NoSuchState {
                available: states.into_iter().map(|s| s.name).collect(),
            });
        };

        let variables = serde_json::json!({ "id": data.issue.id, "stateId": state_id });
        let update: IssueUpdateData = self
            .request(ISSUE_UPDATE_STATE_MUTATION, Some(variables))
            .await?;
        if update.issue_update.success {
            Ok(SetStateOutcome::Moved { state: name })
        } else {
            Err(LinearError::Api(
                "issueUpdate returned success=false".into(),
            ))
        }
    }
}

/// Result of a [`LinearClient::set_state`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum SetStateOutcome {
    /// The issue was moved; carries the canonical state name.
    Moved { state: String },
    /// The workflow has no state by that name; carries the available
    /// names so the caller can pick a valid one or leave the state.
    NoSuchState { available: Vec<String> },
}

// ---------------------------------------------------------------------------
// Pure core
// ---------------------------------------------------------------------------

/// Parse a raw HTTP response into a GraphQL `data` payload.
///
/// Pure function — no IO, no async. Non-2xx statuses and GraphQL-level
/// errors are both surfaced as [`LinearError`].
pub fn interpret_response<T: DeserializeOwned>(raw: &RawResponse) -> Result<T, LinearError> {
    if !(200..=299).contains(&raw.status) {
        let body = String::from_utf8_lossy(&raw.body);
        return Err(LinearError::Network(format!("{}: {body}", raw.status)));
    }
    let resp: GraphqlResponse<T> =
        serde_json::from_slice(&raw.body).map_err(|e| LinearError::Deserialize(e.to_string()))?;
    if !resp.errors.is_empty() {
        let messages: Vec<String> = resp.errors.into_iter().map(|e| e.message).collect();
        return Err(LinearError::Api(messages.join("; ")));
    }
    resp.data
        .ok_or_else(|| LinearError::Api("no data in response".into()))
}

// ---------------------------------------------------------------------------
// Wire format types (Linear GraphQL API)
// ---------------------------------------------------------------------------

// Intentionally no `deny_unknown_fields` — the API grows over time.

#[derive(Debug, Serialize)]
struct GraphqlRequest<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    variables: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ViewerData {
    viewer: Viewer,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignedIssuesData {
    viewer: AssignedIssuesViewer,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignedIssuesViewer {
    assigned_issues: Nodes<Issue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentCreateData {
    comment_create: CommentCreatePayload,
}

#[derive(Debug, Deserialize)]
struct CommentCreatePayload {
    success: bool,
    comment: Option<CreatedComment>,
}

#[derive(Debug, Deserialize)]
struct CreatedComment {
    id: String,
}

#[derive(Debug, Deserialize)]
struct IssueStatesData {
    issue: IssueStates,
}

#[derive(Debug, Deserialize)]
struct IssueStates {
    id: String,
    team: TeamStates,
}

#[derive(Debug, Deserialize)]
struct TeamStates {
    states: Nodes<WorkflowState>,
}

#[derive(Clone, Debug, Deserialize)]
struct WorkflowState {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueUpdateData {
    issue_update: IssueUpdatePayload,
}

#[derive(Debug, Deserialize)]
struct IssueUpdatePayload {
    success: bool,
}

/// Generic GraphQL connection: `{ nodes: [...] }`.
#[derive(Clone, Debug, Deserialize)]
pub struct Nodes<T> {
    pub nodes: Vec<T>,
}

/// The user the API key belongs to.
#[derive(Clone, Debug, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub name: String,
    pub email: String,
}

/// An issue assigned to the viewer.
#[derive(Clone, Debug, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub labels: Nodes<Label>,
    pub comments: Nodes<Comment>,
}

/// An issue label.
#[derive(Clone, Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

/// A comment on an issue.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    pub body: String,
    /// RFC 3339 timestamp.
    pub created_at: String,
    /// Absent for comments from integrations.
    pub user: Option<CommentUser>,
}

/// The author of a comment.
#[derive(Clone, Debug, Deserialize)]
pub struct CommentUser {
    pub id: String,
    pub name: String,
    pub email: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn raw(status: u16, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.as_bytes().to_vec(),
            retry_after_secs: None,
        }
    }

    const VIEWER_JSON: &str =
        r#"{"data":{"viewer":{"id":"u1","name":"Kitaebot","email":"bot@example.com"}}}"#;

    const ISSUES_JSON: &str = r#"{"data":{"viewer":{"assignedIssues":{"nodes":[{
        "id":"i1","identifier":"MDK-123","title":"Fix login","description":"Broken",
        "labels":{"nodes":[{"name":"owner/repo"}]},
        "comments":{"nodes":[{"id":"c1","body":"looks good","createdAt":"2026-07-05T12:00:00.000Z",
        "user":{"id":"u2","name":"Alice","email":"alice@example.com"}}]}
    }]}}}}"#;

    #[test]
    fn interpret_viewer_success() {
        let data: ViewerData = interpret_response(&raw(200, VIEWER_JSON)).unwrap();
        assert_eq!(data.viewer.id, "u1");
        assert_eq!(data.viewer.email, "bot@example.com");
    }

    #[test]
    fn interpret_issues_success() {
        let data: AssignedIssuesData = interpret_response(&raw(200, ISSUES_JSON)).unwrap();
        let issues = &data.viewer.assigned_issues.nodes;
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "MDK-123");
        assert_eq!(issues[0].labels.nodes[0].name, "owner/repo");
        assert_eq!(
            issues[0].comments.nodes[0].user.as_ref().unwrap().email,
            "alice@example.com"
        );
    }

    #[test]
    fn interpret_null_comment_user() {
        let json = r#"{"data":{"viewer":{"assignedIssues":{"nodes":[{
            "id":"i1","identifier":"MDK-1","title":"T","description":null,
            "labels":{"nodes":[]},
            "comments":{"nodes":[{"id":"c1","body":"bot","createdAt":"2026-01-01T00:00:00.000Z","user":null}]}
        }]}}}}"#;
        let data: AssignedIssuesData = interpret_response(&raw(200, json)).unwrap();
        assert!(
            data.viewer.assigned_issues.nodes[0].comments.nodes[0]
                .user
                .is_none()
        );
    }

    #[test]
    fn interpret_graphql_errors() {
        let json = r#"{"data":null,"errors":[{"message":"nope"},{"message":"still nope"}]}"#;
        let err = interpret_response::<ViewerData>(&raw(200, json)).unwrap_err();
        assert!(matches!(err, LinearError::Api(m) if m == "nope; still nope"));
    }

    #[test]
    fn interpret_missing_data() {
        let json = r#"{"data":null}"#;
        let err = interpret_response::<ViewerData>(&raw(200, json)).unwrap_err();
        assert!(matches!(err, LinearError::Api(_)));
    }

    #[test]
    fn interpret_http_error() {
        let err = interpret_response::<ViewerData>(&raw(401, "unauthorized")).unwrap_err();
        assert!(matches!(err, LinearError::Network(m) if m.starts_with("401")));
    }

    #[test]
    fn interpret_malformed_json() {
        let err = interpret_response::<ViewerData>(&raw(200, "not json")).unwrap_err();
        assert!(matches!(err, LinearError::Deserialize(_)));
    }

    #[tokio::test]
    async fn client_viewer_roundtrip() {
        let client = LinearClient::from_fn(|_body| async {
            Ok(RawResponse {
                status: 200,
                body: VIEWER_JSON.as_bytes().to_vec(),
                retry_after_secs: None,
            })
        });
        let viewer = client.viewer().await.unwrap();
        assert_eq!(viewer.id, "u1");
    }

    #[tokio::test]
    async fn client_create_comment_success() {
        let client = LinearClient::from_fn(|body| async move {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(req["variables"]["issueId"], "i1");
            assert_eq!(req["variables"]["body"], "a plan");
            Ok(RawResponse {
                status: 200,
                body: br#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-9"}}}}"#
                    .to_vec(),
                retry_after_secs: None,
            })
        });
        let id = client.create_comment("i1", "a plan").await.unwrap();
        assert_eq!(id, "c-9");
    }

    #[tokio::test]
    async fn client_create_comment_success_without_id_is_api_error() {
        let client = LinearClient::from_fn(|_body| async {
            Ok(RawResponse {
                status: 200,
                body: br#"{"data":{"commentCreate":{"success":true}}}"#.to_vec(),
                retry_after_secs: None,
            })
        });
        let err = client.create_comment("i1", "a plan").await.unwrap_err();
        assert!(matches!(err, LinearError::Api(m) if m.contains("comment id")));
    }

    #[tokio::test]
    async fn client_create_comment_failure() {
        let client = LinearClient::from_fn(|_body| async {
            Ok(RawResponse {
                status: 200,
                body: br#"{"data":{"commentCreate":{"success":false}}}"#.to_vec(),
                retry_after_secs: None,
            })
        });
        let err = client.create_comment("i1", "a plan").await.unwrap_err();
        assert!(matches!(err, LinearError::Api(_)));
    }

    #[tokio::test]
    async fn client_propagates_closure_error() {
        let client =
            LinearClient::from_fn(|_body| async { Err(LinearError::Network("boom".into())) });
        let err = client.viewer().await.unwrap_err();
        assert!(matches!(err, LinearError::Network(_)));
    }

    const STATES_JSON: &str = r#"{"data":{"issue":{"id":"uuid-1","team":{"states":{"nodes":[
        {"id":"s-todo","name":"Todo"},
        {"id":"s-prog","name":"In Progress"}
    ]}}}}}"#;

    /// Fake client routing on the GraphQL op: the states query returns
    /// `STATES_JSON`; `issueUpdate` returns `update`; every request body
    /// is captured for assertions.
    fn state_client(
        update: &'static str,
        sent: Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> LinearClient {
        LinearClient::from_fn(move |body| {
            let sent = Arc::clone(&sent);
            async move {
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let is_update = req["query"].as_str().unwrap().contains("issueUpdate");
                sent.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(req);
                let payload = if is_update { update } else { STATES_JSON };
                Ok(RawResponse {
                    status: 200,
                    body: payload.as_bytes().to_vec(),
                    retry_after_secs: None,
                })
            }
        })
    }

    const UPDATE_OK: &str = r#"{"data":{"issueUpdate":{"success":true}}}"#;

    #[tokio::test]
    async fn set_state_moves_when_state_matches_case_insensitive() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let client = state_client(UPDATE_OK, sent.clone());

        let outcome = client.set_state("MDK-1", "in progress").await.unwrap();

        assert_eq!(
            outcome,
            SetStateOutcome::Moved {
                state: "In Progress".into()
            }
        );
        let sent = sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(sent.len(), 2);
        // The update targets the resolved UUID and the matched state id.
        assert_eq!(sent[1]["variables"]["id"], "uuid-1");
        assert_eq!(sent[1]["variables"]["stateId"], "s-prog");
    }

    #[tokio::test]
    async fn set_state_reports_available_without_updating() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let client = state_client(UPDATE_OK, sent.clone());

        let outcome = client.set_state("MDK-1", "Plan Review").await.unwrap();

        assert_eq!(
            outcome,
            SetStateOutcome::NoSuchState {
                available: vec!["Todo".into(), "In Progress".into()]
            }
        );
        // Only the lookup ran — no issueUpdate.
        assert_eq!(
            sent.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn set_state_update_failure_is_api_error() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let client = state_client(
            r#"{"data":{"issueUpdate":{"success":false}}}"#,
            sent.clone(),
        );

        let err = client.set_state("MDK-1", "Todo").await.unwrap_err();

        assert!(matches!(err, LinearError::Api(_)));
    }
}

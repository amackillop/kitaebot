//! GitHub REST API client.
//!
//! Pure response parsing lives in [`interpret_response`]. The IO layer is a
//! stored closure inside [`GithubClient`] — swap it for tests without
//! traits or generics. The base URL is injected so e2e tests can point at
//! a local fixture server; `mock-network` builds refuse non-loopback hosts.
//!
//! List endpoints fetch one page of 100; Link-header pagination lands
//! when a caller needs more than that.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::RawResponse;
use crate::error::GithubError;
use crate::secrets::Secret;

// ---------------------------------------------------------------------------
// Closure type alias
// ---------------------------------------------------------------------------

type RequestResult = Result<RawResponse, GithubError>;
type RequestFuture = Pin<Box<dyn Future<Output = RequestResult> + Send>>;
type RequestFn = Arc<dyn Fn(&'static str, String, Option<Vec<u8>>) -> RequestFuture + Send + Sync>;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HTTP client for the GitHub REST API.
///
/// Concrete struct — no generics. The IO strategy is a closure injected at
/// construction time. `Clone` is free (`Arc`).
#[derive(Clone)]
pub struct GithubClient {
    request: RequestFn,
}

impl GithubClient {
    pub fn new(token: Secret, api_base: &str) -> Self {
        #[cfg(feature = "mock-network")]
        super::assert_loopback(api_base);
        let base = api_base.trim_end_matches('/').to_string();
        let client =
            super::http_client(reqwest::Client::builder().timeout(Duration::from_secs(30)));
        Self {
            request: Arc::new(move |method, path, body| {
                let client = client.clone();
                let token = token.clone();
                let url = format!("{base}/{path}");
                Box::pin(async move {
                    let method = reqwest::Method::from_bytes(method.as_bytes())
                        .map_err(|e| GithubError::Network(e.to_string()))?;
                    let mut req = client
                        .request(method, &url)
                        .header("Authorization", format!("Bearer {}", token.expose()))
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .header("User-Agent", "kitaebot");
                    if let Some(body) = body {
                        req = req.header("Content-Type", "application/json").body(body);
                    }
                    let resp = req
                        .send()
                        .await
                        .map_err(|e| GithubError::Network(e.to_string()))?;
                    let status = resp.status().as_u16();
                    let bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| GithubError::Network(e.to_string()))?;
                    Ok(RawResponse {
                        status,
                        body: bytes.to_vec(),
                    })
                })
            }),
        }
    }

    /// Test constructor — inject an arbitrary closure.
    #[cfg(test)]
    pub fn from_fn<F, Fut>(f: F) -> Self
    where
        F: Fn(&'static str, String, Option<Vec<u8>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = RequestResult> + Send + 'static,
    {
        Self {
            request: Arc::new(move |method, path, body| Box::pin(f(method, path, body))),
        }
    }

    async fn get_json<T: DeserializeOwned>(&self, path: String) -> Result<T, GithubError> {
        let raw = (self.request)("GET", path, None).await?;
        interpret_response(&raw)
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        path: String,
        payload: &serde_json::Value,
    ) -> Result<T, GithubError> {
        let body = serde_json::to_vec(payload).map_err(|e| GithubError::Network(e.to_string()))?;
        let raw = (self.request)("POST", path, Some(body)).await?;
        interpret_response(&raw)
    }

    /// The user the token belongs to.
    pub async fn user(&self) -> Result<User, GithubError> {
        self.get_json("user".into()).await
    }

    /// Search issues and PRs. `query` uses search qualifiers with
    /// spaces, e.g. `is:pr is:open author:kitaebot`.
    pub async fn search_issues(&self, query: &str) -> Result<Vec<SearchIssue>, GithubError> {
        let q = query.replace(' ', "+");
        let results: SearchResults = self
            .get_json(format!("search/issues?q={q}&per_page=50"))
            .await?;
        Ok(results.items)
    }

    /// A pull request's head, base, state, and title.
    pub async fn pull(&self, nwo: &str, number: u32) -> Result<Pull, GithubError> {
        self.get_json(format!("repos/{nwo}/pulls/{number}")).await
    }

    /// Submitted reviews on a pull request.
    pub async fn pull_reviews(&self, nwo: &str, number: u32) -> Result<Vec<PrReview>, GithubError> {
        self.get_json(format!("repos/{nwo}/pulls/{number}/reviews?per_page=100"))
            .await
    }

    /// Commits on a pull request.
    pub async fn pull_commits(&self, nwo: &str, number: u32) -> Result<Vec<PrCommit>, GithubError> {
        self.get_json(format!("repos/{nwo}/pulls/{number}/commits?per_page=100"))
            .await
    }

    /// Changed files on a pull request.
    pub async fn pull_files(&self, nwo: &str, number: u32) -> Result<Vec<PrFile>, GithubError> {
        self.get_json(format!("repos/{nwo}/pulls/{number}/files?per_page=100"))
            .await
    }

    /// Inline review (diff) comments on a pull request.
    pub async fn pull_comments(
        &self,
        nwo: &str,
        number: u32,
    ) -> Result<Vec<DiffComment>, GithubError> {
        self.get_json(format!("repos/{nwo}/pulls/{number}/comments?per_page=100"))
            .await
    }

    /// Conversation comments on an issue or pull request.
    pub async fn issue_comments(
        &self,
        nwo: &str,
        number: u32,
    ) -> Result<Vec<IssueComment>, GithubError> {
        self.get_json(format!("repos/{nwo}/issues/{number}/comments?per_page=100"))
            .await
    }

    /// Open an issue.
    pub async fn create_issue(
        &self,
        nwo: &str,
        title: &str,
        body: &str,
    ) -> Result<CreatedIssue, GithubError> {
        self.post_json(
            format!("repos/{nwo}/issues"),
            &json!({ "title": title, "body": body }),
        )
        .await
    }

    /// Comment on an issue or pull request conversation.
    pub async fn create_issue_comment(
        &self,
        nwo: &str,
        number: u32,
        body: &str,
    ) -> Result<IssueComment, GithubError> {
        self.post_json(
            format!("repos/{nwo}/issues/{number}/comments"),
            &json!({ "body": body }),
        )
        .await
    }

    /// One conversation comment, for authorship checks before editing.
    pub async fn issue_comment(
        &self,
        nwo: &str,
        comment_id: u64,
    ) -> Result<IssueComment, GithubError> {
        self.get_json(format!("repos/{nwo}/issues/comments/{comment_id}"))
            .await
    }

    /// Replace a conversation comment's body. GitHub keeps the edit
    /// history visible in the UI.
    pub async fn update_issue_comment(
        &self,
        nwo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<IssueComment, GithubError> {
        let payload = serde_json::to_vec(&json!({ "body": body }))
            .map_err(|e| GithubError::Network(e.to_string()))?;
        let raw = (self.request)(
            "PATCH",
            format!("repos/{nwo}/issues/comments/{comment_id}"),
            Some(payload),
        )
        .await?;
        interpret_response(&raw)
    }

    /// Users and teams whose review is still requested.
    pub async fn requested_reviewers(
        &self,
        nwo: &str,
        number: u64,
    ) -> Result<RequestedReviewers, GithubError> {
        self.get_json(format!("repos/{nwo}/pulls/{number}/requested_reviewers"))
            .await
    }

    /// Submit a review. `payload` carries `body`, `event`, and
    /// `comments` per the create-review endpoint.
    pub async fn create_review(
        &self,
        nwo: &str,
        number: u64,
        payload: &serde_json::Value,
    ) -> Result<CreatedReview, GithubError> {
        self.post_json(format!("repos/{nwo}/pulls/{number}/reviews"), payload)
            .await
    }

    /// Reply in-thread to an inline review comment.
    pub async fn reply_to_diff_comment(
        &self,
        nwo: &str,
        number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<DiffComment, GithubError> {
        self.post_json(
            format!("repos/{nwo}/pulls/{number}/comments/{comment_id}/replies"),
            &json!({ "body": body }),
        )
        .await
    }

    /// The repository itself, for its default branch.
    pub async fn repo(&self, nwo: &str) -> Result<Repo, GithubError> {
        self.get_json(format!("repos/{nwo}")).await
    }

    /// Pull requests by REST state: `open`, `closed`, or `all`
    /// (`merged` is not a REST state — filter on `merged_at`).
    pub async fn pulls(&self, nwo: &str, state: &str) -> Result<Vec<PullSummary>, GithubError> {
        self.get_json(format!("repos/{nwo}/pulls?state={state}&per_page=100"))
            .await
    }

    /// Open a pull request. `payload` carries `title`, `body`, `head`,
    /// `base`, and `draft` per the create-pull endpoint.
    pub async fn create_pull(
        &self,
        nwo: &str,
        payload: &serde_json::Value,
    ) -> Result<CreatedPull, GithubError> {
        self.post_json(format!("repos/{nwo}/pulls"), payload).await
    }

    /// The most recent failed workflow run on a branch, if any.
    pub async fn latest_failed_run(
        &self,
        nwo: &str,
        branch: &str,
    ) -> Result<Option<WorkflowRun>, GithubError> {
        let runs: WorkflowRuns = self
            .get_json(format!(
                "repos/{nwo}/actions/runs?branch={branch}&status=failure&per_page=1"
            ))
            .await?;
        Ok(runs.workflow_runs.into_iter().next())
    }

    /// Jobs of a workflow run.
    pub async fn run_jobs(&self, nwo: &str, run_id: u64) -> Result<Vec<Job>, GithubError> {
        let jobs: Jobs = self
            .get_json(format!(
                "repos/{nwo}/actions/runs/{run_id}/jobs?per_page=100"
            ))
            .await?;
        Ok(jobs.jobs)
    }

    /// Raw escape-hatch request. Returns the response unparsed so the
    /// caller owns status interpretation.
    pub async fn raw(
        &self,
        method: &'static str,
        path: String,
        body: Option<Vec<u8>>,
    ) -> Result<RawResponse, GithubError> {
        (self.request)(method, path, body).await
    }

    /// Raw log text of a job. GitHub answers with a redirect to a
    /// blob URL, which reqwest follows. Tail-truncated to the
    /// tool-output ceiling before returning (CI logs are tail-weighted).
    pub async fn job_logs(&self, nwo: &str, job_id: u64) -> Result<String, GithubError> {
        let raw = (self.request)(
            "GET",
            format!("repos/{nwo}/actions/jobs/{job_id}/logs"),
            None,
        )
        .await?;
        if !(200..=299).contains(&raw.status) {
            return Err(GithubError::Api {
                status: raw.status,
                body: String::from_utf8_lossy(&raw.body).into_owned(),
            });
        }
        let text = String::from_utf8_lossy(&raw.body);
        Ok(
            crate::tools::truncate_tail(&text, crate::tools::TOOL_OUTPUT_CEILING_BYTES)
                .into_owned(),
        )
    }
}

// ---------------------------------------------------------------------------
// Pure core
// ---------------------------------------------------------------------------

/// Parse a raw HTTP response into a GitHub API result.
///
/// Pure function — no IO, no async. Non-2xx statuses carry the body
/// for diagnosis (GitHub error payloads name the problem).
pub fn interpret_response<T: DeserializeOwned>(raw: &RawResponse) -> Result<T, GithubError> {
    if !(200..=299).contains(&raw.status) {
        return Err(GithubError::Api {
            status: raw.status,
            body: String::from_utf8_lossy(&raw.body).into_owned(),
        });
    }
    serde_json::from_slice(&raw.body).map_err(|e| GithubError::Deserialize(e.to_string()))
}

// ---------------------------------------------------------------------------
// Wire format types (GitHub REST API)
// ---------------------------------------------------------------------------

// Intentionally no `deny_unknown_fields` — GitHub returns many fields
// we don't care about, and the API grows over time.

/// The authenticated user.
#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub login: String,
}

/// A user reference on reviews, comments, and issues.
#[derive(Clone, Debug, Deserialize)]
pub struct UserRef {
    pub login: String,
}

#[derive(Debug, Deserialize)]
struct SearchResults {
    items: Vec<SearchIssue>,
}

/// An issue or PR from `/search/issues`.
#[derive(Clone, Debug, Deserialize)]
pub struct SearchIssue {
    pub number: u32,
    pub title: String,
    /// Null on PRs created without a description.
    pub body: Option<String>,
    pub user: UserRef,
    /// API URL of the repository, e.g.
    /// `https://api.github.com/repos/owner/repo`.
    pub repository_url: String,
    /// RFC 3339 timestamp of the last change, comments included.
    pub updated_at: String,
    /// Issue labels. Defaulted so PR-search consumers and fixtures
    /// that never set labels keep working.
    #[serde(default)]
    pub labels: Vec<IssueLabel>,
}

/// A label on an issue.
#[derive(Clone, Debug, Deserialize)]
pub struct IssueLabel {
    pub name: String,
}

impl SearchIssue {
    /// Whether the issue carries `label`, matched case-insensitively.
    pub fn has_label(&self, label: &str) -> bool {
        self.labels
            .iter()
            .any(|l| l.name.eq_ignore_ascii_case(label))
    }

    /// `owner/repo`, parsed from the repository URL.
    pub fn nwo(&self) -> Option<String> {
        let mut segments = self.repository_url.rsplit('/');
        let repo = segments.next()?;
        let owner = segments.next()?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(format!("{owner}/{repo}"))
    }
}

/// A pull request from `/pulls/{number}`.
#[derive(Clone, Debug, Deserialize)]
pub struct Pull {
    /// `open` or `closed` (merged PRs are `closed`).
    pub state: String,
    pub title: String,
    pub head: PullRef,
    pub base: PullRef,
}

/// One end of a pull request.
#[derive(Clone, Debug, Deserialize)]
pub struct PullRef {
    pub sha: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
}

/// A submitted PR review.
#[derive(Clone, Debug, Deserialize)]
pub struct PrReview {
    pub user: UserRef,
    /// Null on reviews submitted without a body.
    pub body: Option<String>,
    /// `APPROVED`, `CHANGES_REQUESTED`, or `COMMENTED`.
    pub state: String,
    /// Absent on pending reviews.
    pub submitted_at: Option<String>,
}

/// A conversation comment on an issue or PR.
#[derive(Clone, Debug, Deserialize)]
pub struct IssueComment {
    /// Comment id, needed to edit in place via the update endpoint.
    pub id: u64,
    pub user: UserRef,
    pub body: String,
    pub created_at: String,
}

/// An inline review comment on a PR diff.
#[derive(Clone, Debug, Deserialize)]
pub struct DiffComment {
    /// Comment id, needed to reply in-thread via the replies endpoint.
    pub id: u64,
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
    pub user: UserRef,
    pub created_at: String,
}

/// A commit on a pull request.
#[derive(Clone, Debug, Deserialize)]
pub struct PrCommit {
    pub sha: String,
    pub commit: CommitDetail,
}

/// The git-level part of a PR commit.
#[derive(Clone, Debug, Deserialize)]
pub struct CommitDetail {
    /// Full commit message, headline and body joined by newlines.
    pub message: String,
}

/// A changed file on a pull request.
#[derive(Clone, Debug, Deserialize)]
pub struct PrFile {
    pub filename: String,
    pub additions: u64,
    pub deletions: u64,
}

/// Reviewers still requested on a pull request.
#[derive(Clone, Debug, Deserialize)]
pub struct RequestedReviewers {
    pub users: Vec<UserRef>,
    pub teams: Vec<TeamRef>,
}

/// A team reference on review requests.
#[derive(Clone, Debug, Deserialize)]
pub struct TeamRef {
    pub name: String,
}

/// The review created by the create-review endpoint.
#[derive(Clone, Debug, Deserialize)]
pub struct CreatedReview {
    pub id: u64,
    /// `APPROVED`, `COMMENTED`, or `PENDING`.
    pub state: String,
}

/// A repository from `/repos/{nwo}`.
#[derive(Clone, Debug, Deserialize)]
pub struct Repo {
    pub default_branch: String,
}

/// A pull request from the list endpoint.
#[derive(Clone, Debug, Deserialize)]
pub struct PullSummary {
    pub number: u64,
    pub title: String,
    /// `open` or `closed`.
    pub state: String,
    pub html_url: String,
    /// Set iff the PR was merged (merged PRs are `closed`).
    pub merged_at: Option<String>,
}

/// The issue created by the create-issue endpoint.
#[derive(Clone, Debug, Deserialize)]
pub struct CreatedIssue {
    pub number: u32,
    pub html_url: String,
}

/// The pull request created by the create-pull endpoint.
#[derive(Clone, Debug, Deserialize)]
pub struct CreatedPull {
    pub number: u64,
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowRuns {
    workflow_runs: Vec<WorkflowRun>,
}

/// A workflow run from the actions runs endpoint.
#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub display_title: String,
    /// Workflow name.
    pub name: String,
    pub created_at: String,
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
struct Jobs {
    jobs: Vec<Job>,
}

/// A job of a workflow run.
#[derive(Clone, Debug, Deserialize)]
pub struct Job {
    pub id: u64,
    pub name: String,
    /// `success`, `failure`, `cancelled`, ...; absent while running.
    pub conclusion: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, body: &str) -> RawResponse {
        RawResponse {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn interpret_user_success() {
        let user: User = interpret_response(&raw(200, r#"{"login":"kitaebot"}"#)).unwrap();
        assert_eq!(user.login, "kitaebot");
    }

    #[test]
    fn interpret_api_error_carries_body() {
        let err =
            interpret_response::<User>(&raw(403, r#"{"message":"rate limited"}"#)).unwrap_err();
        assert!(
            matches!(err, GithubError::Api { status: 403, ref body } if body.contains("rate limited"))
        );
    }

    #[test]
    fn interpret_malformed_json() {
        let err = interpret_response::<User>(&raw(200, "not json")).unwrap_err();
        assert!(matches!(err, GithubError::Deserialize(_)));
    }

    #[test]
    fn nwo_parses_repository_url() {
        let issue = SearchIssue {
            number: 1,
            title: String::new(),
            body: None,
            user: UserRef {
                login: "alice".into(),
            },
            repository_url: "https://api.github.com/repos/owner/repo".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            labels: Vec::new(),
        };
        assert_eq!(issue.nwo().as_deref(), Some("owner/repo"));
    }

    #[test]
    fn nwo_rejects_empty_segments() {
        let issue = SearchIssue {
            number: 1,
            title: String::new(),
            body: None,
            user: UserRef {
                login: "alice".into(),
            },
            repository_url: "repo/".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            labels: Vec::new(),
        };
        assert_eq!(issue.nwo(), None);
    }

    #[tokio::test]
    async fn search_encodes_qualifiers_into_the_path() {
        let client = GithubClient::from_fn(|method, path, body| async move {
            assert_eq!(method, "GET");
            assert!(body.is_none());
            assert_eq!(path, "search/issues?q=is:pr+is:open+author:bot&per_page=50");
            Ok(RawResponse {
                status: 200,
                body: br#"{"items":[]}"#.to_vec(),
            })
        });
        let items = client
            .search_issues("is:pr is:open author:bot")
            .await
            .unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn pull_hits_the_pulls_endpoint() {
        let client = GithubClient::from_fn(|method, path, _body| async move {
            assert_eq!(method, "GET");
            assert_eq!(path, "repos/owner/repo/pulls/7");
            Ok(RawResponse {
                status: 200,
                body: br#"{"state":"open","title":"T",
                    "head":{"sha":"abc","ref":"feature"},
                    "base":{"sha":"def","ref":"master"}}"#
                    .to_vec(),
            })
        });
        let pull = client.pull("owner/repo", 7).await.unwrap();
        assert_eq!(pull.head.sha, "abc");
        assert_eq!(pull.base.ref_name, "master");
        assert_eq!(pull.state, "open");
    }

    #[tokio::test]
    async fn create_review_posts_the_payload() {
        let client = GithubClient::from_fn(|method, path, body| async move {
            assert_eq!(method, "POST");
            assert_eq!(path, "repos/owner/repo/pulls/141/reviews");
            let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            assert_eq!(payload["event"], "COMMENT");
            Ok(RawResponse {
                status: 200,
                body: br#"{"id":9,"state":"COMMENTED"}"#.to_vec(),
            })
        });
        let review = client
            .create_review("owner/repo", 141, &json!({"event": "COMMENT"}))
            .await
            .unwrap();
        assert_eq!(review.id, 9);
        assert_eq!(review.state, "COMMENTED");
    }

    #[tokio::test]
    async fn create_issue_comment_posts_the_body() {
        let client = GithubClient::from_fn(|method, path, body| async move {
            assert_eq!(method, "POST");
            assert_eq!(path, "repos/owner/repo/issues/42/comments");
            let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            assert_eq!(payload["body"], "A plan");
            Ok(RawResponse {
                status: 201,
                body: br#"{"id":7,"user":{"login":"bot"},"body":"A plan",
                    "created_at":"2026-01-01T00:00:00Z"}"#
                    .to_vec(),
            })
        });
        let comment = client
            .create_issue_comment("owner/repo", 42, "A plan")
            .await
            .unwrap();
        assert_eq!(comment.body, "A plan");
    }

    #[tokio::test]
    async fn reply_posts_to_the_replies_endpoint() {
        let client = GithubClient::from_fn(|method, path, body| async move {
            assert_eq!(method, "POST");
            assert_eq!(path, "repos/owner/repo/pulls/5/comments/123/replies");
            let payload: serde_json::Value = serde_json::from_slice(&body.unwrap()).unwrap();
            assert_eq!(payload["body"], "Fixed");
            Ok(RawResponse {
                status: 200,
                body: br#"{"id":124,"path":"a.rs","line":1,"body":"Fixed",
                    "user":{"login":"bot"},"created_at":"2026-01-01T00:00:00Z"}"#
                    .to_vec(),
            })
        });
        let comment = client
            .reply_to_diff_comment("owner/repo", 5, 123, "Fixed")
            .await
            .unwrap();
        assert_eq!(comment.id, 124);
    }

    #[tokio::test]
    async fn client_propagates_closure_error() {
        let client = GithubClient::from_fn(|_method, _path, _body| async {
            Err(GithubError::Network("boom".into()))
        });
        let err = client.user().await.unwrap_err();
        assert!(matches!(err, GithubError::Network(_)));
    }
}

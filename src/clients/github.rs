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
    pub async fn search_prs(&self, query: &str) -> Result<Vec<SearchIssue>, GithubError> {
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
}

impl SearchIssue {
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
        let items = client.search_prs("is:pr is:open author:bot").await.unwrap();
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

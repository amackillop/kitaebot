//! GitHub API response types.
//!
//! Deserialized from `gh` CLI JSON output or REST API responses.
//! Shared across tool modules.

use serde::Deserialize;

/// A pull request from `gh pr list --json`.
#[derive(Deserialize)]
pub(super) struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
}

/// A review from `gh pr view --json reviews`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Review {
    pub author: Author,
    pub body: String,
    pub state: String,
    pub submitted_at: String,
}

/// A review request from `gh pr view --json reviewRequests`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReviewRequest {
    pub login: Option<String>,
    pub name: Option<String>,
}

/// A PR conversation comment from `gh pr view --json comments`.
///
/// Named `PrCommentEntry` to avoid collision with the `PrComment` tool.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PrCommentEntry {
    pub author: Author,
    pub body: String,
    pub created_at: String,
}

/// Aggregate response from `gh pr view --json reviews,reviewRequests,comments`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PrReviewsResponse {
    pub reviews: Vec<Review>,
    pub review_requests: Vec<ReviewRequest>,
    pub comments: Vec<PrCommentEntry>,
}

/// An inline code review comment from the REST API.
#[derive(Deserialize)]
pub(super) struct DiffComment {
    pub id: u64,
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
    pub user: Author,
}

/// Minimal user/author object (shared across response types).
#[derive(Deserialize)]
pub(super) struct Author {
    pub login: String,
}

/// A workflow run from `gh run list --json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkflowRun {
    pub database_id: u64,
    pub display_title: String,
    pub created_at: String,
    pub url: String,
    pub workflow_name: String,
}

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

//! GitHub integration tools.
//!
//! Provides authenticated git and GitHub CLI operations. The token never
//! reaches the exec tool — it is injected only into subprocesses spawned
//! by this module via `GIT_ASKPASS` (for git) or `GH_TOKEN` (for `gh`).
//!
//! # Architecture
//!
//! [`crate::tools::cli_runner::CliRunner`] is the raw subprocess boundary
//! — a single `exec` method that spawns any binary with an explicit env.
//! [`client::GitHubClient`] carries the shared context (runner, token,
//! workspace root, co-authors) and provides plumbing methods (`run_gh`,
//! `run_git`, etc.). Each tool file holds an `Arc<GitHubClient<R>>` and
//! owns only its business logic.
//!
//! Tests substitute `StubCliRunner` to exercise the logic without
//! spawning real subprocesses.
//!
//! # Token injection
//!
//! For `git clone`/`push`, a temporary helper script is written to a
//! private directory, set as `GIT_ASKPASS`, and deleted immediately after
//! the subprocess exits. The script prints the token to stdout when
//! invoked by git. The token is on disk for the duration of one git
//! command only.

mod ci_status;
mod client;
mod commit;
mod git_clone;
mod pr_comment;
mod pr_create;
mod pr_diff_comments;
mod pr_diff_reply;
mod pr_list;
mod pr_reviews;
mod push;
#[cfg(test)]
mod test_helpers;
mod types;
mod url;

// Re-export parent utility so tool files can `use super::Tool`.
pub(crate) use super::Tool;

pub use ci_status::CiStatus;
pub use client::GitHubClient;
pub use commit::Commit;
pub use git_clone::GitClone;
pub use pr_comment::PrComment;
pub use pr_create::PrCreate;
pub use pr_diff_comments::PrDiffComments;
pub use pr_diff_reply::PrDiffReply;
pub use pr_list::PrList;
pub use pr_reviews::PrReviews;
pub use push::Push;

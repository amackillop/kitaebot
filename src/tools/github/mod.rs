//! GitHub integration tools.
//!
//! Provides authenticated GitHub CLI operations. The token never
//! reaches the exec tool — it is injected only into subprocesses spawned
//! by this module via `GH_TOKEN` (for `gh`).
//!
//! # Architecture
//!
//! [`crate::tools::cli_runner::exec`] is the subprocess boundary.
//! [`crate::tools::git::GitCli`] wraps the `git` binary (clone, push, commit).
//! [`gh_cli::GhCli`] wraps the `gh` CLI (PRs, CI, API calls).
//! Each tool owns a clone of the appropriate CLI struct and holds
//! only its business logic.
//!
//! Tools expose a `prepare()` method that returns a
//! [`crate::tools::cli_runner::SubprocessCall`] — a pure value
//! describing what to run. Tests check this value directly without
//! spawning subprocesses.
//!
//! # Token injection
//!
//! For `gh` commands, `GH_TOKEN` is injected into the subprocess
//! environment. For `git clone`/`push`, a temporary `GIT_ASKPASS`
//! script is used — see [`crate::tools::git`].

mod ci_status;
mod gh;
mod gh_cli;
mod pr_create;
mod pr_diff_comments;
mod pr_diff_reply;
mod pr_list;
mod pr_reviews;
#[cfg(test)]
mod test_helpers;
mod types;

pub use ci_status::CiStatus;
pub use gh::Gh;
pub use gh_cli::GhCli;
pub use pr_create::PrCreate;
pub use pr_diff_comments::PrDiffComments;
pub use pr_diff_reply::PrDiffReply;
pub use pr_list::PrList;
pub use pr_reviews::PrReviews;

// Re-export parent utility so tool files can `use super::Tool`.
pub(crate) use super::Tool;

/// Build the GitHub tools from a pre-constructed [`GhCli`].
pub(crate) fn build(gh: GhCli) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(CiStatus(gh.clone())),
        Box::new(Gh(gh.clone())),
        Box::new(PrCreate(gh.clone())),
        Box::new(PrDiffComments(gh.clone())),
        Box::new(PrDiffReply(gh.clone())),
        Box::new(PrList(gh.clone())),
        Box::new(PrReviews(gh)),
    ]
}

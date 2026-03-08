//! Tools that require network access.
//!
//! Gated behind `#[cfg(not(feature = "mock-network"))]` at the parent
//! module level so the entire subtree is excluded from stub builds.

mod github;
mod web_fetch;
mod web_search;

pub use github::GitHub;
pub use web_fetch::WebFetch;
pub use web_search::WebSearch;

// Re-export parent utilities so child modules can use `super::`.
pub(crate) use super::{Tool, safe_env, truncate_output};

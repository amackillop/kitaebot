//! Channels (spec 10): the surfaces the agent talks through.
//!
//! Each submodule is one channel — a Unix socket for local chat, the
//! Telegram bot, and the GitHub and Linear pollers. The daemon drives
//! their loops; `dispatch` holds the shared input/reply vocabulary.

pub(crate) mod execution_checkout;
pub(crate) mod github;
pub(crate) mod github_issues;
pub(crate) mod linear;
pub(crate) mod socket;
pub(crate) mod telegram;

/// Plan instructions for a ticket announcement, shared by the ticket
/// channels (GitHub issues, Linear). Channels append tracker-specific
/// sentences after it.
/// The plan brief a `needs-plan` ticket asks for. A file, not a
/// string: it is a document with structure, and prompts/*.md is where
/// those live.
const PLAN_INSTRUCTIONS: &str = include_str!("../prompts/plan-format.md");

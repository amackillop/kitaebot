//! Channels (spec 10): the surfaces the agent talks through.
//!
//! Each submodule is one channel — a Unix socket for local chat, the
//! Telegram bot, and the GitHub and Linear pollers. The daemon drives
//! their loops; `dispatch` holds the shared input/reply vocabulary.

mod execution_checkout;
pub(crate) mod github;
pub(crate) mod github_issues;
pub(crate) mod linear;
pub(crate) mod socket;
pub(crate) mod telegram;

/// Plan instructions for a ticket announcement, shared by the ticket
/// channels (GitHub issues, Linear). Channels append tracker-specific
/// sentences after it.
const PLAN_INSTRUCTIONS: &str = "Analyze the task and reply with a review-ready implementation plan \
     in markdown, ordered for a human reviewer. Lead with a short prose \
     summary of the approach and the key decisions and trade-offs, then \
     the assumptions you made and the unresolved questions you need \
     answered — a reviewer must be able to spot a bad assumption without \
     reading the whole plan. End with the implementation broken into a \
     sequence of small, atomic commits: each builds and passes tests on \
     its own, and a reviewer can hold the whole diff in their head. \
     Do not implement anything yet — your reply will be posted as a \
     comment on the ticket for approval.";

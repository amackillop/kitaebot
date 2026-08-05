//! Channels (spec 10): the surfaces the agent talks through.
//!
//! Each submodule is one channel — a Unix socket for local chat, the
//! Telegram bot, and the GitHub and Linear pollers. The daemon drives
//! their loops; `dispatch` holds the shared input/reply vocabulary.

mod execution_checkout;
pub(crate) mod github;
pub(crate) mod linear;
pub(crate) mod socket;
pub(crate) mod telegram;

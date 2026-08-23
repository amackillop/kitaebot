//! GitHub channels: two poll loops over one integration.
//!
//! [`prs`] polls pull requests (spec 20); [`issues`] polls issue
//! assignments and events (spec 25). Each has its own loop, state
//! document, and spec; they share the REST client, the bot identity,
//! and the trust model, and nothing else.

pub(crate) mod issues;
pub(crate) mod prs;
mod review_checkout;
mod trust;

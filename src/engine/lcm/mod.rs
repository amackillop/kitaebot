//! LCM (Lossless Context Management) engine.
//!
//! See `specs/14-context-engine.md` for the design.

pub mod compaction;
pub mod engine;
// Not yet wired into push_message.
#[allow(dead_code)]
pub mod explore;
pub mod schema;
pub mod summarize;
pub mod tools;

pub use engine::LcmEngine;

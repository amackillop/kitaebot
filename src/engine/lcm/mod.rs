//! LCM (Lossless Context Management) engine.
//!
//! See `specs/14-context-engine.md` for the design.

pub mod compaction;
pub mod engine;
pub mod schema;
pub mod summarize;
pub mod tools;

pub use engine::LcmEngine;

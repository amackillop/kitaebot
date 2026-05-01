//! LCM (Lossless Context Management) engine.
//!
//! See `specs/14-context-engine.md` for the design.

pub mod engine;
pub mod schema;

pub use engine::LcmEngine;

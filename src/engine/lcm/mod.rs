//! LCM (Lossless Context Management) engine.
//!
//! See `specs/14-context-engine.md` for the design.

#![allow(dead_code)] // Wired into the daemon by config selection.

pub mod engine;
pub mod schema;

#[allow(unused_imports)] // Public API; consumed when the daemon picks LCM.
pub use engine::LcmEngine;

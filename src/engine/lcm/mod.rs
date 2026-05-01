//! LCM (Lossless Context Management) engine.
//!
//! See `specs/14-context-engine.md` for the design. This module is
//! built up across multiple commits — 3.1 lands the schema, 3.2 the
//! `LcmEngine` skeleton, 3.3 onwards retrieval and compaction.

#![allow(dead_code)] // Wired into LcmEngine in commit 3.2.

pub mod schema;

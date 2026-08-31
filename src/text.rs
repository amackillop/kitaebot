//! Panic-free string and pattern helpers.
//!
//! `string_slice` and `expect_used` are denied crate-wide; the slices
//! here are boundary-proven and the expects fire only on malformed
//! pattern literals, so each lint is allowed once, here.

use regex::{Regex, RegexSet};

/// Longest prefix of at most `max_bytes`, cut on a char boundary.
#[allow(clippy::string_slice)] // floor_char_boundary proves the cut
pub(crate) fn prefix(s: &str, max_bytes: usize) -> &str {
    &s[..s.floor_char_boundary(max_bytes)]
}

/// Longest suffix of at most `max_bytes`, cut on a char boundary.
#[allow(clippy::string_slice)] // ceil_char_boundary proves the cut
pub(crate) fn suffix(s: &str, max_bytes: usize) -> &str {
    &s[s.ceil_char_boundary(s.len().saturating_sub(max_bytes))..]
}

/// A regex from a pattern known at compile time. Panics on a malformed
/// pattern; deterministic, so any test touching the static catches it.
#[allow(clippy::expect_used)] // pattern is assembled from literals
pub(crate) fn static_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("malformed compile-time regex pattern")
}

/// A regex set from patterns known at compile time; see [`static_regex`].
#[allow(clippy::expect_used)] // patterns are compile-time literals
pub(crate) fn static_regex_set<I, S>(patterns: I) -> RegexSet
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    RegexSet::new(patterns).expect("malformed compile-time regex pattern")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_respects_char_boundaries() {
        assert_eq!(prefix("a€b", 2), "a");
        assert_eq!(prefix("a€b", 4), "a€");
        assert_eq!(prefix("abc", 10), "abc");
    }

    #[test]
    fn suffix_respects_char_boundaries() {
        assert_eq!(suffix("a€b", 2), "b");
        assert_eq!(suffix("a€b", 4), "€b");
        assert_eq!(suffix("abc", 10), "abc");
    }
}

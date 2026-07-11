//! Memory subsystem (spec 21).
//!
//! The agent keeps durable, cross-session knowledge in `memory/`. The
//! index file `memory/MEMORY.md` is read fresh each root turn and
//! appended to the system prompt so the agent always sees what it knows
//! without a tool call. Detail lives in `memory/topics/*.md`, reached on
//! demand with the ordinary file tools.

use std::path::Path;

use tracing::warn;

/// Header wrapping the injected index so the model reads it as its own
/// durable memory rather than conversation content.
const INDEX_HEADER: &str = "# Memory\n\n\
This is your durable memory, carried across every session. Treat it as \
fact you already know. Follow pointers into `memory/topics/` when a \
line refers to a topic file.\n\n";

/// Build the system-prompt segment for the memory index, or `None` when
/// there is nothing to inject.
///
/// Reads `memory/MEMORY.md` fresh (the agent rewrites it at runtime).
/// A missing file is a normal empty state and yields `None` silently;
/// any other read error is logged and also yields `None` — a broken
/// read must never take a turn down. Content over `cap_bytes` is
/// truncated on a UTF-8 boundary with a marker.
pub fn index_segment(memory_dir: &Path, cap_bytes: usize) -> Option<String> {
    let path = memory_dir.join("MEMORY.md");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!("failed to read memory index {}: {e}", path.display());
            return None;
        }
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (body, truncated) = truncate_on_boundary(trimmed, cap_bytes);
    if truncated {
        warn!(cap_bytes, "memory index exceeds cap, truncating");
        Some(format!(
            "{INDEX_HEADER}{body}\n\n[memory index truncated at {cap_bytes} bytes]"
        ))
    } else {
        Some(format!("{INDEX_HEADER}{body}"))
    }
}

/// Truncate `s` to at most `cap` bytes without splitting a UTF-8
/// character. Returns the kept prefix and whether truncation happened.
fn truncate_on_boundary(s: &str, cap: usize) -> (&str, bool) {
    if s.len() <= cap {
        return (s, false);
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_shorter_than_cap_is_unchanged() {
        let (out, cut) = truncate_on_boundary("hello", 100);
        assert_eq!(out, "hello");
        assert!(!cut);
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // "é" occupies bytes 1..3; a cap of 2 lands mid-character and
        // must walk back to byte 1, keeping only "a".
        let (out, cut) = truncate_on_boundary("aéb", 2);
        assert_eq!(out, "a");
        assert!(cut);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn segment_absent_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(index_segment(dir.path(), 8192).is_none());
    }

    #[test]
    fn segment_absent_when_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "   \n\t\n").unwrap();
        assert!(index_segment(dir.path(), 8192).is_none());
    }

    #[test]
    fn segment_present_with_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "- prefers tabs\n").unwrap();
        let seg = index_segment(dir.path(), 8192).unwrap();
        assert!(seg.contains("# Memory"));
        assert!(seg.contains("prefers tabs"));
        assert!(!seg.contains("truncated"));
    }

    #[test]
    fn segment_truncated_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "x".repeat(10_000)).unwrap();
        let seg = index_segment(dir.path(), 100).unwrap();
        assert!(seg.contains("truncated at 100 bytes"));
    }
}

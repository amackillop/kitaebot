//! Memory subsystem (spec 21).
//!
//! The agent keeps durable, cross-session knowledge in `memory/`. The
//! index file `memory/MEMORY.md` is read fresh each root turn and
//! appended to the system prompt so the agent always sees what it knows
//! without a tool call. Detail lives in `memory/topics/*.md`, reached on
//! demand with the ordinary file tools.

pub mod distill;

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
/// truncated on a UTF-8 boundary with a marker naming the dropped
/// `##` sections.
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
            "{INDEX_HEADER}{body}\n\n{}",
            truncation_marker(trimmed, body, cap_bytes)
        ))
    } else {
        Some(format!("{INDEX_HEADER}{body}"))
    }
}

/// Truncate `s` to at most `cap` bytes without splitting a UTF-8
/// character. Returns the kept prefix and whether truncation happened.
fn truncate_on_boundary(s: &str, cap: usize) -> (&str, bool) {
    let kept = crate::text::prefix(s, cap);
    (kept, kept.len() < s.len())
}

/// Dropped section titles named in the marker before it truncates to
/// "+K more"; the cap is the injection contract and an unbounded
/// marker would breach it right where it is stated.
const MARKER_SECTIONS_MAX: usize = 8;

/// Why the tail is invisible and what it held: the marker names the
/// `##` sections that fall entirely past the cut, so the agent (and
/// the log) can see exactly what memory was hidden. A section
/// straddling the cut keeps its header visible, so only its body tail
/// is lost; the marker says the list covers whole sections. Dropped
/// sections are found by position, not title: a duplicate title
/// surviving the cut must not hide its dropped twin, and a header the
/// cut split mid-line is dropped — it is not shown whole.
fn truncation_marker(full: &str, kept: &str, cap_bytes: usize) -> String {
    let mut offset = 0usize;
    let mut dropped: Vec<&str> = Vec::new();
    for line in full.split_inclusive('\n') {
        if offset + line.trim_end().len() > kept.len()
            && let Some(title) = line.trim_end().strip_prefix("## ")
        {
            dropped.push(title);
        }
        offset += line.len();
    }
    let mut marker = format!("[memory index truncated at {cap_bytes} bytes");
    if !dropped.is_empty() {
        use std::fmt::Write as _;
        let shown = dropped.len().min(MARKER_SECTIONS_MAX);
        marker.push_str(" — sections not shown: ");
        marker.push_str(&dropped[..shown].join("; "));
        if dropped.len() > shown {
            let _ = write!(marker, " (+{} more)", dropped.len() - shown);
        }
    }
    marker.push(']');
    marker
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

    #[test]
    fn truncation_marker_names_dropped_sections() {
        let kept = "# Memory\n\n## Tooling\n- a\n";
        // "## Repos" exists only past the cut, so it must be named;
        // "Tooling" is shown whole and must not be.
        let marker = truncation_marker(&format!("{kept}## Repos\n- b\n"), kept, 10);
        assert!(marker.contains("truncated at 10 bytes"), "{marker}");
        assert!(marker.contains("sections not shown: Repos"), "{marker}");
        assert!(!marker.contains("Tooling"), "{marker}");
    }

    #[test]
    fn truncation_marker_names_a_straddling_section() {
        // The cut lands mid-body of the last section: its header
        // survives, so it is not in the whole-section list — the
        // marker stays honest about what it names.
        let full = "# Memory\n\n## Repos\n\n- repo facts that go on and on\n";
        let kept = "# Memory\n\n## Repos\n\n- repo facts";
        let marker = truncation_marker(full, kept, kept.len());
        assert!(
            !marker.contains("sections not shown"),
            "a straddling section keeps its header and must not be listed: {marker}"
        );
    }

    /// Positional detection: a surviving "## Repos" must not hide a
    /// second "## Repos" that fell past the cut.
    #[test]
    fn truncation_marker_lists_a_dropped_duplicate_title() {
        let kept = "## Repos\n- a\n";
        let full = format!("{kept}## Repos\n- b\n");
        let marker = truncation_marker(&full, kept, kept.len());
        assert!(marker.contains("sections not shown: Repos"), "{marker}");
    }

    /// The marker itself honors the cap contract it sits next to: a
    /// pathological index cannot inflate it without bound.
    #[test]
    fn truncation_marker_caps_the_dropped_list() {
        let kept = "# Memory\n";
        let dropped = (0..MARKER_SECTIONS_MAX + 3).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "## Section{i}\n- x");
            s
        });
        let marker = truncation_marker(&format!("{kept}{dropped}"), kept, kept.len());
        assert!(marker.contains("Section0"), "{marker}");
        assert!(
            marker.contains(&format!("Section{}", MARKER_SECTIONS_MAX - 1)),
            "{marker}"
        );
        assert!(
            !marker.contains(&format!("Section{MARKER_SECTIONS_MAX}")),
            "{marker}"
        );
        assert!(marker.contains("(+3 more)"), "{marker}");
    }

    /// The cut splits a `##` header mid-line: the partial header is
    /// not shown whole, so it must be named as dropped.
    #[test]
    fn truncation_marker_names_a_split_header() {
        let kept = "# Memory\n\n## To";
        let full = "# Memory\n\n## Tooling\n- a\n";
        let marker = truncation_marker(full, kept, kept.len());
        assert!(marker.contains("sections not shown: Tooling"), "{marker}");
    }

    #[test]
    fn truncation_marker_without_sections_is_bare() {
        let marker = truncation_marker("x".repeat(50).as_str(), "x".repeat(10).as_str(), 10);
        assert_eq!(marker, "[memory index truncated at 10 bytes]");
    }

    #[test]
    fn truncated_segment_names_dropped_sections() {
        let dir = tempfile::tempdir().unwrap();
        let content = format!(
            "{}\n## Repos\n\n- repo facts\n",
            "# Memory\n\n## Tooling\n\n- tool facts\n"
        );
        std::fs::write(dir.path().join("MEMORY.md"), content).unwrap();
        let seg = index_segment(dir.path(), 40).unwrap();
        assert!(seg.contains("sections not shown: Repos"), "{seg}");
    }
}

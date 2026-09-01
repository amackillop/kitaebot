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

/// Why the tail is invisible and what it held: the marker names the
/// `##` sections that fall entirely past the cut, so the agent (and
/// the log) can see exactly what memory was hidden. A section
/// straddling the cut keeps its header visible, so only its body tail
/// is lost; the marker says the list covers whole sections. Sections
/// are matched by header line, not title: two same-titled sections
/// are distinct. The list is bounded so the marker cannot itself
/// blow the cap it reports.
const MARKER_SECTION_LIMIT: usize = 8;

fn truncation_marker(full: &str, kept: &str, cap_bytes: usize) -> String {
    use std::fmt::Write;

    // Multiset match: identical header lines are distinct sections, so
    // count the kept ones and name whatever the count cannot absorb.
    let mut kept_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for l in kept.lines().filter(|l| l.starts_with("## ")) {
        *kept_counts.entry(l).or_default() += 1;
    }
    let dropped: Vec<&str> = full
        .lines()
        .filter(|l| l.starts_with("## "))
        .filter(|l| match kept_counts.get_mut(l) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .collect();
    let mut marker = format!("[memory index truncated at {cap_bytes} bytes");
    if !dropped.is_empty() {
        marker.push_str(" — sections not shown: ");
        let shown = &dropped[..MARKER_SECTION_LIMIT.min(dropped.len())];
        marker.push_str(&shown.join("; "));
        if dropped.len() > MARKER_SECTION_LIMIT {
            let _ = write!(
                marker,
                "; and {} more",
                dropped.len() - MARKER_SECTION_LIMIT
            );
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
        assert!(marker.contains("sections not shown: ## Repos"), "{marker}");
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

    #[test]
    fn truncation_marker_without_sections_is_bare() {
        let marker = truncation_marker("x".repeat(50).as_str(), "x".repeat(10).as_str(), 10);
        assert_eq!(marker, "[memory index truncated at 10 bytes]");
    }

    #[test]
    fn truncation_marker_caps_the_section_list() {
        let full: String = (0..12).fold(String::new(), |s, i| format!("{s}## S{i}\n"));
        let marker = truncation_marker(&full, "", 10);
        assert!(marker.contains("## S7"), "{marker}");
        assert!(!marker.contains("S8"), "{marker}");
        assert!(marker.contains("and 4 more"), "{marker}");
    }

    #[test]
    fn truncation_marker_distinguishes_same_titled_sections() {
        // "## Repos" survives the cut and another "## Repos" falls past
        // it: header-line matching must still name the dropped one.
        let full = "## Repos\n- kept\n\n## Repos\n- dropped\n";
        let kept = full.strip_suffix("## Repos\n- dropped\n").unwrap();
        let marker = truncation_marker(full, kept, kept.len());
        assert!(marker.contains("sections not shown: ## Repos"), "{marker}");
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
        assert!(seg.contains("sections not shown: ## Repos"), "{seg}");
    }
}

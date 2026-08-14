//! Self-analysis discovery duty (spec 24 phase 2).
//!
//! Mines the bot's own problem record — `[notify]`-tagged journal
//! entries and the error tee — for evidence of defects in kitaebot
//! itself, and files at most one proposal per run through the
//! proposal contract. The delta is incident-shaped, not volume-shaped:
//! successful-run prose never enters it, so the threshold stays low.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tools::truncate_output;
use crate::types::estimate_tokens;

/// Open bot-authored proposals allowed per repo before filing stops.
pub const PROPOSAL_CAP: usize = 3;

/// Byte cap per delta section injected into the dispatch prompt.
const SECTION_MAX_BYTES: usize = 32 * 1024;

/// The duty's two symptom sources.
pub struct Sources {
    pub journal: PathBuf,
    pub errors_dir: PathBuf,
}

/// Read positions across the sources, persisted as the duty's cursor
/// (JSON in the duty state's cursor slot).
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Byte offset into the journal.
    journal: u64,
    /// The last error file read, and how far.
    errors_file: Option<String>,
    errors_offset: u64,
}

impl std::str::FromStr for Cursor {
    type Err = serde_json::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&serde_json::to_string(self).map_err(|_| std::fmt::Error)?)
    }
}

/// What a probe decided.
#[derive(Debug)]
pub enum Probe {
    /// New symptom material past the threshold: dispatch, advance the
    /// cursor on success.
    Open { delta: Delta, next: Cursor },
    /// Not enough new material; cursor stays, delta accumulates.
    Closed,
    /// First contact: record end-of-sources, dispatch nothing — the
    /// backlog predating the cursor is not this duty's to replay.
    Prime(Cursor),
}

/// New symptom material since the cursor.
#[derive(Debug)]
pub struct Delta {
    /// `[notify]`-tagged journal entries.
    pub notify: String,
    /// Error-tee JSON lines.
    pub errors: String,
}

/// Probe the sources against the cursor and threshold.
pub fn probe(
    sources: &Sources,
    cursor: Option<Cursor>,
    min_delta_tokens: usize,
) -> std::io::Result<Probe> {
    let Some(cursor) = cursor else {
        return Ok(Probe::Prime(end_of_sources(sources)?));
    };
    let (notify, journal_end) = journal_delta(&sources.journal, cursor.journal)?;
    let (errors, errors_end) = errors_delta(
        &sources.errors_dir,
        cursor.errors_file.as_deref(),
        cursor.errors_offset,
    )?;
    if estimate_tokens(&notify) + estimate_tokens(&errors) < min_delta_tokens {
        return Ok(Probe::Closed);
    }
    Ok(Probe::Open {
        delta: Delta { notify, errors },
        next: Cursor {
            journal: journal_end,
            errors_file: errors_end.0,
            errors_offset: errors_end.1,
        },
    })
}

/// The cursor pointing at the current end of both sources.
fn end_of_sources(sources: &Sources) -> std::io::Result<Cursor> {
    let journal = file_len(&sources.journal)?;
    let newest = error_files(&sources.errors_dir)?.pop();
    let errors_offset = match &newest {
        Some(f) => file_len(f)?,
        None => 0,
    };
    Ok(Cursor {
        journal,
        errors_file: newest.as_deref().and_then(file_name),
        errors_offset,
    })
}

fn file_len(path: &Path) -> std::io::Result<u64> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// `[notify]` entries appended past `offset`, and the new offset.
/// An externally shrunken journal skips to its end rather than
/// replaying history.
fn journal_delta(path: &Path, offset: u64) -> std::io::Result<(String, u64)> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((String::new(), 0)),
        Err(e) => return Err(e),
    };
    let end = bytes.len() as u64;
    let Some(tail) = usize::try_from(offset).ok().and_then(|o| bytes.get(o..)) else {
        return Ok((String::new(), end));
    };
    let notify = String::from_utf8_lossy(tail)
        .lines()
        .filter(|l| is_notify_entry(l))
        .collect::<Vec<_>>()
        .join("\n");
    Ok((notify, end))
}

/// A journal entry line of the shape `[timestamp] [notify] …`.
fn is_notify_entry(line: &str) -> bool {
    line.starts_with('[') && line.contains("] [notify] ")
}

/// Error-tee content past the cursor, and the new (file, offset).
///
/// Rolled files are date-named, so lexicographic order is
/// chronological: the remainder of the cursor file, then every newer
/// file whole. A pruned cursor file just means nothing older matches.
fn errors_delta(
    dir: &Path,
    cursor_file: Option<&str>,
    cursor_offset: u64,
) -> std::io::Result<(String, (Option<String>, u64))> {
    let files = error_files(dir)?;
    let mut out = String::new();
    let mut end: (Option<String>, u64) = (cursor_file.map(str::to_string), cursor_offset);
    for path in files {
        let Some(name) = file_name(&path) else {
            continue;
        };
        let offset = match cursor_file {
            Some(c) if name.as_str() < c => continue,
            Some(c) if name.as_str() == c => cursor_offset,
            _ => 0,
        };
        let bytes = std::fs::read(&path)?;
        if let Some(tail) = usize::try_from(offset).ok().and_then(|o| bytes.get(o..)) {
            out.push_str(&String::from_utf8_lossy(tail));
        }
        end = (Some(name), bytes.len() as u64);
    }
    Ok((out, end))
}

/// The tee's files, lexicographically sorted (chronological).
fn error_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

/// The dispatch prompt: symptoms, open issues on the repo for
/// dedup, and the one-proposal contract.
pub fn format_prompt(repo: &str, delta: &Delta, open_proposals: &[String]) -> String {
    use std::fmt::Write;
    let mut s = String::from(
        "Self-analysis: review your own operational record since the \
         last run and decide whether it evidences a defect in kitaebot \
         worth filing.\n",
    );
    if !delta.notify.is_empty() {
        let _ = write!(
            s,
            "\nProblem reports from your journal:\n{}\n",
            truncate_output(&delta.notify, SECTION_MAX_BYTES),
        );
    }
    if !delta.errors.is_empty() {
        let _ = write!(
            s,
            "\nError events (WARN/ERROR):\n{}\n",
            truncate_output(&delta.errors, SECTION_MAX_BYTES),
        );
    }
    if !open_proposals.is_empty() {
        let _ = write!(
            s,
            "\nOpen issues on this repo — if your symptom matches one \
             (yours or human-filed), do not file; cite it instead:\n",
        );
        for title in open_proposals {
            let _ = writeln!(s, "- {title}");
        }
    }
    let _ = write!(
        s,
        "\nInvestigate the most significant symptom. Ground it in the \
         code first: explore the checkout of {repo} before concluding. \
         Then either file ONE issue on {repo} with github_issue_create \
         — evidence quoted, your analysis, the suspected code location \
         — or reply briefly that nothing is actionable. Transient \
         one-off failures that self-corrected are not actionable. Do \
         not fix anything: proposals are triaged by a human, and an \
         issue you file only becomes work when a human assigns it."
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(dir: &Path) -> Sources {
        Sources {
            journal: dir.join("JOURNAL.md"),
            errors_dir: dir.join("errors"),
        }
    }

    fn write_journal(dir: &Path, content: &str) {
        std::fs::write(dir.join("JOURNAL.md"), content).unwrap();
    }

    fn write_error_file(dir: &Path, name: &str, content: &str) {
        let errors = dir.join("errors");
        std::fs::create_dir_all(&errors).unwrap();
        std::fs::write(errors.join(name), content).unwrap();
    }

    const NOTIFY: &str = "[2026-08-05T03:50:53Z] [notify] turn halted by policy gate\n";
    const ROUTINE: &str = "[2026-08-05T16:30:27Z] [duty] warm: all warm\n";

    #[test]
    fn cursor_round_trips_through_its_string_form() {
        let cursor = Cursor {
            journal: 42,
            errors_file: Some("errors.2026-08-06.jsonl".into()),
            errors_offset: 7,
        };
        let parsed: Cursor = cursor.to_string().parse().unwrap();
        assert_eq!(parsed, cursor);
    }

    #[test]
    fn no_cursor_primes_at_end_of_sources() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), ROUTINE);
        write_error_file(
            dir.path(),
            "errors.2026-08-06.jsonl",
            "{\"level\":\"WARN\"}\n",
        );

        let probe = probe(&sources(dir.path()), None, 1).unwrap();

        let Probe::Prime(cursor) = probe else {
            panic!("expected prime, got {probe:?}");
        };
        assert_eq!(cursor.journal, ROUTINE.len() as u64);
        assert_eq!(
            cursor.errors_file.as_deref(),
            Some("errors.2026-08-06.jsonl")
        );
        // A second probe from the primed cursor sees nothing.
        let probe = probe_from(&sources(dir.path()), cursor);
        assert!(matches!(probe, Probe::Closed));
    }

    fn probe_from(sources: &Sources, cursor: Cursor) -> Probe {
        probe(sources, Some(cursor), 1).unwrap()
    }

    #[test]
    fn routine_journal_entries_never_enter_the_delta() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), "");
        let start = probe(&sources(dir.path()), None, 1).unwrap();
        let Probe::Prime(cursor) = start else {
            panic!()
        };

        write_journal(dir.path(), &format!("{ROUTINE}{NOTIFY}{ROUTINE}"));
        let probe = probe_from(&sources(dir.path()), cursor);

        let Probe::Open { delta, .. } = probe else {
            panic!("expected open, got {probe:?}");
        };
        assert!(delta.notify.contains("turn halted by policy gate"));
        assert!(!delta.notify.contains("warm"));
    }

    #[test]
    fn below_threshold_stays_closed_and_accumulates() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), "");
        let Probe::Prime(cursor) = probe(&sources(dir.path()), None, 1).unwrap() else {
            panic!()
        };

        write_journal(dir.path(), NOTIFY);
        let first = probe(&sources(dir.path()), Some(cursor), 10_000).unwrap();
        assert!(matches!(first, Probe::Closed));

        // The cursor never advanced, so the same material still counts
        // once the threshold drops.
        let again = probe(&sources(dir.path()), Some(Cursor::default()), 1).unwrap();
        assert!(matches!(again, Probe::Open { .. }));
    }

    #[test]
    fn errors_delta_spans_rolled_files() {
        let dir = tempfile::tempdir().unwrap();
        write_journal(dir.path(), "");
        write_error_file(dir.path(), "errors.2026-08-05.jsonl", "old-a\n");
        let Probe::Prime(cursor) = probe(&sources(dir.path()), None, 1).unwrap() else {
            panic!()
        };

        // The cursored file grows, and a newer file appears.
        write_error_file(dir.path(), "errors.2026-08-05.jsonl", "old-a\nnew-b\n");
        write_error_file(dir.path(), "errors.2026-08-06.jsonl", "new-c\n");
        let probe = probe_from(&sources(dir.path()), cursor);

        let Probe::Open { delta, next } = probe else {
            panic!("expected open, got {probe:?}");
        };
        assert!(!delta.errors.contains("old-a"));
        assert!(delta.errors.contains("new-b"));
        assert!(delta.errors.contains("new-c"));
        assert_eq!(next.errors_file.as_deref(), Some("errors.2026-08-06.jsonl"));

        // Nothing new after advancing.
        assert!(matches!(
            probe_from(&sources(dir.path()), next),
            Probe::Closed
        ));
    }

    #[test]
    fn missing_sources_prime_at_zero() {
        let dir = tempfile::tempdir().unwrap();

        let Probe::Prime(cursor) = probe(&sources(dir.path()), None, 1).unwrap() else {
            panic!()
        };
        assert_eq!(cursor, Cursor::default());
    }

    #[test]
    fn prompt_carries_symptoms_proposals_and_contract() {
        let delta = Delta {
            notify: "halted by policy gate".into(),
            errors: "{\"level\":\"ERROR\"}".into(),
        };
        let prompt = format_prompt(
            "owner/repo",
            &delta,
            &["#12 exec leaks process groups".into()],
        );

        assert!(prompt.contains("halted by policy gate"));
        assert!(prompt.contains("{\"level\":\"ERROR\"}"));
        assert!(prompt.contains("#12 exec leaks process groups"));
        assert!(prompt.contains("file ONE issue on owner/repo"));
        assert!(prompt.contains("github_issue_create"));
    }
}

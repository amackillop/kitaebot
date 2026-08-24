//! Error tee: WARN and ERROR tracing events mirrored as JSON lines
//! under `state/errors/`.
//!
//! The daemon's own symptoms — blocked commands, tool timeouts, policy
//! halts — otherwise exist only in journald, which the daemon cannot
//! read back. The tee makes them a durable, cursorable source for the
//! self-analysis duty (spec 24 phase 2). Daily rolling files, bounded
//! retention; `state/` staging carries them into backups.
//!
//! This is an evidence set, not a severity filter: WARN is only the
//! mechanism that fills it. Emitting below WARN withholds the event
//! from self-analysis permanently, and one oversized entry evicts
//! every other incident in the duty's window. Spec 24, "What belongs
//! in the error tee", states the selection rule; read it before
//! choosing a level or a payload for anything the daemon logs.

use std::io::Write;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::registry::LookupSpan;

/// Days of error files kept. The duty reads daily; a week covers any
/// realistic gap without threatening the disk.
const MAX_LOG_FILES: usize = 7;

/// Byte cap per emitted log line, both sinks. Past it, journald cuts
/// silently and one tee entry can evict the duty window's other
/// incidents; per-site truncation is semantic and preferred, this is
/// the backstop for the sites that forget.
const LINE_MAX_BYTES: usize = 8_192;

/// How an oversized line is shortened without breaking its sink.
#[derive(Clone, Copy, Debug)]
pub enum LineFormat {
    /// JSON lines: replaced by a stub object carrying the head, so
    /// the tee stays parseable for self-analysis.
    Json,
    /// Plain text: head-truncated with a byte-count marker.
    Text,
}

/// Bound one complete line (newline excluded) to [`LINE_MAX_BYTES`].
/// `None` means the line passes unchanged. Pure.
fn bound_line(line: &[u8], format: LineFormat) -> Option<Vec<u8>> {
    if line.len() <= LINE_MAX_BYTES {
        return None;
    }
    // Lossy is fine on the cut path: the head is for a human or a
    // JSON string, and the cut point may split a code point anyway.
    let text = String::from_utf8_lossy(line);
    let head: String = text
        .chars()
        .scan(0, |bytes, c| {
            *bytes += c.len_utf8();
            (*bytes <= LINE_MAX_BYTES / 2).then_some(c)
        })
        .collect();
    let bounded = match format {
        LineFormat::Json => serde_json::json!({
            "truncated_line": head,
            "original_bytes": line.len(),
        })
        .to_string(),
        LineFormat::Text => {
            format!("{head}...[line truncated, {} bytes total]", line.len())
        }
    };
    Some(bounded.into_bytes())
}

/// Line-buffering writer that bounds each completed line before
/// passing it on. tracing's fmt layers write one event per line; a
/// runaway buffer without a newline is force-flushed at twice the cap
/// so a malformed writer upstream cannot balloon memory.
pub struct BoundedLineWriter<W: Write> {
    inner: W,
    format: LineFormat,
    buf: Vec<u8>,
}

impl<W: Write> BoundedLineWriter<W> {
    pub fn new(inner: W, format: LineFormat) -> Self {
        Self {
            inner,
            format,
            buf: Vec::new(),
        }
    }

    fn emit(&mut self, line: &[u8]) -> std::io::Result<()> {
        match bound_line(line, self.format) {
            Some(bounded) => self.inner.write_all(&bounded)?,
            None => self.inner.write_all(line)?,
        }
        self.inner.write_all(b"\n")
    }

    fn drain_complete_lines(&mut self) -> std::io::Result<()> {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let rest = self.buf.split_off(pos + 1);
            let line = std::mem::replace(&mut self.buf, rest);
            self.emit(&line[..line.len() - 1])?;
        }
        Ok(())
    }
}

impl<W: Write> Write for BoundedLineWriter<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        self.drain_complete_lines()?;
        if self.buf.len() > 2 * LINE_MAX_BYTES {
            let line = std::mem::take(&mut self.buf);
            self.emit(&line)?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Drop for BoundedLineWriter<W> {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let _ = self.emit(&line);
        }
        let _ = self.inner.flush();
    }
}

/// The tee layer plus the guard that flushes it; hold the guard for
/// the process lifetime. Creates `dir` if needed.
pub fn layer<S>(dir: &Path) -> std::io::Result<(impl Layer<S> + use<S>, WorkerGuard)>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    std::fs::create_dir_all(dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("errors")
        .filename_suffix("jsonl")
        .max_log_files(MAX_LOG_FILES)
        .build(dir)
        .map_err(std::io::Error::other)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(move || BoundedLineWriter::new(writer.clone(), LineFormat::Json))
        .with_filter(LevelFilter::WARN);
    Ok((layer, guard))
}

/// Mirror panics into the tee: they otherwise reach only stderr and
/// journald, which the daemon cannot read back. Delegates to the
/// previous hook, so the stderr backtrace survives.
///
/// Install after the subscriber is initialized or the event goes
/// nowhere.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map_or_else(|| "unknown".to_string(), ToString::to_string);
        tracing::error!(%location, "panic: {info}");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    /// A panic must leave evidence self-analysis can read.
    #[test]
    fn panic_reaches_the_tee() {
        let dir = tempfile::tempdir().unwrap();
        install_panic_hook();
        let lines = emit_and_read(dir.path(), || {
            let _ = std::panic::catch_unwind(|| panic!("boom for the tee"));
        });
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["level"], "ERROR");
        let msg = lines[0]["fields"]["message"].as_str().unwrap();
        assert!(msg.contains("boom for the tee"), "{msg}");
        assert!(
            lines[0]["fields"]["location"]
                .as_str()
                .unwrap()
                .contains("errlog.rs")
        );
    }

    /// One tee'd file's parsed JSON lines after emitting through a
    /// scoped subscriber.
    fn emit_and_read(dir: &Path, emit: impl FnOnce()) -> Vec<serde_json::Value> {
        let (layer, guard) = layer(dir).unwrap();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, emit);
        // Dropping the guard flushes the non-blocking writer.
        drop(guard);

        let file = std::fs::read_dir(dir)
            .unwrap()
            .next()
            .expect("one rolled file")
            .unwrap()
            .path();
        std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn tees_warn_and_error_as_json_lines() {
        let dir = tempfile::tempdir().unwrap();

        let lines = emit_and_read(dir.path(), || {
            tracing::info!("routine noise");
            tracing::warn!(command = "git fetch", "Command blocked");
            tracing::error!("timed out after 600s");
        });

        assert_eq!(lines.len(), 2, "info must not reach the tee");
        assert_eq!(lines[0]["level"], "WARN");
        assert_eq!(lines[0]["fields"]["command"], "git fetch");
        assert_eq!(lines[1]["level"], "ERROR");
        assert_eq!(lines[1]["fields"]["message"], "timed out after 600s");
    }

    #[test]
    fn bound_line_passes_small_lines_untouched() {
        assert_eq!(bound_line(b"short", LineFormat::Text), None);
        assert_eq!(bound_line(b"{\"a\":1}", LineFormat::Json), None);
    }

    #[test]
    fn bound_line_truncates_text_with_marker() {
        let long = "x".repeat(LINE_MAX_BYTES + 1);
        let out = bound_line(long.as_bytes(), LineFormat::Text).unwrap();
        assert!(out.len() < LINE_MAX_BYTES);
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with(&format!("[line truncated, {} bytes total]", long.len())));
    }

    #[test]
    fn bound_line_keeps_json_lines_parseable() {
        let mut obj = serde_json::json!({ "fields": { "message": "y".repeat(LINE_MAX_BYTES) } });
        let line = obj.to_string();
        let out = bound_line(line.as_bytes(), LineFormat::Json).unwrap();
        assert!(out.len() < LINE_MAX_BYTES);
        obj = serde_json::from_slice(&out).expect("stub must stay valid JSON");
        assert_eq!(obj["original_bytes"], line.len());
        assert!(
            obj["truncated_line"]
                .as_str()
                .unwrap()
                .starts_with("{\"fields\""),
            "stub carries the head"
        );
    }

    #[test]
    fn writer_bounds_each_line_independently() {
        let mut sink = Vec::new();
        {
            let mut w = BoundedLineWriter::new(&mut sink, LineFormat::Text);
            w.write_all(b"fine line\n").unwrap();
            w.write_all(format!("{}\n", "z".repeat(LINE_MAX_BYTES * 2)).as_bytes())
                .unwrap();
            // Split across writes, still one line.
            w.write_all(b"tail without").unwrap();
            w.write_all(b" newline yet\n").unwrap();
        }
        let text = String::from_utf8(sink).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "fine line");
        assert!(lines[1].contains("[line truncated,"));
        assert!(lines[1].len() < LINE_MAX_BYTES);
        assert_eq!(lines[2], "tail without newline yet");
    }

    /// An unbounded field at a call site must come out of the tee as
    /// one bounded, parseable line — the backstop this module exists
    /// for.
    #[test]
    fn tee_bounds_oversized_events() {
        let dir = tempfile::tempdir().unwrap();
        let report = "state ".repeat(LINE_MAX_BYTES);
        let lines = emit_and_read(dir.path(), || {
            tracing::error!(report, "iteration cap");
        });
        assert_eq!(lines.len(), 1);
        assert!(lines[0]["original_bytes"].as_u64().unwrap() > LINE_MAX_BYTES as u64);
        assert!(
            lines[0]["truncated_line"]
                .as_str()
                .unwrap()
                .contains("iteration cap")
        );
    }

    #[test]
    fn events_carry_their_span_context() {
        let dir = tempfile::tempdir().unwrap();

        let lines = emit_and_read(dir.path(), || {
            let span = tracing::warn_span!("turn", id = 7, source = "GitHub issue");
            let _g = span.enter();
            tracing::warn!("halted");
        });

        assert_eq!(lines.len(), 1);
        let spans = lines[0]["spans"].as_array().expect("span list");
        assert_eq!(spans[0]["id"], 7);
        assert_eq!(spans[0]["source"], "GitHub issue");
    }
}

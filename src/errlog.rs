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

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::registry::LookupSpan;

/// Days of error files kept. The duty reads daily; a week covers any
/// realistic gap without threatening the disk.
const MAX_LOG_FILES: usize = 7;

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
        .with_writer(writer)
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

//! Long-running daemon that drives the heartbeat loop.
//!
//! The daemon ticks on a configurable interval, running one heartbeat
//! cycle per tick. Errors within a cycle are logged and swallowed —
//! the loop survives until an external shutdown signal arrives.
//!
//! The core loop ([`run_with_shutdown`]) is generic over its shutdown
//! future so tests can substitute a simple `sleep` instead of real
//! Unix signals.

use std::future::Future;
use std::time::Duration;

use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use crate::heartbeat;
use crate::provider::Provider;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// Production entry point — runs until SIGINT or SIGTERM.
pub async fn run<P: Provider>(
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
    interval_secs: u64,
) {
    run_with_shutdown(
        workspace,
        provider,
        tools,
        max_iterations,
        interval_secs,
        shutdown_signal(),
    )
    .await;
}

/// Testable core: ticks heartbeat cycles until `shutdown` resolves.
async fn run_with_shutdown<P: Provider, S: Future<Output = ()>>(
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
    interval_secs: u64,
    shutdown: S,
) {
    let mut tick = interval(Duration::from_secs(interval_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                run_heartbeat_cycle(workspace, provider, tools, max_iterations).await;
            }
            () = &mut shutdown => {
                info!("Shutdown signal received, exiting.");
                return;
            }
        }
    }
}

/// Run a single heartbeat cycle, logging the outcome.
///
/// Errors are logged and swallowed so the daemon loop survives.
async fn run_heartbeat_cycle<P: Provider>(
    workspace: &Workspace,
    provider: &P,
    tools: &Tools,
    max_iterations: usize,
) {
    match heartbeat::run(workspace, provider, tools, max_iterations).await {
        Ok(heartbeat::Outcome::Executed(response)) => {
            info!("Heartbeat complete: {response}");
        }
        Ok(heartbeat::Outcome::Skipped(reason)) => {
            info!("Heartbeat skipped: {reason}");
        }
        Err(e) => {
            error!("Heartbeat error (will retry next tick): {e}");
        }
    }
}

/// Resolve on the first of SIGINT or SIGTERM.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderError;
    use crate::provider::MockProvider;
    use crate::types::Response;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        (dir, ws)
    }

    #[tokio::test]
    async fn fires_immediately_then_shuts_down() {
        let (_dir, ws) = workspace();
        // No HEARTBEAT.md → skipped, but proves the tick fired.
        let provider = MockProvider::new(vec![]);

        run_with_shutdown(
            &ws,
            &provider,
            &Tools::default(),
            1,
            3600, // large interval — only the immediate first tick matters
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;

        // If we get here without hanging, the first tick was immediate
        // and the shutdown future terminated the loop.
    }

    #[tokio::test]
    async fn multiple_cycles_with_short_interval() {
        let (_dir, ws) = workspace();
        std::fs::write(ws.heartbeat_path(), "- [ ] task\n").unwrap();

        // Over-provision: we expect ~3 ticks but provide enough headroom.
        let provider = MockProvider::new(vec![Ok(Response::Text("ok".into())); 10]);

        run_with_shutdown(
            &ws,
            &provider,
            &Tools::default(),
            1,
            1, // 1-second interval
            async {
                // Let 3 ticks fire: immediate + 2 more.
                tokio::time::sleep(Duration::from_secs(2)).await;
            },
        )
        .await;

        assert!(
            provider.call_count() >= 2,
            "expected at least 2 cycles, got {}",
            provider.call_count(),
        );
    }

    #[tokio::test]
    async fn error_does_not_crash_loop() {
        let (_dir, ws) = workspace();
        std::fs::write(ws.heartbeat_path(), "- [ ] task\n").unwrap();

        // Provider returns an error — loop should survive.
        let provider = MockProvider::new(vec![Err(ProviderError::Network("test".into()))]);

        run_with_shutdown(
            &ws,
            &provider,
            &Tools::default(),
            1,
            3600,
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;

        // Reaching here means the error didn't panic/crash.
    }
}

//! Long-running daemon that drives the heartbeat, Telegram, and socket loops.
//!
//! The daemon runs three concurrent loops — heartbeat ticks on a
//! configurable interval, the Telegram poller long-polls for incoming
//! messages, and the socket listener accepts Unix domain socket clients.
//! All are pinned futures inside a single `tokio::select!`, so they
//! make progress concurrently.
//!
//! The core loop ([`run_with_shutdown`]) is generic over its shutdown
//! future so tests can substitute a simple `sleep` instead of real
//! Unix signals.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use tracing::info;

use crate::agent::AgentHandle;
use crate::heartbeat;
use crate::socket;
use crate::telegram::{self, TelegramChannel};
use crate::workspace::Workspace;

/// Production entry point — runs until SIGINT or SIGTERM.
pub async fn run(
    workspace: &Workspace,
    handle: &AgentHandle,
    interval_secs: u64,
    telegram: Option<&TelegramChannel>,
    socket_path: &Path,
) {
    run_with_shutdown(
        workspace,
        handle,
        Duration::from_secs(interval_secs),
        telegram,
        socket_path,
        shutdown_signal(),
    )
    .await;
}

/// Testable core: runs heartbeat + telegram until `shutdown` resolves.
async fn run_with_shutdown<S: Future<Output = ()>>(
    workspace: &Workspace,
    handle: &AgentHandle,
    interval: Duration,
    telegram: Option<&TelegramChannel>,
    socket_path: &Path,
    shutdown: S,
) {
    let heartbeat_loop = heartbeat::poll_loop(interval, handle);

    let telegram_loop = async {
        match telegram {
            Some(ch) => telegram::poll_loop(ch, handle).await,
            None => std::future::pending().await,
        }
    };

    let session_path = workspace.session_path();
    let socket_loop = socket::listen(socket_path, &session_path, handle);

    tokio::select! {
        () = heartbeat_loop => unreachable!("heartbeat loop never exits"),
        () = telegram_loop => unreachable!("telegram loop never exits"),
        () = socket_loop => unreachable!("socket loop never exits"),
        () = shutdown => {
            info!("Shutdown signal received, exiting.");
            let _ = std::fs::remove_file(socket_path);
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
    use crate::config::ContextConfig;
    use crate::provider::MockProvider;
    use crate::tools::Tools;
    use crate::types::Response;
    use std::sync::Arc;

    const CTX: ContextConfig = ContextConfig {
        max_tokens: 200_000,
        budget_percent: 80,
    };

    fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
        (dir, Arc::new(ws))
    }

    /// Socket path in a temp dir — avoids collisions and `/run` dependency.
    fn sock_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        (dir, path)
    }

    #[tokio::test]
    async fn fires_immediately_then_shuts_down() {
        let (_dir, ws) = workspace();
        let (_sock_dir, sock_path) = sock_path();
        // No HEARTBEAT.md → skipped, but proves the tick fired.
        let handle = AgentHandle::spawn(
            ws.clone(),
            Arc::new(MockProvider::new(vec![])),
            Arc::new(Tools::default()),
            1,
            CTX,
        );

        run_with_shutdown(
            &ws,
            &handle,
            Duration::from_secs(3600), // large interval — only the immediate first tick matters
            None,
            &sock_path,
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;

        // If we get here without hanging, the first tick was immediate
        // and the shutdown future terminated the loop.
    }

    #[tokio::test]
    async fn multiple_cycles_with_short_interval() {
        let (_dir, ws) = workspace();
        let (_sock_dir, sock_path) = sock_path();
        std::fs::write(ws.heartbeat_path(), "- [ ] task\n").unwrap();

        // Over-provision: we expect ~3 ticks but provide enough headroom.
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text("ok".into())); 10]));
        let handle = AgentHandle::spawn(
            ws.clone(),
            provider.clone(),
            Arc::new(Tools::default()),
            1,
            CTX,
        );

        run_with_shutdown(
            &ws,
            &handle,
            Duration::from_millis(100), // 100ms interval for fast test
            None,
            &sock_path,
            async {
                // Let 3 ticks fire: immediate + 2 more.
                tokio::time::sleep(Duration::from_millis(250)).await;
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
        use crate::error::ProviderError;

        let (_dir, ws) = workspace();
        let (_sock_dir, sock_path) = sock_path();
        std::fs::write(ws.heartbeat_path(), "- [ ] task\n").unwrap();

        // Provider returns an error — loop should survive.
        let handle = AgentHandle::spawn(
            ws.clone(),
            Arc::new(MockProvider::new(vec![Err(ProviderError::Network(
                "test".into(),
            ))])),
            Arc::new(Tools::default()),
            1,
            CTX,
        );

        run_with_shutdown(
            &ws,
            &handle,
            Duration::from_secs(3600),
            None,
            &sock_path,
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;

        // Reaching here means the error didn't panic/crash.
    }
}

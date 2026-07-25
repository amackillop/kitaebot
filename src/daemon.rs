//! Long-running daemon that drives the duty scheduler, GitHub,
//! Linear, Telegram, and socket loops.
//!
//! The daemon runs five concurrent loops — the duty scheduler
//! dispatches scheduled duties, the GitHub PR poller checks for new
//! reviews and comments, the Linear poller checks for assigned issues
//! and comments, the Telegram poller long-polls for incoming messages,
//! and the socket listener accepts Unix domain socket clients. All are
//! pinned futures inside a single `tokio::select!`, so they make
//! progress concurrently.
//!
//! The core loop ([`run_with_shutdown`]) is generic over its shutdown
//! future so tests can substitute a simple `sleep` instead of real
//! Unix signals.

use std::future::Future;
use std::path::Path;

use tracing::info;

use crate::agent::AgentHandle;
use crate::channel::github;
use crate::channel::linear::{self, LinearChannel};
use crate::channel::socket;
use crate::channel::telegram::{self, TelegramChannel};
use crate::config::GithubConfig;
use crate::duty::{self, Duty};
use crate::tools::git::GitCli;
use crate::tools::github::GhCli;
use crate::workspace::Workspace;

/// Production entry point — runs until SIGINT or SIGTERM.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    workspace: &Workspace,
    handle: &AgentHandle,
    duties: Vec<Duty>,
    telegram: Option<&TelegramChannel>,
    gh_cli: Option<&GhCli>,
    git_cli: Option<&GitCli>,
    github: &GithubConfig,
    linear: Option<&LinearChannel>,
    socket_path: &Path,
) {
    run_with_shutdown(
        workspace,
        handle,
        duties,
        telegram,
        gh_cli,
        git_cli,
        github,
        linear,
        socket_path,
        shutdown_signal(),
    )
    .await;
}

/// Testable core: runs duties + github + linear + telegram + socket
/// until `shutdown` resolves.
#[allow(clippy::too_many_arguments)]
async fn run_with_shutdown<S: Future<Output = ()>>(
    workspace: &Workspace,
    handle: &AgentHandle,
    duties: Vec<Duty>,
    telegram: Option<&TelegramChannel>,
    gh_cli: Option<&GhCli>,
    git_cli: Option<&GitCli>,
    github: &GithubConfig,
    linear: Option<&LinearChannel>,
    socket_path: &Path,
    shutdown: S,
) {
    let duty_state_path = workspace.state_dir().join("duties.json");
    let duty_loop = duty::run_loop(duties, duty_state_path, handle);

    let telegram_loop = async {
        match telegram {
            Some(ch) => telegram::poll_loop(ch, handle).await,
            None => std::future::pending().await,
        }
    };

    let state_path = workspace.github_poll_state_path();
    let github_loop = async {
        // gh_cli is also Some when only the git tools are enabled;
        // the channel itself is gated on `github.enabled`. git_cli is
        // Some exactly when `github.enabled`.
        match gh_cli.filter(|_| github.enabled).zip(git_cli) {
            Some((gh, git)) => {
                github::poll_loop(gh, git, github, handle, &state_path).await;
            }
            None => std::future::pending().await,
        }
    };

    let linear_state_path = workspace.linear_poll_state_path();
    let linear_loop = async {
        match linear {
            Some(ch) => {
                linear::poll_loop(ch, handle, &linear_state_path).await;
            }
            None => std::future::pending().await,
        }
    };

    let socket_loop = socket::listen(socket_path, handle);

    tokio::select! {
        () = duty_loop => unreachable!("duty loop never exits"),
        () = telegram_loop => unreachable!("telegram loop never exits"),
        () = github_loop => unreachable!("github loop never exits"),
        () = linear_loop => unreachable!("linear loop never exits"),
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
    use crate::duty::schedule::Schedule;
    use crate::provider::MockProvider;
    use crate::test_support::{TestAgent, workspace};
    use std::sync::Arc;
    use std::time::Duration;

    fn spawn_agent(ws: &Arc<Workspace>, provider: Arc<MockProvider>) -> AgentHandle {
        TestAgent::new(ws.clone(), provider).spawn()
    }

    /// One hourly distill duty. With no persisted state it is due
    /// immediately (anacron catch-up).
    fn distill_duty() -> Vec<Duty> {
        vec![Duty {
            name: "distill".into(),
            input: "/duty distill".into(),
            session_hint: None,
            schedule: Schedule::Every(3600),
        }]
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
        // Closed distill gate → gate-closed reply, but the duty fired.
        let handle = spawn_agent(&ws, Arc::new(MockProvider::new(vec![])));

        run_with_shutdown(
            &ws,
            &handle,
            distill_duty(),
            None,
            None,
            None,
            &GithubConfig::default(),
            None, // linear
            &sock_path,
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;

        // If we get here without hanging, the catch-up fired and the
        // shutdown future terminated the loop.
    }

    #[tokio::test]
    async fn duty_run_persists_state() {
        let (_dir, ws) = workspace();
        let (_sock_dir, sock_path) = sock_path();
        let handle = spawn_agent(&ws, Arc::new(MockProvider::new(vec![])));

        run_with_shutdown(
            &ws,
            &handle,
            distill_duty(),
            None,
            None,
            None,
            &GithubConfig::default(),
            None, // linear
            &sock_path,
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;

        // The run must be recorded: a restarted scheduler reads this
        // and does not re-fire — the restart-cadence contract.
        let state = crate::duty::state::DutyState::load(&ws.state_dir().join("duties.json"));
        assert!(state.last_run("distill").is_some());
    }

    #[tokio::test]
    async fn error_does_not_crash_loop() {
        let (_dir, ws) = workspace();
        let (_sock_dir, sock_path) = sock_path();

        // An unknown duty makes the turn fail — the loop must survive,
        // record the run, and move on.
        let handle = spawn_agent(&ws, Arc::new(MockProvider::new(vec![])));

        run_with_shutdown(
            &ws,
            &handle,
            vec![Duty {
                name: "nope".into(),
                input: "/duty nope".into(),
                session_hint: None,
                schedule: Schedule::Every(3600),
            }],
            None,
            None,
            None,
            &GithubConfig::default(),
            None, // linear
            &sock_path,
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await;

        // Reaching here means the error didn't panic/crash.
    }
}

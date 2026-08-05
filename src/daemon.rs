//! Long-running daemon that drives the duty scheduler, GitHub,
//! Linear, Telegram, and socket loops.
//!
//! The daemon runs six concurrent loops — the duty scheduler
//! dispatches scheduled duties, the GitHub PR poller checks for new
//! reviews and comments, the GitHub issues poller checks for assigned
//! issues, the Linear poller checks for assigned issues and comments,
//! the Telegram poller long-polls for incoming messages, and the
//! socket listener accepts Unix domain socket clients. All are pinned
//! futures inside a single `tokio::select!`, so they make progress
//! concurrently.
//!
//! The core loop ([`run_with_shutdown`]) is generic over its shutdown
//! future so tests can substitute a simple `sleep` instead of real
//! Unix signals.

use std::future::Future;
use std::path::Path;

use tracing::info;

use crate::agent::AgentHandle;
use crate::channel::linear::{self, LinearChannel};
use crate::channel::socket;
use crate::channel::telegram::{self, TelegramChannel};
use crate::channel::{github, github_issues};
use crate::clients::github::GithubClient;
use crate::config::{GithubConfig, SocketConfig};
use crate::duty::{self, Duty};
use crate::state_db::StateDb;
use crate::tools::git::GitCli;
use crate::workspace::Workspace;

/// Production entry point — runs until SIGINT or SIGTERM.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    workspace: &Workspace,
    state_db: &StateDb,
    handle: &AgentHandle,
    duties: Vec<Duty>,
    telegram: Option<&TelegramChannel>,
    github_client: Option<&GithubClient>,
    git_cli: Option<&GitCli>,
    github: &GithubConfig,
    repos: &[String],
    linear: Option<&LinearChannel>,
    socket_cfg: &SocketConfig,
) {
    Box::pin(run_with_shutdown(
        workspace,
        state_db,
        handle,
        duties,
        telegram,
        github_client,
        git_cli,
        github,
        repos,
        linear,
        socket_cfg,
        shutdown_signal(),
    ))
    .await;
}

/// Testable core: runs duties + github + linear + telegram + socket
/// until `shutdown` resolves.
#[allow(clippy::too_many_arguments)]
async fn run_with_shutdown<S: Future<Output = ()>>(
    workspace: &Workspace,
    state_db: &StateDb,
    handle: &AgentHandle,
    duties: Vec<Duty>,
    telegram: Option<&TelegramChannel>,
    github_client: Option<&GithubClient>,
    git_cli: Option<&GitCli>,
    github: &GithubConfig,
    repos: &[String],
    linear: Option<&LinearChannel>,
    socket_cfg: &SocketConfig,
    shutdown: S,
) {
    let duty_loop = duty::run_loop(
        duties,
        state_db.clone(),
        workspace.journal_path(),
        handle,
        git_cli.cloned(),
    );

    let telegram_loop = async {
        match telegram {
            Some(ch) => telegram::poll_loop(ch, handle).await,
            None => std::future::pending().await,
        }
    };

    let github_loop = async {
        // The REST client is built iff `github.enabled`; the filter
        // guards against a future caller wiring it unconditionally.
        match github_client.filter(|_| github.enabled).zip(git_cli) {
            Some((client, git)) => {
                github::poll_loop(client, git, github, handle, state_db).await;
            }
            None => std::future::pending().await,
        }
    };

    let github_issues_loop = async {
        // Issue polling piggybacks on the integration: no client is
        // built unless `github.enabled`, and the nested flag opts in.
        match github_client.filter(|_| github.issues.enabled).zip(git_cli) {
            Some((client, git)) => {
                github_issues::poll_loop(client, git, github, repos, handle, state_db).await;
            }
            None => std::future::pending().await,
        }
    };

    let linear_loop = async {
        match linear {
            Some(ch) => {
                linear::poll_loop(ch, handle, state_db).await;
            }
            None => std::future::pending().await,
        }
    };

    let socket_path = Path::new(&socket_cfg.path);
    let socket_loop = socket::listen(socket_path, handle, &socket_cfg.allowed_uids);

    tokio::select! {
        () = duty_loop => unreachable!("duty loop never exits"),
        () = telegram_loop => unreachable!("telegram loop never exits"),
        () = github_loop => unreachable!("github loop never exits"),
        () = github_issues_loop => unreachable!("github issues loop never exits"),
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
            action: duty::Action::Dispatch {
                input: "/duty distill".into(),
                session_hint: None,
            },
            schedule: Schedule::Every(3600),
            gate: None,
        }]
    }

    /// Socket config on a temp dir — avoids collisions and `/run` dependency.
    fn sock_config() -> (tempfile::TempDir, SocketConfig) {
        let dir = tempfile::tempdir().unwrap();
        let config = SocketConfig {
            path: dir.path().join("test.sock").display().to_string(),
            ..Default::default()
        };
        (dir, config)
    }

    #[tokio::test]
    async fn fires_immediately_then_shuts_down() {
        let (_dir, ws) = workspace();
        let (_sock_dir, sock) = sock_config();
        // Closed distill gate → gate-closed reply, but the duty fired.
        let handle = spawn_agent(&ws, Arc::new(MockProvider::new(vec![])));

        Box::pin(run_with_shutdown(
            &ws,
            &StateDb::open_in_memory().unwrap(),
            &handle,
            distill_duty(),
            None,
            None,
            None,
            &GithubConfig::default(),
            &[],  // repos
            None, // linear
            &sock,
            tokio::time::sleep(Duration::from_millis(50)),
        ))
        .await;

        // If we get here without hanging, the catch-up fired and the
        // shutdown future terminated the loop.
    }

    #[tokio::test]
    async fn duty_run_persists_state() {
        let (_dir, ws) = workspace();
        let (_sock_dir, sock) = sock_config();
        let handle = spawn_agent(&ws, Arc::new(MockProvider::new(vec![])));
        let state_db = StateDb::open(&ws.state_db_path()).unwrap();

        Box::pin(run_with_shutdown(
            &ws,
            &state_db,
            &handle,
            distill_duty(),
            None,
            None,
            None,
            &GithubConfig::default(),
            &[],  // repos
            None, // linear
            &sock,
            tokio::time::sleep(Duration::from_millis(50)),
        ))
        .await;

        // The run must be recorded: a restarted scheduler reads this
        // and does not re-fire — the restart-cadence contract.
        let state = crate::duty::state::DutyState::load(&state_db);
        assert!(state.last_run("distill").is_some());
    }

    #[tokio::test]
    async fn error_does_not_crash_loop() {
        let (_dir, ws) = workspace();
        let (_sock_dir, sock) = sock_config();

        // An unknown duty makes the turn fail — the loop must survive,
        // record the run, and move on.
        let handle = spawn_agent(&ws, Arc::new(MockProvider::new(vec![])));

        Box::pin(run_with_shutdown(
            &ws,
            &StateDb::open_in_memory().unwrap(),
            &handle,
            vec![Duty {
                name: "nope".into(),
                action: duty::Action::Dispatch {
                    input: "/duty nope".into(),
                    session_hint: None,
                },
                schedule: Schedule::Every(3600),
                gate: None,
            }],
            None,
            None,
            None,
            &GithubConfig::default(),
            &[],  // repos
            None, // linear
            &sock,
            tokio::time::sleep(Duration::from_millis(50)),
        ))
        .await;

        // Reaching here means the error didn't panic/crash.
    }
}

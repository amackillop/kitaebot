//! Shared test scaffolding.
//!
//! One place to build a temp workspace and an [`AgentHandle`] for tests,
//! so a change to `AgentHandle::spawn`'s signature lands here instead of
//! rippling across every channel's test module.

use std::sync::Arc;

use tempfile::TempDir;

use crate::agent::AgentHandle;
use crate::config::ContextConfig;
use crate::engine::flat::FlatSession;
use crate::engine::make_summarize_fn;
use crate::memory::distill::Distiller;
use crate::notify::Notifier;
use crate::provider::MockProvider;
use crate::tools::Tools;
use crate::workspace::Workspace;

/// A temp-backed workspace and its guard. Keep the [`TempDir`] alive for
/// the test's duration; dropping it removes the directory.
pub(crate) fn workspace() -> (TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::init_at(dir.path().to_path_buf()).unwrap();
    (dir, Arc::new(ws))
}

/// Builder for a test [`AgentHandle`]. Workspace and provider are
/// required; the rest default and are overridden with the setters.
pub(crate) struct TestAgent {
    ws: Arc<Workspace>,
    provider: Arc<MockProvider>,
    heartbeat_provider: Arc<MockProvider>,
    tools: Tools,
    notifier: Option<Arc<Notifier>>,
    max_iterations: usize,
}

impl TestAgent {
    /// Defaults: the root provider doubles as the heartbeat and memory
    /// role, no extra tools, no notifier, a single tool-loop iteration.
    pub(crate) fn new(ws: Arc<Workspace>, provider: Arc<MockProvider>) -> Self {
        Self {
            heartbeat_provider: provider.clone(),
            ws,
            provider,
            tools: Tools::default(),
            notifier: None,
            max_iterations: 1,
        }
    }

    pub(crate) fn tools(mut self, tools: Tools) -> Self {
        self.tools = tools;
        self
    }

    pub(crate) fn heartbeat_provider(mut self, provider: Arc<MockProvider>) -> Self {
        self.heartbeat_provider = provider;
        self
    }

    pub(crate) fn notifier(mut self, notifier: Arc<Notifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    pub(crate) fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    pub(crate) fn spawn(self) -> AgentHandle {
        let engine = FlatSession::new(
            self.ws.sessions_dir(),
            self.ws.state_dir(),
            ContextConfig::default(),
        )
        .unwrap();
        let summarize = make_summarize_fn(self.provider.clone());
        // Threshold far above any test transcript, so a /heartbeat turn
        // never trips the distillation gate.
        let distiller = Arc::new(Distiller::new(&Tools::default(), self.ws.path(), 40_000, 1));
        AgentHandle::spawn(
            self.ws,
            self.provider.clone(),
            self.heartbeat_provider,
            self.provider,
            Arc::new(self.tools),
            distiller,
            self.max_iterations,
            8192,
            engine,
            summarize,
            self.notifier,
            None,
        )
    }
}

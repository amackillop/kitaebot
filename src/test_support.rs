//! Shared test scaffolding.
//!
//! One place to build a temp workspace and an [`AgentHandle`] for tests,
//! so a change to `AgentHandle::spawn`'s signature lands here instead of
//! rippling across every channel's test module.

use std::sync::Arc;

use tempfile::TempDir;

use crate::agent::AgentHandle;
use crate::config::ContextConfig;
use crate::context::flat::FlatSession;
use crate::context::make_summarize_fn;
use crate::duty::TriggerHandle;
use crate::memory::distill::Distiller;
use crate::notify::Notifier;
use crate::provider::MockProvider;
use crate::tools::DirenvCache;
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
    /// `None` mirrors an unset `model_overrides.planner`: the root
    /// provider serves planner turns too.
    planner_provider: Option<Arc<MockProvider>>,
    memory_provider: Arc<MockProvider>,
    tools: Tools,
    notifier: Option<Arc<Notifier>>,
    max_iterations: usize,
    duty_trigger: Option<TriggerHandle>,
}

impl TestAgent {
    /// Defaults: the root provider doubles as the memory role, no
    /// extra tools, no notifier, a single tool-loop iteration.
    pub(crate) fn new(ws: Arc<Workspace>, provider: Arc<MockProvider>) -> Self {
        Self {
            memory_provider: provider.clone(),
            ws,
            provider,
            planner_provider: None,
            tools: Tools::default(),
            notifier: None,
            max_iterations: 1,
            duty_trigger: None,
        }
    }

    pub(crate) fn planner(mut self, provider: Arc<MockProvider>) -> Self {
        self.planner_provider = Some(provider);
        self
    }

    pub(crate) fn tools(mut self, tools: Tools) -> Self {
        self.tools = tools;
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

    pub(crate) fn duty_trigger(mut self, handle: TriggerHandle) -> Self {
        self.duty_trigger = Some(handle);
        self
    }

    pub(crate) fn spawn(self) -> AgentHandle {
        let engine = FlatSession::new(&self.ws.context_dir(), ContextConfig::default()).unwrap();
        let summarize = make_summarize_fn(self.provider.clone());
        // Threshold far above any test transcript, so a duty turn
        // never trips the distillation gate.
        let distiller = Arc::new(Distiller::new(
            &Tools::default(),
            self.ws.path(),
            crate::state_db::StateDb::open_in_memory().unwrap(),
            40_000,
            10_000,
            1,
            8192,
        ));
        let planner = self
            .planner_provider
            .unwrap_or_else(|| self.provider.clone());
        AgentHandle::spawn(
            self.ws,
            self.provider,
            planner,
            self.memory_provider,
            Arc::new(self.tools),
            distiller,
            self.max_iterations,
            crate::agent::PromptConfig {
                memory_index_cap: 8192,
                trusted_repos: Vec::new(),
            },
            engine,
            summarize,
            self.notifier,
            None,
            None,
            self.duty_trigger,
        )
    }
}

/// A fake `direnv` shell script, injected through [`DirenvCache`]'s
/// binary seam — no PATH mutation, so fakes coexist across concurrent
/// tests.
pub(crate) struct FakeDirenv {
    _dir: TempDir,
    binary: &'static str,
}

impl FakeDirenv {
    pub(crate) fn install(body: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("direnv");
        std::fs::write(&script, format!("#!/bin/sh\n{body}")).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // SubprocessCall's binary is &'static str; one leaked path per
        // test, reclaimed at process exit.
        let binary = Box::leak(script.display().to_string().into_boxed_str());
        Self { _dir: dir, binary }
    }

    /// A [`DirenvCache`] that spawns this fake.
    pub(crate) fn cache(&self) -> DirenvCache {
        DirenvCache::with_binary(self.binary)
    }
}

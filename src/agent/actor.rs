//! Agent actor run loop.
//!
//! The [`Agent`] struct owns the engine, provider, tools, and config.
//! It processes one [`Envelope`] at a time in a sequential loop, which
//! eliminates the need for session locking or `Arc<Mutex<Session>>`.
//!
//! Spawned by [`AgentHandle::spawn`](super::AgentHandle::spawn).

use std::sync::Arc;

use tracing::{Instrument, error, field, info_span};

use crate::commands;
use crate::context::names::display_name;
use crate::context::{ContextEngine, SummarizeFn};
use crate::dispatch::{Input, Reply};
use crate::duty::TriggerHandle;
use crate::memory::distill::Distiller;
use crate::notify::Notifier;
use crate::provider::Provider;
use crate::review::ReviewLedger;
use crate::tools::Tools;
use crate::usage::{self, TaskKey, TurnRecord, UsageLedger};
use crate::workspace::Workspace;
use tokio::sync::mpsc;

use super::PromptConfig;
use super::envelope::{ChannelSource, Envelope, InputEnvelope};

/// The actor that processes envelopes sequentially.
///
/// Owns all dependencies so the run loop has no borrows and is `'static`.
pub(super) struct Agent<P: Provider, E: ContextEngine> {
    rx: mpsc::Receiver<Envelope>,
    workspace: Arc<Workspace>,
    provider: Arc<P>,
    memory_provider: Arc<P>,
    tools: Arc<Tools>,
    distiller: Arc<Distiller>,
    max_iterations: usize,
    prompt: PromptConfig,
    engine: E,
    summarize: SummarizeFn,
    notifier: Option<Arc<Notifier>>,
    usage_ledger: Option<Arc<UsageLedger>>,
    review_ledger: Option<Arc<ReviewLedger>>,
    duty_trigger: Option<TriggerHandle>,
    /// Monotonic turn counter, the `id` field of the per-turn log span.
    turn_seq: u64,
}

impl<P: Provider + 'static, E: ContextEngine + 'static> Agent<P, E> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rx: mpsc::Receiver<Envelope>,
        workspace: Arc<Workspace>,
        provider: Arc<P>,
        memory_provider: Arc<P>,
        tools: Arc<Tools>,
        distiller: Arc<Distiller>,
        max_iterations: usize,
        prompt: PromptConfig,
        engine: E,
        summarize: SummarizeFn,
        notifier: Option<Arc<Notifier>>,
        usage_ledger: Option<Arc<UsageLedger>>,
        review_ledger: Option<Arc<ReviewLedger>>,
        duty_trigger: Option<TriggerHandle>,
    ) -> Self {
        Self {
            rx,
            workspace,
            provider,
            memory_provider,
            tools,
            distiller,
            max_iterations,
            prompt,
            engine,
            summarize,
            notifier,
            usage_ledger,
            review_ledger,
            duty_trigger,
            turn_seq: 0,
        }
    }

    /// Consume envelopes until all handles are dropped.
    pub async fn run(mut self) {
        while let Some(envelope) = self.rx.recv().await {
            match envelope {
                Envelope::Input(input) => {
                    // Every event inside the turn inherits this span;
                    // `session` is recorded once the target is known.
                    let span = info_span!(
                        "turn",
                        id = self.turn_seq,
                        source = %input.source,
                        session = field::Empty,
                    );
                    self.turn_seq += 1;
                    let result = self.handle(&input).instrument(span).await;
                    let _ = input.reply_tx.send(result);
                    // The reply is out, so rewriting history now can
                    // cost at most one cache hit (the next turn's first
                    // completion); mid-turn it costs every remaining
                    // one. A queued envelope makes even that one likely
                    // real — the next turn arrives inside the provider
                    // cache's TTL — so compaction waits for the mailbox
                    // to drain. A session that never idles is bounded
                    // by the hard threshold instead.
                    if self.rx.is_empty() {
                        match self.engine.compact_between_turns(&self.summarize).await {
                            Ok(Some(event)) => {
                                tracing::info!(
                                    before = event.before,
                                    after = event.after,
                                    "compacted between turns"
                                );
                            }
                            Ok(None) => {}
                            Err(e) => tracing::error!("between-turns compaction failed: {e}"),
                        }
                    } else {
                        tracing::debug!("mailbox non-empty; deferring between-turns compaction");
                    }
                }
                Envelope::Greeting(reply_tx) => {
                    let _ = reply_tx.send(self.format_greeting());
                }
            }
        }
    }

    /// Greeting derived from current engine state.
    fn format_greeting(&self) -> String {
        let active = display_name(self.engine.active_session());
        let count = self.engine.stats().message_count;
        if count == 0 {
            format!("New session: {active}")
        } else {
            format!("Resumed: {active} ({count} messages)")
        }
    }

    /// Route `/duty <name>` and `/duties` from operator sources to the
    /// scheduler's trigger channel — the shared execution path (spec
    /// 24). Duty-source commands fall through: the scheduler itself
    /// dispatches `/duty distill` into the actor, and forwarding those
    /// would loop. Returns `None` when the command is not duty-shaped
    /// or no trigger channel is wired.
    fn forward_duty(
        &self,
        cmd: &commands::SlashCommand,
        source: &ChannelSource,
    ) -> Option<Result<Reply, String>> {
        use commands::SlashCommand;
        if matches!(source, ChannelSource::Duty { .. }) {
            return None;
        }
        let trigger = self.duty_trigger.as_ref()?;
        let name = match cmd {
            SlashCommand::Duties => None,
            SlashCommand::Duty { name } => {
                if !trigger.names.iter().any(|n| n == name) {
                    return Some(Ok(Reply::text(format!(
                        "Unknown duty {name:?}; known: {}",
                        trigger.names.join(", "),
                    ))));
                }
                Some(name.clone())
            }
            _ => return None,
        };
        let described = name.as_deref().unwrap_or("all duties");
        Some(
            match trigger
                .tx
                .try_send(crate::duty::Trigger { name: name.clone() })
            {
                Ok(()) => Ok(Reply::text(format!(
                    "Queued: {described} — gates respected, outcomes land in the journal.",
                ))),
                Err(e) => Err(format!("duty trigger queue unavailable: {e}")),
            },
        )
    }

    async fn handle(&mut self, envelope: &InputEnvelope) -> Result<Reply, String> {
        let result = match Input::parse(&envelope.input) {
            Ok(Input::Command(cmd)) => {
                if let Some(reply) = self.forward_duty(&cmd, &envelope.source) {
                    reply
                } else {
                    commands::execute(
                        cmd,
                        &mut self.engine,
                        &self.summarize,
                        &self.workspace,
                        &*self.memory_provider,
                        &self.distiller,
                        self.usage_ledger.as_deref(),
                        self.review_ledger.as_deref(),
                        &TaskKey::for_source(&envelope.source),
                    )
                    .await
                }
            }
            Ok(Input::Message(text)) => self.handle_message(envelope, text).await,
            Err(_) => Err(format!("Unknown command: {}", envelope.input)),
        };

        // Nobody reads an unattended reply; a failure there would
        // vanish into the logs.
        if let Err(problem) = &result
            && !envelope.source.is_attended()
            && let Some(notifier) = &self.notifier
        {
            notifier
                .alert(&format!("{}: turn failed: {problem}", envelope.source))
                .await;
        }

        // Unattended outcomes are the bot's autonomous work record:
        // they land in the journal, where nothing else preserves them
        // (spec 05). Routine no-ops — a closed gate, nothing to do —
        // stay in the tracing log.
        if !envelope.source.is_attended() {
            let entry = match &result {
                Ok(reply) if !reply.routine => {
                    Some(format!("{}: {}", envelope.source, reply.content))
                }
                Err(e) => Some(format!("{}: failed: {e}", envelope.source)),
                Ok(_) => None,
            };
            if let Some(entry) = entry
                && let Err(e) = crate::workspace::journal(
                    &self.workspace.journal_path(),
                    envelope.source.topic(),
                    &entry,
                )
            {
                tracing::warn!("failed to journal unattended outcome: {e}");
            }
        }

        result
    }

    /// Process a free-text message, optionally switching sessions for the turn.
    ///
    /// If `envelope.session_hint` differs from the active session, switch to it
    /// before processing and restore the original active session afterward.
    /// This is how GitHub PRs get routed to per-repo sessions while keeping
    /// Telegram/Socket on whatever the user's `/project` selection was.
    async fn handle_message(
        &mut self,
        envelope: &InputEnvelope,
        text: &str,
    ) -> Result<Reply, String> {
        let original = self.engine.active_session().to_string();
        let target = envelope.session_hint.as_deref().unwrap_or(&original);
        let switched = target != original;
        // Display rendering (spec 14): the span shows `owner/repo`,
        // the same form every other operator-facing surface shows,
        // never the sanitized storage name.
        tracing::Span::current().record("session", display_name(target));

        if switched {
            // switch_session saves the current session before loading the target.
            if let Err(e) = self.engine.switch_session(target).await {
                return Err(format!("Failed to switch session: {e}"));
            }
        }

        if let Some(notifier) = &self.notifier {
            notifier.begin_turn();
        }

        let tagged = format!("[{}]: {text}", envelope.source);
        // Derived once for both the ToolCtx and the ledger row, so the
        // sub-agent inheritance and the root attribution cannot diverge.
        let task = TaskKey::for_source(&envelope.source);
        let metered = super::process_message_metered(
            &mut self.engine,
            &self.summarize,
            &self.workspace,
            &tagged,
            &*self.provider,
            &self.tools,
            self.max_iterations,
            &self.prompt,
            self.review_ledger.is_some(),
            super::role_segments(&envelope.source),
            &crate::tools::ToolCtx {
                activity: envelope.activity_tx.clone(),
                cancel: envelope.cancel.clone(),
                task: Some(task.clone()),
            },
        )
        .await;

        // Bill the turn whatever its outcome: the calls were made.
        let (result, meter) = metered;
        let source = envelope.source.to_string();
        usage::record_turn(
            self.usage_ledger.as_deref(),
            &TurnRecord {
                session: target,
                source: &source,
                model: self.provider.model(),
                task: Some(&task),
                meter,
            },
        );

        // A policy halt or tool halt is an Ok reply, so the Err hook
        // in `handle` never sees it. Alert here where the outcome is
        // still typed.
        if let Ok(halt) = &result
            && !envelope.source.is_attended()
            && let Some(notifier) = &self.notifier
        {
            match halt {
                super::TurnOutput::PolicyHalt { reasons } => {
                    notifier
                        .alert(&format!(
                            "{}: turn halted by policy gate: {}",
                            envelope.source,
                            reasons.join("; ")
                        ))
                        .await;
                }
                super::TurnOutput::ToolHalt {
                    tool,
                    error_class,
                    count,
                    ..
                } => {
                    notifier
                        .alert(&format!(
                            "{}: turn halted by tool strike: \
                             {tool} failed {count}x ({error_class})",
                            envelope.source,
                        ))
                        .await;
                }
                super::TurnOutput::Text(_) => {}
            }
        }

        // Deliver batched low-urgency notifications after every turn —
        // success, error, and cancellation alike.
        if let Some(notifier) = &self.notifier {
            notifier.flush().await;
        }

        if switched {
            // Restore. switch_session saves the target before loading original.
            if let Err(e) = self.engine.switch_session(&original).await {
                error!("Failed to restore active session '{original}': {e}");
            }
        } else if let Err(e) = self.engine.save().await {
            error!("Failed to save session: {e}");
        }

        result
            .map(|out| Reply::text(out.into_text()))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentHandle;
    use crate::agent::envelope::{ChannelSource, GitHubRole};
    use crate::provider::MockProvider;
    use crate::test_support::{TestAgent, workspace};
    use crate::types::Response;
    use tokio_util::sync::CancellationToken;

    fn spawn_agent(ws: Arc<Workspace>, provider: Arc<MockProvider>) -> AgentHandle {
        spawn_agent_with(ws, provider, Tools::default(), None, 1)
    }

    fn spawn_agent_with(
        ws: Arc<Workspace>,
        provider: Arc<MockProvider>,
        tools: Tools,
        notifier: Option<Arc<Notifier>>,
        max_iterations: usize,
    ) -> AgentHandle {
        let mut builder = TestAgent::new(ws, provider)
            .tools(tools)
            .max_iterations(max_iterations);
        if let Some(notifier) = notifier {
            builder = builder.notifier(notifier);
        }
        builder.spawn()
    }

    #[tokio::test]
    async fn text_roundtrip() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![Ok(Response::Text(
            "hello back".into(),
        ))]));

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hello".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(result.unwrap().content, "hello back");
    }

    #[tokio::test]
    async fn slash_new_clears_session() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "/new".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(result.unwrap().content, "Session cleared.");
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "/bogus".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown command"));
    }

    #[tokio::test]
    async fn cancelled_token_returns_error() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let cancel = CancellationToken::new();
        cancel.cancel();

        let handle = spawn_agent(ws, provider);
        let result = handle
            .send_message(ChannelSource::Socket, "hi".into(), None, None, cancel)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sequential_messages_share_session() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text("first".into())),
            Ok(Response::Text("second".into())),
        ]));

        let handle = spawn_agent(ws, provider);

        let r1 = handle
            .send_message(
                ChannelSource::Telegram,
                "msg1".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r1.unwrap().content, "first");

        let r2 = handle
            .send_message(
                ChannelSource::Telegram,
                "msg2".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r2.unwrap().content, "second");
    }

    #[tokio::test]
    async fn session_hint_routes_to_named_session() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text("first".into())),
            Ok(Response::Text("second".into())),
        ]));

        let handle = spawn_agent(ws.clone(), provider);

        // Default active session is "general". Send to "owner/repo" via hint.
        let r1 = handle
            .send_message(
                ChannelSource::GitHub {
                    pr_number: 1,
                    repo: "owner/repo".into(),
                    role: GitHubRole::Author,
                },
                "github msg".into(),
                Some("owner/repo".into()),
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r1.unwrap().content, "first");

        // The next message has no hint -- should land in "general", not "owner/repo".
        let r2 = handle
            .send_message(
                ChannelSource::Socket,
                "socket msg".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(r2.unwrap().content, "second");

        // Verify on disk: each session has exactly one user message.
        let sessions = ws.context_dir().join("flat/sessions");
        let general = std::fs::read_to_string(sessions.join("general.json")).unwrap();
        let github = std::fs::read_to_string(sessions.join("owner--repo.json")).unwrap();
        assert!(general.contains("socket msg"));
        assert!(!general.contains("github msg"));
        assert!(github.contains("github msg"));
        assert!(!github.contains("socket msg"));
    }

    /// The turn span's `session` field is display rendering (spec 14):
    /// `owner/repo`, never the sanitized storage name — the same
    /// conversation must not render under two names in the log. The
    /// leak case is a no-hint turn (the span records the *active*
    /// session, stored sanitized), so the test first makes a
    /// repo-style session active via `/project`.
    #[tokio::test]
    async fn turn_span_shows_desanitized_session() {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        /// Captures values recorded onto span fields named `session`.
        struct Capture(Arc<std::sync::Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>> Layer<S>
            for Capture
        {
            fn on_record(
                &self,
                _id: &tracing::span::Id,
                values: &tracing::span::Record<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut visit = SessionField(None);
                values.record(&mut visit);
                if let Some(s) = visit.0 {
                    self.0.lock().unwrap().push(s);
                }
            }
        }
        struct SessionField(Option<String>);
        impl tracing::field::Visit for SessionField {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "session" {
                    self.0 = Some(value.to_string());
                }
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "session" {
                    self.0 = Some(format!("{value:?}"));
                }
            }
        }

        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text("first".into())),
            Ok(Response::Text("second".into())),
        ]));
        let handle = spawn_agent(ws, provider);

        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = recorded.clone();
        let subscriber = tracing_subscriber::registry().with(Capture(captured));
        let _guard = tracing::subscriber::set_default(subscriber);

        // Create the repo-style session (GitHub turns route by hint).
        handle
            .send_message(
                ChannelSource::GitHub {
                    pr_number: 1,
                    repo: "owner/repo".into(),
                    role: GitHubRole::Author,
                },
                "github msg".into(),
                Some("owner/repo".into()),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // Make it the persisted active session.
        handle
            .send_message(
                ChannelSource::Socket,
                "/project owner/repo".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // The leak case: no hint, so the span records the active
        // session — stored sanitized, displayed desanitized.
        handle
            .send_message(
                ChannelSource::Socket,
                "socket msg".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let sessions = recorded.lock().unwrap().clone();
        assert!(
            !sessions.is_empty(),
            "no session field was recorded onto a turn span"
        );
        assert!(
            sessions.iter().all(|s| s == "owner/repo"),
            "sanitized session name leaked into a turn span: {sessions:?}"
        );
    }

    /// Notifier over a fake Telegram client that records sendMessage bodies.
    fn fake_notifier(sent: &Arc<std::sync::Mutex<Vec<serde_json::Value>>>) -> Arc<Notifier> {
        use crate::clients::RawResponse;
        use crate::clients::telegram::TelegramClient;

        let sent = sent.clone();
        let client = TelegramClient::from_fn(move |_method, body| {
            let sent = sent.clone();
            async move {
                sent.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                Ok(RawResponse {
                    status: 200,
                    body: br#"{"ok":true,"result":{"message_id":1,"chat":{"id":42},"text":null}}"#
                        .to_vec(),
                    retry_after_secs: None,
                })
            }
        });
        Arc::new(Notifier::new(client, 42))
    }

    fn notify_call() -> Response {
        use crate::types::{ToolCall, ToolFunction};
        Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "n1".to_string(),
                ToolFunction {
                    name: "notify".parse().unwrap(),
                    arguments: r#"{"message":"ping"}"#.to_string(),
                },
            )],
        }
    }

    #[tokio::test]
    async fn batched_notification_delivered_after_turn() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(notify_call()),
            Ok(Response::Text("done".into())),
        ]));

        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifier = fake_notifier(&sent);
        let tools = Tools::new(
            vec![Arc::new(crate::notify::NotifyTool(notifier.clone()))],
            &[],
        )
        .unwrap();

        let handle = spawn_agent_with(ws, provider, tools, Some(notifier), 2);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hi".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.unwrap().content, "done");

        let calls = sent.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["text"], "ping");
        assert_eq!(calls[0]["chat_id"], 42);
    }

    #[tokio::test]
    async fn batched_notification_delivered_when_turn_errors() {
        let (_dir, ws) = workspace();
        // The notification is buffered on iteration 1; iteration 2 fails.
        let provider = Arc::new(MockProvider::new(vec![
            Ok(notify_call()),
            Err(crate::error::ProviderError::Network("boom".into())),
        ]));

        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifier = fake_notifier(&sent);
        let tools = Tools::new(
            vec![Arc::new(crate::notify::NotifyTool(notifier.clone()))],
            &[],
        )
        .unwrap();

        let handle = spawn_agent_with(ws, provider, tools, Some(notifier), 2);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hi".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());

        let calls = sent.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["text"], "ping");
    }

    fn blocked_call_response() -> Response {
        use crate::types::{ToolCall, ToolFunction};
        Response::ToolCalls {
            content: String::new(),
            calls: vec![ToolCall::new(
                "b1".to_string(),
                ToolFunction {
                    name: "mock_blocked".parse().unwrap(),
                    arguments: "{}".to_string(),
                },
            )],
        }
    }

    fn blocked_tools() -> Tools {
        Tools::new(
            vec![Arc::new(crate::tools::MockBlockedTool::new("not allowed"))],
            &[],
        )
        .unwrap()
    }

    fn github_source() -> ChannelSource {
        ChannelSource::GitHub {
            pr_number: 7,
            repo: "owner/repo".into(),
            role: GitHubRole::Reviewer,
        }
    }

    #[tokio::test]
    async fn unattended_policy_halt_sends_alert() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_call_response()),
            Ok(blocked_call_response()),
        ]));

        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifier = fake_notifier(&sent);

        let handle = spawn_agent_with(ws, provider, blocked_tools(), Some(notifier), 5);
        let result = handle
            .send_message(
                github_source(),
                "review this".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.unwrap().content.contains("halted automatically"));

        let calls = sent.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let text = calls[0]["text"].as_str().unwrap();
        assert!(text.contains("GitHub PR #7"), "got: {text}");
        assert!(text.contains("halted by policy gate"), "got: {text}");
    }

    #[tokio::test]
    async fn attended_policy_halt_does_not_alert() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(blocked_call_response()),
            Ok(blocked_call_response()),
        ]));

        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifier = fake_notifier(&sent);

        let handle = spawn_agent_with(ws, provider, blocked_tools(), Some(notifier), 5);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hi".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.unwrap().content.contains("halted automatically"));

        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unattended_outcomes_are_journaled_and_attended_are_not() {
        let (_dir, ws) = workspace();
        let journal = ws.journal_path();
        let provider = Arc::new(MockProvider::new(vec![
            Ok(Response::Text("reviewed the PR".into())),
            Ok(Response::Text("chat answer".into())),
        ]));
        let handle = spawn_agent(ws, provider);

        handle
            .send_message(
                github_source(),
                "review this".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        handle
            .send_message(
                ChannelSource::Socket,
                "hi".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let text = std::fs::read_to_string(&journal).unwrap();
        assert!(text.contains("[github]"), "unattended reply journaled");
        assert!(text.contains("reviewed the PR"));
        assert!(
            !text.contains("chat answer"),
            "attended replies have a reader; they stay out"
        );
    }

    #[tokio::test]
    async fn routine_replies_stay_out_of_the_journal() {
        let (_dir, ws) = workspace();
        let journal = ws.journal_path();
        let provider = Arc::new(MockProvider::new(vec![]));
        let handle = spawn_agent(ws, provider);

        // Closed distill gate: a routine no-op reply on an unattended
        // duty turn.
        let reply = handle
            .send_message(
                ChannelSource::Duty {
                    duty: "distill".into(),
                },
                "/duty distill".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(reply.routine);
        assert!(
            !journal.exists(),
            "a closed gate is mechanics, not a journal event"
        );
    }

    #[tokio::test]
    async fn unattended_turn_error_sends_alert() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![Err(
            crate::error::ProviderError::Network("boom".into()),
        )]));

        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifier = fake_notifier(&sent);

        let handle = spawn_agent_with(ws, provider, Tools::default(), Some(notifier), 1);
        let result = handle
            .send_message(
                github_source(),
                "review this".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());

        let calls = sent.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let text = calls[0]["text"].as_str().unwrap();
        assert!(text.contains("GitHub PR #7"), "got: {text}");
        assert!(text.contains("turn failed"), "got: {text}");
    }

    #[tokio::test]
    async fn attended_turn_error_does_not_alert() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![Err(
            crate::error::ProviderError::Network("boom".into()),
        )]));

        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let notifier = fake_notifier(&sent);

        let handle = spawn_agent_with(ws, provider, Tools::default(), Some(notifier), 1);
        let result = handle
            .send_message(
                ChannelSource::Socket,
                "hi".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());

        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn distill_duty_reports_closed_gate() {
        let (_dir, ws) = workspace();
        // The root provider has no responses: a duty turn must not
        // hit it — the gate is closed and no distillation runs.
        let root = Arc::new(MockProvider::new(vec![]));

        let handle = spawn_agent_with(ws, root.clone(), Tools::default(), None, 1);
        let result = handle
            .send_message(
                ChannelSource::Duty {
                    duty: "distill".into(),
                },
                "/duty distill".into(),
                None,
                None,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(result.unwrap().content, "Distillation gate closed.");
        assert_eq!(root.call_count(), 0);
    }

    #[tokio::test]
    async fn drop_handle_shuts_down_actor() {
        let (_dir, ws) = workspace();
        let provider = Arc::new(MockProvider::new(vec![]));

        let handle = spawn_agent(ws, provider);
        drop(handle);
    }
    #[tokio::test]
    async fn duty_commands_route_to_trigger_channel() {
        let (_dir, ws) = crate::test_support::workspace();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let handle = TestAgent::new(ws, Arc::new(MockProvider::new(vec![])))
            .duty_trigger(crate::duty::TriggerHandle {
                names: vec!["warm".into()],
                tx,
            })
            .spawn();

        let send = |input: &str| {
            handle.send_message(
                ChannelSource::Socket,
                input.into(),
                None,
                None,
                CancellationToken::new(),
            )
        };

        let reply = send("/duty warm").await.unwrap();
        assert!(reply.content.contains("Queued: warm"), "{}", reply.content);
        let t = rx.try_recv().unwrap();
        assert_eq!(t.name.as_deref(), Some("warm"));

        let reply = send("/duties").await.unwrap();
        assert!(
            reply.content.contains("Queued: all duties"),
            "{}",
            reply.content
        );
        let t = rx.try_recv().unwrap();
        assert_eq!(t.name, None);

        let reply = send("/duty nope").await.unwrap();
        assert!(
            reply.content.contains("Unknown duty \"nope\""),
            "{}",
            reply.content
        );
        assert!(rx.try_recv().is_err(), "unknown names must not queue");
    }
}

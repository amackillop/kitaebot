//! Append-only ledger of per-turn cost.
//!
//! Every completed turn records one row: the session and source that
//! drove it, the model that billed it, the summed token counts, and the
//! charged cost. Rows are stamped with the build's git revision so a
//! cost shift can be attributed to the change that caused it.
//!
//! This is telemetry, not core state: a write failure is logged by the
//! caller and never fails the turn.

use std::cmp::Ordering;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::agent::envelope::ChannelSource;
use crate::clients::openrouter_pricing::PricingClient;
use crate::config::ModelRates;
use crate::state_db::StateDb;
use tracing::warn;

use crate::agent::TurnMeter;

/// The ledger's SQL, as consts so the schema-drift test in
/// `state_db` can prepare every query against the migrated schema.
pub(crate) const SELECT_TURN_ROWS: &str =
    "SELECT git_sha, model, prompt_tokens, completion_tokens, cost,
            task, started_at, duration_ms, cached_tokens, provider
         FROM turns ORDER BY id";

pub(crate) const INSERT_TURN: &str = "INSERT INTO turns
         (git_sha, session, source, model, task,
          calls, prompt_tokens, cached_tokens, completion_tokens, cost,
          started_at, duration_ms, outcome, provider)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";

/// The build's git revision, injected by the flake at compile time.
/// `None` in plain `cargo` dev builds, where the env var is unset.
const GIT_SHA: Option<&str> = option_env!("GIT_SHA");

/// The ledger's grouping identity for a unit of work (spec 27): the
/// issue, PR, duty, or chat surface a turn belongs to. Deliberately
/// decoupled from [`ChannelSource`]'s Display strings, which feed the
/// model-visible input tag, journal, and alerts — rewording those must
/// not split ledger history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskKey(String);

impl TaskKey {
    /// Derive the key from a dispatch's source. PR roles fold: the
    /// feedback, contributor, and reviewer turns on one PR are one
    /// task. Interactive channels aggregate under one key each.
    pub fn for_source(source: &ChannelSource) -> Self {
        let key = match source {
            ChannelSource::Duty { duty } => format!("duty:{duty}"),
            ChannelSource::GitHub {
                pr_number, repo, ..
            } => format!("pr:{repo}#{pr_number}"),
            ChannelSource::GitHubIssue { issue } => format!("issue:{issue}"),
            ChannelSource::Linear { issue } => format!("linear:{issue}"),
            ChannelSource::Socket => "chat:socket".to_string(),
            ChannelSource::Telegram => "chat:telegram".to_string(),
        };
        Self(key)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One turn's context, paired with its [`TurnMeter`] at write time.
/// Borrowed — nothing is retained past the insert.
pub struct TurnRecord<'a> {
    pub session: &'a str,
    pub source: &'a str,
    pub model: &'a str,
    /// `None` only where no task exists: legacy rows and turns with no
    /// dispatch identity (tests, the distiller's ephemeral engine).
    pub task: Option<&'a TaskKey>,
    pub meter: TurnMeter,
}

/// Append-only ledger of per-turn usage, on the shared operational
/// database ([`StateDb`]); the `turns` schema lives in its baseline
/// migration.
pub struct UsageLedger {
    conn: Arc<Mutex<Connection>>,
    /// Operator-supplied prices for the report's savings estimate;
    /// carried here so command sites need no extra config plumbing.
    rates: HashMap<String, ModelRates>,
    /// Live endpoint-pricing lookups; `None` when the API has no
    /// pricing endpoint (non-`OpenRouter`, e2e fixtures).
    pricing: Option<PricingClient>,
}

impl UsageLedger {
    pub fn new(db: &StateDb, rates: HashMap<String, ModelRates>) -> Self {
        Self {
            conn: db.connection(),
            rates,
            pricing: None,
        }
    }

    pub fn with_pricing(self, pricing: PricingClient) -> Self {
        Self {
            pricing: Some(pricing),
            ..self
        }
    }

    pub fn rates(&self) -> &HashMap<String, ModelRates> {
        &self.rates
    }

    /// Live per-endpoint rates for the models these rows need priced,
    /// keyed `(model, provider_name)`. Best-effort report garnish: no
    /// client, a fetch failure, or an unknown model degrades to an
    /// empty or partial map (the row then falls back to config rates
    /// or counts as unpriced), never to a failed report.
    pub async fn live_rates(&self, rows: &[TurnRow]) -> HashMap<(String, String), ModelRates> {
        let mut out = HashMap::new();
        let Some(pricing) = &self.pricing else {
            return out;
        };
        // Only models with at least one priceable row: cache data plus
        // a recorded endpoint, not already covered by config override.
        let mut models: Vec<&str> = rows
            .iter()
            .filter(|r| {
                r.cached_tokens.is_some()
                    && r.provider.is_some()
                    && !self.rates.contains_key(&r.model)
            })
            .map(|r| r.model.as_str())
            .collect();
        models.sort_unstable();
        models.dedup();
        for model in models {
            match pricing.endpoint_rates(model).await {
                Ok(endpoints) => {
                    for e in endpoints {
                        out.insert((model.to_string(), e.provider_name), e.rates);
                    }
                }
                Err(e) => warn!("Endpoint pricing unavailable for {model}: {e}"),
            }
        }
        out
    }

    /// Read every recorded turn, projected to the columns the report
    /// aggregates (`outcome` deliberately stays unread until a view
    /// consumes it). The ledger is prunable, so an unbounded read is
    /// fine.
    pub fn rows(&self) -> rusqlite::Result<Vec<TurnRow>> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stmt = conn.prepare(SELECT_TURN_ROWS)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TurnRow {
                    git_sha: r.get(0)?,
                    model: r.get(1)?,
                    prompt_tokens: r.get::<_, i64>(2)?.cast_unsigned(),
                    completion_tokens: r.get::<_, i64>(3)?.cast_unsigned(),
                    cost: r.get(4)?,
                    task: r.get(5)?,
                    started_at: r.get::<_, Option<i64>>(6)?.map(i64::cast_unsigned),
                    duration_ms: r.get::<_, Option<i64>>(7)?.map(i64::cast_unsigned),
                    cached_tokens: r.get::<_, Option<i64>>(8)?.map(i64::cast_unsigned),
                    provider: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Append one turn.
    pub fn record(&self, turn: &TurnRecord) -> rusqlite::Result<()> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Total conversion: a duration that overflows i64 milliseconds
        // is a clock bug, not a reason to lose the row.
        let duration_ms = i64::try_from(turn.meter.duration.as_millis()).unwrap_or(i64::MAX);
        conn.execute(
            INSERT_TURN,
            params![
                GIT_SHA,
                turn.session,
                turn.source,
                turn.model,
                turn.task.map(TaskKey::as_str),
                turn.meter.usage.calls,
                turn.meter.usage.prompt_tokens.cast_signed(),
                turn.meter.usage.cached_tokens.map(u64::cast_signed),
                turn.meter.usage.completion_tokens.cast_signed(),
                turn.meter.usage.cost,
                turn.meter.started_at.cast_signed(),
                duration_ms,
                turn.meter.outcome,
                turn.meter.usage.provider.as_deref(),
            ],
        )?;
        Ok(())
    }
}

/// Record a turn to the ledger if one is configured. A write failure is
/// logged, never propagated — telemetry must not fail the turn.
pub fn record_turn(ledger: Option<&UsageLedger>, record: &TurnRecord) {
    if let Some(ledger) = ledger
        && let Err(e) = ledger.record(record)
    {
        warn!("Failed to record turn usage: {e}");
    }
}

/// One ledger row projected to the columns [`report`] aggregates.
/// The Options mark era boundaries: rows predating a migration carry
/// NULLs, and the report renders absence, never zero.
pub struct TurnRow {
    pub git_sha: Option<String>,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost: Option<f64>,
    pub task: Option<String>,
    pub started_at: Option<u64>,
    pub duration_ms: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub provider: Option<String>,
}

/// Running totals over a group of turns.
#[derive(Default, Clone)]
struct Agg {
    turns: u64,
    tokens: u64,
    cost: f64,
    /// At least one turn in the group reported a cost. When false the
    /// cost column shows "-": the provider never billed, so 0 would lie.
    metered: bool,
    /// Prompt-cache hits, and the prompt tokens of only the rows that
    /// reported them: the hit rate is computed over measurable rows,
    /// so pre-0004 history cannot dilute it toward zero.
    cached: u64,
    cached_prompt: u64,
}

impl Agg {
    fn add(&mut self, row: &TurnRow) {
        self.turns += 1;
        self.tokens += row.prompt_tokens + row.completion_tokens;
        if let Some(cost) = row.cost {
            self.cost += cost;
            self.metered = true;
        }
        if let Some(cached) = row.cached_tokens {
            self.cached += cached;
            self.cached_prompt += row.prompt_tokens;
        }
    }

    /// Share of prompt tokens served from cache, over the rows that
    /// reported cache details; `None` when none did.
    fn cache_rate(&self) -> Option<f64> {
        // Token sums sit far below f64's 2^53 integer ceiling.
        #[allow(clippy::cast_precision_loss)]
        (self.cached_prompt > 0).then(|| self.cached as f64 / self.cached_prompt as f64)
    }
}

/// `87%`, or `-` when the group has no measurable rows. Floored, not
/// rounded: "100%" must mean every prompt token came from cache.
fn fmt_rate(rate: Option<f64>) -> String {
    rate.map_or_else(
        || "-".to_string(),
        |r| format!("{:.0}%", (r * 100.0).floor()),
    )
}

/// Group turns by `key` in first-seen order. Rows arrive in insertion
/// order, so first-seen is chronological.
fn group_by(rows: &[TurnRow], key: impl Fn(&TurnRow) -> String) -> Vec<(String, Agg)> {
    let mut groups: Vec<(String, Agg)> = Vec::new();
    for row in rows {
        let k = key(row);
        if let Some((_, agg)) = groups.iter_mut().find(|(name, _)| *name == k) {
            agg.add(row);
        } else {
            let mut agg = Agg::default();
            agg.add(row);
            groups.push((k, agg));
        }
    }
    groups
}

/// Per-task totals: cost, turn count, and the two wall times (spec 27).
#[derive(Default)]
struct TaskAgg {
    turns: u64,
    cost: f64,
    metered: bool,
    /// Σ duration over the rows that carry timing.
    active_ms: u64,
    /// Span endpoints over timed rows, in ms since the epoch.
    start_min_ms: Option<u64>,
    end_max_ms: Option<u64>,
    /// Insertion index of the group's newest row — the recency key.
    last_seen: usize,
}

impl TaskAgg {
    fn add(&mut self, row: &TurnRow, index: usize) {
        self.turns += 1;
        if let Some(cost) = row.cost {
            self.cost += cost;
            self.metered = true;
        }
        if let (Some(started), Some(duration)) = (row.started_at, row.duration_ms) {
            let start_ms = started.saturating_mul(1000);
            self.active_ms += duration;
            self.start_min_ms = Some(self.start_min_ms.map_or(start_ms, |m| m.min(start_ms)));
            let end_ms = start_ms.saturating_add(duration);
            self.end_max_ms = Some(self.end_max_ms.map_or(end_ms, |m| m.max(end_ms)));
        }
        self.last_seen = index;
    }

    /// First start to last end; `None` until a timed row arrives.
    fn span_ms(&self) -> Option<u64> {
        Some(self.end_max_ms?.saturating_sub(self.start_min_ms?))
    }
}

/// Task groups by the report's cap. `(untracked)` collects NULL-task
/// rows (pre-spec-27 history); tracked groups order newest-activity
/// first, untracked always last.
const TASK_GROUP_CAP: usize = 20;
const UNTRACKED: &str = "(untracked)";

fn group_by_task(rows: &[TurnRow]) -> Vec<(String, TaskAgg)> {
    let mut groups: Vec<(String, TaskAgg)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let key = row.task.as_deref().unwrap_or(UNTRACKED);
        if let Some((_, agg)) = groups.iter_mut().find(|(name, _)| name == key) {
            agg.add(row, index);
        } else {
            let mut agg = TaskAgg::default();
            agg.add(row, index);
            groups.push((key.to_string(), agg));
        }
    }
    groups.sort_by(|a, b| {
        let untracked = |name: &str| name == UNTRACKED;
        untracked(&a.0)
            .cmp(&untracked(&b.0))
            .then(b.1.last_seen.cmp(&a.1.last_seen))
    });
    groups
}

/// The By Task table: cost and wall time per unit of work. Tokens stay
/// in the build/model tables — the task view is what the work cost and
/// how long it took.
fn write_task_table(out: &mut String, groups: &[(String, TaskAgg)]) {
    let _ = writeln!(out, "By Task\n");
    let _ = writeln!(
        out,
        "{:<38} {:>6} {:>12} {:>9} {:>9}",
        "Task", "Turns", "Cost", "Active", "Span"
    );
    for (name, agg) in groups.iter().take(TASK_GROUP_CAP) {
        let cost = fmt_cost(agg.cost, agg.metered);
        let active = if agg.start_min_ms.is_some() {
            fmt_duration(agg.active_ms)
        } else {
            "-".to_string()
        };
        let span = agg.span_ms().map_or("-".to_string(), fmt_duration);
        let _ = writeln!(
            out,
            "{name:<38} {:>6} {cost:>12} {active:>9} {span:>9}",
            agg.turns,
        );
    }
    if groups.len() > TASK_GROUP_CAP {
        let _ = writeln!(out, "(+{} more tasks)", groups.len() - TASK_GROUP_CAP);
    }
    out.push('\n');
}

/// Live endpoint rates keyed by `(model, provider_name)`.
pub type LiveRates = HashMap<(String, String), ModelRates>;

/// The rate that prices one row. Operator config wins — it exists to
/// override whatever the live list says — then the recorded serving
/// endpoint's live rate; a row without a recorded endpoint has no
/// live identity to price against.
fn row_rates(
    row: &TurnRow,
    config: &HashMap<String, ModelRates>,
    live: &LiveRates,
) -> Option<ModelRates> {
    if let Some(rate) = config.get(&row.model) {
        return Some(*rate);
    }
    let provider = row.provider.as_ref()?;
    live.get(&(row.model.clone(), provider.clone())).copied()
}

/// Estimated USD saved by prompt-cache hits: every cached token was
/// billed at the cache-read rate instead of the input rate. Signed on
/// purpose — a future policy whose cache costs exceed its savings must
/// report that, not flatter itself. `None` until any rates exist and a
/// row has recorded cache details. Rows that resolve no rate are
/// counted, not skipped: silence would hide a misconfigured map.
fn cache_savings(
    rows: &[TurnRow],
    config: &HashMap<String, ModelRates>,
    live: &LiveRates,
) -> Option<CacheSavings> {
    if (config.is_empty() && live.is_empty()) || rows.iter().all(|r| r.cached_tokens.is_none()) {
        return None;
    }
    let mut savings = CacheSavings::default();
    for row in rows {
        let Some(cached) = row.cached_tokens else {
            continue;
        };
        match row_rates(row, config, live) {
            // Token sums sit far below f64's 2^53 integer ceiling.
            #[allow(clippy::cast_precision_loss)]
            Some(rate) => {
                savings.dollars +=
                    cached as f64 * (rate.input_per_mtok - rate.cache_read_per_mtok) / 1e6;
                savings.priced = true;
            }
            None => savings.unpriced_turns += 1,
        }
    }
    Some(savings)
}

#[derive(Default)]
struct CacheSavings {
    dollars: f64,
    /// At least one row's model had a configured rate.
    priced: bool,
    /// Rows with cache data but no rate for their model.
    unpriced_turns: u64,
}

impl fmt::Display for CacheSavings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cache savings: ")?;
        match (self.priced, self.dollars < 0.0) {
            (false, _) => write!(f, "-")?,
            (true, true) => write!(f, "-${:.4}", -self.dollars)?,
            (true, false) => write!(f, "${:.4}", self.dollars)?,
        }
        if self.unpriced_turns > 0 {
            write!(f, " ({} turns unpriced)", self.unpriced_turns)?;
        }
        Ok(())
    }
}

/// Render the `/usage` report: totals, the per-task headline, then the
/// per-build and per-model breakdowns. By Task answers what a unit of
/// work cost; By Build attributes a cost shift to the change that
/// shipped it.
pub fn report(rows: &[TurnRow], rates: &HashMap<String, ModelRates>, live: &LiveRates) -> String {
    if rows.is_empty() {
        return "No usage recorded yet.".to_string();
    }

    let mut total = Agg::default();
    for row in rows {
        total.add(row);
    }

    let mut out = String::new();
    // The header carries the overall hit rate only once turns have
    // recorded cache details; older ledgers keep the old shape.
    let cache = total
        .cache_rate()
        .map_or_else(String::new, |r| format!(", cache {}", fmt_rate(Some(r))));
    let _ = writeln!(
        out,
        "Usage ({} turns, {}{cache})",
        total.turns,
        fmt_cost(total.cost, total.metered),
    );
    if let Some(savings) = cache_savings(rows, rates, live) {
        let _ = writeln!(out, "{savings}");
    }
    out.push('\n');

    write_task_table(&mut out, &group_by_task(rows));

    // Chronological: a cost shift reads as a timeline of deploys.
    let build = group_by(rows, |r| {
        r.git_sha
            .as_deref()
            .map_or("unknown".to_string(), short_sha)
    });
    write_table(&mut out, "By Build", "Build", &build, true);

    let mut model = group_by(rows, |r| r.model.clone());
    model.sort_by(|a, b| {
        b.1.cost
            .partial_cmp(&a.1.cost)
            .unwrap_or(Ordering::Equal)
            .then(b.1.tokens.cmp(&a.1.tokens))
    });
    write_table(&mut out, "By Model", "Model", &model, false);

    out
}

/// A per-group table. `per_turn` adds a $/turn column (useful per build,
/// noise per model).
fn write_table(
    out: &mut String,
    title: &str,
    label: &str,
    groups: &[(String, Agg)],
    per_turn: bool,
) {
    let _ = writeln!(out, "{title}\n");
    if per_turn {
        let _ = writeln!(
            out,
            "{label:<24} {:>6} {:>10} {:>6} {:>12} {:>10}",
            "Turns", "Tokens", "Cache", "Cost", "$/turn"
        );
    } else {
        let _ = writeln!(
            out,
            "{label:<24} {:>6} {:>10} {:>6} {:>12}",
            "Turns", "Tokens", "Cache", "Cost"
        );
    }
    for (name, agg) in groups {
        let cost = fmt_cost(agg.cost, agg.metered);
        let cache = fmt_rate(agg.cache_rate());
        if per_turn {
            let per = if agg.metered && agg.turns > 0 {
                #[allow(clippy::cast_precision_loss)]
                let turns = agg.turns as f64;
                format!("${:.4}", agg.cost / turns)
            } else {
                "-".to_string()
            };
            let _ = writeln!(
                out,
                "{name:<24} {:>6} {:>10} {cache:>6} {cost:>12} {per:>10}",
                agg.turns,
                fmt_count(agg.tokens),
            );
        } else {
            let _ = writeln!(
                out,
                "{name:<24} {:>6} {:>10} {cache:>6} {cost:>12}",
                agg.turns,
                fmt_count(agg.tokens),
            );
        }
    }
    out.push('\n');
}

/// First 7 hex characters, matching git's short-SHA convention.
fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// `$0.0000`, or `-` when the group was never billed a cost.
fn fmt_cost(cost: f64, metered: bool) -> String {
    if metered {
        format!("${cost:.4}")
    } else {
        "-".to_string()
    }
}

/// Compact wall time: `800ms`, `12.5s`, `4m02s`, `1h05m`.
fn fmt_duration(ms: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

/// Compact token count: `1.2M`, `500.0K`, or the raw number below 1K.
fn fmt_count(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n >= 1_000_000 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", f / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TurnUsage;

    /// A meter with fixed timing for write-path tests.
    fn meter(usage: TurnUsage) -> TurnMeter {
        TurnMeter {
            usage,
            started_at: 1_700_000_000,
            duration: std::time::Duration::from_millis(1234),
            outcome: "text",
        }
    }

    fn open_temp() -> (tempfile::TempDir, UsageLedger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::new(
            &crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap(),
            HashMap::new(),
        );
        (dir, ledger)
    }

    #[test]
    fn records_a_turn_row() {
        let (_dir, ledger) = open_temp();
        ledger
            .record(&TurnRecord {
                session: "general",
                source: "socket",
                model: "z-ai/glm-5.2",
                task: None,
                meter: meter(TurnUsage {
                    calls: 3,
                    prompt_tokens: 1500,
                    cached_tokens: Some(1200),
                    completion_tokens: 200,
                    cost: Some(0.0042),
                    provider: Some("Sail Research".into()),
                }),
            })
            .unwrap();

        let conn = ledger
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let endpoint: Option<String> = conn
            .query_row("SELECT provider FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(endpoint.as_deref(), Some("Sail Research"));
        let (session, source, model, calls, prompt, cached, completion, cost): (
            String,
            String,
            String,
            i64,
            i64,
            Option<i64>,
            i64,
            Option<f64>,
        ) = conn
            .query_row(
                "SELECT session, source, model, calls, prompt_tokens, \
                 cached_tokens, completion_tokens, cost FROM turns",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(session, "general");
        assert_eq!(source, "socket");
        assert_eq!(model, "z-ai/glm-5.2");
        assert_eq!(calls, 3);
        assert_eq!(prompt, 1500);
        assert_eq!(cached, Some(1200));
        assert_eq!(completion, 200);
        assert_eq!(cost, Some(0.0042));
    }

    #[test]
    fn task_key_covers_every_source() {
        use crate::agent::envelope::GitHubRole;
        let cases = [
            (
                ChannelSource::Duty {
                    duty: "self-analysis".into(),
                },
                "duty:self-analysis",
            ),
            (
                ChannelSource::GitHub {
                    pr_number: 42,
                    repo: "owner/repo".into(),
                    role: GitHubRole::Reviewer,
                },
                "pr:owner/repo#42",
            ),
            (
                ChannelSource::GitHubIssue {
                    issue: "owner/repo#7".into(),
                },
                "issue:owner/repo#7",
            ),
            (
                ChannelSource::Linear {
                    issue: "MDK-123".into(),
                },
                "linear:MDK-123",
            ),
            (ChannelSource::Socket, "chat:socket"),
            (ChannelSource::Telegram, "chat:telegram"),
        ];
        for (source, expected) in cases {
            assert_eq!(TaskKey::for_source(&source).as_str(), expected);
        }
    }

    /// PR roles fold into one task: review and feedback turns on the
    /// same PR must not split the ledger.
    #[test]
    fn task_key_folds_pr_roles() {
        use crate::agent::envelope::GitHubRole;
        let key = |role| {
            TaskKey::for_source(&ChannelSource::GitHub {
                pr_number: 5,
                repo: "o/r".into(),
                role,
            })
        };
        assert_eq!(key(GitHubRole::Author), key(GitHubRole::Reviewer));
        assert_eq!(key(GitHubRole::Author), key(GitHubRole::Contributor));
    }

    #[test]
    fn task_round_trips_through_the_ledger() {
        let (_dir, ledger) = open_temp();
        let task = TaskKey::for_source(&ChannelSource::GitHubIssue {
            issue: "owner/repo#62".into(),
        });
        ledger
            .record(&TurnRecord {
                session: "owner/repo",
                source: "GitHub issue owner/repo#62",
                model: "m",
                task: Some(&task),
                meter: meter(TurnUsage::default()),
            })
            .unwrap();
        let conn = ledger
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored: Option<String> = conn
            .query_row("SELECT task FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored.as_deref(), Some("issue:owner/repo#62"));
    }

    #[test]
    fn timing_and_outcome_round_trip() {
        let (_dir, ledger) = open_temp();
        ledger
            .record(&TurnRecord {
                session: "s",
                source: "Socket",
                model: "m",
                task: None,
                meter: TurnMeter {
                    usage: TurnUsage::default(),
                    started_at: 1_756_000_000,
                    duration: std::time::Duration::from_millis(530_271),
                    outcome: "max_iterations",
                },
            })
            .unwrap();
        let conn = ledger
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (started_at, duration_ms, outcome): (i64, i64, String) = conn
            .query_row(
                "SELECT started_at, duration_ms, outcome FROM turns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(started_at, 1_756_000_000);
        assert_eq!(duration_ms, 530_271);
        assert_eq!(outcome, "max_iterations");
    }

    #[test]
    fn null_cost_when_provider_reports_none() {
        let (_dir, ledger) = open_temp();
        ledger
            .record(&TurnRecord {
                session: "s",
                source: "telegram",
                model: "m",
                task: None,
                meter: meter(TurnUsage {
                    calls: 1,
                    prompt_tokens: 10,
                    cached_tokens: None,
                    completion_tokens: 5,
                    cost: None,
                    provider: None,
                }),
            })
            .unwrap();
        let conn = ledger
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (cost, cached, endpoint): (Option<f64>, Option<i64>, Option<String>) = conn
            .query_row("SELECT cost, cached_tokens, provider FROM turns", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(cost, None);
        // An unnamed endpoint persists as NULL, never a default.
        assert_eq!(endpoint, None);
        // Unreported cache details persist as NULL, never zero.
        assert_eq!(cached, None);
    }

    #[test]
    fn append_only_accumulates_rows() {
        let (_dir, ledger) = open_temp();
        for _ in 0..3 {
            ledger
                .record(&TurnRecord {
                    session: "s",
                    source: "socket",
                    model: "m",
                    task: None,
                    meter: meter(TurnUsage::default()),
                })
                .unwrap();
        }
        let conn = ledger
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    fn row(sha: Option<&str>, model: &str, tokens: u64, cost: Option<f64>) -> TurnRow {
        TurnRow {
            git_sha: sha.map(String::from),
            model: model.to_string(),
            prompt_tokens: tokens,
            completion_tokens: 0,
            cost,
            task: None,
            started_at: None,
            duration_ms: None,
            cached_tokens: None,
            provider: None,
        }
    }

    /// The report without configured rates: no savings line.
    fn report_no_rates(rows: &[TurnRow]) -> String {
        report(rows, &HashMap::new(), &HashMap::new())
    }

    fn glm_rates(input: f64, cache_read: f64) -> HashMap<String, ModelRates> {
        HashMap::from([(
            "glm".to_string(),
            ModelRates {
                input_per_mtok: input,
                cache_read_per_mtok: cache_read,
            },
        )])
    }

    /// A task-attributed row with timing, the post-spec-27 shape.
    fn task_row(task: &str, cost: f64, started_at: u64, duration_ms: u64) -> TurnRow {
        TurnRow {
            task: Some(task.to_string()),
            cost: Some(cost),
            started_at: Some(started_at),
            duration_ms: Some(duration_ms),
            ..row(Some("abcdef1234"), "glm", 100, None)
        }
    }

    #[test]
    fn by_task_is_the_headline() {
        let out = report_no_rates(&[task_row("issue:o/r#1", 0.01, 100, 1_000)]);
        let task = out.find("By Task").unwrap();
        let build = out.find("By Build").unwrap();
        assert!(task < build);
        assert!(out.contains("issue:o/r#1"));
    }

    /// Active sums durations; span runs first start to last end, so a
    /// gap between turns counts toward span but not active.
    #[test]
    fn task_active_and_span_arithmetic() {
        let rows = vec![
            task_row("issue:o/r#1", 0.01, 100, 60_000),
            task_row("issue:o/r#1", 0.02, 400, 120_000),
        ];
        let out = report_no_rates(&rows);
        // Active: 1m + 2m; span: 100s..(400s + 120s) = 420s.
        assert!(out.contains("3m00s"), "active missing: {out}");
        assert!(out.contains("7m00s"), "span missing: {out}");
        // One group, two turns, summed cost.
        assert!(out.contains("issue:o/r#1"));
        assert!(out.contains("$0.0300"));
    }

    /// Legacy rows have no task and no timing: one (untracked) bucket,
    /// dashes for time, always after the tracked groups.
    #[test]
    fn untracked_bucket_renders_last_with_dashes() {
        let rows = vec![
            row(Some("abcdef1234"), "glm", 100, Some(0.01)),
            task_row("duty:distill", 0.02, 100, 1_000),
        ];
        let out = report_no_rates(&rows);
        let tracked = out.find("duty:distill").unwrap();
        let untracked = out.find("(untracked)").unwrap();
        assert!(tracked < untracked);
        let line = out
            .lines()
            .find(|l| l.starts_with("(untracked)"))
            .unwrap()
            .to_string();
        assert!(
            line.contains('-'),
            "untracked timing must be absent: {line}"
        );
    }

    #[test]
    fn tasks_ordered_by_recency_and_capped() {
        let mut rows: Vec<TurnRow> = (0..TASK_GROUP_CAP + 2)
            .map(|n| task_row(&format!("issue:o/r#{n}"), 0.01, 100, 1_000))
            .collect();
        // Re-touch the oldest task so recency puts it first.
        rows.push(task_row("issue:o/r#0", 0.01, 200, 1_000));
        let out = report_no_rates(&rows);
        let first = out.find("issue:o/r#0 ").unwrap();
        let second = out
            .find(&format!("issue:o/r#{} ", TASK_GROUP_CAP + 1))
            .unwrap();
        assert!(first < second, "recency must order the table");
        assert!(
            out.contains("(+2 more tasks)"),
            "cap trailer missing: {out}"
        );
    }

    #[test]
    fn fmt_rate_floors_instead_of_rounding() {
        // 1499/1500 is not a full cache hit; it must not display as one.
        assert_eq!(fmt_rate(Some(1499.0 / 1500.0)), "99%");
        assert_eq!(fmt_rate(Some(1.0)), "100%");
        assert_eq!(fmt_rate(None), "-");
    }

    #[test]
    fn fmt_duration_units() {
        assert_eq!(fmt_duration(800), "800ms");
        assert_eq!(fmt_duration(12_500), "12.5s");
        assert_eq!(fmt_duration(242_000), "4m02s");
        assert_eq!(fmt_duration(3_900_000), "1h05m");
    }

    #[test]
    fn report_empty_is_a_notice() {
        assert_eq!(report_no_rates(&[]), "No usage recorded yet.");
    }

    #[test]
    fn report_totals_and_groups() {
        let rows = vec![
            row(Some("abcdef1234"), "glm", 1000, Some(0.01)),
            row(Some("abcdef1234"), "kimi", 500, Some(0.02)),
            row(Some("9999999999"), "glm", 2000, Some(0.05)),
        ];
        let out = report_no_rates(&rows);
        // Header total: 3 turns, summed cost.
        assert!(out.contains("Usage (3 turns, $0.0800)"));
        // Short SHA, not the full hash.
        assert!(out.contains("abcdef1"));
        assert!(!out.contains("abcdef1234"));
        // Both axes present.
        assert!(out.contains("By Build"));
        assert!(out.contains("By Model"));
        // Per-model aggregation folds the two glm rows.
        assert!(out.contains("glm"));
        assert!(out.contains("kimi"));
    }

    #[test]
    fn builds_listed_chronologically_not_by_cost() {
        // The older build costs more; it must still come first.
        let rows = vec![
            row(Some("aaaaaaa111"), "glm", 100, Some(0.90)),
            row(Some("bbbbbbb222"), "glm", 100, Some(0.01)),
        ];
        let out = report_no_rates(&rows);
        let old = out.find("aaaaaaa").unwrap();
        let new = out.find("bbbbbbb").unwrap();
        assert!(old < new);
    }

    #[test]
    fn report_unmetered_shows_dash() {
        let out = report_no_rates(&[row(None, "local", 10, None)]);
        assert!(out.contains("Usage (1 turns, -)"));
        assert!(out.contains("unknown"));
    }

    #[test]
    fn rows_round_trip_through_the_ledger() {
        let (_dir, ledger) = open_temp();
        ledger
            .record(&TurnRecord {
                session: "s",
                source: "socket",
                model: "m",
                task: None,
                meter: meter(TurnUsage {
                    calls: 1,
                    prompt_tokens: 42,
                    cached_tokens: Some(30),
                    completion_tokens: 8,
                    cost: Some(0.5),
                    provider: None,
                }),
            })
            .unwrap();
        let rows = ledger.rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prompt_tokens, 42);
        assert_eq!(rows[0].cached_tokens, Some(30));
        assert_eq!(rows[0].completion_tokens, 8);
        assert_eq!(rows[0].cost, Some(0.5));
    }

    /// The hit rate is computed over the rows that reported cache
    /// details; a pre-0004 row in the same group must not dilute it.
    #[test]
    fn cache_rate_ignores_unreporting_rows() {
        let mut agg = Agg::default();
        agg.add(&TurnRow {
            cached_tokens: Some(800),
            ..row(Some("abcdef1234"), "glm", 1000, None)
        });
        agg.add(&row(Some("abcdef1234"), "glm", 500, None));
        assert_eq!(agg.cache_rate(), Some(0.8));
    }

    #[test]
    fn report_renders_cache_rate_per_group_and_header() {
        let rows = vec![
            TurnRow {
                cached_tokens: Some(750),
                ..row(Some("abcdef1234"), "glm", 1000, Some(0.01))
            },
            row(Some("9999999999"), "glm", 500, Some(0.01)),
        ];
        let out = report_no_rates(&rows);
        assert!(out.contains("cache 75%"), "header rate missing: {out}");
        let cached_build = out
            .lines()
            .find(|l| l.starts_with("abcdef1"))
            .unwrap()
            .to_string();
        assert!(
            cached_build.contains("75%"),
            "build rate missing: {cached_build}"
        );
        // The era without details renders absence, never 0%.
        let uncached_build = out
            .lines()
            .find(|l| l.starts_with("9999999"))
            .unwrap()
            .to_string();
        assert!(
            uncached_build.contains('-'),
            "expected dash: {uncached_build}"
        );
    }

    /// 2M cached tokens at $0.40/M input vs $0.075/M cache read
    /// saves 2 x $0.325 = $0.65.
    #[test]
    fn savings_priced_from_configured_rates() {
        let rows = vec![
            TurnRow {
                cached_tokens: Some(1_500_000),
                ..row(Some("abcdef1234"), "glm", 2_000_000, Some(0.5))
            },
            TurnRow {
                cached_tokens: Some(500_000),
                ..row(Some("abcdef1234"), "glm", 600_000, Some(0.2))
            },
        ];
        let out = report(&rows, &glm_rates(0.4, 0.075), &HashMap::new());
        assert!(out.contains("Cache savings: $0.6500"), "missing: {out}");
        assert!(!out.contains("unpriced"), "nothing is unpriced: {out}");
    }

    /// The estimate is signed: rates where caching costs more than it
    /// saves must show a negative number, not clamp to zero.
    #[test]
    fn savings_can_go_negative() {
        let rows = vec![TurnRow {
            cached_tokens: Some(1_000_000),
            ..row(Some("abcdef1234"), "glm", 1_000_000, None)
        }];
        let out = report(&rows, &glm_rates(0.4, 0.5), &HashMap::new());
        assert!(out.contains("Cache savings: -$0.1000"), "missing: {out}");
    }

    /// A model missing from the rates map is counted, not skipped:
    /// silence would hide a misconfigured map behind a rosy number.
    #[test]
    fn savings_reports_unpriced_models() {
        let rows = vec![
            TurnRow {
                cached_tokens: Some(1_000_000),
                ..row(Some("abcdef1234"), "glm", 1_000_000, None)
            },
            TurnRow {
                cached_tokens: Some(100),
                ..row(Some("abcdef1234"), "kimi", 200, None)
            },
        ];
        let out = report(&rows, &glm_rates(0.4, 0.075), &HashMap::new());
        assert!(
            out.contains("Cache savings: $0.3250 (1 turns unpriced)"),
            "missing: {out}"
        );
        // Rates that price nothing still surface the row count.
        let other = HashMap::from([(
            "other-model".to_string(),
            ModelRates {
                input_per_mtok: 0.4,
                cache_read_per_mtok: 0.075,
            },
        )]);
        let none_priced = report(&rows, &other, &HashMap::new());
        assert!(
            none_priced.contains("Cache savings: - (2 turns unpriced)"),
            "missing: {none_priced}"
        );
    }

    fn live(model: &str, endpoint: &str, input: f64, cache_read: f64) -> LiveRates {
        HashMap::from([(
            (model.to_string(), endpoint.to_string()),
            ModelRates {
                input_per_mtok: input,
                cache_read_per_mtok: cache_read,
            },
        )])
    }

    /// Live rates price a row by the endpoint that served it; config,
    /// when present for the model, overrides the live number.
    #[test]
    fn live_rates_price_by_serving_endpoint_config_overrides() {
        let rows = vec![TurnRow {
            cached_tokens: Some(1_000_000),
            provider: Some("Sail Research".into()),
            ..row(Some("abcdef1234"), "glm", 1_000_000, None)
        }];
        let live = live("glm", "Sail Research", 0.5, 0.115);
        let out = report(&rows, &HashMap::new(), &live);
        assert!(out.contains("Cache savings: $0.3850"), "live: {out}");
        // Config says otherwise; config wins.
        let out = report(&rows, &glm_rates(0.4, 0.075), &live);
        assert!(out.contains("Cache savings: $0.3250"), "override: {out}");
    }

    /// A row that never recorded its endpoint has no live identity to
    /// price against: unpriced, not silently matched to some endpoint.
    #[test]
    fn live_rates_skip_rows_without_a_recorded_endpoint() {
        let rows = vec![TurnRow {
            cached_tokens: Some(100),
            provider: None,
            ..row(Some("abcdef1234"), "glm", 200, None)
        }];
        let out = report(
            &rows,
            &HashMap::new(),
            &live("glm", "Sail Research", 0.5, 0.115),
        );
        assert!(
            out.contains("Cache savings: - (1 turns unpriced)"),
            "missing: {out}"
        );
    }

    /// `live_rates` fetches only models that need pricing: cache data,
    /// a recorded endpoint, and no config override.
    #[tokio::test]
    async fn live_rates_fetches_only_unpriced_models() {
        use crate::clients::openrouter_pricing::PricingClient;
        let dir = tempfile::tempdir().unwrap();
        let db = crate::state_db::StateDb::open(&dir.path().join("kitaebot.db")).unwrap();
        let ledger = UsageLedger::new(&db, glm_rates(0.4, 0.075)).with_pricing(
            PricingClient::from_fn(|path| async move {
                assert_eq!(path, "kimi/endpoints", "only kimi needs live rates");
                Ok(crate::clients::RawResponse {
                    status: 200,
                    body: br#"{"data":{"endpoints":[{"provider_name":"Moonshot",
                        "pricing":{"prompt":"0.000001","input_cache_read":"0.0000001"}}]}}"#
                        .to_vec(),
                    retry_after_secs: None,
                })
            }),
        );
        let rows = vec![
            // Config-covered: no fetch.
            TurnRow {
                cached_tokens: Some(10),
                provider: Some("Sail Research".into()),
                ..row(None, "glm", 20, None)
            },
            // No endpoint recorded: nothing to price against, no fetch.
            TurnRow {
                cached_tokens: Some(10),
                ..row(None, "local", 20, None)
            },
            TurnRow {
                cached_tokens: Some(10),
                provider: Some("Moonshot".into()),
                ..row(None, "kimi", 20, None)
            },
        ];
        let fetched = ledger.live_rates(&rows).await;
        assert_eq!(fetched.len(), 1);
        let rate = fetched
            .get(&("kimi".to_string(), "Moonshot".to_string()))
            .unwrap();
        assert!((rate.input_per_mtok - 1.0).abs() < 1e-9);
    }

    /// Without a pricing client (non-`OpenRouter`, tests), live rates
    /// are empty and the report falls back to config alone.
    #[tokio::test]
    async fn live_rates_empty_without_a_client() {
        let (_dir, ledger) = open_temp();
        let rows = vec![TurnRow {
            cached_tokens: Some(10),
            provider: Some("Sail Research".into()),
            ..row(None, "glm", 20, None)
        }];
        assert!(ledger.live_rates(&rows).await.is_empty());
    }

    /// No rates configured, or no rows with cache data: no line.
    #[test]
    fn savings_line_absent_without_rates_or_data() {
        let cached = vec![TurnRow {
            cached_tokens: Some(100),
            ..row(Some("abcdef1234"), "glm", 200, None)
        }];
        assert!(!report_no_rates(&cached).contains("Cache savings"));
        let uncached = vec![row(Some("abcdef1234"), "glm", 200, None)];
        let out = report(&uncached, &glm_rates(0.4, 0.075), &HashMap::new());
        assert!(!out.contains("Cache savings"), "no cache-era rows: {out}");
    }

    /// A ledger with no cache-era rows keeps the old header shape.
    #[test]
    fn report_header_omits_cache_when_unmeasured() {
        let out = report_no_rates(&[row(Some("abcdef1234"), "glm", 100, Some(0.01))]);
        assert!(out.contains("Usage (1 turns, $0.0100)"));
        assert!(!out.contains("cache"));
    }

    #[test]
    fn fmt_count_scales() {
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_500), "1.5K");
        assert_eq!(fmt_count(2_400_000), "2.4M");
    }

    #[test]
    fn short_sha_takes_seven() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
    }
}

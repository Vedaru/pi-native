//! Trigger engine: turn time (and later events) into agent episodes.
//!
//! A [`Trigger`] has a schedule and a prompt. Each time the schedule is due, the
//! [`Runner`] turns it into an **episode**: it finds-or-creates the trigger's
//! long-lived session, sends the prompt to that unit, and records a durable run
//! record. Records are the dedupe/idempotency layer, so restarting the runner
//! does not re-fire an episode that already ran in its window.
//!
//! Sources implemented here: interval and cron. Webhooks, file watches, and
//! queues feed the same [`Runner::tick`] by advancing a due trigger, so they can
//! land without changing the episode model.
//!
//! Budgets are enforced at two levels: dedupe windows and max runs per window
//! bound *when* a trigger fires; a [`Budget`] envelope bounds the *cost* of each
//! run (tokens, spend, wall-clock) and each agent's cumulative spend. When a run
//! crosses a budget the runner aborts it, rolls the working tree back (or parks
//! the changes), and records a [`Receipt`] on the run so the outcome is
//! inspectable and attached to the card.

use pi_host::Host;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often a trigger runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Fire every `every`.
    Interval { every: Duration },
    /// Fire on a 5-field cron expression (`min hour day month weekday`).
    Cron(CronSchedule),
}

impl Schedule {
    /// A one-line description: `every 300s` or `cron 0 * * * *`.
    pub fn describe(&self) -> String {
        match self {
            Schedule::Interval { every } => format!("every {}s", every.as_secs()),
            Schedule::Cron(cron) => format!("cron {}", cron.expression),
        }
    }
}

/// One trigger definition.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub id: String,
    pub schedule: Schedule,
    /// Prompt sent to the session each time the trigger fires.
    pub prompt: String,
    /// Deliver to this existing session id instead of the trigger's own session
    /// (a unit scheduling its own tick).
    pub target: Option<String>,
    /// If non-zero, at most one episode per `dedupe_window` (restart-safe).
    pub dedupe_window: Duration,
    /// If non-zero, at most this many episodes per `budget_window`.
    pub max_runs_per_window: usize,
    pub budget_window: Duration,
    /// Model recorded on the receipt (the provider is fixed at startup).
    pub model: Option<String>,
    /// Per-run budget. A zero field is unlimited.
    pub budget: Budget,
    /// Cumulative per-agent budget across runs. A zero field is unlimited.
    pub agent_budget: Budget,
}

impl Trigger {
    pub fn interval(id: impl Into<String>, every: Duration, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            schedule: Schedule::Interval { every },
            prompt: prompt.into(),
            target: None,
            dedupe_window: Duration::ZERO,
            max_runs_per_window: 0,
            budget_window: Duration::ZERO,
            model: None,
            budget: Budget::default(),
            agent_budget: Budget::default(),
        }
    }

    pub fn cron(
        id: impl Into<String>,
        expression: &str,
        prompt: impl Into<String>,
    ) -> Result<Self, String> {
        Ok(Self {
            id: id.into(),
            schedule: Schedule::Cron(CronSchedule::parse(expression)?),
            prompt: prompt.into(),
            target: None,
            dedupe_window: Duration::ZERO,
            max_runs_per_window: 0,
            budget_window: Duration::ZERO,
            model: None,
            budget: Budget::default(),
            agent_budget: Budget::default(),
        })
    }
}

/// A spend/token/time envelope for a run. A zero value is unlimited, so the
/// default [`Budget`] never stops a run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Total prompt+completion tokens allowed (0 = unlimited).
    #[serde(default)]
    pub max_tokens: i64,
    /// Total spend allowed in micro-units of the configured currency
    /// (0 = unlimited).
    #[serde(default)]
    pub max_cost_micros: i64,
    /// Wall-clock seconds allowed (0 = unlimited).
    #[serde(default)]
    pub max_seconds: u64,
}

impl Budget {
    /// Whether any limit is set.
    pub fn is_bounded(&self) -> bool {
        self.max_tokens > 0 || self.max_cost_micros > 0 || self.max_seconds > 0
    }
}

/// Token pricing used to turn usage into a cost. Rates are micro-units per
/// million tokens, so an integer cost is exact enough for budget checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    #[serde(default)]
    pub input_micros_per_million: i64,
    #[serde(default)]
    pub output_micros_per_million: i64,
    #[serde(default)]
    pub cache_read_micros_per_million: i64,
    #[serde(default)]
    pub cache_write_micros_per_million: i64,
}

impl Price {
    /// Cost in micro-units for one model call's usage.
    pub fn cost_micros(&self, usage: &UsageTotals) -> i64 {
        let per_million = |tokens: i64, rate: i64| -> i64 {
            // Multiply before dividing to keep precision; i128 avoids overflow.
            ((tokens as i128) * (rate as i128) / 1_000_000) as i64
        };
        per_million(usage.input, self.input_micros_per_million)
            + per_million(usage.output, self.output_micros_per_million)
            + per_million(usage.cache_read, self.cache_read_micros_per_million)
            + per_million(usage.cache_write, self.cache_write_micros_per_million)
    }
}

/// Accumulated provider usage across the model calls in one run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl UsageTotals {
    /// Total prompt+completion tokens (cache reads are prompt tokens too).
    pub fn total_tokens(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    fn add(&mut self, input: i64, output: i64, cache_read: i64, cache_write: i64) {
        self.input += input;
        self.output += output;
        self.cache_read += cache_read;
        self.cache_write += cache_write;
    }
}

/// A recorded episode (one trigger firing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    pub trigger_id: String,
    pub session_id: String,
    pub session_path: PathBuf,
    pub prompt: String,
    pub dedupe_key: String,
    /// Unix seconds.
    pub started_at: i64,
    /// Set when the firing attempt failed (for example the unit could not be
    /// opened or the prompt could not be delivered). Failed attempts are still
    /// recorded so the failure is observable, but they are ignored by
    /// [`RunStore::seen`] and [`RunStore::count_since`] so the trigger retries
    /// on the next tick instead of silently burning its window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptOutcome {
    /// The turn completed within budget.
    Completed,
    /// A per-run or per-agent budget was crossed; the run was aborted.
    BudgetExceeded,
    /// The unit failed before a turn completed.
    Failed,
}

/// Whether a breached run was rolled back or parked for inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// No changes had to be undone.
    None,
    /// The working tree was restored to its pre-run state.
    RolledBack,
    /// Changes were stashed for inspection (`git stash`) rather than destroyed.
    Parked,
}

/// The evidence one run leaves behind: what ran, what it cost, what it touched,
/// and how it ended. Attached to the card so a breach is inspectable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub trigger_id: String,
    pub session_id: String,
    pub model: Option<String>,
    pub prompt: String,
    pub usage: UsageTotals,
    /// Spend in micro-units, computed from usage and the configured [`Price`].
    pub cost_micros: i64,
    /// Process exit code analogue: the unit's `stop_reason`, when reported.
    pub exit_code: Option<String>,
    pub outcome: ReceiptOutcome,
    /// Which limit was crossed, when `outcome` is `BudgetExceeded`.
    pub breach: Option<String>,
    pub disposition: Disposition,
    /// Working-tree files the run changed, relative to the repository root.
    pub files_changed: Vec<String>,
    /// Unified diff of the run's working-tree changes (bounded).
    pub diff: String,
    /// Unix seconds.
    pub started_at: i64,
    pub finished_at: i64,
}

/// Largest diff (bytes) embedded in a receipt; a larger diff is truncated.
const MAX_DIFF_BYTES: usize = 64 * 1024;
/// Default wall-clock ceiling for a run when the budget is unlimited, so a
/// stuck unit cannot hang the trigger loop forever.
const UNBOUNDED_RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// Durable run records. The in-memory store is for tests and short-lived runs;
/// [`JsonlRuns`] survives restarts. `Send` so a server can own one.
pub trait RunStore: Send {
    fn record(&mut self, episode: Episode);
    /// Record the receipt for a finished run. Default is a no-op so stores that
    /// only track episodes keep working.
    fn record_receipt(&mut self, _receipt: Receipt) {}
    fn seen(&self, trigger_id: &str, dedupe_key: &str) -> bool;
    fn count_since(&self, trigger_id: &str, since: i64) -> usize;
    /// Receipts recorded so far, newest last. Default is empty.
    fn receipts(&self) -> &[Receipt] {
        &[]
    }
    /// Cumulative spend (micro-units) recorded for an agent, for per-agent
    /// budgets. Default derives it from the receipts.
    fn agent_cost_micros(&self, trigger_id: &str) -> i64 {
        self.receipts()
            .iter()
            .filter(|receipt| receipt.trigger_id == trigger_id)
            .map(|receipt| receipt.cost_micros)
            .sum()
    }
    /// Cumulative tokens recorded for an agent. Default derives it from the
    /// receipts.
    fn agent_tokens(&self, trigger_id: &str) -> i64 {
        self.receipts()
            .iter()
            .filter(|receipt| receipt.trigger_id == trigger_id)
            .map(|receipt| receipt.usage.total_tokens())
            .sum()
    }
}

/// In-memory run records.
#[derive(Debug, Default)]
pub struct InMemoryRuns {
    episodes: Vec<Episode>,
    receipts: Vec<Receipt>,
}

impl RunStore for InMemoryRuns {
    fn record(&mut self, episode: Episode) {
        self.episodes.push(episode);
    }

    fn record_receipt(&mut self, receipt: Receipt) {
        self.receipts.push(receipt);
    }

    fn seen(&self, trigger_id: &str, dedupe_key: &str) -> bool {
        self.episodes.iter().any(|episode| {
            episode.error.is_none()
                && episode.trigger_id == trigger_id
                && episode.dedupe_key == dedupe_key
        })
    }

    fn count_since(&self, trigger_id: &str, since: i64) -> usize {
        self.episodes
            .iter()
            .filter(|episode| {
                episode.error.is_none()
                    && episode.trigger_id == trigger_id
                    && episode.started_at >= since
            })
            .count()
    }

    fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }
}

/// Append-only JSONL run records. Episodes and receipts are written as two
/// sibling files (`<path>` and `<path>.receipts.jsonl`) so the episode format
/// stays backward-compatible.
pub struct JsonlRuns {
    path: PathBuf,
    receipts_path: PathBuf,
    episodes: Vec<Episode>,
    receipts: Vec<Receipt>,
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<T>(line).ok())
        .collect()
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write as _;
        if let Ok(line) = serde_json::to_string(value) {
            let _ = writeln!(file, "{line}");
        }
    }
}

impl JsonlRuns {
    /// Open (loading existing records) or create at `path`.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let receipts_path = path.with_extension("receipts.jsonl");
        let episodes = read_jsonl::<Episode>(&path);
        let receipts = read_jsonl::<Receipt>(&receipts_path);
        Ok(Self {
            path,
            receipts_path,
            episodes,
            receipts,
        })
    }

    pub fn load(&self) -> &[Episode] {
        &self.episodes
    }

    /// Receipts loaded from disk (newest last).
    pub fn load_receipts(&self) -> &[Receipt] {
        &self.receipts
    }
}

impl RunStore for JsonlRuns {
    fn record(&mut self, episode: Episode) {
        append_jsonl(&self.path, &episode);
        self.episodes.push(episode);
    }

    fn record_receipt(&mut self, receipt: Receipt) {
        append_jsonl(&self.receipts_path, &receipt);
        self.receipts.push(receipt);
    }

    fn seen(&self, trigger_id: &str, dedupe_key: &str) -> bool {
        self.episodes.iter().any(|episode| {
            episode.error.is_none()
                && episode.trigger_id == trigger_id
                && episode.dedupe_key == dedupe_key
        })
    }

    fn count_since(&self, trigger_id: &str, since: i64) -> usize {
        self.episodes
            .iter()
            .filter(|episode| {
                episode.error.is_none()
                    && episode.trigger_id == trigger_id
                    && episode.started_at >= since
            })
            .count()
    }

    fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }
}

/// How a breached run disposes of its working-tree changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackPolicy {
    /// Restore the tree to its pre-run state (`git checkout` + clean).
    Restore,
    /// Park the changes in a git stash, leaving the tree clean.
    Park,
    /// Leave the tree untouched (record only).
    Keep,
}

/// Fires due triggers against a [`Host`].
pub struct Runner {
    triggers: Vec<Trigger>,
    sessions_root: PathBuf,
    last_fired: HashMap<String, i64>,
    /// Token pricing used to turn usage into spend.
    price: Price,
    /// Repository whose working tree a breached run rolls back or parks.
    workspace: Option<PathBuf>,
    /// What to do with a breached run's changes.
    rollback: RollbackPolicy,
}

impl Runner {
    pub fn new(triggers: Vec<Trigger>, sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            triggers,
            sessions_root: sessions_root.into(),
            last_fired: HashMap::new(),
            price: Price::default(),
            workspace: None,
            rollback: RollbackPolicy::Park,
        }
    }

    /// Set the token pricing used to compute run spend.
    pub fn with_price(mut self, price: Price) -> Self {
        self.price = price;
        self
    }

    /// Set the working tree a breached run disposes of. Defaults to the host's
    /// cwd at tick time.
    pub fn with_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Choose how a breached run's changes are handled.
    pub fn with_rollback(mut self, policy: RollbackPolicy) -> Self {
        self.rollback = policy;
        self
    }

    pub fn triggers(&self) -> &[Trigger] {
        &self.triggers
    }

    /// Fire every trigger due at `now` (unix seconds). Returns the episodes.
    ///
    /// The session for a trigger is stable across episodes, so a long-lived
    /// trigger accumulates context. Each episode is driven to completion so its
    /// usage can be measured against the run budget; records are written before
    /// the return, so a restart will not repeat an episode in the same window.
    pub fn tick<S: RunStore + ?Sized>(
        &mut self,
        host: &mut Host,
        store: &mut S,
        now: i64,
    ) -> Vec<Episode> {
        let mut episodes = Vec::new();
        // Clone the due trigger ids first so the loop can mutate `self`.
        let due: Vec<usize> = self
            .triggers
            .iter()
            .enumerate()
            .filter(|(_, trigger)| self.is_due(trigger, store, now))
            .map(|(index, _)| index)
            .collect();
        std::fs::create_dir_all(&self.sessions_root).ok();

        for index in due {
            let trigger = &self.triggers[index];
            let dedupe_secs = trigger.dedupe_window.as_secs();
            let dedupe_key = if dedupe_secs == 0 {
                String::new()
            } else {
                format!("{}:{}", trigger.id, now / dedupe_secs as i64)
            };
            if !dedupe_key.is_empty() && store.seen(&trigger.id, &dedupe_key) {
                self.last_fired.insert(trigger.id.clone(), now);
                continue;
            }
            if trigger.max_runs_per_window > 0 {
                let window = trigger.budget_window.as_secs() as i64;
                if store.count_since(&trigger.id, now - window) >= trigger.max_runs_per_window {
                    self.last_fired.insert(trigger.id.clone(), now);
                    continue;
                }
            }

            // The per-agent budget is checked before starting: a run that would
            // only add spend is refused, and the refusal is receipted.
            if let Some(breach) = self.agent_breach(trigger, store) {
                let receipt = self.refusal_receipt(trigger, breach, now);
                store.record_receipt(receipt);
                self.last_fired.insert(trigger.id.clone(), now);
                continue;
            }

            let trigger_path = self.session_path(&trigger.id);
            // A target delivers to an existing unit (a unit scheduling its own
            // tick); otherwise the trigger owns a stable session.
            let opened = match &trigger.target {
                Some(target) => Ok(target.clone()),
                None => host.open(trigger_path.clone()),
            };
            let path = match &trigger.target {
                Some(target) => host.session_path(target).unwrap_or(trigger_path),
                None => trigger_path,
            };
            match opened {
                Ok(session_id) => {
                    let workspace = self
                        .workspace
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(host.cwd()));
                    let before = workspace_snapshot(&workspace);
                    let subscription = host.subscribe(&session_id).ok();
                    let command = serde_json::json!({ "type": "prompt", "text": trigger.prompt });
                    match host.send(&session_id, command) {
                        Ok(()) => {
                            let episode = Episode {
                                trigger_id: trigger.id.clone(),
                                session_id: session_id.clone(),
                                session_path: path,
                                prompt: trigger.prompt.clone(),
                                dedupe_key,
                                started_at: now,
                                error: None,
                            };
                            store.record(episode.clone());
                            // Drive the episode to completion and record its
                            // receipt (usage/cost/disposition, VED-372).
                            let receipt = self.run_episode(
                                &trigger.clone(),
                                host,
                                subscription,
                                &session_id,
                                now,
                                before,
                                &workspace,
                            );
                            store.record_receipt(receipt);
                            episodes.push(episode);
                            self.last_fired.insert(trigger.id.clone(), now);
                        }
                        Err(error) => {
                            // Do not advance `last_fired`: the window is still
                            // owed and the next tick retries. Record the
                            // failure so it is durable and observable.
                            let message = error.to_string();
                            eprintln!(
                                "pi-triggers: cannot send to session for {}: {message}",
                                trigger.id
                            );
                            let episode = Episode {
                                trigger_id: trigger.id.clone(),
                                session_id,
                                session_path: path,
                                prompt: trigger.prompt.clone(),
                                dedupe_key,
                                started_at: now,
                                error: Some(message),
                            };
                            store.record(episode.clone());
                            episodes.push(episode);
                        }
                    }
                }
                Err(error) => {
                    // Same contract as a failed send: surface the failure and
                    // retry, rather than silently skipping the window.
                    let message = error.to_string();
                    eprintln!(
                        "pi-triggers: cannot open session for {}: {message}",
                        trigger.id
                    );
                    let episode = Episode {
                        trigger_id: trigger.id.clone(),
                        session_id: String::new(),
                        session_path: path,
                        prompt: trigger.prompt.clone(),
                        dedupe_key,
                        started_at: now,
                        error: Some(message),
                    };
                    store.record(episode.clone());
                    episodes.push(episode);
                }
            }
        }
        episodes
    }

    /// A breach of the cumulative per-agent budget, if any.
    fn agent_breach<S: RunStore + ?Sized>(&self, trigger: &Trigger, store: &S) -> Option<String> {
        let budget = &trigger.agent_budget;
        if budget.max_cost_micros > 0
            && store.agent_cost_micros(&trigger.id) >= budget.max_cost_micros
        {
            return Some("agent cost budget exhausted".to_string());
        }
        if budget.max_tokens > 0 && store.agent_tokens(&trigger.id) >= budget.max_tokens {
            return Some("agent token budget exhausted".to_string());
        }
        None
    }

    /// A receipt for a run refused before it started.
    fn refusal_receipt(&self, trigger: &Trigger, breach: String, now: i64) -> Receipt {
        Receipt {
            trigger_id: trigger.id.clone(),
            session_id: String::new(),
            model: trigger.model.clone(),
            prompt: trigger.prompt.clone(),
            usage: UsageTotals::default(),
            cost_micros: 0,
            exit_code: None,
            outcome: ReceiptOutcome::BudgetExceeded,
            breach: Some(breach),
            disposition: Disposition::None,
            files_changed: Vec::new(),
            diff: String::new(),
            started_at: now,
            finished_at: now,
        }
    }

    /// Drive one episode to completion, accumulate its usage, and dispose of
    /// changes if a budget was crossed.
    #[allow(clippy::too_many_arguments)]
    fn run_episode(
        &self,
        trigger: &Trigger,
        host: &mut Host,
        subscription: Option<pi_host::Subscription>,
        session_id: &str,
        now: i64,
        before: WorkspaceSnapshot,
        workspace: &Path,
    ) -> Receipt {
        let started = Instant::now();
        let wall_limit = if trigger.budget.max_seconds > 0 {
            Duration::from_secs(trigger.budget.max_seconds)
        } else {
            UNBOUNDED_RUN_TIMEOUT
        };
        let mut usage = UsageTotals::default();
        let mut stop_reason = None;
        let mut budget_breach = None;
        let mut aborted = false;

        if let Some(subscription) = subscription {
            // A reused session replays its previous turns; only events at or
            // after this sequence id belong to the run we just started.
            let live_from = subscription.next_seq;
            loop {
                let elapsed = started.elapsed();
                if elapsed >= wall_limit {
                    budget_breach = Some("wall-clock budget exhausted".to_string());
                    let _ = host.send(session_id, serde_json::json!({ "type": "abort" }));
                    aborted = true;
                    break;
                }
                let remaining = wall_limit - elapsed;
                match subscription.recv_sequenced_timeout(remaining.min(Duration::from_secs(1))) {
                    Ok((seq, _)) if seq < live_from => continue,
                    Ok((_, event)) => match event.get("type").and_then(serde_json::Value::as_str) {
                        Some("usage") => {
                            usage.add(
                                int_field(&event, "input"),
                                int_field(&event, "output"),
                                int_field(&event, "cache_read"),
                                int_field(&event, "cache_write"),
                            );
                        }
                        Some("done") => {
                            stop_reason = event
                                .get("stop_reason")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string);
                            break;
                        }
                        _ => {}
                    },
                    Err(pi_host::RecvError::Timeout) => continue,
                    Err(pi_host::RecvError::Disconnected) => {
                        budget_breach = Some("unit disconnected".to_string());
                        break;
                    }
                }
                if let Some(breach) = self.budget_breach(trigger, &usage, &before) {
                    budget_breach = Some(breach);
                    let _ = host.send(session_id, serde_json::json!({ "type": "abort" }));
                    aborted = true;
                    break;
                }
            }
        }

        let after = workspace_snapshot(workspace);
        // Files the run touched are those changed now but not before it started.
        let mut files_changed = after.changed_files(&before);
        if files_changed.is_empty() {
            files_changed = after.changed.clone();
        }
        let diff = after.diff.clone();
        let (outcome, disposition) = if budget_breach.is_some() {
            // A breached run is always contained: even a change to an
            // already-dirty file must not survive the abort.
            let disposition = if after.is_git {
                self.dispose_workspace(workspace, trigger, now)
            } else {
                Disposition::None
            };
            (ReceiptOutcome::BudgetExceeded, disposition)
        } else if stop_reason.is_none() && !aborted {
            (ReceiptOutcome::Failed, Disposition::None)
        } else {
            (ReceiptOutcome::Completed, Disposition::None)
        };

        Receipt {
            trigger_id: trigger.id.clone(),
            session_id: session_id.to_string(),
            model: trigger.model.clone(),
            prompt: trigger.prompt.clone(),
            cost_micros: self.price.cost_micros(&usage),
            usage,
            exit_code: stop_reason,
            outcome,
            breach: budget_breach,
            disposition,
            files_changed,
            diff,
            started_at: now,
            finished_at: now + started.elapsed().as_secs() as i64,
        }
    }

    /// The per-run budget crossed by the usage accumulated so far, if any.
    fn budget_breach(
        &self,
        trigger: &Trigger,
        usage: &UsageTotals,
        before: &WorkspaceSnapshot,
    ) -> Option<String> {
        let budget = &trigger.budget;
        if budget.max_tokens > 0 && usage.total_tokens() > budget.max_tokens {
            return Some("token budget exceeded".to_string());
        }
        if budget.max_cost_micros > 0 && self.price.cost_micros(usage) > budget.max_cost_micros {
            return Some("cost budget exceeded".to_string());
        }
        // A fresh diff is captured lazily only when a file budget would apply;
        // there is no file budget yet, so `before` exists for the receipt.
        let _ = before;
        None
    }

    /// Restore or park the working tree after a breach.
    fn dispose_workspace(&self, workspace: &Path, trigger: &Trigger, now: i64) -> Disposition {
        match self.rollback {
            RollbackPolicy::Keep => Disposition::None,
            RollbackPolicy::Restore => {
                if git(workspace, &["checkout", "--", "."]).is_some()
                    && git(workspace, &["clean", "-fd"]).is_some()
                {
                    Disposition::RolledBack
                } else {
                    Disposition::None
                }
            }
            RollbackPolicy::Park => {
                let message = format!("pi-native rollback: {} @ {now}", trigger.id);
                if git(workspace, &["stash", "push", "-u", "-m", &message]).is_some() {
                    Disposition::Parked
                } else {
                    Disposition::None
                }
            }
        }
    }

    /// Add a trigger, replacing any with the same id.
    pub fn upsert(&mut self, trigger: Trigger) {
        let id = trigger.id.clone();
        if let Some(existing) = self.triggers.iter_mut().find(|t| t.id == id) {
            *existing = trigger;
        } else {
            self.triggers.push(trigger);
        }
        self.last_fired.remove(&id);
    }

    /// Remove a trigger by id. Returns whether one was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.triggers.len();
        self.triggers.retain(|trigger| trigger.id != id);
        self.last_fired.remove(id);
        self.triggers.len() != before
    }

    /// Unix-seconds of the last firing for a trigger, if any.
    pub fn last_fired_at(&self, id: &str) -> Option<i64> {
        self.last_fired.get(id).copied()
    }

    fn is_due<S: RunStore + ?Sized>(&self, trigger: &Trigger, store: &S, now: i64) -> bool {
        let last = self.last_fired.get(&trigger.id).copied();
        match &trigger.schedule {
            Schedule::Interval { every } => {
                last.is_none_or(|last| now - last >= every.as_secs() as i64)
            }
            Schedule::Cron(cron) => {
                let Some(time) = time::OffsetDateTime::from_unix_timestamp(now).ok() else {
                    return false;
                };
                if !cron.matches(&time) {
                    return false;
                }
                // At most once per matching minute, within this runner...
                if last.is_some_and(|last| last / 60 == now / 60) {
                    return false;
                }
                // ...and across restarts. `last_fired` is not persisted, so the
                // durable run records must decide whether the current minute
                // already fired; otherwise a restart re-fires the same minute.
                let minute_start = now - now.rem_euclid(60);
                store.count_since(&trigger.id, minute_start) == 0
            }
        }
    }

    fn session_path(&self, trigger_id: &str) -> PathBuf {
        self.sessions_root
            .join(format!("{}.jsonl", session_file_stem(trigger_id)))
    }
}

fn int_field(event: &serde_json::Value, key: &str) -> i64 {
    event
        .get(key)
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
}

/// A cheap snapshot of a git working tree: changed files, a bounded diff, and
/// whether the path is a repository at all.
#[derive(Debug, Clone, Default)]
struct WorkspaceSnapshot {
    is_git: bool,
    changed: Vec<String>,
    diff: String,
}

impl WorkspaceSnapshot {
    fn changed_files(&self, before: &WorkspaceSnapshot) -> Vec<String> {
        self.changed
            .iter()
            .filter(|path| !before.changed.contains(path))
            .cloned()
            .collect()
    }
}

fn workspace_snapshot(workspace: &Path) -> WorkspaceSnapshot {
    if !workspace.is_dir() {
        return WorkspaceSnapshot::default();
    }
    let Some(status) = git(workspace, &["status", "--porcelain"]) else {
        return WorkspaceSnapshot::default();
    };
    let changed = status
        .lines()
        .filter_map(|line| line.get(3..).map(str::to_string))
        .map(|path| path.trim().trim_matches('"').to_string())
        .collect();
    let diff = git(workspace, &["diff", "HEAD"]).unwrap_or_default();
    let diff = if diff.len() > MAX_DIFF_BYTES {
        let mut truncated = diff[..MAX_DIFF_BYTES].to_string();
        truncated.push_str("\n… diff truncated\n");
        truncated
    } else {
        diff
    };
    WorkspaceSnapshot {
        is_git: true,
        changed,
        diff,
    }
}

/// Run a git command in `dir`, returning stdout on success.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

/// A filesystem-safe, injective session file stem for `trigger_id`.
///
/// The readable prefix keeps the file identifiable by eye, but [`sanitize`] is
/// lossy: `daily.report`, `daily/report`, and `daily report` all collapse to
/// `daily-report`. Appending a hash of the full id makes the mapping injective,
/// so distinct triggers never share a session file.
fn session_file_stem(trigger_id: &str) -> String {
    let readable = sanitize(trigger_id);
    let readable = readable.trim_matches('-');
    let mut prefix: String = readable.chars().take(48).collect();
    if prefix.is_empty() {
        prefix.push_str("trigger");
    }
    let digest = Sha256::digest(trigger_id.as_bytes());
    let mut hash = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    format!("{prefix}-{hash}")
}

/// A parsed 5-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    /// The original 5-field expression, for listing/round-tripping.
    expression: String,
    minute: Field,
    hour: Field,
    day: Field,
    month: Field,
    weekday: Field,
}

impl CronSchedule {
    pub fn parse(expression: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expression.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron expression needs 5 fields (minute hour day month weekday), got {}",
                parts.len()
            ));
        }
        Ok(Self {
            expression: expression.to_string(),
            minute: Field::parse(parts[0], 0, 59)?,
            hour: Field::parse(parts[1], 0, 23)?,
            day: Field::parse(parts[2], 1, 31)?,
            month: Field::parse(parts[3], 1, 12)?,
            weekday: Field::parse(parts[4], 0, 6)?,
        })
    }

    /// Whether `time` (UTC) satisfies the expression.
    pub fn matches(&self, time: &time::OffsetDateTime) -> bool {
        if !self.minute.matches(time.minute() as u32, 0)
            || !self.hour.matches(time.hour() as u32, 0)
            || !self.month.matches(time.month() as u32, 1)
        {
            return false;
        }
        let day = self.day.matches(time.day() as u32, 1);
        // Cron weekday is Sunday=0; `number_days_from_monday` is Monday=0.
        let weekday_number = ((time.weekday().number_days_from_monday() + 1) % 7) as u32;
        let weekday = self.weekday.matches(weekday_number, 0);
        match (self.day.restricted, self.weekday.restricted) {
            (true, true) => day || weekday,
            (true, false) => day,
            (false, true) => weekday,
            (false, false) => true,
        }
    }
}

/// A trigger definition as loaded from a JSON file.
#[derive(Debug, Clone, Deserialize)]
pub struct TriggerSpec {
    pub id: String,
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub cron: Option<String>,
    pub prompt: String,
    /// Deliver to this existing session id instead of a per-trigger session.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub dedupe_window_secs: u64,
    #[serde(default)]
    pub max_runs_per_window: usize,
    #[serde(default)]
    pub budget_window_secs: u64,
    /// Model recorded on the receipt.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-run budget (tokens/spend/wall-clock).
    #[serde(default)]
    pub budget: Budget,
    /// Cumulative per-agent budget.
    #[serde(default)]
    pub agent_budget: Budget,
}

impl TriggerSpec {
    pub fn into_trigger(self) -> Result<Trigger, String> {
        let schedule = match (self.interval_secs, self.cron.as_deref()) {
            (Some(seconds), None) => Schedule::Interval {
                every: Duration::from_secs(seconds),
            },
            (None, Some(expression)) => Schedule::Cron(CronSchedule::parse(expression)?),
            _ => {
                return Err(format!(
                    "trigger `{}` needs exactly one of `interval_secs` or `cron`",
                    self.id
                ))
            }
        };
        Ok(Trigger {
            id: self.id,
            schedule,
            prompt: self.prompt,
            target: self.target,
            dedupe_window: Duration::from_secs(self.dedupe_window_secs),
            max_runs_per_window: self.max_runs_per_window,
            budget_window: Duration::from_secs(self.budget_window_secs),
            model: self.model,
            budget: self.budget,
            agent_budget: self.agent_budget,
        })
    }
}

/// Load trigger definitions from a JSON array file.
pub fn load_triggers(path: &Path) -> Result<Vec<Trigger>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let specs: Vec<TriggerSpec> = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let mut triggers = Vec::with_capacity(specs.len());
    let mut seen = std::collections::HashSet::new();
    for spec in specs {
        let trigger = spec.into_trigger()?;
        if !seen.insert(trigger.id.clone()) {
            return Err(format!("duplicate trigger id `{}`", trigger.id));
        }
        triggers.push(trigger);
    }
    Ok(triggers)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    allowed: Vec<bool>,
    restricted: bool,
}

impl Field {
    fn parse(spec: &str, min: u32, max: u32) -> Result<Self, String> {
        let mut allowed = vec![false; (max - min + 1) as usize];
        let mut restricted = false;
        for part in spec.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((range, step)) => (
                    range,
                    step.parse::<u32>()
                        .map_err(|_| format!("invalid step in `{part}`"))?
                        .max(1),
                ),
                None => (part, 1),
            };
            if range != "*" {
                restricted = true;
            }
            let (start, end) = if range == "*" {
                (min, max)
            } else if let Some((start, end)) = range.split_once('-') {
                (
                    start
                        .parse::<u32>()
                        .map_err(|_| format!("invalid range `{range}`"))?,
                    end.parse::<u32>()
                        .map_err(|_| format!("invalid range `{range}`"))?,
                )
            } else {
                let value = range
                    .parse::<u32>()
                    .map_err(|_| format!("invalid value `{range}`"))?;
                (value, value)
            };
            if start < min || end > max || start > end {
                return Err(format!("`{part}` is outside {min}-{max}"));
            }
            let mut value = start;
            while value <= end {
                allowed[(value - min) as usize] = true;
                value += step;
            }
        }
        Ok(Self {
            allowed,
            restricted,
        })
    }

    fn matches(&self, value: u32, min: u32) -> bool {
        self.allowed
            .get((value.wrapping_sub(min)) as usize)
            .copied()
            .unwrap_or(false)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

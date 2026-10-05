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
//! Budgets currently enforced: dedupe windows, max runs per window. Model-level
//! budgets (iterations, tokens, wall-clock) belong to the unit host, which owns
//! the agent configuration.

use pi_host::Host;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a trigger runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Fire every `every`.
    Interval { every: Duration },
    /// Fire on a 5-field cron expression (`min hour day month weekday`).
    Cron(CronSchedule),
}

/// One trigger definition.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub id: String,
    pub schedule: Schedule,
    /// Prompt sent to the session each time the trigger fires.
    pub prompt: String,
    /// If non-zero, at most one episode per `dedupe_window` (restart-safe).
    pub dedupe_window: Duration,
    /// If non-zero, at most this many episodes per `budget_window`.
    pub max_runs_per_window: usize,
    pub budget_window: Duration,
}

impl Trigger {
    pub fn interval(id: impl Into<String>, every: Duration, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            schedule: Schedule::Interval { every },
            prompt: prompt.into(),
            dedupe_window: Duration::ZERO,
            max_runs_per_window: 0,
            budget_window: Duration::ZERO,
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
            dedupe_window: Duration::ZERO,
            max_runs_per_window: 0,
            budget_window: Duration::ZERO,
        })
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
}

/// Durable run records. The in-memory store is for tests and short-lived runs;
/// [`JsonlRuns`] survives restarts. `Send` so a server can own one.
pub trait RunStore: Send {
    fn record(&mut self, episode: Episode);
    fn seen(&self, trigger_id: &str, dedupe_key: &str) -> bool;
    fn count_since(&self, trigger_id: &str, since: i64) -> usize;
}

/// In-memory run records.
#[derive(Debug, Default)]
pub struct InMemoryRuns {
    episodes: Vec<Episode>,
}

impl RunStore for InMemoryRuns {
    fn record(&mut self, episode: Episode) {
        self.episodes.push(episode);
    }

    fn seen(&self, trigger_id: &str, dedupe_key: &str) -> bool {
        self.episodes
            .iter()
            .any(|episode| episode.trigger_id == trigger_id && episode.dedupe_key == dedupe_key)
    }

    fn count_since(&self, trigger_id: &str, since: i64) -> usize {
        self.episodes
            .iter()
            .filter(|episode| episode.trigger_id == trigger_id && episode.started_at >= since)
            .count()
    }
}

/// Append-only JSONL run records.
pub struct JsonlRuns {
    path: PathBuf,
    episodes: Vec<Episode>,
}

impl JsonlRuns {
    /// Open (loading existing records) or create at `path`.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut episodes = Vec::new();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(episode) = serde_json::from_str::<Episode>(line) {
                    episodes.push(episode);
                }
            }
        }
        Ok(Self { path, episodes })
    }

    pub fn load(&self) -> &[Episode] {
        &self.episodes
    }
}

impl RunStore for JsonlRuns {
    fn record(&mut self, episode: Episode) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            use std::io::Write as _;
            if let Ok(line) = serde_json::to_string(&episode) {
                let _ = writeln!(file, "{line}");
            }
        }
        self.episodes.push(episode);
    }

    fn seen(&self, trigger_id: &str, dedupe_key: &str) -> bool {
        self.episodes
            .iter()
            .any(|episode| episode.trigger_id == trigger_id && episode.dedupe_key == dedupe_key)
    }

    fn count_since(&self, trigger_id: &str, since: i64) -> usize {
        self.episodes
            .iter()
            .filter(|episode| episode.trigger_id == trigger_id && episode.started_at >= since)
            .count()
    }
}

/// Fires due triggers against a [`Host`].
pub struct Runner {
    triggers: Vec<Trigger>,
    sessions_root: PathBuf,
    last_fired: HashMap<String, i64>,
}

impl Runner {
    pub fn new(triggers: Vec<Trigger>, sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            triggers,
            sessions_root: sessions_root.into(),
            last_fired: HashMap::new(),
        }
    }

    pub fn triggers(&self) -> &[Trigger] {
        &self.triggers
    }

    /// Fire every trigger due at `now` (unix seconds). Returns the episodes.
    ///
    /// The session for a trigger is stable across episodes, so a long-lived
    /// trigger accumulates context. Records are written before the return, so a
    /// restart will not repeat an episode in the same dedupe window.
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
            .filter(|(_, trigger)| self.is_due(trigger, now))
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

            let path = self.session_path(&trigger.id);
            match host.open(path.clone()) {
                Ok(session_id) => {
                    let command = serde_json::json!({ "type": "prompt", "text": trigger.prompt });
                    if host.send(&session_id, command).is_ok() {
                        let episode = Episode {
                            trigger_id: trigger.id.clone(),
                            session_id,
                            session_path: path,
                            prompt: trigger.prompt.clone(),
                            dedupe_key,
                            started_at: now,
                        };
                        store.record(episode.clone());
                        episodes.push(episode);
                    }
                }
                Err(error) => {
                    eprintln!(
                        "pi-triggers: cannot open session for {}: {error}",
                        trigger.id
                    );
                }
            }
            self.last_fired.insert(trigger.id.clone(), now);
        }
        episodes
    }

    fn is_due(&self, trigger: &Trigger, now: i64) -> bool {
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
                // At most once per matching minute.
                last.is_none_or(|last| last / 60 != now / 60)
            }
        }
    }

    fn session_path(&self, trigger_id: &str) -> PathBuf {
        self.sessions_root
            .join(format!("{}.jsonl", sanitize(trigger_id)))
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

/// A parsed 5-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
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
    #[serde(default)]
    pub dedupe_window_secs: u64,
    #[serde(default)]
    pub max_runs_per_window: usize,
    #[serde(default)]
    pub budget_window_secs: u64,
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
            dedupe_window: Duration::from_secs(self.dedupe_window_secs),
            max_runs_per_window: self.max_runs_per_window,
            budget_window: Duration::from_secs(self.budget_window_secs),
        })
    }
}

/// Load trigger definitions from a JSON array file.
pub fn load_triggers(path: &Path) -> Result<Vec<Trigger>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let specs: Vec<TriggerSpec> = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    specs.into_iter().map(TriggerSpec::into_trigger).collect()
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

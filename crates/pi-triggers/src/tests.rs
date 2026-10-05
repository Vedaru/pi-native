use super::*;
use pi_agent::{Agent, AssistantTurn, FauxProvider, ToolContext};
use std::path::PathBuf;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-triggers-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn test_host(dir: &Path) -> Host {
    let turns = vec![AssistantTurn {
        text: "ok".to_string(),
        stop_reason: Some("end_turn".to_string()),
        ..Default::default()
    }];
    let cwd = dir.to_path_buf();
    Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(FauxProvider::new(turns.clone())),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    })
}

#[test]
fn interval_triggers_only_after_the_period() {
    let dir = temp_dir("interval");
    let mut host = test_host(&dir);
    let mut runner = Runner::new(
        vec![Trigger::interval("t1", Duration::from_secs(60), "go")],
        dir.join("sessions"),
    );
    let mut store = InMemoryRuns::default();

    assert_eq!(runner.tick(&mut host, &mut store, 1_000_000).len(), 1);
    // Same second: not due.
    assert_eq!(runner.tick(&mut host, &mut store, 1_000_000).len(), 0);
    // Before the interval elapses: not due.
    assert_eq!(runner.tick(&mut host, &mut store, 1_000_030).len(), 0);
    // Past the interval: due.
    assert_eq!(runner.tick(&mut host, &mut store, 1_000_060).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_episode_creates_and_reuses_the_session() {
    let dir = temp_dir("session");
    let mut host = test_host(&dir);
    let mut runner = Runner::new(
        vec![Trigger::interval(
            "nightly",
            Duration::from_secs(10),
            "summarize",
        )],
        dir.join("sessions"),
    );
    let mut store = InMemoryRuns::default();

    let first = runner.tick(&mut host, &mut store, 2_000_000);
    assert_eq!(first.len(), 1);
    assert!(
        first[0].session_path.exists(),
        "{:?}",
        first[0].session_path
    );

    // A second firing reuses the same session file/id.
    let second = runner.tick(&mut host, &mut store, 2_000_010);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].session_path, first[0].session_path);
    assert_eq!(second[0].session_id, first[0].session_id);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dedupe_window_suppresses_repeats() {
    let dir = temp_dir("dedupe");
    let mut host = test_host(&dir);
    let mut trigger = Trigger::interval("t", Duration::from_secs(1), "go");
    trigger.dedupe_window = Duration::from_secs(3600);
    let mut runner = Runner::new(vec![trigger], dir.join("sessions"));
    let mut store = InMemoryRuns::default();

    assert_eq!(runner.tick(&mut host, &mut store, 100).len(), 1);
    assert_eq!(runner.tick(&mut host, &mut store, 101).len(), 0);
    // Next hour window fires again.
    assert_eq!(runner.tick(&mut host, &mut store, 3700).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn max_runs_per_window_is_enforced() {
    let dir = temp_dir("budget");
    let mut host = test_host(&dir);
    let mut trigger = Trigger::interval("t", Duration::from_secs(1), "go");
    trigger.max_runs_per_window = 1;
    trigger.budget_window = Duration::from_secs(3600);
    let mut runner = Runner::new(vec![trigger], dir.join("sessions"));
    let mut store = InMemoryRuns::default();

    assert_eq!(runner.tick(&mut host, &mut store, 100).len(), 1);
    assert_eq!(runner.tick(&mut host, &mut store, 200).len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn jsonl_runs_survive_a_restart() {
    let dir = temp_dir("jsonl");
    let path = dir.join("runs.jsonl");
    {
        let mut store = JsonlRuns::open(&path).expect("open");
        store.record(Episode {
            trigger_id: "t".into(),
            session_id: "s".into(),
            session_path: dir.join("s.jsonl"),
            prompt: "go".into(),
            dedupe_key: "t:1".into(),
            started_at: 10,
        });
    }
    let reopened = JsonlRuns::open(&path).expect("reopen");
    assert!(reopened.seen("t", "t:1"));
    assert_eq!(reopened.load().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn loads_trigger_specs_from_json() {
    let dir = temp_dir("spec");
    let path = dir.join("triggers.json");
    std::fs::write(
        &path,
        r#"[
          {"id":"every-hour","interval_secs":3600,"prompt":"sweep","dedupe_window_secs":3600},
          {"id":"weekday","cron":"0 9 * * 1-5","prompt":"standup","max_runs_per_window":1,"budget_window_secs":86400}
        ]"#,
    )
    .expect("write");
    let triggers = load_triggers(&path).expect("load");
    assert_eq!(triggers.len(), 2);
    assert_eq!(triggers[0].id, "every-hour");
    assert_eq!(triggers[0].dedupe_window, Duration::from_secs(3600));
    assert!(matches!(triggers[1].schedule, Schedule::Cron(_)));
    assert_eq!(triggers[1].max_runs_per_window, 1);

    // A spec with neither schedule is rejected.
    std::fs::write(&path, r#"[{"id":"bad","prompt":"x"}]"#).expect("write");
    assert!(load_triggers(&path).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cron_fields_parse_and_match() {
    let every_quarter = CronSchedule::parse("*/15 * * * *").expect("parse");
    assert!(every_quarter.matches(&time::macros::datetime!(2026-01-05 09:15:00 UTC)));
    assert!(every_quarter.matches(&time::macros::datetime!(2026-01-05 09:45:00 UTC)));
    assert!(!every_quarter.matches(&time::macros::datetime!(2026-01-05 09:16:00 UTC)));

    let weekday_mornings = CronSchedule::parse("0 9 * * 1-5").expect("parse");
    // Monday 2026-01-05.
    assert!(weekday_mornings.matches(&time::macros::datetime!(2026-01-05 09:00:00 UTC)));
    // Saturday 2026-01-10.
    assert!(!weekday_mornings.matches(&time::macros::datetime!(2026-01-10 09:00:00 UTC)));

    assert!(CronSchedule::parse("* * *").is_err());
    assert!(CronSchedule::parse("90 * * * *").is_err());
}

#[test]
fn cron_runner_fires_once_per_matching_minute() {
    let dir = temp_dir("cron-run");
    let mut host = test_host(&dir);
    let trigger = Trigger::cron("cron", "*/15 * * * *", "go").expect("trigger");
    let mut runner = Runner::new(vec![trigger], dir.join("sessions"));
    let mut store = InMemoryRuns::default();

    let at = time::macros::datetime!(2026-01-05 09:15:00 UTC).unix_timestamp();
    assert_eq!(runner.tick(&mut host, &mut store, at).len(), 1);
    // Same minute: not due again.
    assert_eq!(runner.tick(&mut host, &mut store, at + 10).len(), 0);
    // Next matching minute.
    let next = time::macros::datetime!(2026-01-05 09:30:00 UTC).unix_timestamp();
    assert_eq!(runner.tick(&mut host, &mut store, next).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

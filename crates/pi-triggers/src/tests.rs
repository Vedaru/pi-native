use super::*;
use pi_agent::{Agent, AssistantTurn, FauxProvider, ToolContext};
use pi_providers::Usage;
use std::path::PathBuf;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-triggers-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn test_host(dir: &Path) -> Host {
    test_host_with_usage(dir, None)
}

fn test_host_with_usage(dir: &Path, usage: Option<Usage>) -> Host {
    let turns = vec![AssistantTurn {
        text: "ok".to_string(),
        stop_reason: Some("end_turn".to_string()),
        usage,
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
fn a_budgeted_run_stops_and_records_a_receipt() {
    let dir = temp_dir("receipt-budget");
    let usage = Usage {
        input: 100,
        output: 50,
        ..Default::default()
    };
    let mut host = test_host_with_usage(&dir, Some(usage));
    let mut trigger = Trigger::interval("t", Duration::from_secs(1), "go");
    trigger.model = Some("deepseek-flash".to_string());
    trigger.budget.max_tokens = 10;
    let mut runner = Runner::new(vec![trigger], dir.join("sessions"));
    let mut store = InMemoryRuns::default();

    assert_eq!(runner.tick(&mut host, &mut store, 100).len(), 1);
    let receipts = store.receipts();
    assert_eq!(receipts.len(), 1);
    let receipt = &receipts[0];
    assert_eq!(receipt.outcome, ReceiptOutcome::BudgetExceeded);
    assert_eq!(receipt.model.as_deref(), Some("deepseek-flash"));
    assert_eq!(receipt.usage.total_tokens(), 150);
    assert!(receipt.breach.as_deref().unwrap().contains("token"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cost_budget_uses_configured_price() {
    let dir = temp_dir("receipt-cost");
    let usage = Usage {
        input: 1_000_000,
        output: 1_000_000,
        ..Default::default()
    };
    let mut host = test_host_with_usage(&dir, Some(usage));
    let mut trigger = Trigger::interval("t", Duration::from_secs(1), "go");
    trigger.budget.max_cost_micros = 1_000;
    let price = Price {
        input_micros_per_million: 1_000,
        output_micros_per_million: 1_000,
        ..Default::default()
    };
    let mut runner = Runner::new(vec![trigger], dir.join("sessions")).with_price(price);
    let mut store = InMemoryRuns::default();

    runner.tick(&mut host, &mut store, 100);
    let receipt = &store.receipts()[0];
    assert_eq!(receipt.cost_micros, 2_000);
    assert_eq!(receipt.outcome, ReceiptOutcome::BudgetExceeded);
    assert!(receipt.breach.as_deref().unwrap().contains("cost"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_completed_run_records_a_receipt() {
    let dir = temp_dir("receipt-ok");
    let usage = Usage {
        input: 5,
        output: 5,
        ..Default::default()
    };
    let mut host = test_host_with_usage(&dir, Some(usage));
    let trigger = Trigger::interval("t", Duration::from_secs(1), "go");
    let mut runner = Runner::new(vec![trigger], dir.join("sessions"));
    let mut store = InMemoryRuns::default();

    runner.tick(&mut host, &mut store, 100);
    let receipt = &store.receipts()[0];
    assert_eq!(receipt.outcome, ReceiptOutcome::Completed);
    assert_eq!(receipt.exit_code.as_deref(), Some("end_turn"));
    assert_eq!(receipt.usage.total_tokens(), 10);
    assert!(receipt.breach.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn per_agent_budget_refuses_and_receipts_the_next_run() {
    let dir = temp_dir("agent-budget");
    let usage = Usage {
        input: 100,
        output: 0,
        ..Default::default()
    };
    let mut host = test_host_with_usage(&dir, Some(usage));
    let mut trigger = Trigger::interval("t", Duration::from_secs(1), "go");
    trigger.agent_budget.max_tokens = 10;
    let mut runner = Runner::new(vec![trigger], dir.join("sessions"));
    let mut store = InMemoryRuns::default();

    // First run completes but spends past the agent budget.
    assert_eq!(runner.tick(&mut host, &mut store, 100).len(), 1);
    assert_eq!(store.receipts().len(), 1);
    // The next firing is refused before it starts, with its own receipt.
    assert_eq!(runner.tick(&mut host, &mut store, 200).len(), 0);
    assert_eq!(store.receipts().len(), 2);
    let refusal = &store.receipts()[1];
    assert_eq!(refusal.outcome, ReceiptOutcome::BudgetExceeded);
    assert!(refusal.breach.as_deref().unwrap().contains("agent token"));
    assert!(refusal.session_id.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_breached_run_restores_the_working_tree() {
    let dir = temp_dir("rollback");
    // A git repository whose tracked file we modify after the run starts.
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    assert!(git(&repo, &["init"]).is_some());
    std::fs::write(repo.join("tracked.txt"), "original\n").expect("write");
    assert!(git(&repo, &["add", "tracked.txt"]).is_some());
    assert!(git(
        &repo,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "init",
        ],
    )
    .is_some());

    let usage = Usage {
        input: 100,
        output: 0,
        ..Default::default()
    };
    let mut host = test_host_with_usage(&repo, Some(usage));
    let mut trigger = Trigger::interval("t", Duration::from_secs(1), "go");
    trigger.budget.max_tokens = 1;
    let mut runner = Runner::new(vec![trigger], dir.join("sessions"))
        .with_workspace(&repo)
        .with_rollback(RollbackPolicy::Restore);
    let mut store = InMemoryRuns::default();

    // Simulate the run leaving a change behind: the runner snapshots before and
    // disposes after, so mutate the file before the abort is applied is hard to
    // interleave; instead assert a change made before the tick is rolled back.
    std::fs::write(repo.join("tracked.txt"), "dirty\n").expect("write");
    runner.tick(&mut host, &mut store, 100);
    let receipt = &store.receipts()[0];
    assert_eq!(receipt.outcome, ReceiptOutcome::BudgetExceeded);
    assert_eq!(receipt.disposition, Disposition::RolledBack);
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).expect("read"),
        "original\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn jsonl_receipts_survive_a_restart() {
    let dir = temp_dir("jsonl-receipts");
    let path = dir.join("runs.jsonl");
    let receipt = Receipt {
        trigger_id: "t".into(),
        session_id: "s".into(),
        model: Some("m".into()),
        prompt: "go".into(),
        usage: UsageTotals {
            input: 1,
            output: 2,
            ..Default::default()
        },
        cost_micros: 3,
        exit_code: Some("end_turn".into()),
        outcome: ReceiptOutcome::Completed,
        breach: None,
        disposition: Disposition::None,
        files_changed: vec!["a.txt".into()],
        diff: "diff".into(),
        started_at: 10,
        finished_at: 11,
    };
    {
        let mut store = JsonlRuns::open(&path).expect("open");
        store.record_receipt(receipt.clone());
    }
    let reopened = JsonlRuns::open(&path).expect("reopen");
    assert_eq!(reopened.load_receipts(), &[receipt]);
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
            error: None,
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

#[test]
fn sanitized_ids_get_distinct_session_files() {
    let dir = temp_dir("session-collision");
    let runner = Runner::new(Vec::new(), dir.join("sessions"));

    // These three ids all sanitize to `daily-report`, but must not collide.
    let dotted = runner.session_path("daily.report");
    let slashed = runner.session_path("daily/report");
    let spaced = runner.session_path("daily report");
    assert_ne!(dotted, slashed);
    assert_ne!(slashed, spaced);
    assert_ne!(dotted, spaced);

    // The id stays recognizable in the on-disk name.
    let name = dotted.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.starts_with("daily-report-"), "{name}");
    assert!(name.ends_with(".jsonl"), "{name}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_triggers_rejects_duplicate_ids() {
    let dir = temp_dir("duplicate-ids");
    let path = dir.join("triggers.json");
    std::fs::write(
        &path,
        r#"[
          {"id":"dup","interval_secs":60,"prompt":"first"},
          {"id":"dup","interval_secs":120,"prompt":"second"}
        ]"#,
    )
    .expect("write");
    let error = load_triggers(&path).expect_err("duplicate ids must be rejected");
    assert!(error.contains("duplicate"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failed_send_is_recorded_and_retried() {
    let dir = temp_dir("send-failure");
    let mut runner = Runner::new(
        vec![Trigger::interval("t", Duration::from_secs(1), "go")],
        dir.join("sessions"),
    );
    // Open the exact session the runner will use, with a host whose unit thread
    // dies immediately (the factory panics). This models a unit that died while
    // retaining the runner, so a later `send` fails.
    let path = runner.session_path("t");
    std::fs::create_dir_all(path.parent().unwrap()).expect("sessions dir");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let cwd = dir.to_string_lossy().to_string();
    let mut host = Host::new(cwd, move |_unit_cwd: &str| {
        let _ = ready_tx.send(());
        panic!("unit died");
    });
    let session_id = host.open(path.clone()).expect("open");
    ready_rx.recv().expect("factory ran");
    // Wait until the dead unit's channel actually rejects sends.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host
        .send(
            &session_id,
            serde_json::json!({ "type": "prompt", "text": "probe" }),
        )
        .is_ok()
    {
        assert!(std::time::Instant::now() < deadline, "unit never died");
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut store = InMemoryRuns::default();

    // The failed attempt is reported and recorded rather than silently dropped.
    let first = runner.tick(&mut host, &mut store, 1_000);
    assert_eq!(first.len(), 1);
    assert!(first[0].error.is_some(), "failure must be surfaced");
    assert_eq!(
        store.count_since("t", 0),
        0,
        "failures do not count as runs"
    );

    // `last_fired` was not advanced, so the next tick retries.
    let second = runner.tick(&mut host, &mut store, 1_001);
    assert_eq!(second.len(), 1, "a failed trigger must be retried");
    assert!(second[0].error.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cron_does_not_double_fire_after_restart() {
    let dir = temp_dir("cron-restart");
    let mut host = test_host(&dir);
    let runs = dir.join("runs.jsonl");

    let trigger = || Trigger::cron("cron", "*/15 * * * *", "go").expect("trigger");
    let at = time::macros::datetime!(2026-01-05 09:15:00 UTC).unix_timestamp();

    let mut store = JsonlRuns::open(&runs).expect("open runs");
    let mut runner = Runner::new(vec![trigger()], dir.join("sessions"));
    assert_eq!(runner.tick(&mut host, &mut store, at).len(), 1);

    // Simulate a restart: fresh Runner and a fresh store over the same records,
    // still inside the same matching minute.
    let mut store = JsonlRuns::open(&runs).expect("reopen runs");
    let mut runner = Runner::new(vec![trigger()], dir.join("sessions"));
    assert_eq!(
        runner.tick(&mut host, &mut store, at + 10).len(),
        0,
        "a restart must not re-fire the same cron minute"
    );

    // The following matching minute is free to fire again.
    let following = time::macros::datetime!(2026-01-05 09:30:00 UTC).unix_timestamp();
    let mut store = JsonlRuns::open(&runs).expect("reopen runs");
    let mut runner = Runner::new(vec![trigger()], dir.join("sessions"));
    assert_eq!(runner.tick(&mut host, &mut store, following).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

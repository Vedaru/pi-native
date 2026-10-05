use super::*;
use pi_agent::{AssistantTurn, FauxProvider, ToolContext};
use serde_json::json;
use std::time::{Duration, Instant};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-host-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn one_turn_host(cwd: &std::path::Path, text: &str) -> Host {
    let turns = vec![AssistantTurn {
        text: text.to_string(),
        stop_reason: Some("end_turn".to_string()),
        ..Default::default()
    }];
    let cwd = cwd.to_path_buf();
    Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(FauxProvider::new(turns.clone())),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    })
}

/// Receive until an event with `kind`, or timeout.
fn wait_for(subscription: &Subscription, kind: &str, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match subscription.recv_timeout(remaining) {
            Some(event) if event.get("type").and_then(Value::as_str) == Some(kind) => {
                return Some(event)
            }
            Some(_) => continue,
            None => return None,
        }
    }
}

#[test]
fn two_subscribers_see_the_same_events() {
    let dir = temp_dir("fanout");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");

    let first = host.subscribe(&id).expect("subscribe");
    let second = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "go" }))
        .expect("send");

    let done = wait_for(&first, "done", Duration::from_secs(2)).expect("first sees done");
    assert_eq!(done["type"], json!("done"));

    let mut second_saw_text = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(event) = second.try_recv() {
            if event["type"] == json!("assistant_text") {
                second_saw_text = true;
                break;
            }
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(
        second_saw_text,
        "second subscriber missed the assistant text"
    );
}

#[test]
fn subscribers_are_isolated_per_session() {
    let dir = temp_dir("isolated");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    let subject = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "go" }))
        .expect("send");
    assert!(wait_for(&subject, "done", Duration::from_secs(2)).is_some());
}

#[test]
fn a_late_subscriber_replays_the_turn() {
    let dir = temp_dir("replay");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    let early = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "go" }))
        .expect("send");
    assert!(wait_for(&early, "done", Duration::from_secs(2)).is_some());

    // Attaching now still sees the turn from the replay buffer.
    let late = host.subscribe(&id).expect("subscribe");
    let types: Vec<String> = late
        .replay
        .iter()
        .filter_map(|event| {
            event
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert!(types.contains(&"assistant_text".to_string()), "{types:?}");
    assert!(types.contains(&"done".to_string()), "{types:?}");
}

#[test]
fn suspend_releases_the_agent_and_wake_resumes() {
    let dir = temp_dir("suspend");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    let first = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "one" }))
        .expect("send");
    assert!(wait_for(&first, "done", Duration::from_secs(2)).is_some());

    host.suspend(&id);
    assert!(!host.is_running(&id), "unit should be suspended");

    // Sending wakes it and a new turn runs.
    let resumed = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "two" }))
        .expect("send");
    assert!(host.is_running(&id));
    assert!(wait_for(&resumed, "done", Duration::from_secs(2)).is_some());
}

#[test]
fn unknown_session_is_an_error() {
    let dir = temp_dir("unknown");
    let mut host = one_turn_host(&dir, "hello");
    match host.send("nope", json!({ "type": "get_state" })) {
        Err(HostError::UnknownSession(id)) => assert_eq!(id, "nope"),
        other => panic!("expected unknown session, got {other:?}"),
    }
}

#[test]
fn units_run_in_their_session_cwd() {
    let dir = temp_dir("per-cwd");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let mut host = Host::new(dir.to_string_lossy().to_string(), move |unit_cwd: &str| {
        recorder.lock().unwrap().push(unit_cwd.to_string());
        Agent::new(
            Box::new(FauxProvider::new(Vec::new())),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    });

    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let a = pi_agent::new_session_path_in(&agent_dir, std::path::Path::new("/tmp/project-a"))
        .expect("session a");
    let b = pi_agent::new_session_path_in(&agent_dir, std::path::Path::new("/tmp/project-b"))
        .expect("session b");
    host.open(a).expect("open a");
    host.open(b).expect("open b");

    let deadline = Instant::now() + Duration::from_secs(2);
    while seen.lock().unwrap().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let cwds = seen.lock().unwrap().clone();
    assert!(cwds.contains(&"/tmp/project-a".to_string()), "{cwds:?}");
    assert!(cwds.contains(&"/tmp/project-b".to_string()), "{cwds:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

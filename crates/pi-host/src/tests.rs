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
            Ok(event) if event.get("type").and_then(Value::as_str) == Some(kind) => {
                return Some(event)
            }
            Ok(_) => continue,
            Err(_) => return None,
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
fn remove_forgets_the_unit_and_reports_unknown() {
    let dir = temp_dir("remove");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");

    assert!(host.remove(&id), "removing a known unit returns true");
    assert!(!host.is_running(&id));
    assert!(!host.session_ids().contains(&id));
    assert!(host.swarm().is_empty());
    // A removed unit is no longer addressable.
    match host.subscribe(&id) {
        Err(HostError::UnknownSession(seen)) => assert_eq!(seen, id),
        Ok(_) => panic!("expected unknown session, got Ok"),
        Err(other) => panic!("expected unknown session, got {other:?}"),
    }
    assert!(!host.remove(&id), "removing twice returns false");

    // The session file survives, so reopening the same path works again.
    let reopened = host.open(dir.join("s.jsonl")).expect("reopen");
    assert_eq!(reopened, id);
    assert!(host.is_running(&reopened));
    let _ = std::fs::remove_dir_all(&dir);
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

#[test]
fn published_events_get_increasing_sequence_ids() {
    let dir = temp_dir("seq");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    let subscription = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "go" }))
        .expect("send");

    let mut seen: Vec<u64> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match subscription.recv_sequenced_timeout(Duration::from_millis(50)) {
            Ok((seq, event)) => {
                seen.push(seq);
                if event["type"] == json!("done") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(seen.len() >= 2, "expected several events: {seen:?}");
    assert!(
        seen.windows(2).all(|pair| pair[0] < pair[1]),
        "sequence ids must strictly increase: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_ids_align_with_replay_and_track_the_oldest() {
    let dir = temp_dir("replay-ids");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    let early = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "go" }))
        .expect("send");
    assert!(wait_for(&early, "done", Duration::from_secs(2)).is_some());

    let late = host.subscribe(&id).expect("subscribe");
    assert_eq!(late.replay.len(), late.replay_ids.len());
    assert!(
        late.replay_ids.windows(2).all(|pair| pair[0] < pair[1]),
        "replay ids must increase: {:?}",
        late.replay_ids
    );
    assert_eq!(
        late.oldest_replay_seq(),
        late.replay_ids[0],
        "oldest_replay_seq must be the first buffered id"
    );
    assert!(
        late.next_seq > *late.replay_ids.last().unwrap(),
        "next_seq must be beyond the buffer"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_refuses_new_units_at_the_cap() {
    let dir = temp_dir("cap");
    let mut host = one_turn_host(&dir, "hello").with_max_units(1);
    let first = host.open(dir.join("a.jsonl")).expect("open a");

    match host.open(dir.join("b.jsonl")) {
        Err(HostError::AtCapacity(1)) => {}
        other => panic!("expected AtCapacity, got {other:?}"),
    }

    // A unit already inside the cap can still be reopened.
    let again = host.open(dir.join("a.jsonl")).expect("reopen a");
    assert_eq!(again, first);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A gate the test opens to let a blocked provider continue.
type Gate = std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

/// A provider whose first call blocks until the gate opens, so the unit stays
/// mid-turn with no intervening events (the long-silent-step case).
struct BlockingProvider {
    gate: Gate,
    blocked: std::sync::atomic::AtomicBool,
}

impl pi_agent::ModelProvider for BlockingProvider {
    fn complete(
        &self,
        _request: &pi_agent::CompletionRequest<'_>,
    ) -> Result<AssistantTurn, pi_agent::AgentError> {
        if !self.blocked.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let (lock, cv) = &*self.gate;
            let mut released = lock.lock().expect("gate lock");
            while !*released {
                released = cv.wait(released).expect("gate wait");
            }
        }
        Ok(AssistantTurn {
            text: "released".to_string(),
            stop_reason: Some("end_turn".to_string()),
            ..Default::default()
        })
    }
}

fn blocking_host(cwd: &std::path::Path) -> (Host, Gate) {
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let provider_gate = gate.clone();
    let cwd = cwd.to_path_buf();
    let host = Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(BlockingProvider {
                gate: provider_gate.clone(),
                blocked: std::sync::atomic::AtomicBool::new(false),
            }),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    });
    (host, gate)
}

/// A unit inside a long, silent step reports `in_flight`; once the run settles
/// it reports idle (VED-387).
#[test]
fn in_flight_is_true_during_a_silent_turn_and_false_after() {
    let dir = temp_dir("in-flight");
    let (mut host, gate) = blocking_host(&dir);
    let id = host.open(dir.join("s.jsonl")).expect("open");

    // Idle before any work.
    assert!(!host.in_flight(&id).expect("known session"));
    assert!(!host.swarm()[0].in_flight);

    let subscription = host.subscribe(&id).expect("subscribe");
    host.send(&id, json!({ "type": "prompt", "text": "go" }))
        .expect("send");

    // `agent_start` opens the run; the provider then blocks with no further
    // events, so the old event-timestamp heuristic would go stale here.
    wait_for(&subscription, "agent_start", Duration::from_secs(2)).expect("agent_start");
    // Give any race a moment, then assert the explicit flag is set.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !host.in_flight(&id).expect("known session") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        host.in_flight(&id).expect("known session"),
        "mid-turn unit must be in flight"
    );
    assert!(
        host.swarm()[0].in_flight,
        "swarm snapshot must carry the flag"
    );

    // Open the gate: the turn finishes and the run settles.
    {
        let (lock, cv) = &*gate;
        *lock.lock().expect("gate lock") = true;
        cv.notify_all();
    }
    wait_for(&subscription, "agent_settled", Duration::from_secs(2)).expect("agent_settled");
    let deadline = Instant::now() + Duration::from_secs(2);
    while host.in_flight(&id).expect("known session") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !host.in_flight(&id).expect("known session"),
        "settled unit must be idle"
    );
    assert!(!host.swarm()[0].in_flight);

// ---- VED-376: honest worker liveness and restart -------------------------

/// A host whose unit factory panics immediately, standing in for a worker that
/// crashed while the host still holds its command sender.
fn panicking_host(cwd: &std::path::Path) -> Host {
    let cwd = cwd.to_path_buf();
    Host::new(cwd.to_string_lossy().to_string(), move |_unit_cwd: &str| {
        panic!("worker crash for the VED-376 liveness test");
    })
}

/// Poll `predicate` until it holds or `timeout` elapses.
fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    predicate()
}

#[test]
fn a_crashed_worker_is_not_reported_running() {
    // The host holds the command sender, so the channel alone says "running".
    // The Drop guard must clear liveness on panic (H1/H3/H4).
    let dir = temp_dir("crash");
    let mut host = panicking_host(&dir);
    let id = host.open(dir.join("s.jsonl")).expect("open");

    assert!(
        wait_until(|| !host.is_alive(&id), Duration::from_secs(2)),
        "a panicked worker must not stay alive"
    );
    assert!(!host.is_alive(&id), "crashed worker reports alive");
    assert!(
        !host.is_running(&id),
        "crashed worker must not report running (the running-lies-after-panic bug)"
    );
    let info = host
        .swarm()
        .into_iter()
        .find(|unit| unit.session_id == id)
        .expect("unit in swarm");
    assert!(
        !info.running,
        "UnitInfo.running must be honest after a crash"
    );
    assert!(info.dead, "a crashed unit must be marked dead");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn in_flight_is_an_error_for_an_unknown_session() {
    let dir = temp_dir("in-flight-unknown");
    let host = one_turn_host(&dir, "hello");
    assert!(host.in_flight("missing").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_live_unit_reports_running_and_alive() {
    // H3: a healthy unit is both alive and running.
    let dir = temp_dir("alive");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    assert!(host.is_alive(&id));
    assert!(host.is_running(&id));
    let info = host
        .swarm()
        .into_iter()
        .find(|u| u.session_id == id)
        .unwrap();
    assert!(info.running && !info.dead && !info.suspended);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_suspended_unit_is_addressable_but_not_alive() {
    // H2/H9: suspend drops the sender; the worker exits and clears its guard,
    // but the unit stays addressable (a later send wakes it).
    let dir = temp_dir("suspended-liveness");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    host.suspend(&id);
    assert!(
        wait_until(|| !host.is_alive(&id), Duration::from_secs(2)),
        "a suspended worker exits and is not alive"
    );
    assert!(!host.is_running(&id));
    let info = host
        .swarm()
        .into_iter()
        .find(|u| u.session_id == id)
        .unwrap();
    assert!(info.suspended, "a suspended unit is marked suspended");
    assert!(!info.dead, "a suspended unit is not dead");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restart_wakes_a_suspended_unit_preserving_the_handle() {
    // H5/H8: restart rebuilds the agent without changing the session id.
    let dir = temp_dir("restart-suspended");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    host.suspend(&id);
    assert!(wait_until(|| !host.is_alive(&id), Duration::from_secs(2)));

    assert!(host.restart(&id), "restart knows the session");
    assert!(host.is_alive(&id), "restart wakes the unit");
    // The same handle still addresses the session and can run a turn.
    let sub = host.subscribe(&id).expect("subscribe after restart");
    host.send(&id, json!({ "type": "prompt", "text": "go" }))
        .expect("send after restart");
    assert!(wait_for(&sub, "done", Duration::from_secs(2)).is_some());
    assert_eq!(host.swarm()[0].session_id, id);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restart_respawns_a_dead_worker_without_reopening() {
    // H6/H10: a dead thread is respawned through restart, never `open`, so it
    // cannot hit the capacity ceiling even at the cap.
    let dir = temp_dir("restart-dead");
    let mut host = panicking_host(&dir).with_max_units(1);
    let id = host.open(dir.join("s.jsonl")).expect("open");
    assert!(wait_until(|| !host.is_alive(&id), Duration::from_secs(2)));
    assert!(host.is_dead(&id));

    assert!(host.restart(&id), "restart knows the session");
    // The new worker panics again, but restart itself must not have needed a
    // new unit slot: the session id is unchanged and capacity was never hit.
    let info = host
        .swarm()
        .into_iter()
        .find(|u| u.session_id == id)
        .unwrap();
    assert_eq!(info.session_id, id);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restart_leaves_a_live_unit_alone() {
    // H7: a live unit is a no-op, so double recovery cannot spawn two threads.
    let dir = temp_dir("restart-live");
    let mut host = one_turn_host(&dir, "hello");
    let id = host.open(dir.join("s.jsonl")).expect("open");
    assert!(host.restart(&id));
    assert!(host.is_alive(&id));
    assert!(host.is_running(&id));
    let _ = std::fs::remove_dir_all(&dir);
}

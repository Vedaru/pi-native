use super::*;
use pi_agent::{AssistantTurn, FauxProvider, ToolContext};
use serde_json::{json, Value};

fn agent_with(turns: Vec<AssistantTurn>) -> Agent {
    Agent::new(
        Box::new(FauxProvider::new(turns)),
        Vec::new(),
        "system",
        ToolContext::new(std::env::temp_dir()),
    )
}

#[test]
fn prompt_runs_the_agent_and_reports_events() {
    let mut agent = agent_with(vec![AssistantTurn {
        text: "hello".into(),
        stop_reason: Some("end_turn".into()),
        ..Default::default()
    }]);
    let events = handle(&mut agent, Request::Prompt { text: "hi".into() });
    assert_eq!(
        events,
        vec![
            Event::AgentStart,
            Event::TurnStart,
            Event::AssistantText {
                text: "hello".into()
            },
            Event::TurnEnd,
            Event::Done {
                stop_reason: Some("end_turn".into())
            },
            Event::AgentSettled,
        ]
    );
}

#[test]
fn get_state_reports_the_resolved_context() {
    let mut agent = agent_with(vec![AssistantTurn::default()]);
    agent.push_user("hello");
    let events = handle(&mut agent, Request::GetState);
    let Event::State {
        messages,
        system,
        transcript,
        settings: _,
    } = &events[0]
    else {
        panic!("expected state, got {events:?}");
    };
    assert_eq!(*messages, 1);
    assert_eq!(system, "system");
    assert_eq!(transcript[0]["role"], serde_json::json!("user"));
    assert_eq!(
        transcript[0]["content"],
        serde_json::json!([{ "type": "text", "text": "hello" }])
    );
}

#[test]
fn serve_writes_ready_and_streams_events_as_json_lines() {
    let mut agent = agent_with(vec![AssistantTurn {
        text: "hi".into(),
        ..Default::default()
    }]);
    let input = concat!(
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
        "{\"type\":\"get_state\"}\n",
    );
    let mut output = Vec::new();
    serve(&mut agent, input.as_bytes(), &mut output).expect("serves");

    let lines: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid json"))
        .collect();

    assert_eq!(lines[0]["type"], json!("ready"));
    assert_eq!(lines[0]["version"], json!(1));
    assert!(lines
        .iter()
        .any(|line| line["type"] == json!("assistant_text")));
    assert!(lines.iter().any(|line| line["type"] == json!("done")));
    assert!(lines.iter().any(|line| line["type"] == json!("state")));
}

#[test]
fn invalid_request_yields_an_error_event() {
    let mut agent = agent_with(Vec::new());
    let mut output = Vec::new();
    serve(&mut agent, "{not json}\n".as_bytes(), &mut output).expect("serves");
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("\"type\":\"error\""), "{text}");
}

#[test]
fn serve_with_runs_the_hook_after_each_prompt() {
    let mut agent = agent_with(vec![AssistantTurn {
        text: "hi".into(),
        ..Default::default()
    }]);
    let input = "{\"type\":\"prompt\",\"text\":\"go\"}\n";
    let mut output = Vec::new();
    let mut counts = Vec::new();
    serve_with(&mut agent, input.as_bytes(), &mut output, |agent| {
        counts.push(agent.messages().len());
    })
    .expect("serves");
    // The hook sees the finished transcript: the user prompt and the reply.
    assert_eq!(counts, vec![2]);
}

#[test]
fn serve_session_runs_tools_without_approval() {
    use pi_agent::ToolCall;
    use pi_tools::{default_tools, ToolContext};

    #[derive(Clone)]
    struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let turns = vec![
        AssistantTurn {
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "echo hi" }),
            }],
            stop_reason: Some("tool_use".into()),
            ..Default::default()
        },
        AssistantTurn {
            text: "done".into(),
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        },
    ];
    let mut agent = Agent::new(
        Box::new(FauxProvider::new(turns)),
        default_tools(),
        "system",
        ToolContext::new(std::env::temp_dir()),
    );
    let input = "{\"type\":\"prompt\",\"text\":\"go\"}\n";
    let buf = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        ".",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(!text.contains("\"type\":\"ui_request\""), "{text}");
    assert!(text.contains("\"type\":\"tool_end\""), "{text}");
    assert!(!text.contains("\"is_error\":true"), "{text}");
}

#[derive(Clone)]
struct SessionBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
impl std::io::Write for SessionBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn serve_session_answers_tree_messages_and_resume() {
    let dir = std::env::temp_dir().join(format!("pi-rpc-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("s.jsonl");
    let lines = concat!(
        "{\"type\":\"header\",\"id\":\"h\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"version\":1}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        "{\"type\":\"message\",\"id\":\"m2\",\"parentId\":\"m1\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    );
    std::fs::write(&path, lines).expect("write session");

    // The unit seeds from the session and answers navigation commands.
    let mut agent = agent_with(vec![AssistantTurn::default()]);
    let input = concat!(
        "{\"type\":\"get_tree\"}\n",
        "{\"type\":\"get_messages\"}\n",
        "{\"type\":\"get_last_assistant_text\"}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        Some(path.clone()),
        "/tmp",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"command\":\"get_tree\""), "{text}");
    assert!(text.contains("\"parentId\":\"m1\""), "{text}");
    assert!(
        text.contains("\"command\":\"get_last_assistant_text\""),
        "{text}"
    );
    assert!(text.contains("\"text\":\"hi\""), "{text}");
    // The seeded transcript is 2 messages.
    assert_eq!(agent.messages().len(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn compact_command_reports_first_kept_entry() {
    let dir = std::env::temp_dir().join(format!("pi-rpc-compact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("s.jsonl");
    let mut lines = String::from(
        "{\"type\":\"session\",\"id\":\"h\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"version\":3}\n",
    );
    for index in 0..12 {
        let parent = if index == 0 {
            "null".to_string()
        } else {
            format!("\"m{}\"", index - 1)
        };
        lines.push_str(&format!(
            "{{\"type\":\"message\",\"id\":\"m{index}\",\"parentId\":{parent},\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"message number {index} with some words\"}}}}\n"
        ));
    }
    std::fs::write(&path, lines).expect("write session");

    let mut agent = Agent::new(
        Box::new(FauxProvider::new(Vec::new())),
        Vec::new(),
        "system",
        ToolContext::new(std::env::temp_dir()),
    )
    .with_compaction(100, 4);

    let input = "{\"type\":\"compact\"}\n";
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        Some(path.clone()),
        &dir.to_string_lossy(),
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"command\":\"compact\""), "{text}");
    assert!(text.contains("\"reason\":\"manual\""), "{text}");
    assert!(
        text.contains("\"firstKeptEntryId\":\""),
        "firstKeptEntryId missing: {text}"
    );

    // The persisted file records a compaction entry too.
    let session = SessionFile::read(&path).expect("read session");
    assert!(
        session
            .entries
            .iter()
            .any(|entry| entry.kind == "compaction"),
        "no compaction entry"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_session_file_matches_the_pi_layout() {
    let dir = std::env::temp_dir().join(format!("pi-rpc-layout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("s.jsonl");
    let lines = concat!(
        "{\"type\":\"session\",\"id\":\"h\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"version\":3}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
    );
    std::fs::write(&path, lines).expect("write session");

    let mut agent = agent_with(vec![AssistantTurn::default()]);
    let input = "{\"type\":\"new_session\"}\n";
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        Some(path),
        &dir.to_string_lossy(),
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");

    // A new `<timestamp>_<id>.jsonl` file appears next to the old one, and the
    // filename stem id matches the header id (what pi-web addresses by).
    let created: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|candidate| candidate.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|candidate| candidate != &dir.join("s.jsonl"))
        .collect();
    assert_eq!(created.len(), 1, "{created:?}");
    let name = created[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let stem = name.trim_end_matches(".jsonl");
    let (_timestamp, id) = stem.rsplit_once('_').expect("<timestamp>_<id>");
    let session = SessionFile::read(&created[0]).expect("read new session");
    assert_eq!(session.header.id, id);
    assert_eq!(session.header.kind, "session");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_session_records_the_parent_session() {
    let dir = std::env::temp_dir().join(format!("pi-rpc-parent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("s.jsonl");
    let lines = concat!(
        "{\"type\":\"session\",\"id\":\"h\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"version\":3}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
    );
    std::fs::write(&path, lines).expect("write session");

    let mut agent = agent_with(vec![AssistantTurn::default()]);
    let input = format!(
        "{{\"type\":\"new_session\",\"parentSession\":{:?}}}\n",
        path.to_string_lossy()
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        Some(path.clone()),
        &dir.to_string_lossy(),
        std::io::Cursor::new(input.into_bytes()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");

    let created: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|candidate| candidate.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|candidate| candidate != &path)
        .collect();
    assert_eq!(created.len(), 1, "{created:?}");
    let session = SessionFile::read(&created[0]).expect("read new session");
    // The header links back to the parent session path (pi's `parentSession`).
    assert_eq!(
        session.header.parent_session.as_deref(),
        Some(path.to_string_lossy().as_ref())
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reset_clears_context_in_place_and_reruns_the_first_message() {
    let dir = std::env::temp_dir().join(format!("pi-rpc-reset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("s.jsonl");
    let lines = concat!(
        "{\"type\":\"session\",\"id\":\"h\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"version\":3}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"first task\"}}\n",
    );
    std::fs::write(&path, lines).expect("write session");

    let mut agent = agent_with(vec![AssistantTurn {
        text: "done".into(),
        stop_reason: Some("end_turn".into()),
        ..Default::default()
    }]);
    // `reset` with no text re-runs the first user message; the response reports
    // the cleared/rerun bookkeeping and the turn streams afterwards.
    let input = "{\"type\":\"reset\"}\n";
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        Some(path.clone()),
        &dir.to_string_lossy(),
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"command\":\"reset\""), "{text}");
    assert!(text.contains("\"reran\":true"), "{text}");
    assert!(text.contains("first task"), "{text}");
    assert!(text.contains("\"type\":\"done\""), "{text}");
    // The transcript was cleared, then re-seeded with only the re-run task and
    // its reply.
    assert_eq!(agent.messages().len(), 2);
    // The session file keeps its id and holds only the fresh transcript.
    let session = SessionFile::read(&path).expect("read session");
    assert_eq!(session.header.id, "h");
    let messages: Vec<_> = session
        .entries
        .iter()
        .filter(|entry| entry.kind == "message")
        .collect();
    assert_eq!(messages.len(), 2, "{messages:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reset_without_rerun_only_clears() {
    let mut agent = agent_with(Vec::new());
    agent.push_user("stale context");
    let events = handle(
        &mut agent,
        Request::Reset {
            text: None,
            rerun: false,
        },
    );
    assert_eq!(agent.messages().len(), 0);
    let Event::Response { command, data, .. } = &events[0] else {
        panic!("expected response, got {events:?}");
    };
    assert_eq!(command, "reset");
    assert_eq!(data["cleared"], serde_json::json!(true));
    assert_eq!(data["reran"], serde_json::json!(false));
}

#[test]
fn reset_with_explicit_text_reruns_that_task() {
    let mut agent = agent_with(vec![AssistantTurn {
        text: "ok".into(),
        stop_reason: Some("end_turn".into()),
        ..Default::default()
    }]);
    agent.push_user("old task");
    let events = handle(
        &mut agent,
        Request::Reset {
            text: Some("new task".into()),
            rerun: true,
        },
    );
    assert!(matches!(events[0], Event::Response { ref command, .. } if command == "reset"));
    // The re-run replaced the context with the explicit task and its reply.
    assert_eq!(agent.messages().len(), 2);
    let Event::Response { data, .. } = &events[0] else {
        unreachable!()
    };
    assert_eq!(data["text"], serde_json::json!("new task"));
}

#[test]
fn entries_and_stats_read_the_live_journal() {
    let dir = std::env::temp_dir().join(format!("pi-rpc-entries-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("s.jsonl");
    let lines = concat!(
        "{\"type\":\"session\",\"id\":\"h\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"version\":3}\n",
        "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
    );
    std::fs::write(&path, lines).expect("write session");

    let mut agent = agent_with(vec![AssistantTurn {
        text: "hi".into(),
        stop_reason: Some("end_turn".into()),
        ..Default::default()
    }]);
    // The prompt appends to the transcript; the journal must reflect it in
    // get_entries/get_session_stats without a file re-read.
    let input = concat!(
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
        "{\"type\":\"get_session_stats\"}\n",
        "{\"type\":\"get_entries\"}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        Some(path.clone()),
        &dir.to_string_lossy(),
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"command\":\"get_session_stats\""), "{text}");
    // The user prompt added a second message entry beyond the seeded one.
    let entries_line = text
        .lines()
        .find(|line| line.contains("\"command\":\"get_entries\""))
        .expect("get_entries response");
    let entries: Value = serde_json::from_str(entries_line).expect("json");
    let list = entries["data"]["entries"].as_array().expect("entries");
    assert!(list.len() >= 2, "{entries}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn serve_session_answers_model_thinking_and_bash() {
    let mut agent = agent_with(vec![AssistantTurn::default()]);
    let input = concat!(
        "{\"type\":\"get_commands\"}\n",
        "{\"type\":\"set_model\",\"provider\":\"anthropic\",\"modelId\":\"m\"}\n",
        "{\"type\":\"get_available_models\"}\n",
        "{\"type\":\"set_thinking_level\",\"level\":\"high\"}\n",
        "{\"type\":\"cycle_thinking_level\"}\n",
        "{\"type\":\"get_available_thinking_levels\"}\n",
        "{\"type\":\"set_auto_compaction\",\"enabled\":false}\n",
        "{\"type\":\"bash\",\"command\":\"echo out-of-band\",\"excludeFromContext\":true}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        "/tmp",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"command\":\"get_commands\""), "{text}");
    assert!(text.contains("\"provider\":\"anthropic\""), "{text}");
    // high -> xhigh
    assert!(text.contains("\"level\":\"xhigh\""), "{text}");
    assert!(text.contains("\"command\":\"bash\""), "{text}");
    assert!(text.contains("out-of-band"), "{text}");
}

#[test]
fn out_of_band_bash_runs_without_approval() {
    let mut agent = agent_with(vec![]);
    let input = "{\"type\":\"bash\",\"command\":\"echo pwned\"}\n";
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        "/tmp",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(!text.contains("\"type\":\"ui_request\""), "{text}");
    assert!(text.contains("\"command\":\"bash\""), "{text}");
    assert!(text.contains("pwned"), "{text}");
}

#[test]
fn steer_and_follow_up_are_cleared_into_separate_lists() {
    let mut agent = agent_with(Vec::new());
    let input = concat!(
        "{\"type\":\"steer\",\"message\":\"s1\"}\n",
        "{\"type\":\"follow_up\",\"message\":\"f1\"}\n",
        "{\"type\":\"steer\",\"message\":\"s2\"}\n",
        "{\"type\":\"follow_up\",\"message\":\"f2\"}\n",
        "{\"type\":\"clear_queue\"}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        ".",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    let line = text
        .lines()
        .find(|line| line.contains("\"command\":\"clear_queue\""))
        .expect("clear_queue response");
    let value: Value = serde_json::from_str(line).expect("json");
    assert_eq!(value["success"], json!(true));
    assert_eq!(value["data"]["steering"], json!(["s1", "s2"]));
    assert_eq!(value["data"]["followUp"], json!(["f1", "f2"]));
}

#[test]
fn steering_is_delivered_before_the_turn_and_follow_up_after() {
    // The provider records the user messages it sees each turn, so we can prove
    // steering precedes the reply and follow-up starts a new run.
    #[derive(Clone)]
    struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_for_provider = std::sync::Arc::clone(&seen);
    let provider = pi_agent::FnProvider::new(move |_index| {
        seen_for_provider.lock().expect("lock").push("turn".into());
        AssistantTurn {
            text: "reply".into(),
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        }
    });
    let mut agent = Agent::new(
        Box::new(provider),
        Vec::new(),
        "system",
        ToolContext::new(std::env::temp_dir()),
    );
    let input = concat!(
        "{\"type\":\"steer\",\"message\":\"steer-1\"}\n",
        "{\"type\":\"follow_up\",\"message\":\"follow-1\"}\n",
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
    );
    let buf = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        ".",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");

    // The prompt + steering run first, then the follow-up runs as a new turn.
    assert_eq!(seen.lock().unwrap().len(), 2, "follow-up did not run");
    let messages: Vec<String> = agent
        .messages()
        .iter()
        .filter_map(|message| match message {
            pi_providers::TranscriptMessage::UserParts(parts) => {
                parts.iter().find_map(|part| match part {
                    pi_providers::ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                })
            }
            _ => None,
        })
        .collect();
    // Ordering in the transcript: prompt, steering, follow-up.
    assert_eq!(messages, vec!["go", "steer-1", "follow-1"]);
}

#[test]
fn one_at_a_time_steering_leaves_the_rest_queued() {
    let mut agent = agent_with(vec![AssistantTurn::default()]);
    let input = concat!(
        "{\"type\":\"set_steering_mode\",\"mode\":\"one-at-a-time\"}\n",
        "{\"type\":\"steer\",\"message\":\"s1\"}\n",
        "{\"type\":\"steer\",\"message\":\"s2\"}\n",
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
        "{\"type\":\"clear_queue\"}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        ".",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    // Only s1 was delivered; s2 is still queued. The transcript holds the
    // prompt and the delivered steering message (the reply is empty).
    assert_eq!(agent.messages().len(), 2, "{}", agent.messages().len());
    let line = text
        .lines()
        .find(|line| line.contains("\"command\":\"clear_queue\""))
        .expect("clear_queue response");
    let value: Value = serde_json::from_str(line).expect("json");
    assert_eq!(value["data"]["steering"], json!(["s2"]));
    assert_eq!(value["data"]["followUp"], json!([]));
}

#[test]
fn unsupported_queue_mode_returns_failure() {
    let mut agent = agent_with(Vec::new());
    let input = concat!(
        "{\"type\":\"set_steering_mode\",\"mode\":\"bogus\"}\n",
        "{\"type\":\"set_follow_up_mode\",\"mode\":\"bogus\"}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        ".",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"command\":\"set_steering_mode\""), "{text}");
    assert!(text.contains("\"success\":false"), "{text}");
    assert!(text.contains("unsupported steering mode"), "{text}");
    assert!(text.contains("unsupported follow-up mode"), "{text}");
}

#[test]
fn auto_compaction_flag_gates_threshold_compaction_over_rpc() {
    // Seed a transcript already past the compaction threshold. With
    // auto_compaction off a prompt must not emit `compacted`; with it on a
    // prompt must.
    fn seeded_agent() -> Agent {
        let mut agent = Agent::new(
            Box::new(FauxProvider::new(vec![AssistantTurn {
                text: "reply".into(),
                stop_reason: Some("end_turn".into()),
                ..Default::default()
            }])),
            Vec::new(),
            "system",
            ToolContext::new(std::env::temp_dir()),
        )
        .with_compaction(100, 10);
        for index in 0..20 {
            agent.push_user(format!(
                "message {index} with several words to spend tokens"
            ));
        }
        agent
    }

    let mut agent = seeded_agent();
    let input = concat!(
        "{\"type\":\"set_auto_compaction\",\"enabled\":false}\n",
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        ".",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(!text.contains("\"type\":\"compacted\""), "{text}");

    let mut agent = seeded_agent();
    let input = concat!(
        "{\"type\":\"set_auto_compaction\",\"enabled\":true}\n",
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        ".",
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"type\":\"compacted\""), "{text}");
}

#[test]
fn export_html_rejects_a_path_outside_the_workspace() {
    let mut agent = agent_with(vec![]);
    let workspace = std::env::temp_dir().join(format!("pi-rpc-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).expect("workspace");
    let outside =
        std::env::temp_dir().join(format!("pi-rpc-export-escape-{}.html", std::process::id()));
    let _ = std::fs::remove_file(&outside);

    let input = format!(
        "{{\"type\":\"export_html\",\"outputPath\":{:?}}}\n",
        outside.to_string_lossy()
    );
    let buf = SessionBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_session(
        &mut agent,
        None,
        &workspace.to_string_lossy(),
        std::io::Cursor::new(input.into_bytes()),
        buf.clone(),
        |_| {},
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"success\":false"), "{text}");
    assert!(!outside.exists(), "export escaped the workspace");

    let _ = std::fs::remove_dir_all(&workspace);
}

use super::*;
use pi_agent::{AssistantTurn, FauxProvider, ToolContext};
use serde_json::json;

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

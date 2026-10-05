use super::*;

fn types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| event.get("type").and_then(Value::as_str))
        .collect()
}

#[test]
fn streamed_deltas_are_not_duplicated_by_the_final_text() {
    let native = vec![
        Event::TurnStart,
        Event::AssistantDelta { text: "Hel".into() },
        Event::AssistantDelta { text: "lo".into() },
        // The authoritative full text arrives after the deltas.
        Event::AssistantText {
            text: "Hello".into(),
        },
        Event::TurnEnd,
    ];
    let events = translate_all(&native);
    let deltas: Vec<&Value> = events
        .iter()
        .filter(|event| event["assistantMessageEvent"]["type"] == json!("text_delta"))
        .collect();
    assert_eq!(deltas.len(), 2, "{events:?}");
    assert_eq!(deltas[0]["assistantMessageEvent"]["delta"], json!("Hel"));
    assert_eq!(deltas[1]["assistantMessageEvent"]["delta"], json!("lo"));
    // Deltas of one text block share its content index.
    assert_eq!(deltas[0]["assistantMessageEvent"]["contentIndex"], json!(0));
    assert_eq!(deltas[1]["assistantMessageEvent"]["contentIndex"], json!(0));
    let message_end = events
        .iter()
        .find(|event| event["type"] == json!("message_end"))
        .expect("message_end");
    assert_eq!(message_end["message"]["content"][0]["text"], json!("Hello"));
}

#[test]
fn thinking_deltas_become_thinking_blocks() {
    let native = vec![
        Event::TurnStart,
        Event::ThinkingDelta { text: "hmm".into() },
        Event::AssistantText {
            text: "done".into(),
        },
        Event::TurnEnd,
    ];
    let events = translate_all(&native);
    let message_end = events
        .iter()
        .find(|event| event["type"] == json!("message_end"))
        .expect("message_end");
    assert_eq!(
        message_end["message"]["content"][0]["type"],
        json!("thinking")
    );
    assert_eq!(
        message_end["message"]["content"][0]["thinking"],
        json!("hmm")
    );
    assert_eq!(message_end["message"]["content"][1]["text"], json!("done"));
}

#[test]
fn confirm_ui_requests_carry_a_message() {
    let native = vec![Event::UiRequest {
        id: "ui-1".into(),
        kind: "confirm".into(),
        prompt: "Run bash?".into(),
        options: vec!["allow".into(), "deny".into()],
    }];
    let events = translate_all(&native);
    let request = events
        .iter()
        .find(|event| event["type"] == json!("extension_ui_request"))
        .expect("ui request");
    assert_eq!(request["method"], json!("confirm"));
    assert_eq!(request["title"], json!("Run bash?"));
    // pi's confirm dialog reads `message`; omitting it crashed the client.
    assert_eq!(request["message"], json!("Run bash?"));
}

#[test]
fn a_text_turn_reconstructs_the_pi_lifecycle() {
    let native = vec![
        Event::AgentStart,
        Event::TurnStart,
        Event::AssistantText {
            text: "hello".into(),
        },
        Event::TurnEnd,
        Event::Done {
            stop_reason: Some("end_turn".into()),
        },
        Event::AgentSettled,
    ];
    let events = translate_all(&native);
    assert_eq!(
        types(&events),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled",
        ]
    );
    let message_end = events
        .iter()
        .find(|event| event["type"] == json!("message_end"))
        .expect("message_end");
    assert_eq!(message_end["message"]["content"][0]["text"], json!("hello"));
    let delta = events
        .iter()
        .find(|event| event["assistantMessageEvent"]["type"] == json!("text_delta"))
        .expect("delta");
    assert_eq!(delta["assistantMessageEvent"]["delta"], json!("hello"));
}

#[test]
fn turn_end_carries_the_turns_tool_results() {
    let native = vec![
        Event::TurnStart,
        Event::ToolStart {
            tool_call_id: "call-1".into(),
            name: "bash".into(),
            input: json!({ "command": "ls" }),
        },
        Event::ToolEnd {
            tool_call_id: "call-1".into(),
            name: "bash".into(),
            is_error: false,
            content: "ok".into(),
        },
        Event::TurnEnd,
    ];
    let events = translate_all(&native);
    let turn_end = events
        .iter()
        .find(|event| event["type"] == json!("turn_end"))
        .expect("turn_end");
    let results = turn_end["toolResults"].as_array().expect("toolResults");
    assert_eq!(results.len(), 1, "{events:?}");
    // Each element must be a pi `ToolResultMessage`: `role` identifies it
    // (pi's `_findPersistedMessageEntryId` looks results up by role) and
    // `timestamp` is part of the shape.
    assert_eq!(results[0]["role"], json!("toolResult"), "{events:?}");
    assert!(
        results[0]["timestamp"].as_u64().is_some(),
        "timestamp missing: {events:?}"
    );
    assert_eq!(results[0]["toolCallId"], json!("call-1"));
    assert_eq!(results[0]["toolName"], json!("bash"));
    assert_eq!(results[0]["isError"], json!(false));
    assert_eq!(results[0]["content"][0]["type"], json!("text"));
    assert_eq!(results[0]["content"][0]["text"], json!("ok"));
    assert!(results[0].get("details").is_some());
}

#[test]
fn compaction_end_matches_pi_shape() {
    let native = vec![Event::Compacted {
        dropped: 3,
        summary: Some("SUMMARY".into()),
        reason: "threshold".into(),
        tokens_before: 120_000,
        estimated_tokens_after: 30_000,
        first_kept_entry_id: Some("entry-7".into()),
    }];
    let events = translate_all(&native);
    let end = events
        .iter()
        .find(|event| event["type"] == json!("compaction_end"))
        .expect("compaction_end");
    assert_eq!(end["reason"], json!("threshold"));
    assert_eq!(end["aborted"], json!(false));
    assert_eq!(end["willRetry"], json!(false));
    assert_eq!(end["result"]["summary"], json!("SUMMARY"));
    assert_eq!(end["result"]["firstKeptEntryId"], json!("entry-7"));
    assert_eq!(end["result"]["tokensBefore"], json!(120_000));
    assert_eq!(end["result"]["estimatedTokensAfter"], json!(30_000));
    assert!(end["result"].get("details").is_some());
}

#[test]
fn tool_calls_get_ids_and_execution_events() {
    let native = vec![
        Event::TurnStart,
        Event::AssistantText {
            text: "working".into(),
        },
        Event::ToolStart {
            tool_call_id: "call-1".into(),
            name: "bash".into(),
            input: json!({ "command": "ls" }),
        },
        Event::ToolEnd {
            tool_call_id: "call-1".into(),
            name: "bash".into(),
            is_error: false,
            content: "ok".into(),
        },
        Event::TurnEnd,
    ];
    let events = translate_all(&native);
    let start = events
        .iter()
        .find(|event| event["type"] == json!("tool_execution_start"))
        .expect("tool start");
    assert_eq!(start["toolCallId"], json!("call-1"));
    assert_eq!(start["toolName"], json!("bash"));
    let message_end = events
        .iter()
        .find(|event| event["type"] == json!("message_end"))
        .expect("message_end");
    assert_eq!(
        message_end["message"]["content"][1]["type"],
        json!("toolCall")
    );
    assert_eq!(message_end["message"]["content"][1]["id"], json!("call-1"));
    // pi's tool result carries content blocks plus an (empty) `details`.
    let end = events
        .iter()
        .find(|event| event["type"] == json!("tool_execution_end"))
        .expect("tool end");
    assert_eq!(end["result"]["content"][0]["type"], json!("text"));
    assert_eq!(end["result"]["content"][0]["text"], json!("ok"));
    assert!(end["result"].get("details").is_some(), "{end}");
}

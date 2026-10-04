use super::*;

const SAMPLE: &str = include_str!("../../../../harness/fixtures/session-sample.jsonl");

#[test]
fn parses_a_real_session_sample() {
    let session = SessionFile::parse(SAMPLE).expect("parses");
    assert_eq!(session.header.kind, "session");
    assert!(!session.header.id.is_empty());
    assert!(!session.header.cwd.is_empty());
    assert!(!session.entries.is_empty());
    // The sample starts with a model change, then messages.
    assert_eq!(session.entries[0].kind, "model_change");
    assert!(session.message_entries().count() > 0);
}

#[test]
fn round_trips_without_loss() {
    let session = SessionFile::parse(SAMPLE).expect("parses");
    let jsonl = session.to_jsonl().expect("serializes");
    let reparsed = SessionFile::parse(&jsonl).expect("reparses");
    assert_eq!(session.header, reparsed.header);
    assert_eq!(session.entries, reparsed.entries);
}

#[test]
fn preserves_unknown_fields() {
    let line = r#"{"type":"custom","id":"x1","parentId":null,"timestamp":"2026-01-01T00:00:00.000Z","customType":"demo","payload":{"deep":[1,2,3]}}"#;
    let entry: SessionEntry = serde_json::from_str(line).expect("parses");
    assert_eq!(entry.kind, "custom");
    assert_eq!(entry.parent_id, None);
    assert_eq!(
        entry.get("customType").and_then(Value::as_str),
        Some("demo")
    );
    let round = serde_json::to_value(&entry).expect("serializes");
    assert_eq!(round["payload"]["deep"], serde_json::json!([1, 2, 3]));
}

/// Opt-in: point `PI_SESSION_SAMPLE` at a real session file to round-trip it.
#[test]
fn round_trips_a_full_session_when_provided() {
    let Ok(path) = std::env::var("PI_SESSION_SAMPLE") else {
        return;
    };
    let session = SessionFile::read(std::path::Path::new(&path)).expect("reads");
    let jsonl = session.to_jsonl().expect("serializes");
    let reparsed = SessionFile::parse(&jsonl).expect("reparses");
    assert_eq!(session.header, reparsed.header);
    assert_eq!(session.entries.len(), reparsed.entries.len());
    assert_eq!(session.entries, reparsed.entries);
}

#[test]
fn message_usage_is_accessible() {
    let line = r#"{"type":"message","id":"m1","parentId":"h","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"assistant","provider":"anthropic","model":"x","timestamp":1,"usage":{"input":10,"cacheRead":90,"cacheWrite":0,"cost":{"input":0,"cacheRead":0,"cacheWrite":0}}}}"#;
    let session = SessionFile::parse(&format!(
        "{}\n{}\n",
        r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#, line
    ))
    .expect("parses");
    let entry = &session.entries[0];
    assert_eq!(
        entry
            .usage()
            .and_then(|u| u.get("cacheRead"))
            .and_then(Value::as_i64),
        Some(90)
    );
}

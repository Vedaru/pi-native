use super::*;

#[test]
fn parses_a_simple_event() {
    let mut parser = SseParser::new();
    let events = parser.push(b"event: message_start\ndata: {\"a\":1}\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "message_start");
    assert_eq!(events[0].data, "{\"a\":1}");
}

#[test]
fn default_event_type_is_message() {
    let mut parser = SseParser::new();
    let events = parser.push(b"data: hello\n\n");
    assert_eq!(events[0].event, "message");
}

#[test]
fn joins_multiline_data_with_newline() {
    let mut parser = SseParser::new();
    let events = parser.push(b"data: line1\ndata: line2\n\n");
    assert_eq!(events[0].data, "line1\nline2");
}

#[test]
fn handles_chunk_boundaries_inside_a_line() {
    let mut parser = SseParser::new();
    assert!(parser.push(b"data: hel").is_empty());
    assert!(parser.push(b"lo\n").is_empty());
    let events = parser.push(b"\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "hello");
}

#[test]
fn handles_crlf_line_endings() {
    let mut parser = SseParser::new();
    let events = parser.push(b"data: hi\r\n\r\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "hi");
}

#[test]
fn comment_lines_are_ignored() {
    let mut parser = SseParser::new();
    let events = parser.push(b": keep-alive\ndata: x\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "x");
}

#[test]
fn strips_one_leading_space_only() {
    let mut parser = SseParser::new();
    let events = parser.push(b"data:  two\n\n");
    assert_eq!(events[0].data, " two");
}

#[test]
fn persists_last_event_id_across_events() {
    let mut parser = SseParser::new();
    parser.push(b"id: 42\ndata: a\n\n");
    let events = parser.push(b"data: b\n\n");
    assert_eq!(events[0].id.as_deref(), Some("42"));
}

#[test]
fn parses_retry_field() {
    let mut parser = SseParser::new();
    let events = parser.push(b"retry: 3000\ndata: x\n\n");
    assert_eq!(events[0].retry, Some(3000));
}

#[test]
fn strips_utf8_bom() {
    let mut parser = SseParser::new();
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"data: hi\n\n");
    let events = parser.push(&bytes);
    assert_eq!(events[0].data, "hi");
}

#[test]
fn finish_flushes_trailing_event() {
    let mut parser = SseParser::new();
    parser.push(b"data: tail");
    let events = parser.finish();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "tail");
}

#[test]
fn multiple_events_in_one_chunk() {
    let mut parser = SseParser::new();
    let events = parser.push(b"data: a\n\ndata: b\n\n");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data, "a");
    assert_eq!(events[1].data, "b");
}

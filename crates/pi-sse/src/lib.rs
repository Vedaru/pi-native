//! Incremental Server-Sent Events (SSE) parser.
//!
//! Provider streaming (Anthropic, OpenAI, Google) delivers SSE frames over
//! chunked HTTP. This parser turns an arbitrary sequence of byte chunks into
//! complete events, following the WHATWG event-stream rules:
//!
//! - lines end in `\r\n`, `\n`, or `\r`
//! - a blank line dispatches the pending event
//! - `:` starts a comment; `field: value` strips one leading space
//! - multiple `data:` lines join with `\n`
//! - the last event id persists across events
//!
//! It is deliberately dependency-free so it can sit in the hot path of every
//! provider without pulling an HTTP stack.

/// A fully parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Event name; defaults to `"message"` when the `event:` field is absent.
    pub event: String,
    /// Joined `data:` payload (without the trailing newline).
    pub data: String,
    /// Last event id seen so far, including ids set on earlier events.
    pub id: Option<String>,
    /// Reconnection delay in milliseconds, if a valid `retry:` was seen.
    pub retry: Option<u64>,
}

impl SseEvent {
    fn new(event: String, data: String, id: Option<String>, retry: Option<u64>) -> Self {
        Self {
            event,
            data,
            id,
            retry,
        }
    }
}

/// Incremental parser state.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Buffered bytes that do not yet form a complete line.
    pending: Vec<u8>,
    /// Fields accumulated for the event currently being read.
    event_type: Option<String>,
    data: Vec<String>,
    id: Option<String>,
    retry: Option<u64>,
    /// Last dispatched event id, retained across events.
    last_event_id: Option<String>,
    /// Set once a UTF-8 BOM has been observed at the very start.
    started: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes, returning every event completed by this chunk.
    ///
    /// Chunk boundaries may fall anywhere, including inside a line or in the
    /// middle of a multi-byte UTF-8 sequence.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut bytes = chunk;
        if !self.started {
            self.started = true;
            if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                bytes = &bytes[3..];
            }
        }

        let mut events = Vec::new();
        for &byte in bytes {
            match byte {
                b'\n' => self.end_of_line(&mut events),
                b'\r' => {
                    // A following \n is handled as the start of the next line;
                    // simplify by treating \r immediately as a line end.
                    self.end_of_line(&mut events);
                }
                other => self.pending.push(other),
            }
        }
        events
    }

    /// Flush any trailing event if the stream ended without a blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.process_line(&line, &mut events);
        }
        self.dispatch(&mut events);
        events
    }

    fn end_of_line(&mut self, events: &mut Vec<SseEvent>) {
        let line = std::mem::take(&mut self.pending);
        // With CRLF the `\n` produces a second empty line; dispatching an
        // empty pending state is a no-op, so no guard is required here.
        self.process_line(&line, events);
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line[0] == b':' {
            return; // comment
        }

        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(index) => {
                let field = &line[..index];
                let mut value = &line[index + 1..];
                if value.first() == Some(&b' ') {
                    value = &value[1..];
                }
                (field, value)
            }
            None => (line, &[][..]),
        };

        let value = String::from_utf8_lossy(value).into_owned();
        match field {
            b"event" => self.event_type = Some(value),
            b"data" => self.data.push(value),
            b"id" => {
                if !value.contains('\0') {
                    self.id = Some(value);
                }
            }
            b"retry" => {
                if let Ok(ms) = value.parse::<u64>() {
                    self.retry = Some(ms);
                }
            }
            _ => {} // unknown fields are ignored
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let has_data = !self.data.is_empty();
        let has_type = self.event_type.is_some();
        let has_id = self.id.is_some();
        if !has_data && !has_type && !has_id {
            // Blank keep-alive with nothing pending.
            self.retry = None;
            return;
        }

        if let Some(id) = self.id.take() {
            self.last_event_id = Some(id);
        }
        let event = SseEvent::new(
            self.event_type
                .take()
                .unwrap_or_else(|| "message".to_string()),
            self.data.join("\n"),
            self.last_event_id.clone(),
            self.retry.take(),
        );
        self.data.clear();
        events.push(event);
    }
}

#[cfg(test)]
mod tests {
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
}

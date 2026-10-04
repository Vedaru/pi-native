//! Shared truncation for tool output.
//!
//! Two independent limits, whichever is hit first: a line limit and a byte
//! limit. Output keeps whole lines (a line that would cross the byte limit is
//! not included), matching pi's `truncate.ts`.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Result of truncating a tool output.
#[derive(Debug, Clone, PartialEq)]
pub struct Truncated {
    pub content: String,
    pub truncated: bool,
    pub total_lines: usize,
}

/// Keep the first lines that fit (head truncation; pi's `read`).
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    truncate(content, max_lines, max_bytes, Direction::Head)
}

/// Keep the last lines that fit (tail truncation; pi's `bash`).
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> Truncated {
    truncate(content, max_lines, max_bytes, Direction::Tail)
}

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Head,
    Tail,
}

fn truncate(content: &str, max_lines: usize, max_bytes: usize, direction: Direction) -> Truncated {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    // Collect the lines that fit within both limits.
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut source: Box<dyn Iterator<Item = &str>> = match direction {
        Direction::Head => Box::new(lines.iter().copied()),
        Direction::Tail => Box::new(lines.iter().rev().copied()),
    };
    for line in source.by_ref() {
        let line_bytes = line.len() + 1;
        if !kept.is_empty() && (kept.len() >= max_lines || bytes + line_bytes > max_bytes) {
            break;
        }
        if kept.is_empty() && line_bytes > max_bytes {
            // A single line larger than the byte limit: keep a prefix of it so
            // the result is never empty.
            let slice = &line[..line.len().min(max_bytes)];
            return Truncated {
                content: slice.to_string(),
                truncated: true,
                total_lines,
            };
        }
        kept.push(line);
        bytes += line_bytes;
    }

    let truncated = kept.len() < total_lines;
    if direction == Direction::Tail {
        kept.reverse();
    }

    let mut out = kept.join("\n");
    if !out.is_empty() && content.ends_with('\n') {
        out.push('\n');
    }
    Truncated {
        content: out,
        truncated,
        total_lines,
    }
}

#[cfg(test)]
#[path = "../tests/unit/truncate.rs"]
mod tests;

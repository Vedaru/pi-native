//! Bounded line reading.
//!
//! Tool output must never be bounded by the size of a file. These helpers read
//! one line at a time into a fixed-capacity buffer, so a huge file (or a single
//! huge line) cannot allocate unbounded memory.

use std::io::BufRead;

/// Read one line (up to and including `\n`) into `buf`, keeping at most `max`
/// bytes in `buf`. The rest of an over-long line is consumed but discarded.
///
/// Returns `Ok(false)` at end of input.
pub fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<bool> {
    buf.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let used = newline.map(|index| index + 1).unwrap_or(available.len());
        let room = max.saturating_sub(buf.len());
        let take = used.min(room);
        buf.extend_from_slice(&available[..take]);
        reader.consume(used);
        if newline.is_some() {
            return Ok(true);
        }
        if buf.len() >= max {
            // Over-long line: consume the remainder without buffering it.
            loop {
                let available = reader.fill_buf()?;
                if available.is_empty() {
                    return Ok(true);
                }
                match available.iter().position(|&byte| byte == b'\n') {
                    Some(index) => {
                        reader.consume(index + 1);
                        return Ok(true);
                    }
                    None => {
                        let length = available.len();
                        reader.consume(length);
                    }
                }
            }
        }
    }
}

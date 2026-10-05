//! Interactive screen composition (VED-307).
//!
//! [`Screen`] is the transport-free model of the interactive TUI: a scrollback
//! transcript, a single-line editor with a cursor, and an optional modal dialog,
//! composed into a [`Buffer`] each frame. The differential renderer
//! ([`crate::render`]) turns successive frames into the minimal ANSI bytes, so a
//! large transcript is not re-emitted when only the editor line changes.
//!
//! Keeping composition here (not in the terminal driver) means the whole
//! interactive layout is unit-testable: build a screen, compose a `Buffer`, and
//! assert its cells, without a TTY.

use crate::buffer::{Buffer, Style};
use crate::dialog::{Dialog, DialogState};
use crate::editor::{cursor_column, wrap_line};

/// One line in the scrollback transcript, with the style to draw it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub text: String,
    pub style: Style,
}

impl TranscriptLine {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// The interactive screen: transcript + editor + optional dialog.
#[derive(Debug, Default)]
pub struct Screen {
    transcript: Vec<TranscriptLine>,
    editor: String,
    cursor: usize,
    dialog: Option<DialogState>,
    /// Scrollback offset in wrapped rows: 0 = pinned to the bottom.
    scroll: usize,
}

/// A composed frame plus where the terminal cursor should sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub buffer: Buffer,
    /// Cursor position in the buffer, if the editor line is visible. The driver
    /// moves the real terminal cursor here after writing the frame.
    pub cursor: Option<(usize, usize)>,
}

impl Screen {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a transcript line.
    pub fn push_line(&mut self, line: TranscriptLine) {
        self.transcript.push(line);
        self.scroll = 0;
    }

    /// Append plain text, splitting on newlines into separate lines.
    pub fn push_text(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            self.push_line(TranscriptLine::styled(line, style));
        }
    }

    pub fn transcript(&self) -> &[TranscriptLine] {
        &self.transcript
    }

    /// The current editor contents.
    pub fn editor(&self) -> &str {
        &self.editor
    }

    /// The editor cursor as a byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace the editor contents and move the cursor to the end.
    pub fn set_editor(&mut self, text: impl Into<String>) {
        self.editor = text.into();
        self.cursor = self.editor.len();
    }

    /// Insert a character at the cursor.
    pub fn insert(&mut self, ch: char) {
        self.editor.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Insert a string at the cursor.
    pub fn insert_str(&mut self, text: &str) {
        self.editor.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// Delete the grapheme before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = crate::editor::move_cursor(&self.editor, self.cursor, -1);
        self.editor.replace_range(previous..self.cursor, "");
        self.cursor = previous;
    }

    /// Move the editor cursor by display columns.
    pub fn move_cursor(&mut self, delta: isize) {
        self.cursor = crate::editor::move_cursor(&self.editor, self.cursor, delta);
    }

    /// Take the editor contents (leaving it empty) for submission.
    pub fn take_editor(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.editor)
    }

    /// Scroll the transcript up by `rows` wrapped rows.
    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows);
    }

    /// Scroll back toward the live bottom.
    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    pub fn set_dialog(&mut self, dialog: Dialog) {
        self.dialog = Some(DialogState::new(dialog));
    }

    pub fn dialog_mut(&mut self) -> Option<&mut DialogState> {
        self.dialog.as_mut()
    }

    pub fn dialog(&self) -> Option<&DialogState> {
        self.dialog.as_ref()
    }

    pub fn clear_dialog(&mut self) {
        self.dialog = None;
    }

    /// Compose the current state into a frame of the given size.
    ///
    /// Layout, top to bottom: transcript (scrolled, clipped) fills the rows
    /// above the editor; the editor occupies the last row (or the dialog, when
    /// one is open, occupies the rows above the editor).
    pub fn compose(&self, width: usize, height: usize) -> Frame {
        let mut buffer = Buffer::new(width, height);
        if width == 0 || height == 0 {
            return Frame {
                buffer,
                cursor: None,
            };
        }

        // Reserve the bottom row for the editor.
        let editor_row = height - 1;
        // A dialog sits directly above the editor, if present.
        let dialog_height = self
            .dialog
            .as_ref()
            .map(|state| state.dialog.height().min(editor_row));
        let dialog_top = dialog_height.map(|dialog_height| editor_row - dialog_height);
        let transcript_bottom = dialog_top.unwrap_or(editor_row);

        // Wrap every transcript line, then render the visible tail (or the
        // scrolled window).
        let mut rows: Vec<&TranscriptLine> = Vec::new();
        for line in &self.transcript {
            let wrapped = wrap_line(&line.text, width);
            for _ in wrapped {
                rows.push(line);
            }
        }
        let total = rows.len();
        let visible = transcript_bottom;
        // `scroll` counts rows up from the live bottom.
        let end = total.saturating_sub(self.scroll.min(total.saturating_sub(visible)));
        let start = end.saturating_sub(visible);
        for (row, line) in rows[start..end].iter().enumerate() {
            // A wrapped line is drawn from its start; approximations are fine
            // for a tail view.
            buffer.put_str(0, row, &line.text, line.style);
        }

        // Dialog, above the editor.
        if let (Some(state), Some(dialog_top)) = (&self.dialog, dialog_top) {
            state.dialog.render(&mut buffer, dialog_top, width);
        }

        // Editor line: a prompt marker then the contents, with the cursor.
        let prompt = "> ";
        let prompt_width = 2usize;
        let available = width.saturating_sub(prompt_width);
        let cursor_col = cursor_column(&self.editor, self.cursor);
        // Horizontal scroll so the caret stays visible in a long line.
        let offset = if cursor_col >= available {
            cursor_col + 1 - available
        } else {
            0
        };
        let visible_editor: String = {
            let mut column = 0usize;
            let mut out = String::new();
            for ch in self.editor.chars() {
                let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if column + ch_width <= offset {
                    column += ch_width;
                    continue;
                }
                out.push(ch);
                column += ch_width;
            }
            out
        };
        buffer.put_str(0, editor_row, prompt, Style::default());
        let used = buffer.put_str(prompt_width, editor_row, &visible_editor, Style::default());
        let _ = used;

        let cursor_x = prompt_width + cursor_col.saturating_sub(offset);
        let cursor = if cursor_x < width {
            Some((cursor_x, editor_row))
        } else {
            Some((width - 1, editor_row))
        };
        Frame { buffer, cursor }
    }
}

/// The escape sequence that moves the terminal cursor to a buffer cell. The
/// driver appends this after a [`Frame`] so the caret sits on the editor.
pub fn cursor_sequence(position: Option<(usize, usize)>) -> String {
    match position {
        Some((x, y)) => format!("\x1b[{};{}H", y + 1, x + 1),
        None => String::new(),
    }
}

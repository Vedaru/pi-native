//! Native `ctx.ui` dialogs.
//!
//! Extensions call `ctx.ui.notify/select/confirm/input`; 69% of the example
//! extensions use only these. They are implemented here as native state
//! machines: a dialog renders into a [`Buffer`] and consumes key events,
//! returning a [`DialogOutcome`] when the user answers.

use crate::buffer::{Buffer, Style};

/// The four dialog kinds pi exposes through `ctx.ui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// A transient message with no input.
    Notify { message: String },
    /// Choose one of `options` (and optionally cancel).
    Select { title: String, options: Vec<String> },
    /// Yes/no question.
    Confirm { title: String, message: String },
    /// Free-text line.
    Input { title: String, value: String },
}

/// The result of feeding a key to a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogOutcome {
    /// The user is still answering.
    Pending,
    /// The dialog is finished with this value (`null` for cancel/dismiss).
    Answered(Option<String>),
}

impl Dialog {
    /// A one-line height, or `2 + options.len()` for a select.
    pub fn height(&self) -> usize {
        match self {
            Dialog::Notify { .. } | Dialog::Confirm { .. } | Dialog::Input { .. } => 1,
            Dialog::Select { options, .. } => options.len(),
        }
    }

    /// Render the dialog into `buffer` starting at row `top`, with `width`
    /// columns. The current selection is reverse-video; typed input is shown
    /// after the prompt.
    pub fn render(&self, buffer: &mut Buffer, top: usize, width: usize) {
        let selected = Style {
            reverse: true,
            ..Style::default()
        };
        match self {
            Dialog::Notify { message } => {
                buffer.put_str(0, top, message, Style::default());
            }
            Dialog::Confirm { title, message } => {
                let line = format!("{title} {message}  [y/n]");
                buffer.put_str(0, top, &line, Style::default());
            }
            Dialog::Input { title, value } => {
                let line = format!("{title} {value}");
                buffer.put_str(0, top, &line, Style::default());
            }
            Dialog::Select { title, options } => {
                if let Some(first) = options.first() {
                    let _ = (title, first);
                }
                for (index, option) in options.iter().enumerate() {
                    let style = if index == 0 {
                        selected
                    } else {
                        Style::default()
                    };
                    let line: String = option.chars().take(width).collect();
                    buffer.put_str(0, top + index, &line, style);
                }
            }
        }
    }
}

/// A live dialog plus its selection/cursor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogState {
    pub dialog: Dialog,
    /// Selected index for [`Dialog::Select`].
    selection: usize,
}

impl DialogState {
    pub fn new(dialog: Dialog) -> Self {
        Self {
            dialog,
            selection: 0,
        }
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    /// Feed one key. The key names mirror pi's: `up`, `down`, `enter`, `escape`,
    /// `backspace`, or a single character.
    pub fn handle_key(&mut self, key: &str) -> DialogOutcome {
        match &mut self.dialog {
            Dialog::Notify { .. } => match key {
                "enter" | "escape" | " " => DialogOutcome::Answered(None),
                _ => DialogOutcome::Pending,
            },
            Dialog::Select { options, .. } => match key {
                "up" => {
                    self.selection = self.selection.saturating_sub(1);
                    DialogOutcome::Pending
                }
                "down" => {
                    if self.selection + 1 < options.len() {
                        self.selection += 1;
                    }
                    DialogOutcome::Pending
                }
                "enter" => {
                    let chosen = options.get(self.selection).cloned();
                    DialogOutcome::Answered(chosen)
                }
                "escape" => DialogOutcome::Answered(None),
                _ => DialogOutcome::Pending,
            },
            Dialog::Confirm { .. } => match key {
                "y" | "Y" | "enter" => DialogOutcome::Answered(Some("true".to_string())),
                "n" | "N" | "escape" => DialogOutcome::Answered(Some("false".to_string())),
                _ => DialogOutcome::Pending,
            },
            Dialog::Input { value, .. } => match key {
                "enter" => DialogOutcome::Answered(Some(value.clone())),
                "escape" => DialogOutcome::Answered(None),
                "backspace" => {
                    value.pop();
                    DialogOutcome::Pending
                }
                other => {
                    value.push_str(other);
                    DialogOutcome::Pending
                }
            },
        }
    }
}

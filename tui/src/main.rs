//! A thin TUI client for a pipelets unit.
//!
//! It spawns `pipelets --serve --session <file>`, writes prompts to its stdin,
//! reads the JSON event stream, and renders it. The agent runtime, tools, and
//! session store all live in the child; this binary only renders. That is the
//! point: the headless workspace never links a TUI, and this client never links
//! the agent core.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event as CEvent, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::Value;

const POLL: Duration = Duration::from_millis(50);

struct App {
    transcript: Vec<String>,
    input: String,
    status: String,
    streaming: String,
    running: bool,
}

impl App {
    fn new() -> Self {
        App {
            transcript: vec!["pipelets-tui — type a prompt, Enter to send, Esc to quit.".into()],
            input: String::new(),
            status: "connecting".into(),
            streaming: String::new(),
            running: true,
        }
    }

    /// Fold one pipelets event line into the view.
    fn observe(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("");
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "ready" => self.status = "ready".into(),
            "assistant_delta" => self.streaming.push_str(text("text")),
            "assistant_text" => {
                // Deltas already streamed it; only use this when nothing did.
                let full = text("text");
                if self.streaming.is_empty() && !full.is_empty() {
                    self.transcript.push(format!("assistant: {full}"));
                }
                self.flush_stream();
            }
            "thinking_delta" => self.status = "thinking…".into(),
            "tool_start" => {
                self.flush_stream();
                self.transcript
                    .push(format!("  ⚒ {} {}", text("name"), compact(&v["input"])));
            }
            "tool_end" => {
                let mark = if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                    "✗"
                } else {
                    "✓"
                };
                self.transcript.push(format!("  {mark} {}", text("name")));
            }
            "done" => {
                self.flush_stream();
                self.running = false;
                self.status = format!("idle ({})", text("stop_reason"));
            }
            "error" => {
                self.flush_stream();
                self.transcript.push(format!("error: {}", text("message")));
                self.status = "error".into();
            }
            _ => {}
        }
    }

    fn flush_stream(&mut self) {
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.transcript.push(format!("assistant: {text}"));
        }
    }
}

fn compact(input: &Value) -> String {
    let raw = match input {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let one: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(80).collect()
}

fn spawn_unit(bin: &str, session: &str) -> (Child, ChildStdin, Receiver<String>) {
    let mut child = Command::new(bin)
        .args(["--serve", "--session", session])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("cannot spawn `{bin}`: {e}"));
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let (tx, rx) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, stdin, rx)
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut bin = std::env::var("PIPELETS_BIN").unwrap_or_else(|_| "pipelets".into());
    let mut session = "/tmp/pipelets-tui.jsonl".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bin" => bin = args.next().unwrap_or(bin),
            "--session" => session = args.next().unwrap_or(session),
            _ => {}
        }
    }

    let (mut child, mut stdin, rx) = spawn_unit(&bin, &session);

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let mut app = App::new();
    'outer: loop {
        // Drain the unit's event stream.
        loop {
            match rx.try_recv() {
                Ok(line) => app.observe(&line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.transcript.push("unit exited".into());
                    break;
                }
            }
        }
        // Drain keyboard input.
        while event::poll(POLL)? {
            if let CEvent::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Esc => break 'outer,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break 'outer
                    }
                    KeyCode::Enter => {
                        let text = app.input.trim().to_string();
                        if !text.is_empty() {
                            app.transcript.push(format!("you: {text}"));
                            app.input.clear();
                            app.running = true;
                            app.status = "running…".into();
                            let req = serde_json::json!({ "type": "prompt", "text": text });
                            let _ = writeln!(stdin, "{req}");
                            let _ = stdin.flush();
                        }
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => app.input.push(c),
                    _ => {}
                }
            }
        }

        terminal.draw(|frame| draw(frame, &app))?;
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let streaming = if app.streaming.is_empty() {
        String::new()
    } else {
        format!("assistant: {}", app.streaming)
    };
    let mut body: Vec<Line> = app
        .transcript
        .iter()
        .map(|l| Line::from(l.clone()))
        .collect();
    if !streaming.is_empty() {
        body.push(Line::from(streaming));
    }
    let height = chunks[0].height.saturating_sub(2) as usize;
    let scroll = body.len().saturating_sub(height) as u16;

    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(Block::default().borders(Borders::ALL).title("pipelets")),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("prompt")),
        chunks[1],
    );

    let style = if app.running {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    frame.render_widget(Paragraph::new(Span::styled(app.status.clone(), style)), chunks[2]);
    frame.set_cursor_position((chunks[1].x + 1 + app.input.len() as u16, chunks[1].y + 1));
}

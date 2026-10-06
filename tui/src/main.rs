//! A thin TUI client for a pipelets unit.
//!
//! Visuals port pi's dark theme (`dist/modes/interactive/theme/dark.json`) and
//! components: full-width user/tool blocks, markdown-tokenised assistant text,
//! italic collapsible thinking blocks, a telemetry footer, and click-to-expand
//! on thinking/tool blocks. The agent runtime lives in the spawned unit.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use serde_json::Value;

const POLL: Duration = Duration::from_millis(16);
/// Code lines kept in memory per block; the rest are counted, not stored.
const MAX_CODE: usize = 12;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// pi's built-in slash commands (`core/slash-commands.ts`).
struct Cmd {
    name: &'static str,
    desc: &'static str,
    args: Option<&'static str>,
}
const COMMANDS: &[Cmd] = &[
    Cmd {
        name: "settings",
        desc: "Open settings menu",
        args: None,
    },
    Cmd {
        name: "model",
        desc: "Select model",
        args: Some("<provider/model>"),
    },
    Cmd {
        name: "tree",
        desc: "Navigate the session tree",
        args: None,
    },
    Cmd {
        name: "thinking",
        desc: "Set thinking level",
        args: Some("<level>"),
    },
    Cmd {
        name: "scoped-models",
        desc: "Enable/disable models for cycling",
        args: None,
    },
    Cmd {
        name: "export",
        desc: "Export the session",
        args: Some("[path]"),
    },
    Cmd {
        name: "import",
        desc: "Import a session from JSONL",
        args: Some("<path>"),
    },
    Cmd {
        name: "share",
        desc: "Share the session",
        args: None,
    },
    Cmd {
        name: "bug",
        desc: "Report a bug",
        args: Some("<description>"),
    },
    Cmd {
        name: "copy",
        desc: "Copy the last agent message",
        args: None,
    },
    Cmd {
        name: "name",
        desc: "Set the session display name",
        args: Some("<name>"),
    },
    Cmd {
        name: "session",
        desc: "Show session info and stats",
        args: None,
    },
    Cmd {
        name: "changelog",
        desc: "Show changelog entries",
        args: None,
    },
    Cmd {
        name: "hotkeys",
        desc: "Show keyboard shortcuts",
        args: None,
    },
    Cmd {
        name: "fork",
        desc: "Fork from a previous message",
        args: None,
    },
    Cmd {
        name: "clone",
        desc: "Duplicate the session",
        args: None,
    },
    Cmd {
        name: "trust",
        desc: "Save a project trust decision",
        args: None,
    },
    Cmd {
        name: "login",
        desc: "Configure provider auth",
        args: Some("<provider>"),
    },
    Cmd {
        name: "logout",
        desc: "Remove provider auth",
        args: Some("<provider>"),
    },
    Cmd {
        name: "new",
        desc: "Start a new session",
        args: None,
    },
    Cmd {
        name: "compact",
        desc: "Compact the session context",
        args: None,
    },
    Cmd {
        name: "resume",
        desc: "Resume a different session",
        args: Some("<path>"),
    },
    Cmd {
        name: "reload",
        desc: "Reload config, skills, themes",
        args: None,
    },
    Cmd {
        name: "quit",
        desc: "Quit",
        args: None,
    },
];

/// Indices of commands whose name starts with the current `/` query.
fn palette_matches(input: &str) -> Vec<usize> {
    let q = input.trim_start_matches('/');
    if q.contains(' ') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name.starts_with(q))
        .map(|(i, _)| i)
        .collect()
}

/// pi's `dark.json`, resolved to sRGB via pi's own OKHSL math.
struct Theme;
impl Theme {
    const ACCENT: Color = Color::Rgb(167, 152, 215);
    const BORDER_ACCENT: Color = Color::Rgb(160, 142, 213);
    const BORDER_MUTED: Color = Color::Rgb(118, 129, 134);
    const ERROR: Color = Color::Rgb(234, 127, 129);
    const WARNING: Color = Color::Rgb(205, 154, 34);
    const MUTED: Color = Color::Rgb(157, 165, 169);
    const DIM: Color = Color::Rgb(126, 136, 142);
    const TEXT: Color = Color::Rgb(222, 224, 225);
    const USER_BG: Color = Color::Rgb(33, 59, 73);
    const TOOL_PENDING_BG: Color = Color::Rgb(52, 56, 58);
    const TOOL_SUCCESS_BG: Color = Color::Rgb(37, 65, 49);
    const TOOL_ERROR_BG: Color = Color::Rgb(91, 40, 42);
    const MD_HEADING: Color = Color::Rgb(205, 154, 34);
    const MD_CODE: Color = Color::Rgb(167, 152, 215);
    const MD_CODE_BLOCK_BORDER: Color = Color::Rgb(157, 165, 169);
    const SYNTAX_KEYWORD: Color = Color::Rgb(105, 173, 208);
    const SYNTAX_FUNCTION: Color = Color::Rgb(205, 154, 34);
    const SYNTAX_STRING: Color = Color::Rgb(222, 141, 90);
    const SYNTAX_NUMBER: Color = Color::Rgb(104, 183, 141);
    const SYNTAX_TYPE: Color = Color::Rgb(167, 152, 215);
    const SYNTAX_COMMENT: Color = Color::Rgb(157, 165, 169);
    const SYNTAX_OPERATOR: Color = Color::Rgb(118, 129, 134);
    const MD_BULLET: Color = Color::Rgb(167, 152, 215);
    const DIFF_ADDED: Color = Color::Rgb(104, 183, 141);
    const DIFF_REMOVED: Color = Color::Rgb(234, 127, 129);
    const THINKING: Color = Color::Rgb(150, 160, 164);
}

#[derive(Default)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    context: u64,
}

impl Usage {
    fn absorb(&mut self, v: &Value) {
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        self.input += n("input");
        self.output += n("output");
        self.cache_read += n("cache_read");
        self.cache_write += n("cache_write");
        let prompt = n("input") + n("cache_read") + n("cache_write");
        if prompt > 0 {
            self.context = prompt;
        }
    }
    fn cache_hit(&self) -> Option<f64> {
        let prompt = self.input + self.cache_read + self.cache_write;
        (self.cache_read + self.cache_write > 0 && self.context > 0)
            .then(|| self.cache_read as f64 / prompt.max(1) as f64 * 100.0)
    }
}

#[derive(PartialEq)]
enum ToolState {
    Running,
    Ok,
    Failed,
}

struct ToolCard {
    name: String,
    summary: String,
    state: ToolState,
    output: String,
    expanded: bool,
}

enum Entry {
    User(String),
    Assistant { text: String, expanded: bool },
    Thinking { text: String, expanded: bool },
    Tool(ToolCard),
    Error(String),
    Notice(String),
}

impl Entry {
    fn hit(&mut self) {
        match self {
            Entry::Thinking { expanded, .. } => *expanded = !*expanded,
            Entry::Tool(card) => card.expanded = !card.expanded,
            Entry::Assistant { expanded, .. } => *expanded = !*expanded,
            _ => {}
        }
    }
}

/// Per-entry line counts and cumulative starts. This is all we keep: rendering
/// a line is O(visible), so no styled transcript is retained.
#[derive(Default)]
struct LineIndex {
    width: usize,
    start: Vec<usize>,
    count: Vec<usize>,
    total: usize,
}

struct App {
    entries: Vec<Entry>,
    input: String,
    streaming: String,
    thinking: String,
    usage: Usage,
    model: String,
    provider: String,
    thinking_level: String,
    context_window: u64,
    status: String,
    running: bool,
    auto_compact: bool,
    tick: usize,
    scroll: std::cell::Cell<u16>,
    last_total: std::cell::Cell<usize>,
    /// Anchor the view top for the next frame (set on expand/collapse).
    anchor: std::cell::Cell<bool>,
    layout: RefCell<LineIndex>,
    /// Set when a cached (non-tail) entry changed and the cache must rebuild.
    dirty: std::cell::Cell<bool>,
    last_len: std::cell::Cell<usize>,
    /// Transcript height from the last frame, for PageUp/PageDown.
    page: std::cell::Cell<usize>,
    /// Highest useful scroll offset, from the last frame.
    max_scroll: std::cell::Cell<u16>,
    sel: usize,
    /// entry index owning each rendered transcript line (for click hit-testing)
    row_owner: Rc<RefCell<Vec<usize>>>,
}

impl App {
    fn new(model: String, provider: String, thinking_level: String, context_window: u64) -> Self {
        App {
            entries: vec![Entry::Notice(
                "type a prompt — enter sends, ctrl+o tools, ctrl+t thinking, esc quits".into(),
            )],
            input: String::new(),
            streaming: String::new(),
            thinking: String::new(),
            usage: Usage::default(),
            model,
            provider,
            thinking_level,
            context_window,
            status: "connecting".into(),
            running: true,
            auto_compact: true,
            tick: 0,
            scroll: std::cell::Cell::new(0),
            last_total: std::cell::Cell::new(0),
            anchor: std::cell::Cell::new(false),
            layout: RefCell::new(LineIndex::default()),
            dirty: std::cell::Cell::new(false),
            last_len: std::cell::Cell::new(0),
            page: std::cell::Cell::new(20),
            max_scroll: std::cell::Cell::new(0),
            sel: 0,
            row_owner: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn observe(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "ready" => {
                self.status = "ready".into();
                self.running = false;
            }
            "assistant_delta" => {
                self.flush_thinking();
                self.streaming.push_str(text("text"));
            }
            "thinking_delta" => self.thinking.push_str(text("text")),
            "assistant_text" => {
                self.flush_thinking();
                let full = text("text");
                if full.is_empty() {
                    self.flush_stream();
                } else {
                    // The consolidated text is authoritative; streamed deltas
                    // can carry artifacts (an extra blank line).
                    self.streaming.clear();
                    self.push_assistant(full.to_string());
                }
            }
            "usage" => self.usage.absorb(&v),
            "tool_start" => {
                self.flush_thinking();
                self.flush_stream();
                self.entries.push(Entry::Tool(ToolCard {
                    name: text("name").to_string(),
                    summary: tool_summary(text("name"), &v["input"]),
                    state: ToolState::Running,
                    output: String::new(),
                    expanded: true,
                }));
            }
            "tool_end" => {
                let failed = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let content = text("content");
                for e in self.entries.iter_mut().rev() {
                    if let Entry::Tool(card) = e {
                        if card.state == ToolState::Running {
                            card.state = if failed {
                                ToolState::Failed
                            } else {
                                ToolState::Ok
                            };
                            card.output = content.to_string();
                            break;
                        }
                    }
                }
            }
            "done" => {
                self.flush_thinking();
                self.flush_stream();
                self.running = false;
                self.status = "ready".into();
            }
            "response" if !matches!(text("command"), "prompt" | "steer" | "follow_up") => {
                let ok = v.get("success").and_then(Value::as_bool).unwrap_or(false);
                let data = v.get("data").cloned().unwrap_or(Value::Null);
                let detail = compact(&data);
                self.entries.push(Entry::Notice(format!(
                    "{} {}{}",
                    if ok { "✓" } else { "✗" },
                    text("command"),
                    if detail.is_empty() || detail == "null" {
                        String::new()
                    } else {
                        format!(" {detail}")
                    }
                )));
            }
            "error" => {
                self.flush_thinking();
                self.flush_stream();
                self.entries.push(Entry::Error(text("message").to_string()));
                self.status = "error".into();
            }
            _ => {}
        }
    }

    fn flush_thinking(&mut self) {
        if !self.thinking.trim().is_empty() {
            let text = std::mem::take(&mut self.thinking);
            self.entries.push(Entry::Thinking {
                text: text.trim().to_string(),
                expanded: true,
            });
        } else {
            self.thinking.clear();
        }
    }

    fn flush_stream(&mut self) {
        if !self.streaming.is_empty() {
            let t = std::mem::take(&mut self.streaming);
            self.push_assistant(t);
        }
    }

    fn push_assistant(&mut self, text: String) {
        if let Some(Entry::Assistant { text: prev, .. }) = self.entries.last_mut() {
            prev.push('\n');
            prev.push_str(&text);
        } else {
            self.entries.push(Entry::Assistant {
                text,
                expanded: false,
            });
        }
    }

    fn context_percent(&self) -> f64 {
        if self.context_window == 0 {
            return 0.0;
        }
        self.usage.context as f64 / self.context_window as f64 * 100.0
    }
}

fn tool_summary(name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str).map(clip);
    let detail = match name {
        "bash" => field("command"),
        "edit" | "write" | "read" => field("path").or_else(|| field("file_path")),
        _ => None,
    };
    detail.unwrap_or_else(|| clip(&compact(input)))
}

fn compact(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

fn clip(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 88 {
        one.chars().take(87).collect::<String>() + "…"
    } else {
        one
    }
}

fn format_tokens(n: u64) -> String {
    let f = n as f64;
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", f / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", (f / 1000.0).round() as u64)
    } else if n < 10_000_000 {
        format!("{:.1}M", f / 1_000_000.0)
    } else {
        format!("{}M", (f / 1_000_000.0).round() as u64)
    }
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
    let mut context_window: u64 = std::env::var("PIPELETS_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(65536);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bin" => bin = args.next().unwrap_or(bin),
            "--session" => session = args.next().unwrap_or(session),
            "--context-window" => {
                context_window = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(context_window)
            }
            _ => {}
        }
    }
    let model = std::env::var("PIPELETS_MODEL").unwrap_or_else(|_| "pi".into());
    let provider = std::env::var("PIPELETS_PROVIDER_LABEL")
        .or_else(|_| std::env::var("PIPELETS_PROVIDER"))
        .unwrap_or_else(|_| "pi".into());
    let thinking_level = std::env::var("PIPELETS_THINKING").unwrap_or_else(|_| "off".to_string());
    let (mut child, mut stdin, rx) = spawn_unit(&bin, &session);

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let mut app = App::new(model, provider, thinking_level, context_window);
    'outer: loop {
        loop {
            match rx.try_recv() {
                Ok(line) => app.observe(&line),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.entries.push(Entry::Error("unit exited".into()));
                    app.running = false;
                    break;
                }
            }
        }
        let mut had = false;
        loop {
            let timeout = if had { Duration::ZERO } else { POLL };
            if !event::poll(timeout)? {
                break;
            }
            had = true;
            match event::read()? {
                CEvent::Key(key) => {
                    let palette = palette_matches(&app.input);
                    let palette_active =
                        app.input.starts_with('/') && !app.input[1..].contains(' ');
                    let sel = if palette.is_empty() {
                        0
                    } else {
                        app.sel.min(palette.len() - 1)
                    };
                    match key.code {
                        KeyCode::Esc if palette_active => {
                            app.input.clear();
                            app.sel = 0;
                        }
                        KeyCode::Esc => break 'outer,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break 'outer
                        }
                        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            toggle_all(&mut app);
                            app.dirty.set(true);
                            app.anchor.set(true);
                        }
                        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            toggle_thinking(&mut app)
                        }
                        KeyCode::Up if palette_active => app.sel = app.sel.saturating_sub(1),
                        KeyCode::Down if palette_active => {
                            app.sel = app.sel.saturating_add(1).min(palette.len() - 1)
                        }
                        KeyCode::Tab if palette_active => {
                            let c = &COMMANDS[palette[sel]];
                            app.input = format!("/{} ", c.name);
                            app.sel = 0;
                        }
                        KeyCode::Enter if palette_active => {
                            let c = &COMMANDS[palette[sel]];
                            let typed = app.input.trim_start_matches('/').trim();
                            if typed == c.name && c.args.is_some() {
                                app.input = format!("/{} ", c.name);
                            } else {
                                let text = app.input.clone();
                                app.input.clear();
                                app.sel = 0;
                                if run_command(&mut app, &mut stdin, &text) {
                                    break 'outer;
                                }
                            }
                        }
                        KeyCode::Enter => {
                            let text = app.input.trim().to_string();
                            if !text.is_empty() {
                                if text.starts_with('/') {
                                    app.input.clear();
                                    if run_command(&mut app, &mut stdin, &text) {
                                        break 'outer;
                                    }
                                } else {
                                    app.entries.push(Entry::User(text.clone()));
                                    app.input.clear();
                                    app.scroll.set(0);
                                    app.running = true;
                                    app.status = "working".into();
                                    let req = serde_json::json!({ "type": "prompt", "text": text });
                                    let _ = writeln!(stdin, "{req}");
                                    let _ = stdin.flush();
                                }
                            }
                        }
                        KeyCode::Up => app
                            .scroll
                            .set(app.scroll.get().saturating_add(1).min(app.max_scroll.get())),
                        KeyCode::Down => app.scroll.set(app.scroll.get().saturating_sub(1)),
                        KeyCode::PageUp => app.scroll.set(
                            app.scroll
                                .get()
                                .saturating_add(app.page.get() as u16)
                                .min(app.max_scroll.get()),
                        ),
                        KeyCode::PageDown => app
                            .scroll
                            .set(app.scroll.get().saturating_sub(app.page.get() as u16)),
                        KeyCode::Home => app.scroll.set(u16::MAX),
                        KeyCode::End => app.scroll.set(0),
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input.clear();
                            app.sel = 0;
                        }
                        KeyCode::Backspace => {
                            app.input.pop();
                            app.sel = 0;
                        }
                        // Ignore control/alt chords so they never type a letter.
                        KeyCode::Char(c)
                            if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && !key.modifiers.contains(KeyModifiers::ALT) =>
                        {
                            app.input.push(c);
                            app.sel = 0;
                        }
                        _ => {}
                    }
                }
                CEvent::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => app
                        .scroll
                        .set(app.scroll.get().saturating_add(3).min(app.max_scroll.get())),
                    MouseEventKind::ScrollDown => {
                        app.scroll.set(app.scroll.get().saturating_sub(3))
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let owner = app.row_owner.borrow().get(m.row as usize).copied();
                        if let Some(index) = owner {
                            if let Some(entry) = app.entries.get_mut(index) {
                                entry.hit();
                                app.dirty.set(true);
                                app.anchor.set(true);
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|f| draw(f, &app))?;
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// Cache every entry but the streaming last, and clone only the visible window.
/// This keeps a frame cheap even when the transcript is long.
fn build_window(app: &App, width: usize, height: usize) -> (Vec<Line<'static>>, Vec<usize>) {
    ensure_layout(app, width);
    let l = app.layout.borrow();
    // Live, not-yet-committed content streams at the tail: thinking, then text.
    let mut live: Vec<Line<'static>> = Vec::new();
    if !app.thinking.trim().is_empty() {
        render_thinking(&mut live, &app.thinking, true, width);
    }
    if !app.streaming.is_empty() {
        render_assistant(&mut live, &app.streaming, width, true);
    }
    let live_len = live.len();
    let total = l.total + live_len;
    // Keep the viewport anchored when the transcript grows/shrinks while
    // scrolled up, so expanding a block does not scroll it out from under you.
    if app.anchor.get() || app.scroll.get() > 0 {
        let delta = total as isize - app.last_total.get() as isize;
        if delta != 0 {
            app.scroll
                .set((app.scroll.get() as isize + delta).max(0) as u16);
        }
    }
    app.anchor.set(false);
    app.last_total.set(total);
    let max_scroll = total.saturating_sub(height);
    app.max_scroll.set(max_scroll as u16);
    let scroll = (app.scroll.get() as usize).min(max_scroll);
    let start = max_scroll - scroll;
    let end = (start + height).min(total);

    let mut visible = Vec::with_capacity(end - start);
    let mut owners = Vec::with_capacity(end - start);
    if !app.entries.is_empty() {
        let mut idx = l.start.partition_point(|s| *s <= start).saturating_sub(1);
        let mut li = l.start[idx];
        while idx < app.entries.len() && li < end {
            let (lines, entry_owners) = render_entry(&app.entries[idx], idx, width);
            for (j, line) in lines.into_iter().enumerate() {
                let n = li + j;
                if n >= start && n < end {
                    visible.push(line);
                    owners.push(entry_owners[j]);
                }
            }
            li += l.count[idx];
            idx += 1;
        }
    }
    for (j, line) in live.into_iter().enumerate() {
        let n = l.total + j;
        if n >= start && n < end {
            visible.push(line);
            owners.push(usize::MAX);
        }
    }
    (visible, owners)
}

fn spinner_line(app: &App) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{} ", SPINNER[app.tick % SPINNER.len()]),
            Style::default().fg(Theme::ACCENT),
        ),
        Span::styled(
            if app.streaming.is_empty() {
                "working\u{2026}"
            } else {
                "responding\u{2026}"
            },
            Style::default().fg(Theme::MUTED),
        ),
    ])
}

/// Refresh per-entry counts and cumulative starts. Only the streaming last entry
/// is re-counted each frame; a toggle or resize rebuilds the table.
fn ensure_layout(app: &App, width: usize) {
    let mut l = app.layout.borrow_mut();
    if l.width != width || l.start.len() != app.entries.len() || app.dirty.get() {
        l.width = width;
        l.start.clear();
        l.count.clear();
        l.total = 0;
        for (i, e) in app.entries.iter().enumerate() {
            let c = render_entry(e, i, width).0.len();
            let at = l.total;
            l.start.push(at);
            l.count.push(c);
            l.total += c;
        }
        app.dirty.set(false);
        app.last_len.set(0);
        return;
    }
    if let Some(e) = app.entries.last() {
        let c = render_entry(e, app.entries.len() - 1, width).0.len();
        let last = l.count.len() - 1;
        if c != l.count[last] {
            l.total = l.total - l.count[last] + c;
            l.count[last] = c;
        }
    }
}

/// Run a `/command`. Returns true to quit.
fn run_command(app: &mut App, stdin: &mut ChildStdin, text: &str) -> bool {
    let body = text.trim_start_matches('/').trim();
    let (name, args) = match body.split_once(|c: char| c.is_whitespace()) {
        Some((n, a)) => (n, a.trim()),
        None => (body, ""),
    };
    let send = |app: &mut App, stdin: &mut ChildStdin, line: serde_json::Value, label: &str| {
        let _ = writeln!(stdin, "{line}");
        let _ = stdin.flush();
        app.entries.push(Entry::Notice(format!("· /{label}")));
    };
    match name {
        "quit" => return true,
        "new" => send(
            app,
            stdin,
            serde_json::json!({ "type": "new_session" }),
            "new",
        ),
        "compact" => send(
            app,
            stdin,
            serde_json::json!({ "type": "compact" }),
            "compact",
        ),
        "session" => send(
            app,
            stdin,
            serde_json::json!({ "type": "get_session_stats" }),
            "session",
        ),
        "thinking" if !args.is_empty() => send(
            app,
            stdin,
            serde_json::json!({ "type": "set_thinking_level", "level": args }),
            "thinking",
        ),
        "model" if !args.is_empty() => send(
            app,
            stdin,
            serde_json::json!({ "type": "set_model", "provider": "", "modelId": args }),
            "model",
        ),
        "name" if !args.is_empty() => send(
            app,
            stdin,
            serde_json::json!({ "type": "set_session_name", "name": args }),
            "name",
        ),
        "resume" if !args.is_empty() => send(
            app,
            stdin,
            serde_json::json!({ "type": "switch_session", "sessionPath": args }),
            "resume",
        ),
        _ => app.entries.push(Entry::Notice(format!(
            "· /{name} is not supported by pipelets-tui yet"
        ))),
    }
    false
}

fn draw_palette(frame: &mut ratatui::Frame, area: Rect, app: &App, matches: &[usize]) {
    let sel = app.sel.min(matches.len().saturating_sub(1));
    const WIN: usize = 8;
    let start = if sel < WIN { 0 } else { sel + 1 - WIN };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::BORDER_MUTED))
        .title(" commands ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    for (i, &ci) in matches.iter().enumerate().skip(start).take(WIN) {
        let c = &COMMANDS[ci];
        let selected = i == sel;
        let name_style = if selected {
            Style::default().fg(Theme::ACCENT)
        } else {
            Style::default().fg(Theme::TEXT)
        };
        let hint = c.args.map(|a| format!(" {a}")).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} /{}{hint}", if selected { "❯" } else { " " }, c.name),
                name_style,
            ),
            Span::styled(format!("   {}", c.desc), Style::default().fg(Theme::MUTED)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let palette: Vec<usize> = if app.input.starts_with('/') && !app.input[1..].contains(' ') {
        palette_matches(&app.input)
    } else {
        Vec::new()
    };
    let pal_rows = if palette.is_empty() {
        0
    } else {
        palette.len().min(8) as u16 + 2
    };
    let status_rows = if app.running { 1 } else { 0 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(pal_rows),
            Constraint::Length(status_rows),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let (visible, vis_owners) = build_window(app, rows[0].width as usize, rows[0].height as usize);
    frame.render_widget(Paragraph::new(visible), rows[0]);

    // Record the absolute screen row owning each entry, for click hit-testing.
    let mut map = app.row_owner.borrow_mut();
    map.clear();
    map.resize(area.height as usize, usize::MAX);
    for (i, owner) in vis_owners.iter().enumerate() {
        let row = rows[0].y as usize + i;
        if row < map.len() {
            map[row] = *owner;
        }
    }

    if pal_rows > 0 {
        draw_palette(frame, rows[1], app, &palette);
    }
    if status_rows > 0 {
        frame.render_widget(Paragraph::new(spinner_line(app)), rows[2]);
    }
    draw_editor(frame, rows[3], app);
    draw_footer(frame, rows[4], app);
}

/// Left: usage + context. Right: provider/model/thinking.
fn draw_footer(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let u = &app.usage;
    let mut parts: Vec<Span> = Vec::new();
    if u.input > 0 {
        parts.push(Span::styled(
            format!("↑{}", format_tokens(u.input)),
            Style::default().fg(Theme::MUTED),
        ));
    }
    if u.output > 0 {
        parts.push(Span::styled(
            format!(" ↓{}", format_tokens(u.output)),
            Style::default().fg(Theme::MUTED),
        ));
    }
    if u.cache_read > 0 {
        parts.push(Span::styled(
            format!(" R{}", format_tokens(u.cache_read)),
            Style::default().fg(Theme::MUTED),
        ));
    }
    if u.cache_write > 0 {
        parts.push(Span::styled(
            format!(" W{}", format_tokens(u.cache_write)),
            Style::default().fg(Theme::MUTED),
        ));
    }
    if let Some(hit) = u.cache_hit() {
        parts.push(Span::styled(
            format!(" CH{hit:.1}%"),
            Style::default().fg(Theme::MUTED),
        ));
    }
    let pct = app.context_percent();
    let ctx_color = if pct > 90.0 {
        Theme::ERROR
    } else if pct > 70.0 {
        Theme::WARNING
    } else {
        Theme::MUTED
    };
    parts.push(Span::styled(
        format!(
            " {:.1}%/{}{}",
            pct,
            format_tokens(app.context_window),
            if app.auto_compact { " (auto)" } else { "" }
        ),
        Style::default().fg(ctx_color),
    ));

    let right = if app.thinking_level == "off" || app.thinking_level.is_empty() {
        format!("({}) {} ", app.provider, app.model)
    } else {
        format!("({}) {} • {} ", app.provider, app.model, app.thinking_level)
    };
    let left_width: usize = parts.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(left_width + right.chars().count());
    parts.push(Span::raw(" ".repeat(pad)));
    parts.push(Span::styled(right, Style::default().fg(Theme::MUTED)));
    frame.render_widget(Paragraph::new(Line::from(parts)), area);
}

fn draw_editor(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let border = if app.running {
        Theme::BORDER_MUTED
    } else {
        Theme::BORDER_ACCENT
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let line = Line::from(vec![
        Span::styled("> ", Style::default().fg(Theme::ACCENT)),
        Span::styled(app.input.clone(), Style::default().fg(Theme::TEXT)),
    ]);
    frame.render_widget(Paragraph::new(line), inner);
    frame.set_cursor_position((inner.x + 2 + app.input.chars().count() as u16, inner.y));
}

fn render_entry(entry: &Entry, index: usize, width: usize) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = Vec::new();
    match entry {
        Entry::User(text) => block_msg(&mut lines, text, width, Theme::USER_BG, Theme::TEXT),
        Entry::Assistant { text, expanded } => render_assistant(&mut lines, text, width, *expanded),
        Entry::Thinking { text, expanded } => render_thinking(&mut lines, text, *expanded, width),
        Entry::Tool(card) => render_tool(&mut lines, card, width),
        Entry::Error(text) => block_msg(&mut lines, text, width, Theme::TOOL_ERROR_BG, Theme::TEXT),
        Entry::Notice(text) => block_msg(
            &mut lines,
            text,
            width,
            Theme::TOOL_PENDING_BG,
            Theme::MUTED,
        ),
    }
    let mut owners = vec![index; lines.len()];
    lines.push(Line::from(""));
    owners.push(usize::MAX);
    (lines, owners)
}

/// Thinking: italic, `thinkingText` colour, click to collapse (pi's MouseRegion).
fn render_thinking(out: &mut Vec<Line<'static>>, text: &str, expanded: bool, width: usize) {
    let base = Style::default()
        .fg(Theme::THINKING)
        .add_modifier(Modifier::ITALIC);
    if !expanded {
        out.push(Line::from(Span::styled(" ✻ Thinking…", base)));
        return;
    }
    let mut first = true;
    for raw in text.split('\n') {
        for chunk in wrap(raw, width.saturating_sub(3)) {
            let prefix = if first { " ✻ " } else { "   " };
            first = false;
            out.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Theme::DIM)),
                Span::styled(chunk, base),
            ]));
        }
    }
}

fn block_msg(out: &mut Vec<Line<'static>>, text: &str, width: usize, bg: Color, fg: Color) {
    let inner = width.saturating_sub(2);
    for raw in text.split('\n') {
        for chunk in wrap(raw, inner) {
            out.push(pad_line(
                vec![Span::styled(format!(" {chunk}"), Style::default().fg(fg))],
                width,
                Some(bg),
            ));
        }
    }
}

fn render_tool(out: &mut Vec<Line<'static>>, card: &ToolCard, width: usize) {
    let (bg, mark) = match card.state {
        ToolState::Running => (Theme::TOOL_PENDING_BG, "◐"),
        ToolState::Ok => (Theme::TOOL_SUCCESS_BG, "✓"),
        ToolState::Failed => (Theme::TOOL_ERROR_BG, "✗"),
    };
    let header = if card.summary.is_empty() {
        format!("{mark} {}", card.name)
    } else {
        format!("{mark} {}  {}", card.name, card.summary)
    };
    out.push(pad_line(
        vec![Span::styled(
            format!(" {header}"),
            Style::default()
                .fg(Theme::TEXT)
                .add_modifier(Modifier::BOLD),
        )],
        width,
        Some(bg),
    ));
    if card.output.is_empty() {
        return;
    }
    if !card.expanded {
        out.push(pad_line(
            vec![Span::styled(
                format!("  ⤸ {} line(s)", card.output.lines().count()),
                Style::default().fg(Theme::MUTED),
            )],
            width,
            Some(bg),
        ));
        return;
    }
    let total = card.output.lines().count();
    for (i, raw) in card.output.lines().enumerate() {
        if i >= 12 {
            out.push(pad_line(
                vec![Span::styled(
                    format!("  ⤸ {} more line(s)", total - i),
                    Style::default().fg(Theme::MUTED),
                )],
                width,
                Some(bg),
            ));
            break;
        }
        let color = if raw.trim_start().starts_with('+') {
            Theme::DIFF_ADDED
        } else if raw.trim_start().starts_with('-') {
            Theme::DIFF_REMOVED
        } else {
            Theme::MUTED
        };
        for chunk in wrap(raw, width.saturating_sub(4)) {
            out.push(pad_line(
                vec![Span::styled(
                    format!("  {chunk}"),
                    Style::default().fg(color),
                )],
                width,
                Some(bg),
            ));
        }
    }
}

fn render_assistant(out: &mut Vec<Line<'static>>, text: &str, width: usize, expanded: bool) {
    let mut in_code = false;
    let mut lang = String::new();
    let mut code: Vec<String> = Vec::new();
    let mut code_total = 0usize;
    let store = if expanded { usize::MAX } else { MAX_CODE };
    let mut last_blank = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                render_code_block(out, &code, code_total, &lang, width, expanded);
                code.clear();
                code_total = 0;
                in_code = false;
            } else {
                in_code = true;
                lang = trimmed.trim_start_matches('`').trim().to_string();
            }
            continue;
        }
        if in_code {
            code_total += 1;
            if code.len() < store {
                code.push(raw.to_string());
            }
            continue;
        }
        let (style, body, marker) = markdown_line(raw);
        if marker.is_none() && body.trim().is_empty() {
            if last_blank {
                continue;
            }
            last_blank = true;
        } else {
            last_blank = false;
        }
        let indent = if marker.is_some() { 4 } else { 1 };
        for (i, chunk) in wrap(&body, width.saturating_sub(indent))
            .into_iter()
            .enumerate()
        {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if i == 0 {
                match &marker {
                    Some(m) => spans.push(Span::styled(
                        format!(" {m} "),
                        Style::default().fg(Theme::MD_BULLET),
                    )),
                    None => spans.push(Span::raw(" ")),
                }
            } else {
                spans.push(Span::raw(" ".repeat(indent)));
            }
            spans.extend(inline_spans(&chunk, style));
            out.push(Line::from(spans));
        }
    }
    if in_code {
        render_code_block(out, &code, code_total, &lang, width, expanded);
    }
}

/// pi renders a ```lang border, the highlighted body, then a ``` border
/// (`components/markdown.ts`).
fn render_code_block(
    out: &mut Vec<Line<'static>>,
    code: &[String],
    total: usize,
    lang: &str,
    width: usize,
    _expanded: bool,
) {
    let _ = lang;
    let bar = Style::default().fg(Theme::MD_CODE_BLOCK_BORDER);
    for line in code {
        for chunk in wrap_code(line, width.saturating_sub(4)) {
            let mut spans = vec![Span::styled("  \u{2502} ", bar)];
            spans.extend(highlight(&chunk, lang));
            out.push(Line::from(spans));
        }
    }
    if code.len() < total {
        out.push(Line::from(Span::styled(
            format!(
                "  \u{2502} ... ({} more lines, ctrl+o to expand)",
                total - code.len()
            ),
            Style::default().fg(Theme::MUTED),
        )));
    }
}

/// A small highlighter over pi's `syntax*` tokens. pi delegates to cli-highlight
/// (highlight.js); this covers the common languages without the dependency, and
/// uses the same token colours pi maps its highlight scopes to.
fn highlight(line: &str, lang: &str) -> Vec<Span<'static>> {
    let kw = keywords(lang);
    let ty = types(lang);
    let hash_comment = matches!(
        lang,
        "python" | "py" | "bash" | "sh" | "shell" | "yaml" | "yml" | "ruby" | "rb" | "toml"
    );
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c == '/' && chars.get(i + 1) == Some(&'/')) || (hash_comment && c == '#') {
            flush_buf(&mut buf, &mut spans);
            spans.push(Span::styled(
                chars[i..].iter().collect::<String>(),
                Style::default().fg(Theme::SYNTAX_COMMENT),
            ));
            return spans;
        }
        if c == '"' || c == '\'' || c == '`' {
            flush_buf(&mut buf, &mut spans);
            let quote = c;
            let mut s = String::from(c);
            i += 1;
            while i < chars.len() {
                let ch = chars[i];
                s.push(ch);
                i += 1;
                if ch == '\\' && i < chars.len() {
                    s.push(chars[i]);
                    i += 1;
                    continue;
                }
                if ch == quote {
                    break;
                }
            }
            spans.push(Span::styled(s, Style::default().fg(Theme::SYNTAX_STRING)));
            continue;
        }
        if c.is_ascii_digit() {
            flush_buf(&mut buf, &mut spans);
            let mut n = String::new();
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                n.push(chars[i]);
                i += 1;
            }
            spans.push(Span::styled(n, Style::default().fg(Theme::SYNTAX_NUMBER)));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            flush_buf(&mut buf, &mut spans);
            let mut w = String::new();
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                w.push(chars[i]);
                i += 1;
            }
            let mut j = i;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            let color = if kw.contains(&w.as_str()) {
                Theme::SYNTAX_KEYWORD
            } else if ty.contains(&w.as_str()) {
                Theme::SYNTAX_TYPE
            } else if chars.get(j) == Some(&'(') {
                Theme::SYNTAX_FUNCTION
            } else {
                Theme::TEXT
            };
            spans.push(Span::styled(w, Style::default().fg(color)));
            continue;
        }
        if "(){}[]<>=+-*/%!&|^~?:;,.".contains(c) {
            flush_buf(&mut buf, &mut spans);
            spans.push(Span::styled(
                c.to_string(),
                Style::default().fg(Theme::SYNTAX_OPERATOR),
            ));
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush_buf(&mut buf, &mut spans);
    spans
}

/// Word-wrap a code line, keeping its leading indentation as a hanging indent
/// so a wrapped line still reads as code.
fn wrap_code(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.chars().count() <= width {
        return vec![text.to_string()];
    }
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let avail = width.saturating_sub(indent.chars().count()).max(8);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text[indent.len()..].split(' ') {
        let wlen = word.chars().count();
        let cur = line.chars().count();
        if cur == 0 {
            line = word.to_string();
        } else if cur + 1 + wlen <= avail {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(format!("{indent}{line}"));
            line = word.to_string();
        }
        while line.chars().count() > avail {
            let split: String = line.chars().take(avail).collect();
            let rest: String = line.chars().skip(avail).collect();
            out.push(format!("{indent}{split}"));
            line = rest;
        }
    }
    out.push(format!("{indent}{line}"));
    out
}

fn flush_buf(buf: &mut String, spans: &mut Vec<Span<'static>>) {
    if !buf.is_empty() {
        spans.push(Span::styled(
            std::mem::take(buf),
            Style::default().fg(Theme::TEXT),
        ));
    }
}

fn keywords(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" | "rs" => &[
            "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait", "for",
            "while", "loop", "if", "else", "match", "return", "self", "Self", "async", "await",
            "move", "ref", "where", "const", "static", "type", "as", "in", "crate", "super", "dyn",
            "unsafe", "extern", "box", "true", "false",
        ],
        "python" | "py" => &[
            "def", "class", "import", "from", "as", "if", "elif", "else", "for", "while", "return",
            "yield", "with", "try", "except", "finally", "raise", "lambda", "pass", "break",
            "continue", "and", "or", "not", "in", "is", "None", "True", "False", "self", "async",
            "await", "global", "nonlocal", "assert", "del",
        ],
        "javascript" | "js" | "typescript" | "ts" | "tsx" | "jsx" => &[
            "function",
            "const",
            "let",
            "var",
            "return",
            "if",
            "else",
            "for",
            "while",
            "class",
            "extends",
            "new",
            "import",
            "export",
            "from",
            "default",
            "async",
            "await",
            "try",
            "catch",
            "finally",
            "throw",
            "typeof",
            "instanceof",
            "this",
            "null",
            "undefined",
            "true",
            "false",
            "interface",
            "type",
            "enum",
            "public",
            "private",
            "readonly",
            "static",
        ],
        "bash" | "sh" | "shell" => &[
            "if", "then", "else", "fi", "for", "do", "done", "while", "case", "esac", "function",
            "return", "echo", "export", "local", "in", "set", "source",
        ],
        "go" => &[
            "func",
            "package",
            "import",
            "var",
            "const",
            "type",
            "struct",
            "interface",
            "map",
            "chan",
            "go",
            "defer",
            "return",
            "if",
            "else",
            "for",
            "range",
            "switch",
            "case",
            "default",
            "break",
            "continue",
            "nil",
            "true",
            "false",
        ],
        "json" => &["true", "false", "null"],
        _ => &[
            "true", "false", "null", "if", "else", "return", "function", "class", "import", "from",
            "def",
        ],
    }
}

fn types(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" | "rs" => &[
            "String", "Vec", "Option", "Result", "Box", "Arc", "Rc", "HashMap", "HashSet",
            "BTreeMap", "Cow", "str", "bool", "char", "u8", "u16", "u32", "u64", "usize", "i8",
            "i16", "i32", "i64", "isize", "f32", "f64", "Self",
        ],
        "python" | "py" => &[
            "str", "int", "float", "bool", "list", "dict", "set", "tuple", "bytes", "Any",
        ],
        "go" => &[
            "string", "int", "int64", "float64", "bool", "error", "byte", "rune", "any",
        ],
        _ => &["string", "number", "boolean", "object", "any", "void"],
    }
}

/// Ctrl+O: if anything is collapsed, expand everything; else collapse everything.
fn toggle_all(app: &mut App) {
    let any_collapsed = app.entries.iter().any(|e| match e {
        Entry::Tool(c) => !c.expanded,
        Entry::Assistant { expanded, .. } => !*expanded,
        Entry::Thinking { expanded, .. } => !*expanded,
        _ => false,
    });
    for e in app.entries.iter_mut() {
        match e {
            Entry::Tool(c) => c.expanded = any_collapsed,
            Entry::Assistant { expanded, .. } => *expanded = any_collapsed,
            Entry::Thinking { expanded, .. } => *expanded = any_collapsed,
            _ => {}
        }
    }
}

fn toggle_thinking(app: &mut App) {
    let any_expanded = app
        .entries
        .iter()
        .any(|e| matches!(e, Entry::Thinking { expanded, .. } if *expanded));
    for e in app.entries.iter_mut() {
        if let Entry::Thinking { expanded, .. } = e {
            *expanded = !any_expanded;
        }
    }
}

fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let code = rest
            .find('`')
            .and_then(|i| rest[i + 1..].find('`').map(|j| (i, j)));
        let bold = rest
            .find("**")
            .and_then(|i| rest[i + 2..].find("**").map(|j| (i, j)));
        let next = match (code, bold) {
            (Some(c), Some(b)) => Some(if c.0 <= b.0 { (true, c) } else { (false, b) }),
            (Some(c), None) => Some((true, c)),
            (None, Some(b)) => Some((false, b)),
            (None, None) => None,
        };
        let Some((is_code, (i, j))) = next else {
            spans.push(Span::styled(rest.to_string(), base));
            break;
        };
        if i > 0 {
            spans.push(Span::styled(rest[..i].to_string(), base));
        }
        if is_code {
            spans.push(Span::styled(
                rest[i + 1..i + 1 + j].to_string(),
                Style::default().fg(Theme::MD_CODE),
            ));
            rest = &rest[i + 2 + j..];
        } else {
            spans.push(Span::styled(
                rest[i + 2..i + 2 + j].to_string(),
                base.add_modifier(Modifier::BOLD),
            ));
            rest = &rest[i + 4 + j..];
        }
    }
    spans
}

fn markdown_line(raw: &str) -> (Style, String, Option<String>) {
    let t = raw.trim_start();
    let body = Style::default().fg(Theme::TEXT);
    if let Some(rest) = t.strip_prefix('#') {
        return (
            Style::default()
                .fg(Theme::MD_HEADING)
                .add_modifier(Modifier::BOLD),
            rest.trim_start_matches('#').trim().to_string(),
            None,
        );
    }
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
        return (body, rest.to_string(), Some("•".into()));
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && t[digits.len()..].starts_with(". ") {
        return (
            body,
            t[digits.len() + 2..].to_string(),
            Some(format!("{digits}.")),
        );
    }
    (body, raw.to_string(), None)
}

fn pad_line(spans: Vec<Span<'static>>, width: usize, bg: Option<Color>) -> Line<'static> {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let mut spans = spans;
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    match bg {
        Some(bg) => Line::from(spans).style(Style::default().bg(bg)),
        None => Line::from(spans),
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let wlen = word.chars().count();
        let cur = line.chars().count();
        if cur == 0 {
            line.push_str(word);
        } else if cur + 1 + wlen <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
        }
        while line.chars().count() > width {
            let split: String = line.chars().take(width).collect();
            let rest: String = line.chars().skip(width).collect();
            out.push(split);
            line = rest;
        }
    }
    out.push(line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: impl Into<String>) -> Entry {
        Entry::Assistant {
            text: text.into(),
            expanded: false,
        }
    }

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_code_block_has_no_extra_blank_lines() {
        let src = "```rust\nstruct C {\n    x: u32,\n}\n\nimpl C {\n}\n```";
        let mut out = Vec::new();
        render_assistant(&mut out, src, 80, false);
        let rendered: Vec<String> = out.iter().map(text_of).collect();
        for l in &rendered {
            eprintln!("[{l}]");
        }
        assert_eq!(rendered.len(), 6, "{rendered:#?}");
    }

    #[test]
    fn an_assistant_entry_renders_code_tightly() {
        let mut app = App::new("m".into(), "p".into(), "off".into(), 65536);
        app.entries
            .push(assistant("```rust\nstruct C {\n}\n\nimpl C {\n}\n```"));
        let last = app.entries.len() - 1;
        let (lines, _) = render_entry(&app.entries[last], last, 80);
        for l in &lines {
            eprintln!("[{}]", text_of(l));
        }
        assert_eq!(lines.len(), 6, "5code+trailing");
    }

    #[test]
    fn long_transcript_render_cost() {
        let mut app = App::new("m".into(), "p".into(), "off".into(), 65536);
        for i in 0..2000 {
            app.entries.push(assistant(format!(
                "line {i}: some assistant text with `code` and **bold** and a - bullet"
            )));
            app.entries.push(Entry::Tool(ToolCard {
                name: "bash".into(),
                summary: format!("echo {i}"),
                state: ToolState::Ok,
                output: format!("out {i}\nsecond line"),
                expanded: true,
            }));
        }
        let start = std::time::Instant::now();
        for _ in 0..30 {
            let mut total = 0;
            for (i, e) in app.entries.iter().enumerate() {
                let (lines, _) = render_entry(e, i, 100);
                total += lines.len();
            }
            assert!(total > 1000);
        }
        let elapsed = start.elapsed();
        eprintln!(
            "30 renders of {} entries took {:?}",
            app.entries.len(),
            elapsed
        );
        assert!(elapsed.as_millis() < 2000, "{elapsed:?}");
    }

    #[test]
    fn long_context_frames_stay_cheap() {
        let mut app = App::new("m".into(), "p".into(), "off".into(), 65536);
        for i in 0..3000 {
            app.entries.push(assistant(format!(
                "line {i}: assistant text with `code` and **bold** and a - bullet"
            )));
            app.entries.push(Entry::Tool(ToolCard {
                name: "bash".into(),
                summary: format!("echo {i}"),
                state: ToolState::Ok,
                output: format!("out {i}\nsecond"),
                expanded: true,
            }));
        }
        let _ = build_window(&app, 100, 30);
        let start = std::time::Instant::now();
        for _ in 0..200 {
            let (visible, _) = build_window(&app, 100, 30);
            assert_eq!(visible.len(), 30);
        }
        let elapsed = start.elapsed();
        eprintln!(
            "200 cached frames over {} entries: {:?}",
            app.entries.len(),
            elapsed
        );
        assert!(elapsed.as_millis() < 200, "{elapsed:?}");
    }

    fn big_app(n: usize) -> App {
        let mut app = App::new("m".into(), "p".into(), "off".into(), 65536);
        for i in 0..n {
            app.entries.push(assistant(format!(
                "line {i}: assistant text with `code`, **bold**, and a - bullet that is fairly long"
            )));
            app.entries.push(Entry::Tool(ToolCard {
                name: "bash".into(),
                summary: format!("echo {i}"),
                state: ToolState::Ok,
                output: format!("output {i}\nsecond line\nthird line"),
                expanded: true,
            }));
        }
        app
    }

    #[test]
    fn scrolling_a_long_context_is_cheap() {
        let mut app = big_app(3000);
        let _ = build_window(&app, 100, 30);
        let start = std::time::Instant::now();
        for k in 0..200u32 {
            app.scroll.set((k * 37) as u16);
            let (v, _) = build_window(&app, 100, 30);
            assert_eq!(v.len(), 30);
        }
        let e = start.elapsed();
        eprintln!("200 scrolled frames: {e:?}");
        assert!(e.as_millis() < 200, "{e:?}");
    }

    #[test]
    fn report_cache_memory() {
        let app = big_app(3000);
        let _ = build_window(&app, 100, 30);
        let r = app.layout.borrow();
        let text: usize = app
            .entries
            .iter()
            .map(|e| match e {
                Entry::Assistant { text, .. } => text.len(),
                Entry::Tool(t) => t.name.len() + t.summary.len() + t.output.len(),
                _ => 0,
            })
            .sum();
        let meta = (r.start.len() + r.count.len()) * 8;
        eprintln!(
            "entries={} raw_text={} KB  layout_meta={} KB  (no styled cache)",
            app.entries.len(),
            text / 1024,
            meta / 1024
        );
    }

    #[test]
    fn huge_code_blocks_scroll_cheap() {
        let mut app = App::new("m".into(), "p".into(), "off".into(), 65536);
        for i in 0..20 {
            let mut block = String::from("```python\n");
            for j in 0..1000 {
                block.push_str(&format!(
                    "value_{j} = compute({j}, {i})  # a line of code here\n"
                ));
            }
            block.push_str("```");
            app.entries.push(assistant(block));
        }
        let _ = build_window(&app, 100, 30);
        let start = std::time::Instant::now();
        for k in 0..200u32 {
            app.scroll.set((k * 7) as u16);
            let _ = build_window(&app, 100, 30);
        }
        let e = start.elapsed();
        eprintln!("200 scrolled frames over 20x1000-line blocks: {e:?}");
        assert!(e.as_millis() < 500, "{e:?}");
    }

    #[test]
    fn clicking_a_code_block_expands_it() {
        let mut app = App::new("m".into(), "p".into(), "off".into(), 65536);
        let mut block = String::from("```rust\n");
        for i in 0..40 {
            block.push_str(&format!("let x{i} = {i};\n"));
        }
        block.push_str("```");
        app.entries.push(Entry::Assistant {
            text: block,
            expanded: false,
        });
        let last = app.entries.len() - 1;
        let compact = render_entry(&app.entries[last], last, 100).0.len();
        app.entries[last].hit();
        let expanded = render_entry(&app.entries[last], last, 100).0.len();
        eprintln!("compact={compact} expanded={expanded}");
        assert!(compact < expanded, "compact={compact} expanded={expanded}");
        assert!(expanded >= 40, "expanded={expanded}");
    }
}

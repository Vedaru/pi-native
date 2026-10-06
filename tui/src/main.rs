//! A thin TUI client for a pipelets unit.
//!
//! Visuals port pi's dark theme (`dist/modes/interactive/theme/dark.json`) and
//! component layout: full-width user/tool blocks, markdown-tokenised assistant
//! text, a spinner loader, and a bordered editor. The agent runtime lives in the
//! spawned unit; this binary only renders.

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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::Value;

const POLL: Duration = Duration::from_millis(40);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// pi's `dark.json`, resolved to sRGB via pi's own OKHSL math.
struct Theme;
#[allow(dead_code)]
impl Theme {
    const ACCENT: Color = Color::Rgb(167, 152, 215);
    const BORDER: Color = Color::Rgb(95, 168, 204);
    const BORDER_ACCENT: Color = Color::Rgb(160, 142, 213);
    const BORDER_MUTED: Color = Color::Rgb(118, 129, 134);
    const SUCCESS: Color = Color::Rgb(104, 183, 141);
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
    const MD_LINK: Color = Color::Rgb(105, 173, 208);
    const MD_CODE: Color = Color::Rgb(167, 152, 215);
    const MD_CODE_BLOCK: Color = Color::Rgb(104, 183, 141);
    const MD_BULLET: Color = Color::Rgb(167, 152, 215);
    const DIFF_ADDED: Color = Color::Rgb(104, 183, 141);
    const DIFF_REMOVED: Color = Color::Rgb(234, 127, 129);
    const THINKING: Color = Color::Rgb(150, 160, 164);
}

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
}

enum Entry {
    User(String),
    Assistant(String),
    Tool(ToolCard),
    Error(String),
    Notice(String),
}

struct App {
    entries: Vec<Entry>,
    input: String,
    streaming: String,
    model: String,
    cwd: String,
    status: String,
    running: bool,
    tick: usize,
}

impl App {
    fn new(model: String) -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        App {
            entries: Vec::new(),
            input: String::new(),
            streaming: String::new(),
            model,
            cwd,
            status: "connecting".into(),
            running: true,
            tick: 0,
        }
    }

    fn observe(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "ready" => self.status = "ready".into(),
            "assistant_delta" => self.streaming.push_str(text("text")),
            "assistant_text" => {
                if self.streaming.is_empty() && !text("text").is_empty() {
                    self.push_assistant(text("text").to_string());
                }
                self.flush_stream();
            }
            "tool_start" => {
                self.flush_stream();
                self.entries.push(Entry::Tool(ToolCard {
                    name: text("name").to_string(),
                    summary: tool_summary(text("name"), &v["input"]),
                    state: ToolState::Running,
                    output: String::new(),
                }));
            }
            "tool_end" => {
                let failed = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let content = text("content");
                for e in self.entries.iter_mut().rev() {
                    if let Entry::Tool(card) = e {
                        if matches!(card.state, ToolState::Running) {
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
                self.flush_stream();
                self.running = false;
                self.status = "ready".into();
            }
            "error" => {
                self.flush_stream();
                self.entries.push(Entry::Error(text("message").to_string()));
                self.status = "error".into();
            }
            _ => {}
        }
    }

    fn flush_stream(&mut self) {
        if !self.streaming.is_empty() {
            let t = std::mem::take(&mut self.streaming);
            self.push_assistant(t);
        }
    }

    fn push_assistant(&mut self, text: String) {
        if let Some(Entry::Assistant(prev)) = self.entries.last_mut() {
            prev.push('\n');
            prev.push_str(&text);
        } else {
            self.entries.push(Entry::Assistant(text));
        }
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
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bin" => bin = args.next().unwrap_or(bin),
            "--session" => session = args.next().unwrap_or(session),
            _ => {}
        }
    }
    let model = std::env::var("PIPELETS_MODEL").unwrap_or_else(|_| "pi".into());
    let (mut child, mut stdin, rx) = spawn_unit(&bin, &session);

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let mut app = App::new(model);
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
                            app.entries.push(Entry::User(text.clone()));
                            app.input.clear();
                            app.running = true;
                            app.status = "working".into();
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
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|f| draw(f, &app))?;
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_status(frame, rows[0], app);

    let mut lines = render_entries(app, rows[1].width as usize);
    if app.running {
        let spinner = SPINNER[app.tick % SPINNER.len()];
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{spinner} "), Style::default().fg(Theme::ACCENT)),
            Span::styled("thinking…", Style::default().fg(Theme::MUTED)),
        ]));
    }
    let height = rows[1].height as usize;
    let lines = if lines.len() > height {
        lines.split_off(lines.len() - height)
    } else {
        lines
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[1]);

    draw_editor(frame, rows[2], app);
    draw_hints(frame, rows[3]);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let right = format!(" {} ", app.model);
    let left = format!(" {} · {} ", app.cwd, app.status);
    let pad = (area.width as usize).saturating_sub(right.chars().count());
    let text = format!("{left:<width$}{right}", width = pad);
    let line = Line::from(Span::styled(
        text.chars().take(area.width as usize).collect::<String>(),
        Style::default().fg(Theme::DIM),
    ));
    frame.render_widget(Paragraph::new(line), area);
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

fn draw_hints(frame: &mut ratatui::Frame, area: Rect) {
    let hints = " enter send  ·  esc quit ";
    frame.render_widget(
        Paragraph::new(Span::styled(hints, Style::default().fg(Theme::DIM))),
        area,
    );
}

fn render_entries(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for entry in &app.entries {
        match entry {
            Entry::User(text) => block_msg(&mut out, text, width, Theme::USER_BG, Theme::TEXT),
            Entry::Assistant(text) => render_assistant(&mut out, text, width),
            Entry::Tool(card) => render_tool(&mut out, card, width),
            Entry::Error(text) => {
                block_msg(&mut out, text, width, Theme::TOOL_ERROR_BG, Theme::TEXT)
            }
            Entry::Notice(text) => {
                block_msg(&mut out, text, width, Theme::TOOL_PENDING_BG, Theme::MUTED)
            }
        }
        out.push(Line::from(""));
    }
    out
}

/// A padded, full-width background block (pi's user/tool message look).
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
    let (bg, mark, title_color) = match card.state {
        ToolState::Running => (Theme::TOOL_PENDING_BG, "◐", Theme::TEXT),
        ToolState::Ok => (Theme::TOOL_SUCCESS_BG, "✓", Theme::TEXT),
        ToolState::Failed => (Theme::TOOL_ERROR_BG, "✗", Theme::TEXT),
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
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        )],
        width,
        Some(bg),
    ));
    if !card.output.is_empty() {
        let mut shown = 0;
        for raw in card.output.split('\n') {
            if shown >= 12 {
                out.push(pad_line(
                    vec![Span::styled(
                        format!("   … (+{} more lines)", card.output.lines().count() - shown),
                        Style::default().fg(Theme::MUTED),
                    )],
                    width,
                    Some(bg),
                ));
                break;
            }
            let (color, prefix) = diff_style(raw);
            for chunk in wrap(&format!("{prefix}{raw}"), width.saturating_sub(4)) {
                out.push(pad_line(
                    vec![Span::styled(
                        format!("  {chunk}"),
                        Style::default().fg(color),
                    )],
                    width,
                    Some(bg),
                ));
            }
            shown += 1;
        }
    }
}

fn diff_style(line: &str) -> (Color, &'static str) {
    let t = line.trim_start();
    if t.starts_with('+') {
        (Theme::DIFF_ADDED, " ")
    } else if t.starts_with('-') {
        (Theme::DIFF_REMOVED, " ")
    } else {
        (Theme::MUTED, " ")
    }
}

/// Assistant markdown with pi's `md*` tokens (headings, code, lists).
fn render_assistant(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    let mut in_code = false;
    for raw in text.split('\n') {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            for chunk in wrap(raw, width.saturating_sub(4)) {
                out.push(Line::from(Span::styled(
                    format!("   {chunk}"),
                    Style::default().fg(Theme::MD_CODE_BLOCK),
                )));
            }
            continue;
        }
        let (style, body, marker) = markdown_line(raw);
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
}

/// Style inline `` `code` `` and `**bold**` within a wrapped chunk.
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
    if t.starts_with('>') {
        return (
            Style::default()
                .fg(Theme::MUTED)
                .add_modifier(Modifier::ITALIC),
            t.trim_start_matches('>').trim().to_string(),
            None,
        );
    }
    (body, raw.to_string(), None)
}

/// Pad a line's spans to `width` and apply a background across the whole row.
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

/// Greedy word wrap; hard-splits over-long words.
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

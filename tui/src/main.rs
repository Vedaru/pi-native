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
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use serde_json::Value;

const POLL: Duration = Duration::from_millis(40);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
    const MD_CODE_BLOCK: Color = Color::Rgb(104, 183, 141);
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
    Assistant(String),
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
            _ => {}
        }
    }
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
    cwd: String,
    status: String,
    running: bool,
    auto_compact: bool,
    tick: usize,
    /// entry index owning each rendered transcript line (for click hit-testing)
    row_owner: Rc<RefCell<Vec<usize>>>,
}

impl App {
    fn new(model: String, provider: String, thinking_level: String, context_window: u64) -> Self {
        let cwd = std::env::current_dir()
            .map(|p| {
                let s = p.display().to_string();
                let home = std::env::var("HOME").unwrap_or_default();
                if !home.is_empty() && s.starts_with(&home) {
                    format!("~{}", &s[home.len()..])
                } else {
                    s
                }
            })
            .unwrap_or_default();
        App {
            entries: Vec::new(),
            input: String::new(),
            streaming: String::new(),
            thinking: String::new(),
            usage: Usage::default(),
            model,
            provider,
            thinking_level,
            context_window,
            cwd,
            status: "connecting".into(),
            running: true,
            auto_compact: true,
            tick: 0,
            row_owner: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn observe(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("");
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "ready" => self.status = "ready".into(),
            "assistant_delta" => {
                self.flush_thinking();
                self.streaming.push_str(text("text"));
            }
            "thinking_delta" => self.thinking.push_str(text("text")),
            "assistant_text" => {
                self.flush_thinking();
                if self.streaming.is_empty() && !text("text").is_empty() {
                    self.push_assistant(text("text").to_string());
                }
                self.flush_stream();
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
        if let Some(Entry::Assistant(prev)) = self.entries.last_mut() {
            prev.push('\n');
            prev.push_str(&text);
        } else {
            self.entries.push(Entry::Assistant(text));
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
        while event::poll(POLL)? {
            match event::read()? {
                CEvent::Key(key) => match key.code {
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
                },
                CEvent::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                    let owner = app.row_owner.borrow().get(m.row as usize).copied();
                    if let Some(index) = owner {
                        if let Some(entry) = app.entries.get_mut(index) {
                            entry.hit();
                        }
                    }
                }
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

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let mut owners: Vec<usize> = Vec::new();
    let mut lines = render_entries(app, rows[0].width as usize, &mut owners);
    if app.running {
        let spinner = SPINNER[app.tick % SPINNER.len()];
        let label = if !app.streaming.is_empty() {
            "responding…"
        } else {
            "working…"
        };
        owners.push(usize::MAX);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{spinner} "), Style::default().fg(Theme::ACCENT)),
            Span::styled(label, Style::default().fg(Theme::MUTED)),
        ]));
    }
    let height = rows[0].height as usize;
    let start = lines.len().saturating_sub(height);
    let lines = lines.split_off(start);
    owners = owners.split_off(start.min(owners.len()));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[0]);

    // Record the absolute screen row owning each entry, for click hit-testing.
    let mut map = app.row_owner.borrow_mut();
    map.clear();
    map.resize(area.height as usize, usize::MAX);
    for (i, owner) in owners.iter().enumerate() {
        let row = rows[0].y as usize + i;
        if row < map.len() {
            map[row] = *owner;
        }
    }

    draw_editor(frame, rows[1], app);
    draw_footer(frame, rows[2], app);
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

fn render_entries(app: &App, width: usize, owners: &mut Vec<usize>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (index, entry) in app.entries.iter().enumerate() {
        let before = out.len();
        match entry {
            Entry::User(text) => block_msg(&mut out, text, width, Theme::USER_BG, Theme::TEXT),
            Entry::Assistant(text) => render_assistant(&mut out, text, width),
            Entry::Thinking { text, expanded } => render_thinking(&mut out, text, *expanded, width),
            Entry::Tool(card) => render_tool(&mut out, card, width),
            Entry::Error(text) => {
                block_msg(&mut out, text, width, Theme::TOOL_ERROR_BG, Theme::TEXT)
            }
            Entry::Notice(text) => {
                block_msg(&mut out, text, width, Theme::TOOL_PENDING_BG, Theme::MUTED)
            }
        }
        for _ in before..out.len() {
            owners.push(index);
        }
        out.push(Line::from(""));
        owners.push(usize::MAX);
    }
    out
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

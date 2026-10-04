//! `pi-native` CLI.
//!
//! Early scaffold. Today it exposes the prompt-cache policy decisions so the
//! provider-parity gate can be exercised from the command line; the agent host
//! lands in later milestones.

use clap::{Parser, Subcommand, ValueEnum};
use pi_agent::{
    anthropic_provider, Agent, AgentEvent, AllowAll, Approval, Approver, AssistantTurn, DenyAll,
    FnProvider, ToolCall, DEFAULT_RESERVE_TOKENS,
};
use pi_cache::{
    clamp_openai_prompt_cache_key, get_cache_control, openai_completions_prompt_cache_key,
    openai_responses_prompt_cache_key, resolve_cache_retention, CacheRetention,
};
use pi_tools::{default_tools, ToolContext};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "pi-native", version, about = "Native Rust runtime for pi")]
struct Cli {
    /// Start in RPC mode and idle on stdin (used by the memory benchmark).
    #[arg(long)]
    rpc: bool,
    /// Serve the RPC protocol over stdio with a real provider.
    #[arg(long)]
    serve: bool,
    /// Connect to a local unit and drive it from the terminal.
    #[arg(long)]
    client: bool,
    /// Non-interactive: run one prompt through the agent and print the result.
    #[arg(short = 'p', long = "print")]
    print: Option<String>,
    /// Model id for `--print` (defaults to an Anthropic Claude model).
    #[arg(long, default_value = "claude-sonnet-4-5")]
    model: String,
    /// Accepted for parity with pi's benchmark invocation; sessions are not
    /// implemented yet, so this is a no-op.
    #[arg(long = "no-session")]
    no_session: bool,
    /// Allow approval-required tools (bash/write/edit) without asking.
    #[arg(long)]
    yolo: bool,
    /// Run a deterministic stress workload of N iterations.
    #[arg(long)]
    stress: Option<usize>,
    /// With --stress: run the agent session loop (compaction) instead of calling
    /// the tool directly.
    #[arg(long)]
    stress_session: bool,
    /// Tool used by --stress: ls (default), grep, find, edit, or read.
    #[arg(long, default_value = "ls")]
    stress_tool: String,
    /// Context byte budget in MB for --stress; 0 disables (compaction is the bound).
    #[arg(long, default_value_t = 0)]
    stress_byte_limit_mb: usize,
    /// Token context window for --stress compaction (reserve is 16,384).
    /// Default matches a real 200k-token model.
    #[arg(long, default_value_t = 200000)]
    stress_context_tokens: usize,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the resolved Anthropic cache_control marker for a retention tier.
    CacheControl {
        #[arg(long, value_enum, default_value_t = RetentionArg::Short)]
        retention: RetentionArg,
        /// Allow `ttl: "1h"` for long retention.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        supports_long: bool,
    },
    /// Print the OpenAI prompt cache key for a session id.
    PromptCacheKey {
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long, value_enum, default_value_t = RetentionArg::Short)]
        retention: RetentionArg,
        /// Whether the base URL is api.openai.com (completions condition).
        #[arg(long, default_value_t = false)]
        openai_api: bool,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        supports_long: bool,
        /// Use the openai-responses wiring instead of openai-completions.
        #[arg(long, default_value_t = false)]
        responses: bool,
    },
    /// Clamp a raw cache key the way pi does (64 Unicode code points).
    ClampKey {
        #[arg(long)]
        key: String,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum RetentionArg {
    Short,
    Long,
    None,
}

impl From<RetentionArg> for CacheRetention {
    fn from(value: RetentionArg) -> Self {
        match value {
            RetentionArg::Short => CacheRetention::Short,
            RetentionArg::Long => CacheRetention::Long,
            RetentionArg::None => CacheRetention::None,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if cli.rpc {
        run_rpc();
        return;
    }
    if cli.serve {
        run_serve(&cli.model, cli.yolo);
        return;
    }
    if cli.client {
        run_client(cli.yolo);
        return;
    }
    if let Some(turns) = cli.stress {
        if cli.stress_session {
            run_stress_session(
                turns,
                &cli.stress_tool,
                cli.stress_byte_limit_mb,
                cli.stress_context_tokens,
            );
        } else {
            run_stress_tool(turns, &cli.stress_tool);
        }
        return;
    }
    if let Some(prompt) = cli.print {
        run_print(&prompt, &cli.model, cli.yolo);
        return;
    }
    match cli.command {
        Some(command) => run_command(command),
        None => {
            eprintln!("pi-native: no command given (try --help, or --rpc to idle)");
            std::process::exit(2);
        }
    }
}

/// Idle RPC loop: keep the process alive reading stdin so the memory benchmark
/// can sample a steady state, and answer `get_state` like pi's RPC mode.
fn run_rpc() {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if request.get("type").and_then(serde_json::Value::as_str) == Some("get_state") {
            let response = serde_json::json!({
                "type": "response",
                "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "command": "get_state",
                "success": true,
                "state": { "native": true },
            });
            let _ = writeln!(out, "{response}");
            let _ = out.flush();
        }
    }
}

/// Run one prompt through the agent and print the result.
fn run_print(prompt: &str, model: &str, yolo: bool) {
    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        eprintln!("pi-native --print: ANTHROPIC_API_KEY is not set");
        std::process::exit(2);
    }
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let provider = anthropic_provider(base_url, api_key, model);
    let cwd = std::env::current_dir().unwrap_or_default();
    let approver: Arc<dyn Approver> = if yolo {
        Arc::new(AllowAll)
    } else {
        Arc::new(TerminalApprover)
    };
    let mut agent = Agent::new(
        Box::new(provider),
        default_tools(),
        "You are pi, a coding agent. Be concise.",
        ToolContext::new(cwd),
    )
    .with_approver(approver);
    agent.push_user(prompt);
    match agent.run() {
        Ok(events) => {
            for event in events {
                match event {
                    AgentEvent::AssistantText(text) => println!("{text}"),
                    AgentEvent::ToolStart { name, .. } => eprintln!("[tool {name} start]"),
                    AgentEvent::ToolEnd { name, is_error, .. } => {
                        eprintln!("[tool {name} {}]", if is_error { "error" } else { "ok" })
                    }
                    AgentEvent::Done { .. } => {}
                    AgentEvent::Compacted { dropped, .. } => {
                        eprintln!("[compacted {dropped} messages]")
                    }
                }
            }
        }
        Err(error) => {
            eprintln!("pi-native: {error}");
            std::process::exit(1);
        }
    }
}

/// Asks the terminal before running an approval-required tool.
struct TerminalApprover;

impl Approver for TerminalApprover {
    fn approve(&self, tool: &str, input: &serde_json::Value) -> Approval {
        print!("[approve] {tool} {input}? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0 {
            return Approval::Deny;
        }
        if answer.trim().eq_ignore_ascii_case("y") {
            Approval::Allow
        } else {
            Approval::Deny
        }
    }
}

/// A small workspace per tool so each has a deterministic input.
fn stress_workspace(tool: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-stress-{}-{tool}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("f.txt"), "x\n");
    if tool == "grep" {
        let _ = std::fs::write(dir.join("g.txt"), "needle\nother\nneedle\n");
    }
    if tool == "edit" {
        let _ = std::fs::write(dir.join("e.txt"), "A\n");
    }
    if tool == "read" {
        let _ = std::fs::write(dir.join("big.txt"), "x".repeat(40 * 1024));
    }
    dir
}

fn stress_args(tool: &str, index: usize) -> serde_json::Value {
    match tool {
        "grep" => serde_json::json!({ "pattern": "needle" }),
        "find" => serde_json::json!({ "pattern": "*.txt" }),
        "read" => serde_json::json!({ "path": "big.txt" }),
        "edit" => {
            // Alternate A<->B so every edit targets unique text.
            let (old, new) = if index % 2 == 0 {
                ("A", "B")
            } else {
                ("B", "A")
            };
            serde_json::json!({ "path": "e.txt", "oldText": old, "newText": new })
        }
        _ => serde_json::json!({}),
    }
}

/// Measure the tool itself: call it `turns` times directly, with no agent loop
/// and no transcript, so the result is the tool's own memory and CPU.
fn run_stress_tool(turns: usize, tool: &str) {
    let dir = stress_workspace(tool);
    let ctx = ToolContext::new(&dir);
    let tools = default_tools();
    let Some(selected) = tools.iter().find(|candidate| candidate.name() == tool) else {
        eprintln!("pi-native --stress: unknown tool `{tool}`");
        std::process::exit(2);
    };

    let started = std::time::Instant::now();
    let mut bytes = 0usize;
    let mut errors = 0usize;
    for index in 0..turns {
        let result = selected.run(&stress_args(tool, index), &ctx);
        bytes += result.content.len();
        if result.is_error {
            errors += 1;
        }
    }
    let elapsed = started.elapsed();

    println!(
        "tool-stress[{tool}]: {turns} calls, {bytes} bytes out, {errors} errors, {:.2}s",
        elapsed.as_secs_f64()
    );
    if let Some(peak) = peak_rss_mb() {
        println!("peak RSS: {peak:.1} MB");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Measure the session loop: N agent turns over one growing transcript, with
/// token-based compaction enabled (this is where compaction is exercised).
fn run_stress_session(turns: usize, tool: &str, byte_limit_mb: usize, context_tokens: usize) {
    let dir = stress_workspace(tool);

    let tool_name = tool.to_string();
    let provider = FnProvider::new(move |index| {
        if index < turns {
            AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("stress-{index}"),
                    name: tool_name.clone(),
                    arguments: stress_args(&tool_name, index),
                }],
                stop_reason: Some("tool_use".to_string()),
                ..Default::default()
            }
        } else {
            AssistantTurn {
                text: "done".to_string(),
                stop_reason: Some("end_turn".to_string()),
                ..Default::default()
            }
        }
    });

    let agent = Agent::new(
        Box::new(provider),
        default_tools(),
        SYSTEM_PROMPT,
        ToolContext::new(&dir),
    )
    .with_max_iterations(turns + 2)
    .with_compaction(context_tokens, DEFAULT_RESERVE_TOKENS);
    let mut agent = if byte_limit_mb > 0 {
        agent.with_context_byte_limit(byte_limit_mb * 1024 * 1024)
    } else {
        agent
    };
    agent.push_user("stress");

    let mut tool_results = 0usize;
    let mut tool_errors = 0usize;
    let mut compactions = 0usize;
    let result = agent.run_with(|event| match event {
        AgentEvent::ToolEnd { is_error, .. } => {
            tool_results += 1;
            if *is_error {
                tool_errors += 1;
            }
        }
        AgentEvent::Compacted { .. } => compactions += 1,
        _ => {}
    });
    match result {
        Ok(()) => {
            println!(
                "session-stress[{tool}]: {turns} turns, {} messages, {tool_results} tool results, {tool_errors} errors, {compactions} compactions",
                agent.messages().len()
            );
            if let Some(peak) = peak_rss_mb() {
                println!("peak RSS: {peak:.1} MB");
            }
        }
        Err(error) => {
            eprintln!("pi-native --stress: {error}");
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Peak resident set size of this process in MB, from `/proc` when available.
fn peak_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}

const SYSTEM_PROMPT: &str = "You are pi, a coding agent. Be concise.";

/// Serve the RPC protocol over stdio with a real Anthropic provider.
fn run_serve(model: &str, yolo: bool) {
    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        eprintln!("pi-native --serve: ANTHROPIC_API_KEY is not set");
        std::process::exit(2);
    }
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let provider = anthropic_provider(base_url, api_key, model);
    let cwd = std::env::current_dir().unwrap_or_default();
    let approver: Arc<dyn Approver> = if yolo {
        Arc::new(AllowAll)
    } else {
        Arc::new(DenyAll)
    };
    let mut agent = Agent::new(
        Box::new(provider),
        default_tools(),
        SYSTEM_PROMPT,
        ToolContext::new(cwd),
    )
    .with_approver(approver);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let _ = pi_rpc::serve(&mut agent, stdin.lock(), stdout.lock());
}

/// A minimal terminal client: spawn a unit serving the protocol and drive it.
fn run_client(yolo: bool) {
    let exe = std::env::current_exe().expect("current executable");
    let mut command = ProcessCommand::new(exe);
    command.arg("--serve");
    if yolo {
        command.arg("--yolo");
    }
    let mut child = match command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("pi-native --client: {error}");
            std::process::exit(1);
        }
    };
    let mut server_in = child.stdin.take().expect("server stdin");
    let mut server_out = BufReader::new(child.stdout.take().expect("server stdout"));
    let stdin = std::io::stdin();

    eprintln!("pi-native client. Type a prompt; Ctrl-D to quit.");
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if send(
            &mut server_in,
            &serde_json::json!({ "type": "prompt", "text": text }),
        )
        .is_err()
        {
            break;
        }
        if !pump(&mut server_out, &mut server_in, &stdin) {
            break;
        }
    }
    let _ = child.kill();
}

fn send(writer: &mut impl Write, value: &serde_json::Value) -> std::io::Result<()> {
    writeln!(writer, "{value}")?;
    writer.flush()
}

/// Read events until `done`, answering any generic UI request.
fn pump(reader: &mut impl BufRead, writer: &mut impl Write, stdin: &std::io::Stdin) -> bool {
    let mut buffer = String::new();
    loop {
        buffer.clear();
        match reader.read_line(&mut buffer) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        let trimmed = buffer.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        match event["type"].as_str() {
            Some("assistant_text") => println!("{}", event["text"].as_str().unwrap_or("")),
            Some("tool_start") => {
                eprintln!("[tool {} start]", event["name"].as_str().unwrap_or(""))
            }
            Some("tool_end") => eprintln!(
                "[tool {} {}]",
                event["name"].as_str().unwrap_or(""),
                if event["is_error"].as_bool().unwrap_or(false) {
                    "error"
                } else {
                    "ok"
                }
            ),
            Some("done") => return true,
            Some("error") => eprintln!("error: {}", event["message"].as_str().unwrap_or("")),
            Some("ui_request") => {
                let kind = event["kind"].as_str().unwrap_or("input");
                let prompt = event["prompt"].as_str().unwrap_or("");
                print!("[{kind}] {prompt} ");
                let _ = std::io::stdout().flush();
                let mut answer = String::new();
                let _ = stdin.read_line(&mut answer);
                let response = serde_json::json!({
                    "type": "ui_response",
                    "id": event["id"],
                    "value": answer.trim(),
                });
                let _ = send(writer, &response);
            }
            _ => {}
        }
    }
}

fn run_command(command: Command) {
    match command {
        Command::CacheControl {
            retention,
            supports_long,
        } => {
            let resolved = resolve_cache_retention(Some(retention.into()), None);
            let result = get_cache_control(resolved, supports_long);
            let marker = result
                .cache_control
                .map(|m| serde_json::to_value(m).expect("cache control serializes"));
            println!(
                "{}",
                serde_json::json!({
                    "retention": result.retention.as_str(),
                    "cache_control": marker,
                })
            );
        }
        Command::PromptCacheKey {
            session_id,
            retention,
            openai_api,
            supports_long,
            responses,
        } => {
            let retention: CacheRetention = retention.into();
            let key = if responses {
                openai_responses_prompt_cache_key(retention, session_id.as_deref())
            } else {
                openai_completions_prompt_cache_key(
                    retention,
                    session_id.as_deref(),
                    openai_api,
                    supports_long,
                )
            };
            println!(
                "{}",
                serde_json::json!({
                    "retention": retention.as_str(),
                    "prompt_cache_key": key,
                })
            );
        }
        Command::ClampKey { key } => {
            println!(
                "{}",
                serde_json::json!({ "prompt_cache_key": clamp_openai_prompt_cache_key(Some(&key)) })
            );
        }
    }
}

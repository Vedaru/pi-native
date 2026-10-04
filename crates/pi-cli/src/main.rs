//! `pi-native` CLI.
//!
//! Early scaffold. Today it exposes the prompt-cache policy decisions so the
//! provider-parity gate can be exercised from the command line; the agent host
//! lands in later milestones.

use clap::{Parser, Subcommand, ValueEnum};
use pi_agent::{
    anthropic_provider, Agent, AgentEvent, AllowAll, Approval, Approver, AssistantTurn, DenyAll,
    FnProvider, ToolCall,
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
    /// Run a deterministic in-process stress workload of N tool-call turns.
    #[arg(long)]
    stress: Option<usize>,
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
        run_stress(turns);
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

/// Deterministic in-process load: N turns of a `read` tool call with a fixed,
/// tiny output, then a final message. No network, no subprocesses, and a
/// constant per-tool-result size, so the measured memory is the harness
/// overhead rather than the size of a directory listing.
fn run_stress(turns: usize) {
    // A directory with a single small file so `ls` (no arguments) returns a
    // tiny, constant output. This measures harness overhead, not payload size.
    let dir = std::env::temp_dir().join(format!("pi-stress-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("f.txt"), "x\n");

    // Generate turns on demand so a long run does not pre-allocate every turn.
    let provider = FnProvider::new(move |index| {
        if index < turns {
            AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("stress-{index}"),
                    name: "ls".to_string(),
                    arguments: serde_json::json!({}),
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
    let mut agent = Agent::new(
        Box::new(provider),
        default_tools(),
        SYSTEM_PROMPT,
        ToolContext::new(&dir),
    )
    .with_max_iterations(turns + 2)
    .with_context_window(4096);
    agent.push_user("stress");

    // Stream events instead of collecting them, so a long run does not retain
    // every event (which would duplicate tool output for the whole turn).
    let mut tool_results = 0usize;
    let result = agent.run_with(|event| {
        if matches!(event, AgentEvent::ToolEnd { .. }) {
            tool_results += 1;
        }
    });
    match result {
        Ok(()) => println!(
            "stress: {turns} turns, {} messages, {tool_results} tool results",
            agent.messages().len()
        ),
        Err(error) => {
            eprintln!("pi-native --stress: {error}");
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
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

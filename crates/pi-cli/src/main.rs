//! `pi-native` CLI.
//!
//! Early scaffold. Today it exposes the prompt-cache policy decisions so the
//! provider-parity gate can be exercised from the command line; the agent host
//! lands in later milestones.

use clap::{Parser, Subcommand, ValueEnum};
use pi_agent::{Agent, AgentEvent, AnthropicProvider};
use pi_cache::{
    clamp_openai_prompt_cache_key, get_cache_control, openai_completions_prompt_cache_key,
    openai_responses_prompt_cache_key, resolve_cache_retention, CacheRetention,
};
use pi_tools::{default_tools, ToolContext};

#[derive(Parser)]
#[command(name = "pi-native", version, about = "Native Rust runtime for pi")]
struct Cli {
    /// Start in RPC mode and idle on stdin (used by the memory benchmark).
    #[arg(long)]
    rpc: bool,
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
    if let Some(prompt) = cli.print {
        run_print(&prompt, &cli.model);
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
fn run_print(prompt: &str, model: &str) {
    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        eprintln!("pi-native --print: ANTHROPIC_API_KEY is not set");
        std::process::exit(2);
    }
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let provider = AnthropicProvider::new(base_url, api_key, model);
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut agent = Agent::new(
        Box::new(provider),
        default_tools(),
        "You are pi, a coding agent. Be concise.",
        ToolContext::new(cwd),
    );
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

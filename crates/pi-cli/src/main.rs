//! `pi-native` CLI.
//!
//! Early scaffold. Today it exposes the prompt-cache policy decisions so the
//! provider-parity gate can be exercised from the command line; the agent host
//! lands in later milestones.

use clap::{Parser, Subcommand, ValueEnum};
use pi_agent::prompt::{build_system_prompt, load_project_context_files, SystemPromptOptions};
use pi_agent::{
    openai_completions_provider, openai_responses_provider, Agent, AgentEvent, AssistantTurn,
    FnProvider, ModelProvider, ProviderSummarizer, SessionJournal, ThinkingFormat, ToolCall,
    DEFAULT_RESERVE_TOKENS,
};
use pi_cache::{
    clamp_openai_prompt_cache_key, get_cache_control, openai_completions_prompt_cache_key,
    openai_responses_prompt_cache_key, resolve_cache_retention, CacheRetention,
};
use pi_plugins::{PluginInstance, PluginPolicy};
use pi_tools::{all_tools, default_tools, Tool, ToolContext, ToolResult};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::rc::Rc;
use std::sync::Arc;

const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("PI_NATIVE_GIT_SHA"),
    ", ",
    env!("PI_NATIVE_TARGET"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "pi-native",
    version,
    long_version = LONG_VERSION,
    about = "Native Rust runtime for pi"
)]
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
    /// Serve the HTTP + SSE gateway for unit agents (web UI / remote clients).
    #[arg(long)]
    gateway: bool,
    /// Bind address for `--gateway`.
    #[arg(long, default_value = "127.0.0.1:30142")]
    gateway_addr: String,
    /// Load trigger definitions from a JSON array file (requires `--gateway`).
    #[arg(long)]
    triggers: Option<PathBuf>,
    /// Durable trigger run records for `--triggers`.
    #[arg(long)]
    trigger_runs: Option<PathBuf>,
    /// How often the trigger loop wakes, in seconds.
    #[arg(long, default_value_t = 1)]
    trigger_interval: u64,
    /// Suspend a gateway unit after this many idle seconds; 0 disables (default 900).
    #[arg(long, default_value_t = 900)]
    idle_timeout: u64,
    /// Refuse new units beyond this many (default 24). A runaway orchestrator is
    /// capped here instead of exhausting the machine.
    #[arg(long, default_value_t = 24)]
    max_units: usize,
    /// Non-interactive: run one prompt through the agent and print the result.
    #[arg(short = 'p', long = "print")]
    print: Option<String>,
    /// Model id (required for agent modes).
    #[arg(long)]
    model: Option<String>,
    /// Provider protocol: openai-completions or openai-responses.
    #[arg(long)]
    provider: Option<String>,
    /// Provider base URL (falls back to OPENAI_BASE_URL).
    #[arg(long)]
    base_url: Option<String>,
    /// Provider API key (falls back to OPENAI_API_KEY).
    #[arg(long)]
    api_key: Option<String>,
    /// Request output-token cap; omitted from the request when unset.
    #[arg(long)]
    max_tokens: Option<i64>,
    /// Reasoning format: `none` (default) or `deepseek`.
    #[arg(long)]
    thinking_format: Option<String>,
    /// Reasoning effort for providers that accept one (e.g. `high`).
    #[arg(long)]
    reasoning_effort: Option<String>,
    /// Token context window for compaction; 0 disables (reserve is 16,384).
    #[arg(long, default_value_t = 200000)]
    context_window: usize,
    /// Load an existing session file to seed the transcript.
    #[arg(long)]
    session: Option<PathBuf>,
    /// Load a pi extension/plugin file (repeatable).
    #[arg(long = "extension", short = 'e')]
    extensions: Vec<PathBuf>,
    /// Extra capabilities to grant `--extension` plugins: read, write, exec,
    /// http (repeatable or comma-separated). Without this, extensions may
    /// register tools but cannot touch the filesystem, spawn processes, or use
    /// the network; paths are jailed to the working directory.
    #[arg(long = "extension-allow", value_delimiter = ',')]
    extension_allow: Vec<String>,
    /// Do not persist the session. Without this, a run with no `--session`
    /// creates a file under the pi agent directory (`~/.pi/agent/sessions`).
    #[arg(long = "no-session")]
    no_session: bool,
    /// Allow tools to reach paths outside the working directory (opt-in).
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
        run_serve(
            &cli.provider_config(),
            cli.yolo,
            cli.context_window,
            cli.session.as_deref(),
            cli.no_session,
            &cli.extensions,
            &cli.extension_allow,
        );
        return;
    }
    if cli.client {
        run_client(
            &cli.provider_config(),
            cli.yolo,
            cli.context_window,
            cli.session.as_deref(),
            cli.no_session,
            &cli.extensions,
            &cli.extension_allow,
        );
        return;
    }
    if cli.gateway {
        run_gateway(
            cli.provider_config(),
            cli.context_window,
            &cli.extensions,
            &cli.extension_allow,
            &cli.gateway_addr,
            cli.triggers.as_deref(),
            cli.trigger_runs.as_deref(),
            cli.trigger_interval,
            cli.idle_timeout,
            cli.max_units,
        );
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
    if let Some(prompt) = cli.print.clone() {
        run_print(
            &prompt,
            &cli.provider_config(),
            cli.yolo,
            cli.context_window,
            cli.session.as_deref(),
            cli.no_session,
            &cli.extensions,
            &cli.extension_allow,
        );
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
#[allow(clippy::too_many_arguments)]
fn run_print(
    prompt: &str,
    config: &ProviderConfig,
    yolo: bool,
    context_window: usize,
    session: Option<&std::path::Path>,
    no_session: bool,
    extensions: &[PathBuf],
    extension_allow: &[String],
) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let system = system_prompt_for(&cwd);
    let mut agent = resolve_agent(
        config,
        yolo,
        context_window,
        &system,
        extensions,
        extension_allow,
        &cwd,
    );
    let session = resolve_session(session, no_session, &cwd);
    let mut journal = open_session(&mut agent, session.as_deref(), &cwd);
    agent.push_user(prompt);
    match agent.run() {
        Ok(events) => {
            for event in events {
                match event {
                    AgentEvent::AgentStart
                    | AgentEvent::TurnStart
                    | AgentEvent::TurnEnd
                    | AgentEvent::AssistantDelta(_)
                    | AgentEvent::ThinkingDelta(_)
                    | AgentEvent::AgentSettled => {}
                    AgentEvent::AssistantText(text) => println!("{text}"),
                    AgentEvent::ToolStart { name, .. } => eprintln!("[tool {name} start]"),
                    AgentEvent::ToolEnd { name, is_error, .. } => {
                        eprintln!("[tool {name} {}]", if is_error { "error" } else { "ok" })
                    }
                    AgentEvent::Done { .. } => {}
                    AgentEvent::Compacted { dropped, .. } => {
                        eprintln!("[compacted {dropped} messages]")
                    }
                    AgentEvent::Usage(usage) => {
                        let rate = usage
                            .cache_hit_rate()
                            .map(|rate| format!("{:.0}%", rate * 100.0))
                            .unwrap_or_else(|| "n/a".to_string());
                        eprintln!(
                            "[usage] prompt {} (cache read {}, input {}, write {}) output {} hit {}",
                            usage.prompt_tokens(),
                            usage.cache_read,
                            usage.input,
                            usage.cache_write,
                            usage.output,
                            rate
                        );
                    }
                }
            }
            if let Some(journal) = journal.as_mut() {
                let _ = journal.persist(agent.messages());
            }
        }
        Err(error) => {
            eprintln!("pi-native: {error}");
            std::process::exit(1);
        }
    }
}

/// Asks the terminal before running an approval-required tool.
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
            let (old, new) = if index.is_multiple_of(2) {
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
    let tools = all_tools();
    let Some(selected) = tools.iter().find(|candidate| candidate.name() == tool) else {
        eprintln!("pi-native --stress: unknown tool `{tool}`");
        std::process::exit(2);
    };

    let cpu_before = cpu_times();
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
    let cpu = match (cpu_before, cpu_times()) {
        (Some((ub, sb)), Some((ua, sa))) => Some((ua - ub, sa - sb)),
        _ => None,
    };

    println!(
        "tool-stress[{tool}]: {turns} calls, {bytes} bytes out, {errors} errors, {:.2}s",
        elapsed.as_secs_f64()
    );
    if let Some((user, sys)) = cpu {
        println!("cpu: user {user:.2}s sys {sys:.2}s");
    }
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
        all_tools(),
        "You are pi, a coding agent. Be concise.",
        ToolContext::new(&dir),
    )
    .with_compaction(context_tokens, DEFAULT_RESERVE_TOKENS);
    let mut agent = if byte_limit_mb > 0 {
        agent.with_context_byte_limit(byte_limit_mb * 1024 * 1024)
    } else {
        agent
    };
    agent.push_user("stress");

    let cpu_before = cpu_times();
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
            if let Some((user, sys)) = match (cpu_before, cpu_times()) {
                (Some((ub, sb)), Some((ua, sa))) => Some((ua - ub, sa - sb)),
                _ => None,
            } {
                println!("cpu: user {user:.2}s sys {sys:.2}s");
            }
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

/// User and system CPU time of this process in seconds, from `/proc` (Linux).
fn cpu_times() -> Option<(f64, f64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After comm: field 3 is index 0, so utime (field 14) is index 11, stime 12.
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime / 100.0, stime / 100.0))
}

/// Peak resident set size of this process in MB, from `/proc` when available.
fn peak_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}

/// Resolve the pi install root for the `docs` section paths. `PI_PACKAGE_DIR`
/// wins; otherwise a well-known global install is used; otherwise paths are bare.
fn pi_package_dir() -> String {
    if let Ok(dir) = std::env::var("PI_PACKAGE_DIR") {
        if !dir.is_empty() {
            return dir;
        }
    }
    let relative = "node_modules/@earendil-works/pi-coding-agent";
    let mut candidates = vec![PathBuf::from("/usr/local/lib").join(relative)];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(&home).join(".npm-global/lib").join(relative));
        candidates.push(PathBuf::from(&home).join(".local/lib").join(relative));
    }
    for candidate in candidates {
        if candidate.is_dir() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    String::new()
}

/// pi's agent config dir (`~/.pi`), overridable with `PI_AGENT_DIR`.
fn agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PI_AGENT_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".pi"),
        Err(_) => PathBuf::new(),
    }
}

/// Build pi's system prompt for the current environment (parity with pi's default).
fn system_prompt_for(cwd: &std::path::Path) -> String {
    let selected_tools = default_tools()
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    let options = SystemPromptOptions {
        selected_tools,
        package_dir: pi_package_dir(),
        cwd: cwd.to_string_lossy().into_owned(),
        context_files: load_project_context_files(cwd, &agent_dir()),
        ..Default::default()
    };
    build_system_prompt(&options)
}

/// Build an agent for the selected provider, with compaction when enabled.
///
/// One generic path: the provider is chosen here, the loop/tools are the same.
/// Resolved provider settings. Nothing vendor-specific is assumed.
#[derive(Clone)]
struct ProviderConfig {
    provider: String,
    model: String,
    base_url: String,
    api_key: String,
    max_tokens: Option<i64>,
    thinking_format: ThinkingFormat,
    reasoning_effort: Option<String>,
}

fn missing(what: &str) -> ! {
    eprintln!("pi-native: set {what}");
    std::process::exit(2);
}

impl Cli {
    fn provider_config(&self) -> ProviderConfig {
        let provider = self
            .provider
            .clone()
            .unwrap_or_else(|| missing("--provider (openai-completions or openai-responses)"));
        let model = self.model.clone().unwrap_or_else(|| missing("--model"));
        let base_url = self
            .base_url
            .clone()
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| missing("--base-url or OPENAI_BASE_URL"));
        let api_key = self
            .api_key
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .unwrap_or_else(|| missing("--api-key or OPENAI_API_KEY"));
        let thinking_format = match self.thinking_format.as_deref() {
            Some("deepseek") => ThinkingFormat::Deepseek,
            Some("none") | None => ThinkingFormat::None,
            Some(other) => {
                eprintln!("pi-native: unknown thinking format `{other}` (none, deepseek)");
                std::process::exit(2);
            }
        };
        ProviderConfig {
            provider,
            model,
            base_url,
            api_key,
            max_tokens: self.max_tokens,
            thinking_format,
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }
}

/// Build a provider instance for the selected protocol.
fn make_provider(config: &ProviderConfig) -> Box<dyn ModelProvider> {
    match config.provider.as_str() {
        "openai-completions" => Box::new(openai_completions_provider(
            config.base_url.clone(),
            config.api_key.clone(),
            config.model.clone(),
            config.max_tokens,
            config.thinking_format,
            config.reasoning_effort.clone(),
        )),
        "openai-responses" => Box::new(openai_responses_provider(
            config.base_url.clone(),
            config.api_key.clone(),
            config.model.clone(),
        )),
        other => {
            eprintln!(
                "pi-native: unknown provider `{other}` (openai-completions, openai-responses)"
            );
            std::process::exit(2);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_agent(
    config: &ProviderConfig,
    yolo: bool,
    context_window: usize,
    system: &str,
    extensions: &[PathBuf],
    extension_allow: &[String],
    cwd: &std::path::Path,
) -> Agent {
    let mut tools = default_tools();
    let (extension_tools, commands) = load_extension_tools(extensions, extension_allow, cwd);
    tools.extend(extension_tools);
    // Tools are jailed to the working directory unless the caller explicitly
    // opted into unrestricted execution with `--yolo`. The system temp dir is an
    // extra read+write root so a unit can stage and read back scratch files
    // (Linear comment bodies, drafts) without escaping the code jail.
    let scratch = std::env::temp_dir();
    let tool_context = ToolContext::new(cwd)
        .allow_outside(yolo)
        .with_read_roots([scratch.clone()])
        .with_write_roots([scratch]);
    let mut agent =
        Agent::new(make_provider(config), tools, system, tool_context).with_commands(commands);
    if context_window > 0 {
        // Compaction summarizes dropped history with the model (pi's behavior).
        agent = agent
            .with_compaction(context_window, DEFAULT_RESERVE_TOKENS)
            .with_summarizer(Arc::new(ProviderSummarizer::new(make_provider(config))));
    }
    agent
}

/// Resolve the session file to use: explicit `--session`, a fresh file in the
/// pi agent dir, or none with `--no-session`.
fn resolve_session(
    session: Option<&std::path::Path>,
    no_session: bool,
    cwd: &std::path::Path,
) -> Option<PathBuf> {
    if no_session {
        return None;
    }
    if let Some(path) = session {
        return Some(path.to_path_buf());
    }
    match pi_agent::new_session_path(cwd) {
        Ok(path) => Some(path),
        Err(error) => {
            eprintln!("pi-native: cannot create a session file: {error}");
            None
        }
    }
}

/// Seed the transcript from `--session` (if given) and return a journal that
/// persists later turns back to the same file.
fn open_session(
    agent: &mut Agent,
    session: Option<&std::path::Path>,
    cwd: &std::path::Path,
) -> Option<SessionJournal> {
    let path = session?;
    match SessionJournal::open(path.to_path_buf(), &cwd.to_string_lossy()) {
        Ok((journal, transcript)) => {
            agent.extend_messages(transcript);
            Some(journal)
        }
        Err(error) => {
            eprintln!("pi-native: cannot open session {}: {error}", path.display());
            std::process::exit(2);
        }
    }
}

/// Serve the RPC protocol over stdio with the selected provider.
fn run_serve(
    config: &ProviderConfig,
    yolo: bool,
    context_window: usize,
    session: Option<&std::path::Path>,
    no_session: bool,
    extensions: &[PathBuf],
    extension_allow: &[String],
) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let system = system_prompt_for(&cwd);
    let mut agent = resolve_agent(
        config,
        yolo,
        context_window,
        &system,
        extensions,
        extension_allow,
        &cwd,
    );
    let path = resolve_session(session, no_session, &cwd);
    let cwd_string = cwd.to_string_lossy().into_owned();
    // Session navigation commands (`get_tree`, `switch_session`, …) work on
    // `--session`; tools run without approval, matching pi.
    let reader = std::io::BufReader::new(std::io::stdin());
    let writer = std::io::stdout();
    let result = pi_rpc::serve_session(&mut agent, path, &cwd_string, reader, writer, |_| {});
    let _ = result;
}

/// Serve the HTTP + SSE gateway. Each session is an independent unit and any
/// number of clients can attach to it. With `--triggers`, a background loop
/// fires scheduled prompts into their long-lived sessions.
#[allow(clippy::too_many_arguments)]
fn run_gateway(
    config: ProviderConfig,
    context_window: usize,
    extensions: &[PathBuf],
    extension_allow: &[String],
    addr: &str,
    triggers: Option<&std::path::Path>,
    trigger_runs: Option<&std::path::Path>,
    trigger_interval: u64,
    idle_timeout: u64,
    max_units: usize,
) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let extensions = extensions.to_vec();
    let extension_allow = extension_allow.to_vec();
    let factory = move |unit_cwd: &str| {
        let path = std::path::Path::new(unit_cwd);
        let system = system_prompt_for(path);
        resolve_agent(
            &config,
            false,
            context_window,
            &system,
            &extensions,
            &extension_allow,
            path,
        )
    };
    let host =
        pi_host::Host::new(cwd.to_string_lossy().to_string(), factory).with_max_units(max_units);
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("pi-native: cannot bind {addr}: {error}");
            std::process::exit(1);
        }
    };
    let gateway = match triggers {
        Some(path) => {
            let triggers = pi_triggers::load_triggers(path).unwrap_or_else(|error| {
                eprintln!("pi-native: {error}");
                std::process::exit(1);
            });
            let runs = trigger_runs
                .map(PathBuf::from)
                .unwrap_or_else(|| cwd.join(".pi-native").join("trigger-runs.jsonl"));
            let store = pi_triggers::JsonlRuns::open(&runs).unwrap_or_else(|error| {
                eprintln!("pi-native: cannot open {}: {error}", runs.display());
                std::process::exit(1);
            });
            let sessions = cwd.join(".pi-native").join("trigger-sessions");
            let runner = pi_triggers::Runner::new(triggers, sessions);
            Arc::new(pi_gateway::Gateway::with_triggers(
                host,
                runner,
                Box::new(store),
            ))
        }
        None => Arc::new(pi_gateway::Gateway::new(host)),
    };
    if triggers.is_some() {
        gateway.spawn_trigger_loop(std::time::Duration::from_secs(trigger_interval.max(1)));
    }
    if idle_timeout > 0 {
        gateway.spawn_idle_reaper(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(idle_timeout),
        );
    }
    eprintln!("pi-native gateway listening on http://{addr}");
    pi_gateway::serve(listener, gateway);
}

/// A minimal terminal client: spawn a unit serving the protocol and drive it.
fn run_client(
    config: &ProviderConfig,
    yolo: bool,
    context_window: usize,
    session: Option<&std::path::Path>,
    no_session: bool,
    extensions: &[PathBuf],
    extension_allow: &[String],
) {
    let exe = std::env::current_exe().expect("current executable");
    let mut command = ProcessCommand::new(exe);
    command
        .arg("--serve")
        .arg("--provider")
        .arg(&config.provider)
        .arg("--model")
        .arg(&config.model)
        .arg("--base-url")
        .arg(&config.base_url)
        .arg("--context-window")
        .arg(context_window.to_string());
    // Pass the key by environment, not argv.
    command.env("OPENAI_API_KEY", &config.api_key);
    if let Some(max_tokens) = config.max_tokens {
        command.arg("--max-tokens").arg(max_tokens.to_string());
    }
    if matches!(config.thinking_format, ThinkingFormat::Deepseek) {
        command.arg("--thinking-format").arg("deepseek");
    }
    if let Some(effort) = &config.reasoning_effort {
        command.arg("--reasoning-effort").arg(effort);
    }
    if yolo {
        command.arg("--yolo");
    }
    if let Some(path) = session {
        command.arg("--session").arg(path);
    }
    if no_session {
        command.arg("--no-session");
    }
    for extension in extensions {
        command.arg("--extension").arg(extension);
    }
    for capability in extension_allow {
        command.arg("--extension-allow").arg(capability);
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

    // Interactive mode renders through the native renderer: the transcript and
    // editor are composed into a cell buffer and diffed frame to frame, so a
    // long transcript is not re-emitted when only the editor line changes.
    let mut screen = pi_tui::Screen::new();
    screen.push_text(
        "pi-native client. Type a prompt; Ctrl-D to quit.",
        dim_style(),
    );
    let (mut width, mut height) = terminal_size();
    let mut renderer = pi_tui::Renderer::new();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    render_frame(&mut out, &mut renderer, &screen, width, height);

    loop {
        screen.set_editor(String::new());
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        // A window resize between prompts is picked up on the next frame.
        let (next_width, next_height) = terminal_size();
        if (next_width, next_height) != (width, height) {
            width = next_width;
            height = next_height;
            renderer.invalidate();
        }
        let text = line.trim();
        if text.is_empty() {
            render_frame(&mut out, &mut renderer, &screen, width, height);
            continue;
        }
        screen.push_line(pi_tui::TranscriptLine::styled(
            format!("> {text}"),
            dim_style(),
        ));
        render_frame(&mut out, &mut renderer, &screen, width, height);
        if send(
            &mut server_in,
            &serde_json::json!({ "type": "prompt", "text": text }),
        )
        .is_err()
        {
            break;
        }
        if !pump(
            &mut server_out,
            &mut server_in,
            &stdin,
            &mut screen,
            &mut renderer,
            &mut out,
            width,
            height,
        ) {
            break;
        }
    }
    let _ = child.kill();
}

/// Dim style for secondary chrome (prompt echo, hint line).
fn dim_style() -> pi_tui::Style {
    pi_tui::Style {
        dim: true,
        ..pi_tui::Style::default()
    }
}

/// Best-effort terminal size, falling back to a conventional 80x24.
fn terminal_size() -> (usize, usize) {
    // `stty size` prints "rows cols"; unavailable when not attached to a TTY.
    let output = ProcessCommand::new("stty").arg("size").output();
    if let Ok(output) = output {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut parts = text.split_whitespace();
            if let (Some(rows), Some(cols)) = (parts.next(), parts.next()) {
                if let (Ok(rows), Ok(cols)) = (rows.parse::<usize>(), cols.parse::<usize>()) {
                    if rows > 0 && cols > 0 {
                        return (cols, rows);
                    }
                }
            }
        }
    }
    (80, 24)
}

/// Emit one frame through the differential renderer and park the cursor on the
/// editor caret.
fn render_frame(
    out: &mut impl Write,
    renderer: &mut pi_tui::Renderer,
    screen: &pi_tui::Screen,
    width: usize,
    height: usize,
) {
    let frame = screen.compose(width, height);
    let delta = renderer.render(frame.buffer);
    let _ = out.write_all(delta.as_bytes());
    let _ = out.write_all(pi_tui::cursor_sequence(frame.cursor).as_bytes());
    let _ = out.flush();
}

fn send(writer: &mut impl Write, value: &serde_json::Value) -> std::io::Result<()> {
    writeln!(writer, "{value}")?;
    writer.flush()
}

/// Read events until `done`, appending them to the transcript and re-rendering,
/// and answering any generic UI request.
#[allow(clippy::too_many_arguments)]
fn pump(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    stdin: &std::io::Stdin,
    screen: &mut pi_tui::Screen,
    renderer: &mut pi_tui::Renderer,
    out: &mut impl Write,
    width: usize,
    height: usize,
) -> bool {
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
        let mut dirty = true;
        match event["type"].as_str() {
            Some("assistant_text") => screen.push_text(
                event["text"].as_str().unwrap_or(""),
                pi_tui::Style::default(),
            ),
            Some("tool_start") => screen.push_line(pi_tui::TranscriptLine::styled(
                format!("[tool {} start]", event["name"].as_str().unwrap_or("")),
                dim_style(),
            )),
            Some("tool_end") => screen.push_line(pi_tui::TranscriptLine::styled(
                format!(
                    "[tool {} {}]",
                    event["name"].as_str().unwrap_or(""),
                    if event["is_error"].as_bool().unwrap_or(false) {
                        "error"
                    } else {
                        "ok"
                    }
                ),
                dim_style(),
            )),
            Some("error") => screen.push_line(pi_tui::TranscriptLine::styled(
                format!("error: {}", event["message"].as_str().unwrap_or("")),
                pi_tui::Style {
                    fg: Some(pi_tui::Color::Ansi(1)),
                    ..pi_tui::Style::default()
                },
            )),
            Some("ui_request") => {
                let kind = event["kind"].as_str().unwrap_or("input");
                let prompt = event["prompt"].as_str().unwrap_or("");
                // Show the dialog, then answer it on the next stdin line. The
                // cursor is parked on the input value for a real TTY driver.
                screen.set_dialog(pi_tui::Dialog::Input {
                    title: format!("[{kind}] {prompt}"),
                    value: String::new(),
                });
                render_frame(out, renderer, screen, width, height);
                let mut answer = String::new();
                let _ = stdin.read_line(&mut answer);
                screen.clear_dialog();
                let response = serde_json::json!({
                    "type": "ui_response",
                    "id": event["id"],
                    "value": answer.trim(),
                });
                let _ = send(writer, &response);
            }
            Some("done") => {
                render_frame(out, renderer, screen, width, height);
                return true;
            }
            _ => dirty = false,
        }
        if dirty {
            render_frame(out, renderer, screen, width, height);
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

/// A tool registered by a loaded pi extension (`pi.registerTool`).
struct PluginTool {
    instance: Rc<PluginInstance>,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn run(&self, input: &serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        // Extension code runs under the extension policy (deny-by-default for
        // ambient access) and always requires approval.
        match self.instance.call_tool(&self.name, input) {
            Ok(value) => {
                let is_error = value
                    .get("isError")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let text = tool_result_text(&value);
                if is_error {
                    ToolResult::error(text)
                } else {
                    ToolResult::ok(text)
                }
            }
            Err(error) => ToolResult::error(format!("{}: {error}", self.name)),
        }
    }
}

/// Extract text from a plugin tool result (`{content:[{type:"text",text}]}` or a string).
fn tool_result_text(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    if let Some(items) = value.get("content").and_then(serde_json::Value::as_array) {
        let text: Vec<String> = items
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                    item.get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect();
        if !text.is_empty() {
            return text.join("\n");
        }
    }
    value.to_string()
}

/// Load each `--extension` file and expose the tools it registered.
fn load_extension_tools(
    paths: &[PathBuf],
    allowed: &[String],
    cwd: &std::path::Path,
) -> (Vec<Box<dyn Tool>>, Vec<serde_json::Value>) {
    let mut policy = PluginPolicy::for_extension(cwd);
    for name in allowed {
        match name.trim().to_ascii_lowercase().as_str() {
            "read" => policy = policy.allow(pi_plugins::Capability::Read),
            "write" => policy = policy.allow(pi_plugins::Capability::Write),
            "exec" => policy = policy.allow(pi_plugins::Capability::Exec),
            "http" => policy = policy.allow(pi_plugins::Capability::Http),
            other => eprintln!("pi-native: unknown extension capability `{other}`"),
        }
    }
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut commands: Vec<serde_json::Value> = Vec::new();
    for path in paths {
        match PluginInstance::from_file(policy.clone(), path) {
            Ok(instance) => {
                let instance = Rc::new(instance);
                for spec in instance.tools() {
                    tools.push(Box::new(PluginTool {
                        instance: instance.clone(),
                        name: spec.name.clone(),
                        description: spec.description.clone(),
                        parameters: spec.parameters.clone(),
                    }));
                }
                // Extension commands become slash commands (pi's
                // `SlashCommandInfo`: name, description, source).
                for command in instance.commands() {
                    let name = command
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    if name.is_empty() {
                        continue;
                    }
                    let description = command
                        .get("spec")
                        .and_then(|spec| spec.get("description"))
                        .and_then(serde_json::Value::as_str);
                    commands.push(serde_json::json!({
                        "name": name,
                        "description": description,
                        "source": "extension",
                    }));
                }
            }
            Err(error) => eprintln!("pi-native: extension {}: {error}", path.display()),
        }
    }
    (tools, commands)
}

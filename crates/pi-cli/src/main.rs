//! `pi-native` CLI.
//!
//! Early scaffold. Today it exposes the prompt-cache policy decisions so the
//! provider-parity gate can be exercised from the command line; the agent host
//! lands in later milestones.

use clap::{Parser, Subcommand, ValueEnum};
use pi_cache::{
    clamp_openai_prompt_cache_key, get_cache_control, openai_completions_prompt_cache_key,
    openai_responses_prompt_cache_key, resolve_cache_retention, CacheRetention,
};

#[derive(Parser)]
#[command(name = "pi-native", version, about = "Native Rust runtime for pi")]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    match cli.command {
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
                openai_completions_prompt_cache_key(retention, session_id.as_deref(), openai_api, supports_long)
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

//! Provider prompt-cache primitives, ported from pi.
//!
//! This crate is the first brick of the provider-parity gate (VED-315): the
//! logic here must match pi's TypeScript exactly so that outbound requests and
//! cache behavior are indistinguishable from the provider's point of view.
//!
//! Sources mirrored (pi monorepo):
//! - `packages/ai/src/api/anthropic-messages.ts` (cache retention, cache_control)
//! - `packages/ai/src/api/openai-prompt-cache.ts` (prompt_cache_key clamp)
//! - `packages/ai/src/api/openai-completions.ts` / `openai-responses.ts` (key wiring)
//! - `packages/coding-agent/src/core/cache-stats.ts` (miss detection)
//! - `packages/coding-agent/src/core/cache-warmer.ts` (warming economics)

pub mod prompt_key;
pub mod retention;
pub mod stats;
pub mod warmer;

pub use prompt_key::{
    clamp_openai_prompt_cache_key, openai_completions_prompt_cache_key, openai_responses_prompt_cache_key,
    OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH,
};
pub use retention::{
    get_cache_control, resolve_cache_retention, CacheControlEphemeral, CacheControlResult, CacheRetention,
};
pub use stats::{
    compute_cache_waste, detect_miss, scan, CacheEntry, CacheMiss, CacheWasteTotals, MissMessage,
    ModelPriceSource, PreviousRequest, Usage, CACHE_TTL_MS,
};
pub use warmer::{
    evaluate_warming, get_cache_warming_delay_ms, is_replayable, WarmAction, WarmingDecision,
    CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS, IDLE_CONTINUATION_PROBABILITY, MAX_IDLE_WARMING_AGE_MS,
    MAX_WARMING_AGE_MS,
};

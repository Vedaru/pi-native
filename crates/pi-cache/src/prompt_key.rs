//! OpenAI prompt-cache key handling.
//!
//! Mirrors pi `packages/ai/src/api/openai-prompt-cache.ts` plus the call-site
//! conditions in `openai-completions.ts` and `openai-responses.ts`.

use crate::retention::CacheRetention;

/// OpenAI accepts at most this many characters in `prompt_cache_key`.
pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// Clamp a cache key to 64 Unicode code points, matching pi's `Array.from`
/// behavior. Returns `None` when there is no key.
pub fn clamp_openai_prompt_cache_key(key: Option<&str>) -> Option<String> {
    let key = key?;
    if key.chars().count() <= OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH {
        return Some(key.to_string());
    }
    Some(
        key.chars()
            .take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH)
            .collect(),
    )
}

/// `openai-responses.ts`: `cacheRetention === "none" ? undefined : clamp(sessionId)`.
pub fn openai_responses_prompt_cache_key(
    retention: CacheRetention,
    session_id: Option<&str>,
) -> Option<String> {
    if retention == CacheRetention::None {
        return None;
    }
    clamp_openai_prompt_cache_key(session_id)
}

/// `openai-completions.ts`: the key is sent when the base URL is the OpenAI API
/// and caching is on, or when long retention is supported.
pub fn openai_completions_prompt_cache_key(
    retention: CacheRetention,
    session_id: Option<&str>,
    base_url_is_openai_api: bool,
    supports_long_cache_retention: bool,
) -> Option<String> {
    let send = (base_url_is_openai_api && retention != CacheRetention::None)
        || (retention == CacheRetention::Long && supports_long_cache_retention);
    if !send {
        return None;
    }
    clamp_openai_prompt_cache_key(session_id)
}

/// `openai-completions.ts`: `prompt_cache_retention = "24h"` only for long
/// retention with support.
pub fn openai_prompt_cache_retention(
    retention: CacheRetention,
    supports_long_cache_retention: bool,
) -> Option<&'static str> {
    if retention == CacheRetention::Long && supports_long_cache_retention {
        Some("24h")
    } else {
        None
    }
}

#[cfg(test)]
#[path = "../tests/unit/prompt_key.rs"]
mod tests;

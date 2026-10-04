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
mod tests {
    use super::*;

    #[test]
    fn clamp_leaves_short_keys_untouched() {
        assert_eq!(
            clamp_openai_prompt_cache_key(Some("abc")),
            Some("abc".to_string())
        );
        assert_eq!(clamp_openai_prompt_cache_key(None), None);
    }

    #[test]
    fn clamp_truncates_to_64_code_points_not_bytes() {
        // 70 multibyte chars: byte truncation would split a codepoint.
        let key: String = "é".repeat(70);
        let clamped = clamp_openai_prompt_cache_key(Some(&key)).unwrap();
        assert_eq!(clamped.chars().count(), 64);
        assert_eq!(clamped, "é".repeat(64));
    }

    #[test]
    fn responses_key_omitted_when_caching_off() {
        assert_eq!(
            openai_responses_prompt_cache_key(CacheRetention::None, Some("sess")),
            None
        );
        assert_eq!(
            openai_responses_prompt_cache_key(CacheRetention::Short, Some("sess")),
            Some("sess".to_string())
        );
    }

    #[test]
    fn completions_key_conditions() {
        // OpenAI API + short retention -> send.
        assert_eq!(
            openai_completions_prompt_cache_key(CacheRetention::Short, Some("s"), true, true),
            Some("s".to_string())
        );
        // OpenAI API + none -> omit.
        assert_eq!(
            openai_completions_prompt_cache_key(CacheRetention::None, Some("s"), true, true),
            None
        );
        // Non-OpenAI + short -> omit.
        assert_eq!(
            openai_completions_prompt_cache_key(CacheRetention::Short, Some("s"), false, true),
            None
        );
        // Non-OpenAI + long + supported -> send.
        assert_eq!(
            openai_completions_prompt_cache_key(CacheRetention::Long, Some("s"), false, true),
            Some("s".to_string())
        );
        // Long but unsupported on a non-OpenAI base URL -> omit.
        assert_eq!(
            openai_completions_prompt_cache_key(CacheRetention::Long, Some("s"), false, false),
            None
        );
    }

    #[test]
    fn retention_field_requires_long_and_support() {
        assert_eq!(
            openai_prompt_cache_retention(CacheRetention::Long, true),
            Some("24h")
        );
        assert_eq!(
            openai_prompt_cache_retention(CacheRetention::Long, false),
            None
        );
        assert_eq!(
            openai_prompt_cache_retention(CacheRetention::Short, true),
            None
        );
    }
}

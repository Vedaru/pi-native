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

//! Cache retention and `cache_control` resolution.
//!
//! Mirrors pi `packages/ai/src/api/anthropic-messages.ts`:
//! `resolveCacheRetention` and `getCacheControl`.

use serde::Serialize;

/// Prompt-cache retention tier. `None` disables caching entirely (no markers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRetention {
    Short,
    Long,
    None,
}

impl CacheRetention {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheRetention::Short => "short",
            CacheRetention::Long => "long",
            CacheRetention::None => "none",
        }
    }
}

/// Anthropic ephemeral cache marker: `{"type":"ephemeral"}` optionally with
/// `{"ttl":"1h"}` for long retention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheControlEphemeral {
    pub r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheControlResult {
    pub retention: CacheRetention,
    pub cache_control: Option<CacheControlEphemeral>,
}

/// Resolve retention: an explicit choice wins, else `PI_CACHE_RETENTION ==
/// "long"` selects long, else short. `none` is only reachable explicitly.
pub fn resolve_cache_retention(
    explicit: Option<CacheRetention>,
    env_value: Option<&str>,
) -> CacheRetention {
    if let Some(retention) = explicit {
        return retention;
    }
    if env_value == Some("long") {
        return CacheRetention::Long;
    }
    CacheRetention::Short
}

/// Build the Anthropic `cache_control` marker for a retention tier.
///
/// `none` yields no marker at all. `long` adds `ttl: "1h"` only when the model
/// advertises support for long retention.
pub fn get_cache_control(
    retention: CacheRetention,
    supports_long_cache_retention: bool,
) -> CacheControlResult {
    if retention == CacheRetention::None {
        return CacheControlResult {
            retention,
            cache_control: None,
        };
    }
    let ttl = if retention == CacheRetention::Long && supports_long_cache_retention {
        Some("1h")
    } else {
        None
    };
    CacheControlResult {
        retention,
        cache_control: Some(CacheControlEphemeral {
            r#type: "ephemeral",
            ttl,
        }),
    }
}

#[cfg(test)]
#[path = "../tests/unit/retention.rs"]
mod tests;

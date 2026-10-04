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
pub fn resolve_cache_retention(explicit: Option<CacheRetention>, env_value: Option<&str>) -> CacheRetention {
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
pub fn get_cache_control(retention: CacheRetention, supports_long_cache_retention: bool) -> CacheControlResult {
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
mod tests {
    use super::*;

    #[test]
    fn explicit_retention_wins_over_env() {
        assert_eq!(
            resolve_cache_retention(Some(CacheRetention::None), Some("long")),
            CacheRetention::None
        );
        assert_eq!(
            resolve_cache_retention(Some(CacheRetention::Short), None),
            CacheRetention::Short
        );
    }

    #[test]
    fn env_long_selects_long_otherwise_short() {
        assert_eq!(resolve_cache_retention(None, Some("long")), CacheRetention::Long);
        assert_eq!(resolve_cache_retention(None, Some("short")), CacheRetention::Short);
        assert_eq!(resolve_cache_retention(None, None), CacheRetention::Short);
    }

    #[test]
    fn none_emits_no_marker() {
        let result = get_cache_control(CacheRetention::None, true);
        assert_eq!(result.cache_control, None);
    }

    #[test]
    fn short_emits_ephemeral_without_ttl() {
        let result = get_cache_control(CacheRetention::Short, true);
        assert_eq!(
            result.cache_control,
            Some(CacheControlEphemeral {
                r#type: "ephemeral",
                ttl: None
            })
        );
    }

    #[test]
    fn long_emits_ttl_only_when_supported() {
        assert_eq!(
            get_cache_control(CacheRetention::Long, true).cache_control,
            Some(CacheControlEphemeral {
                r#type: "ephemeral",
                ttl: Some("1h")
            })
        );
        assert_eq!(
            get_cache_control(CacheRetention::Long, false).cache_control,
            Some(CacheControlEphemeral {
                r#type: "ephemeral",
                ttl: None
            })
        );
    }

    #[test]
    fn serializes_to_pi_wire_shape() {
        let marker = get_cache_control(CacheRetention::Long, true).cache_control.unwrap();
        let json = serde_json::to_string(&marker).unwrap();
        assert_eq!(json, r#"{"type":"ephemeral","ttl":"1h"}"#);
    }
}

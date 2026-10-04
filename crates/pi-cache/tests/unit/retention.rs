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
    assert_eq!(
        resolve_cache_retention(None, Some("long")),
        CacheRetention::Long
    );
    assert_eq!(
        resolve_cache_retention(None, Some("short")),
        CacheRetention::Short
    );
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
    let marker = get_cache_control(CacheRetention::Long, true)
        .cache_control
        .unwrap();
    let json = serde_json::to_string(&marker).unwrap();
    assert_eq!(json, r#"{"type":"ephemeral","ttl":"1h"}"#);
}

use super::*;

struct NoPrices;
impl ModelPriceSource for NoPrices {
    fn cache_read_per_million(&self, _provider: &str, _model: &str) -> Option<f64> {
        None
    }
}
fn msg(input: i64, cache_read: i64, cache_write: i64, timestamp: i64) -> MissMessage {
    MissMessage {
        usage: Usage {
            input,
            cache_read,
            cache_write,
            ..Default::default()
        },
        timestamp,
        provider: "anthropic".to_string(),
        model: "claude".to_string(),
    }
}

#[test]
fn first_turn_is_not_a_miss() {
    assert_eq!(detect_miss(None, &msg(10_000, 0, 0, 1), &NoPrices), None);
}

#[test]
fn miss_below_noise_floor_is_ignored() {
    let prev = PreviousRequest {
        prompt_tokens: 10_000,
        model_key: "anthropic/claude".to_string(),
        timestamp: 0,
        reported_cache: true,
    };
    // missed = 10000 - 9000 = 1000 <= 1024 -> ignored.
    assert_eq!(
        detect_miss(Some(&prev), &msg(1000, 9000, 0, 5), &NoPrices),
        None
    );
}

#[test]
fn counts_a_real_miss() {
    let prev = PreviousRequest {
        prompt_tokens: 10_000,
        model_key: "anthropic/claude".to_string(),
        timestamp: 0,
        reported_cache: true,
    };
    // missed = 10000 - 8000 = 2000 > 1024.
    let miss = detect_miss(Some(&prev), &msg(2000, 8000, 0, 5), &NoPrices).unwrap();
    assert_eq!(miss.missed_tokens, 2000);
    assert_eq!(miss.idle_ms, 5);
    assert!(!miss.model_changed);
}

#[test]
fn model_switch_is_counted_not_exempt() {
    let prev = PreviousRequest {
        prompt_tokens: 10_000,
        model_key: "anthropic/old".to_string(),
        timestamp: 0,
        reported_cache: true,
    };
    let mut message = msg(10_000, 0, 0, 5);
    message.model = "new".to_string();
    let miss = detect_miss(Some(&prev), &message, &NoPrices).unwrap();
    assert!(miss.model_changed);
    assert_eq!(miss.missed_tokens, 10_000);
}

#[test]
fn context_reset_clears_previous_request() {
    let first = msg(10_000, 0, 5_000, 1);
    let second = msg(10_000, 0, 0, 10);
    let entries = vec![
        CacheEntry::Message(&first),
        CacheEntry::ContextReset,
        CacheEntry::Message(&second),
    ];
    let (_, totals) = scan(&entries, &NoPrices);
    // Second message has no prior request after reset -> no miss counted.
    assert_eq!(totals.miss_count, 0);
}

/// Parity against pi's real `computeCacheWaste` (VED-314).
/// Regenerate with `node scripts/harness/capture-cache-stats.mjs`.
#[test]
fn matches_captured_pi_cache_waste() {
    use serde_json::Value;

    struct ScenarioPrices {
        models: Value,
    }
    impl ModelPriceSource for ScenarioPrices {
        fn cache_read_per_million(&self, provider: &str, model: &str) -> Option<f64> {
            self.models
                .get(provider)?
                .get(model)?
                .get("cost")?
                .get("cacheRead")?
                .as_f64()
        }
    }

    let scenario: Value =
        serde_json::from_str(include_str!("../fixtures/cache-stats-scenario.json"))
            .expect("scenario parses");
    let expected: Value =
        serde_json::from_str(include_str!("../fixtures/cache-stats-expected.json"))
            .expect("expected parses");

    fn usage_from(value: &Value) -> Usage {
        let cost = value.get("cost");
        Usage {
            input: value.get("input").and_then(Value::as_i64).unwrap_or(0),
            output: value.get("output").and_then(Value::as_i64).unwrap_or(0),
            cache_read: value.get("cacheRead").and_then(Value::as_i64).unwrap_or(0),
            cache_write: value.get("cacheWrite").and_then(Value::as_i64).unwrap_or(0),
            cost_input: cost
                .and_then(|c| c.get("input"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            cost_cache_read: cost
                .and_then(|c| c.get("cacheRead"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            cost_cache_write: cost
                .and_then(|c| c.get("cacheWrite"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
        }
    }

    enum Item {
        Message(MissMessage),
        Reset,
        Warm(Usage, String, String),
    }

    let mut items = Vec::new();
    for entry in scenario["entries"].as_array().expect("entries") {
        match entry.get("type").and_then(Value::as_str) {
            Some("compaction") | Some("branch_summary") => items.push(Item::Reset),
            Some("usage") if entry.get("kind").and_then(Value::as_str) == Some("cache_warm") => {
                items.push(Item::Warm(
                    usage_from(&entry["usage"]),
                    entry["provider"].as_str().unwrap_or_default().to_string(),
                    entry["model"].as_str().unwrap_or_default().to_string(),
                ));
            }
            Some("message") => {
                let message = &entry["message"];
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                items.push(Item::Message(MissMessage {
                    usage: usage_from(&message["usage"]),
                    timestamp: message
                        .get("timestamp")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    provider: message["provider"].as_str().unwrap_or_default().to_string(),
                    model: message["model"].as_str().unwrap_or_default().to_string(),
                }));
            }
            _ => {}
        }
    }

    let entries: Vec<CacheEntry<'_>> = items
        .iter()
        .map(|item| match item {
            Item::Message(message) => CacheEntry::Message(message),
            Item::Reset => CacheEntry::ContextReset,
            Item::Warm(usage, provider, model) => CacheEntry::CacheWarm {
                usage,
                provider,
                model,
                timestamp: 0,
            },
        })
        .collect();

    let prices = ScenarioPrices {
        models: scenario["models"].clone(),
    };
    let totals = compute_cache_waste(&entries, &prices);

    assert_eq!(
        totals.missed_tokens,
        expected["missedTokens"].as_i64().unwrap(),
        "missed tokens differ from pi"
    );
    assert_eq!(
        totals.miss_count as i64,
        expected["missCount"].as_i64().unwrap(),
        "miss count differs from pi"
    );
    let expected_cost = expected["missedCost"].as_f64().unwrap();
    assert!(
        (totals.missed_cost - expected_cost).abs() < 1e-9,
        "missed cost {} differs from pi {}",
        totals.missed_cost,
        expected_cost
    );
}

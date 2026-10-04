//! Prompt-cache miss detection and waste accounting.
//!
//! Mirrors pi `packages/coding-agent/src/core/cache-stats.ts`.

/// Idle gaps longer than this are worth mentioning as the likely cause of a
/// miss. Anthropic's default cache TTL is 5 minutes.
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache-breakpoint granularity noise.
const NOISE_FLOOR_TOKENS: i64 = 1024;

/// Token and cost usage for a request, in pi's shape.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_input: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
}

/// A completed assistant message the scan evaluates.
#[derive(Debug, Clone)]
pub struct MissMessage {
    pub usage: Usage,
    pub timestamp: i64,
    pub provider: String,
    pub model: String,
}

/// The last request seen by the scan; everything in its prompt should be cached.
#[derive(Debug, Clone)]
pub struct PreviousRequest {
    pub prompt_tokens: i64,
    pub model_key: String,
    pub timestamp: i64,
    /// Sticky: some earlier request in this scan segment reported cache activity.
    pub reported_cache: bool,
}

/// A counted cache miss on a single assistant message.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheMiss {
    pub missed_tokens: i64,
    pub missed_cost: f64,
    pub idle_ms: i64,
    pub model_changed: bool,
}

/// Cache cost lookup, satisfied by the model catalog. Cost is USD per million
/// tokens for `cacheRead`, matching pi's `ModelPriceSource`.
pub trait ModelPriceSource {
    fn cache_read_per_million(&self, provider: &str, model: &str) -> Option<f64>;
}

/// A simplified session entry for scanning.
pub enum CacheEntry<'a> {
    Message(&'a MissMessage),
    /// Compaction or branch summary: the context legitimately changed.
    ContextReset,
    /// A cache-warming usage entry that refreshed the cache.
    CacheWarm {
        usage: &'a Usage,
        provider: &'a str,
        model: &'a str,
        timestamp: i64,
    },
}

/// Cumulative cache waste across a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: i64,
    pub missed_cost: f64,
    pub miss_count: u32,
}

fn model_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

fn prompt_tokens(usage: &Usage) -> i64 {
    usage.input + usage.cache_read + usage.cache_write
}

/// Compute the cache miss for one assistant message relative to the previous
/// request. Returns `None` for the first turn, a reset, a provider that never
/// reports caching, or a miss below the noise floor.
pub fn detect_miss(
    prev: Option<&PreviousRequest>,
    message: &MissMessage,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let usage = &message.usage;
    let prompt = prompt_tokens(usage);

    let prev = prev?;
    if prompt <= 0 {
        return None;
    }
    if usage.cache_read + usage.cache_write == 0 && !prev.reported_cache {
        return None;
    }

    let missed_tokens = std::cmp::min(prev.prompt_tokens, prompt) - usage.cache_read;
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    // Extra cost = missed tokens billed at the paid rate instead of the
    // cache-read rate. Missed tokens can only land in the input or cacheWrite
    // buckets, so the paid rate comes from this message's own cost breakdown.
    let paid_tokens = usage.input + usage.cache_write;
    let paid_per_token = if paid_tokens > 0 {
        (usage.cost_input + usage.cost_cache_write) / paid_tokens as f64
    } else {
        0.0
    };
    let read_per_token = if usage.cache_read > 0 {
        usage.cost_cache_read / usage.cache_read as f64
    } else {
        models
            .cache_read_per_million(&message.provider, &message.model)
            .unwrap_or(0.0)
            / 1_000_000.0
    };

    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
        idle_ms: (message.timestamp - prev.timestamp).max(0),
        model_changed: model_key(&message.provider, &message.model) != prev.model_key,
    })
}

fn as_previous_request(message: &MissMessage, reported_cache: bool) -> Option<PreviousRequest> {
    let prompt = prompt_tokens(&message.usage);
    if prompt <= 0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens: prompt,
        model_key: model_key(&message.provider, &message.model),
        timestamp: message.timestamp,
        reported_cache: reported_cache || message.usage.cache_read + message.usage.cache_write > 0,
    })
}

/// Scan entries, returning the trailing request state and cumulative waste.
pub fn scan(
    entries: &[CacheEntry<'_>],
    models: &dyn ModelPriceSource,
) -> (Option<PreviousRequest>, CacheWasteTotals) {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();

    for entry in entries {
        match entry {
            CacheEntry::ContextReset => {
                // The context changed; the next turn's prompt is new content,
                // not re-billed content. Model switches are NOT exempt.
                prev = None;
            }
            CacheEntry::CacheWarm {
                usage,
                provider,
                model,
                timestamp,
            } => {
                if prompt_tokens(usage) > 0 {
                    prev = Some(PreviousRequest {
                        prompt_tokens: prompt_tokens(usage),
                        model_key: model_key(provider, model),
                        timestamp: *timestamp,
                        reported_cache: true,
                    });
                }
            }
            CacheEntry::Message(message) => {
                if let Some(miss) = detect_miss(prev.as_ref(), message, models) {
                    totals.missed_tokens += miss.missed_tokens;
                    totals.missed_cost += miss.missed_cost;
                    totals.miss_count += 1;
                }
                if let Some(next) = as_previous_request(
                    message,
                    prev.as_ref().map(|p| p.reported_cache).unwrap_or(false),
                ) {
                    prev = Some(next);
                }
            }
        }
    }

    (prev, totals)
}

/// Cumulative cache waste across a session.
pub fn compute_cache_waste(
    entries: &[CacheEntry<'_>],
    models: &dyn ModelPriceSource,
) -> CacheWasteTotals {
    scan(entries, models).1
}

#[cfg(test)]
mod tests {
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
    /// Regenerate with `node harness/capture-cache-stats.mjs`.
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

        let scenario: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/cache-stats-scenario.json"
        ))
        .expect("scenario parses");
        let expected: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/cache-stats-expected.json"
        ))
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
                Some("usage")
                    if entry.get("kind").and_then(Value::as_str) == Some("cache_warm") =>
                {
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
}

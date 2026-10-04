//! Cache warming economics and replayability.
//!
//! Mirrors pi `packages/coding-agent/src/core/cache-warmer.ts`.

/// Streaming warming never continues past this long after the real request
/// that started it.
pub const MAX_WARMING_AGE_MS: i64 = 60 * 60_000;
/// Idle warming uses a shorter horizon because continuation estimates become
/// less reliable with age.
pub const MAX_IDLE_WARMING_AGE_MS: i64 = 30 * 60_000;
/// A refresh is sent only when it is expected to save at least this many dollars.
pub const CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;
/// Chance that a real request arrives before the cache entry expires while the
/// agent sits idle.
pub const IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmAction {
    Warm,
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WarmingDecision {
    pub warm_cost: f64,
    pub miss_cost: f64,
    pub continuation_probability: f64,
    pub expected_savings: f64,
    pub economics_available: bool,
    pub action: WarmAction,
}

/// Refresh at 90% of the TTL while preserving at least ten seconds of margin.
/// Returns `None` when the TTL is too short to be worth warming.
pub fn get_cache_warming_delay_ms(ttl_ms: i64) -> Option<i64> {
    if ttl_ms <= 10_000 {
        return None;
    }
    let ninety_percent = (ttl_ms as f64 * 0.9).floor() as i64;
    Some(std::cmp::max(
        1,
        std::cmp::min(ninety_percent, ttl_ms - 10_000),
    ))
}

/// Whether replaying a request with a one-token output cap leaves its cache
/// entry untouched. Anthropic budget-based thinking derives `budget_tokens`
/// from `max_tokens`, which would be changed by the replay.
pub fn is_replayable(
    reasoning: bool,
    is_anthropic_messages: bool,
    force_adaptive_thinking: bool,
) -> bool {
    if !reasoning || !is_anthropic_messages {
        return true;
    }
    force_adaptive_thinking
}

/// Decide whether a warming refresh is worthwhile.
///
/// `cache_hit_cost`, `cache_miss_cost`, and `warm_cost` are the priced outcomes
/// of a cache-read request, a cache-write/input request, and a warming replay
/// respectively (computed by the caller from the model catalog).
pub fn evaluate_warming(
    prompt_tokens: i64,
    cache_hit_cost: f64,
    cache_miss_cost: f64,
    warm_cost: f64,
    is_idle: bool,
) -> WarmingDecision {
    let miss_cost = (cache_miss_cost - cache_hit_cost).max(0.0);
    let continuation_probability = if is_idle {
        IDLE_CONTINUATION_PROBABILITY
    } else {
        1.0
    };
    let economics_available = prompt_tokens > 0 && (cache_hit_cost > 0.0 || cache_miss_cost > 0.0);
    let expected_savings = continuation_probability * miss_cost - warm_cost;
    WarmingDecision {
        warm_cost,
        miss_cost,
        continuation_probability,
        expected_savings,
        economics_available,
        action: if expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS {
            WarmAction::Warm
        } else {
            WarmAction::Stop
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ttls_are_not_warmed() {
        assert_eq!(get_cache_warming_delay_ms(10_000), None);
        assert_eq!(get_cache_warming_delay_ms(0), None);
    }

    #[test]
    fn delay_is_ninety_percent_with_ten_second_margin() {
        // 5 min TTL: 0.9 * 300000 = 270000, margin = 290000 -> 270000.
        assert_eq!(get_cache_warming_delay_ms(300_000), Some(270_000));
        // 12s TTL: 0.9*12000 = 10800, margin = 2000 -> 2000.
        assert_eq!(get_cache_warming_delay_ms(12_000), Some(2_000));
    }

    #[test]
    fn replayability_only_blocked_by_budget_thinking() {
        assert!(is_replayable(false, true, false));
        assert!(is_replayable(true, false, false));
        assert!(!is_replayable(true, true, false));
        assert!(is_replayable(true, true, true));
    }

    #[test]
    fn warming_happens_only_when_savings_clear_the_floor() {
        // idle probability 0.15, miss cost $10 -> expected $1.5 - $0.10 warm > 0.05.
        let decision = evaluate_warming(1_000_000, 0.5, 10.5, 0.10, true);
        assert_eq!(decision.action, WarmAction::Warm);
        assert!(decision.economics_available);

        // Tiny prompt, tiny miss -> not worth it.
        let decision = evaluate_warming(100, 0.0, 0.10, 0.05, true);
        assert_eq!(decision.action, WarmAction::Stop);
    }
}

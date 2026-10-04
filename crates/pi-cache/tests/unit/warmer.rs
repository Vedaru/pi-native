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

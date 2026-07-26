use super::*;

/// Present zero usage remains covered evidence while its undefined
/// zero-denominator cache ratio stays absent.
#[test]
fn present_zero_usage_is_not_missing() {
    let mut summary = Summary::default();
    summary.add_occurrence().expect("occurrence");
    summary.add_terminal().expect("terminal");
    summary.add_usage(&Usage::new(0, 0, 0)).expect("zero usage");
    summary.add_cost(0).expect("zero cost");
    let agent_id = AgentId::parse("summary-test").expect("agent id");
    let projected = serde_json::to_value(summary.project(&agent_id)).expect("summary JSON");

    assert_eq!(projected["prompt_sent_tokens"], 0);
    assert_eq!(projected["estimated_api_cost_picodollars"], 0);
    assert_eq!(projected["usage_missing_occurrences"], 0);
    assert_eq!(projected["cost_missing_occurrences"], 0);
    assert!(projected.get("cache_hit_ratio_ppm").is_none());
}

/// Fixed-point cache ratios floor fractional parts and use widened
/// multiplication before division.
#[test]
fn cache_ratio_uses_flooring_parts_per_million() {
    let mut summary = Summary::default();
    summary.add_occurrence().expect("occurrence");
    summary.add_terminal().expect("terminal");
    summary
        .add_usage(&Usage::new(u64::MAX, u64::MAX - 1, 0))
        .expect("usage");
    let agent_id = AgentId::parse("summary-test").expect("agent id");
    let projected = serde_json::to_value(summary.project(&agent_id)).expect("summary JSON");

    assert_eq!(projected["cache_hit_ratio_ppm"], 999_999);
}

/// Exact aggregate cost rejects overflow instead of silently saturating a
/// sum whose contributing stored values were distinct.
#[test]
fn aggregate_cost_overflow_is_rejected() {
    let mut summary = Summary::default();
    summary.add_cost(u64::MAX).expect("maximum cost");
    assert!(summary.add_cost(1).is_err());
}

/// Every exact token total rejects overflow.
#[test]
fn aggregate_token_overflow_is_rejected() {
    let mut summary = Summary::default();
    summary
        .add_usage(&Usage::new(u64::MAX, u64::MAX, u64::MAX))
        .expect("maximum usage");
    assert!(summary.add_usage(&Usage::new(1, 0, 0)).is_err());

    let mut received = Summary::default();
    received
        .add_usage(&Usage::new(0, 0, u64::MAX))
        .expect("maximum received usage");
    assert!(received.add_usage(&Usage::new(0, 0, 1)).is_err());
}

/// Exact elapsed totals reject overflow.
#[test]
fn aggregate_elapsed_overflow_is_rejected() {
    let mut summary = Summary::default();
    summary.add_elapsed(u64::MAX).expect("maximum elapsed");
    assert!(summary.add_elapsed(1).is_err());
}

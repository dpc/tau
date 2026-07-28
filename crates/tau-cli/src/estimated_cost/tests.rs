use super::*;

/// The shared picker projection records exact canonical values, snapshots them,
/// and drops every value at a session boundary.
#[test]
fn agent_cost_projection_records_snapshots_and_clears() {
    let projection = AgentCostProjection::default();
    let agent_id = tau_proto::AgentId::parse("agent-a").expect("valid agent id");
    let cost = EstimatedApiCost::from_picodollars(2_140_000_000_000);
    projection.record(agent_id.clone(), cost);
    assert_eq!(projection.snapshot().get(&agent_id), Some(&cost));
    projection.clear();
    assert!(projection.snapshot().is_empty());
}

fn dollars(value: u64) -> EstimatedApiCost {
    EstimatedApiCost::from_picodollars(value.saturating_mul(PICODOLLARS_PER_DOLLAR))
}

/// Zero and sub-dollar estimates omit the leading zero while retaining
/// cents.
#[test]
fn compact_cost_formats_zero_and_cents() {
    assert_eq!(format_compact(EstimatedApiCost::default()), "$.00");
    assert_eq!(
        format_compact(EstimatedApiCost::from_picodollars(3 * PICODOLLARS_PER_CENT)),
        "$.03"
    );
}

/// Exact half-up boundaries promote instead of overflowing a precision
/// band.
#[test]
fn compact_cost_rounding_transitions_are_deterministic() {
    let just_below_one = EstimatedApiCost::from_picodollars(
        99 * PICODOLLARS_PER_CENT + PICODOLLARS_PER_CENT / 2 - 1,
    );
    let rounds_to_one =
        EstimatedApiCost::from_picodollars(99 * PICODOLLARS_PER_CENT + PICODOLLARS_PER_CENT / 2);
    assert_eq!(format_compact(just_below_one), "$.99");
    assert_eq!(format_compact(rounds_to_one), "$1.0");

    assert_eq!(
        format_compact(EstimatedApiCost::from_picodollars(
            99 * PICODOLLARS_PER_TENTH_DOLLAR + PICODOLLARS_PER_TENTH_DOLLAR / 2
        )),
        "$10"
    );
    assert_eq!(
        format_compact(EstimatedApiCost::from_picodollars(
            999 * PICODOLLARS_PER_DOLLAR + PICODOLLARS_PER_DOLLAR / 2
        )),
        "$1k"
    );

    for (cost, expected) in [
        (
            EstimatedApiCost::from_picodollars(
                99 * PICODOLLARS_PER_THOUSAND_DOLLARS + PICODOLLARS_PER_THOUSAND_DOLLARS / 2 - 1,
            ),
            "$99k",
        ),
        (
            EstimatedApiCost::from_picodollars(
                99 * PICODOLLARS_PER_THOUSAND_DOLLARS + PICODOLLARS_PER_THOUSAND_DOLLARS / 2,
            ),
            "$.1m",
        ),
        (
            EstimatedApiCost::from_picodollars(
                9 * PICODOLLARS_PER_TENTH_MILLION_DOLLARS
                    + PICODOLLARS_PER_TENTH_MILLION_DOLLARS / 2
                    - 1,
            ),
            "$.9m",
        ),
        (
            EstimatedApiCost::from_picodollars(
                9 * PICODOLLARS_PER_TENTH_MILLION_DOLLARS
                    + PICODOLLARS_PER_TENTH_MILLION_DOLLARS / 2,
            ),
            "$1m",
        ),
    ] {
        let rendered = format_compact(cost);
        assert_eq!(rendered, expected);
        assert!(rendered.chars().count() <= 4);
    }
}

/// Large estimates retain a scale suffix without exceeding the strict
/// width.
#[test]
fn compact_cost_formats_large_values_within_four_columns() {
    for (cost, expected) in [
        (dollars(23), "$23"),
        (dollars(12_400), "$12k"),
        (dollars(120_000), "$.1m"),
        (dollars(2_400_000), "$2m"),
        (EstimatedApiCost::from_picodollars(u64::MAX), "$18m"),
    ] {
        let rendered = format_compact(cost);
        assert_eq!(rendered, expected);
        assert!(rendered.chars().count() <= 4);
    }
}

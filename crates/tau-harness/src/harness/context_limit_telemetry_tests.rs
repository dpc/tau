use tau_proto::ContextLimitObservation;

use super::context_limit_telemetry::context_limit_observation;

/// Below-limit agreement exposes hidden overhead or provider drift.
#[test]
fn rejection_below_advertised_limit_is_visible() {
    assert_eq!(
        context_limit_observation(Some(127_000), Some(126_000), Some(128_000)),
        ContextLimitObservation::RejectedBelowAdvertisedLimit
    );
}

/// Missing, zero, or contradictory evidence must not manufacture capacity.
#[test]
fn invalid_or_contradictory_evidence_is_insufficient() {
    for observation in [
        context_limit_observation(Some(127_000), None, None),
        context_limit_observation(Some(0), Some(127_000), Some(128_000)),
        context_limit_observation(Some(129_000), Some(127_000), Some(128_000)),
    ] {
        assert_eq!(observation, ContextLimitObservation::InsufficientEvidence);
    }
}

/// Agreement at or above the advertised window is classified distinctly from
/// hidden-overhead drift.
#[test]
fn rejection_at_or_above_limit_is_visible() {
    assert_eq!(
        context_limit_observation(Some(130_000), Some(129_000), Some(128_000)),
        ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit
    );
}

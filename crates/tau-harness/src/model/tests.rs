use super::*;

/// Context percentages must use the mathematical ratio at `u64` boundaries
/// instead of reporting a saturated multiplication as near-zero usage.
#[test]
fn context_percent_used_widens_before_scaling() {
    assert_eq!(
        context_percent_used(tau_proto::TokenCount::MAX, tau_proto::TokenCount::MAX),
        100
    );
    assert_eq!(
        context_percent_used(
            tau_proto::TokenCount::MAX,
            tau_proto::TokenCount::new(u64::MAX / 2),
        ),
        100
    );
    assert_eq!(
        context_percent_used(
            tau_proto::TokenCount::new(u64::MAX / 2),
            tau_proto::TokenCount::MAX,
        ),
        49
    );
}

/// Context percentage projection must preserve zero-window handling, integer
/// truncation, exact fullness, and over-capacity clamping with typed counts.
#[test]
fn context_percent_used_preserves_projection_boundaries() {
    let count = tau_proto::TokenCount::new;
    assert_eq!(
        context_percent_used(count(1), tau_proto::TokenCount::ZERO),
        0
    );
    assert_eq!(context_percent_used(count(999), count(1_000)), 99);
    assert_eq!(context_percent_used(count(1_000), count(1_000)), 100);
    assert_eq!(context_percent_used(count(1_001), count(1_000)), 100);
}

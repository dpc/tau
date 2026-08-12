use super::*;

/// Context percentages must use the mathematical ratio at `u64` boundaries
/// instead of reporting a saturated multiplication as near-zero usage.
#[test]
fn context_percent_used_widens_before_scaling() {
    assert_eq!(context_percent_used(u64::MAX, u64::MAX), 100);
    assert_eq!(context_percent_used(u64::MAX, u64::MAX / 2), 100);
    assert_eq!(context_percent_used(u64::MAX / 2, u64::MAX), 49);
}

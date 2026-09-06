use super::*;

/// Relative remaining time advances through the inclusive fresh boundary but
/// becomes unavailable once the existing 15-minute trust window is exceeded.
#[test]
fn relative_quota_remaining_respects_freshness_boundary() {
    let mut window = quota_window(100_000, 100_000);
    assert_eq!(
        current_relative_remaining_seconds(&window, Some(1_000_000)),
        Some(99_100)
    );
    window.timing_anchor_observed_at_unix_ms = Some(tau_proto::UnixMillis::new(99_000));
    assert_eq!(
        current_relative_remaining_seconds(&window, Some(1_000_000)),
        None
    );
}

/// Future usage and timing observations remain unavailable instead of
/// manufacturing a zero age or increasing provider-declared remaining time.
#[test]
fn future_quota_observations_are_unavailable() {
    assert_eq!(elapsed_seconds(1_001, Some(1_000)), None);
    let window = quota_window(10_000, 2_000);
    assert_eq!(
        current_relative_remaining_seconds(&window, Some(1_000)),
        None
    );
}

/// Build one provider-neutral quota window with a chosen remaining value and
/// timing anchor.
fn quota_window(remaining_seconds: i64, anchor_unix_ms: u64) -> tau_proto::ProviderQuotaWindow {
    tau_proto::ProviderQuotaWindow {
        key: tau_proto::ProviderQuotaWindowKey {
            limit_id: tau_proto::ProviderQuotaLimitId::parse("pool").expect("pool"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("window").expect("window"),
        },
        used_basis_points: 1_000,
        usage_observed_at_unix_ms: tau_proto::UnixMillis::new(anchor_unix_ms),
        window_seconds: tau_proto::QuotaWindowSeconds::new(604_800),
        reset_at_unix_seconds: None,
        remaining_seconds_at_timing_anchor: Some(tau_proto::SignedSeconds::new(remaining_seconds)),
        timing_anchor_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(anchor_unix_ms)),
        server_offset_ms: None,
        server_offset_observed_at_unix_ms: None,
    }
}

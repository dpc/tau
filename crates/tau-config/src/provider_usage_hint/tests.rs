use tau_proto::{
    ProviderQuotaLimitId, ProviderQuotaWindowId, ProviderQuotaWindowKey, QuotaWindowSeconds,
    SignedSeconds, UnixMillis, UnixSeconds,
};

use super::*;

/// Constructs a validated configured provider namespace for these fixtures.
fn provider(name: &str) -> ProviderName {
    ProviderName::new(name)
}

/// Builds a normalized account constraint without route applicability data.
fn window(used: u16, duration: u64) -> ProviderQuotaWindow {
    ProviderQuotaWindow {
        key: ProviderQuotaWindowKey {
            limit_id: ProviderQuotaLimitId::parse("codex").expect("valid pool"),
            window_id: ProviderQuotaWindowId::parse("primary").expect("valid window"),
        },
        used_basis_points: used,
        usage_observed_at_unix_ms: UnixMillis::new(1_000_000),
        window_seconds: QuotaWindowSeconds::new(duration),
        reset_at_unix_seconds: None,
        remaining_seconds_at_timing_anchor: None,
        timing_anchor_observed_at_unix_ms: None,
        server_offset_ms: None,
        server_offset_observed_at_unix_ms: None,
    }
}

/// Expiring a constraining short window cannot expose a misleading weekly
/// score.
#[test]
fn bottleneck_expiry_and_stale_percentages() {
    let hint = ProviderUsageHint::from_windows(
        provider("a"),
        &[window(9000, 18_000), window(1000, 604_800)],
    )
    .expect("nonempty windows");
    assert_eq!(hint.remaining(999_999), None);
    assert_eq!(hint.remaining(1_000_000 + 3_600_000), Some(1000));
    assert_eq!(hint.remaining(19_000_000), None);
    assert!(ProviderUsageHint::from_windows(provider("a"), &[]).is_none());
}

/// Relative and absolute clocks bound observations without fabricating
/// rollover.
#[test]
fn reset_boundaries_and_invalid_timing_are_unknown() {
    let mut window = window(3000, 18_000);
    window.reset_at_unix_seconds = Some(UnixSeconds::new(1010));
    let hint =
        ProviderUsageHint::from_windows(provider("a"), &[window.clone()]).expect("nonempty window");
    assert_eq!(hint.remaining(1_009_999), Some(7000));
    assert_eq!(hint.remaining(1_010_000), None);
    window.remaining_seconds_at_timing_anchor = Some(SignedSeconds::new(5));
    window.timing_anchor_observed_at_unix_ms = Some(UnixMillis::new(1_001_000));
    let hint =
        ProviderUsageHint::from_windows(provider("a"), &[window.clone()]).expect("nonempty window");
    assert_eq!(hint.remaining(1_000_000), None);
    assert_eq!(hint.remaining(1_005_999), Some(7000));
    assert_eq!(hint.remaining(1_006_000), None);
    window.remaining_seconds_at_timing_anchor = Some(SignedSeconds::new(-1));
    assert_eq!(
        ProviderUsageHint::from_windows(provider("a"), &[window])
            .expect("nonempty invalid window")
            .remaining(1_001_000),
        None
    );
}

/// Server clock skew must shift the local reset in the opposite direction;
/// either offset sign still expires exactly at the calibrated local boundary.
#[test]
fn calibrated_server_offsets_preserve_exact_expiry() {
    for (offset, expiry_ms) in [(2000, 1_008_000), (-2000, 1_012_000)] {
        let mut window = window(3000, 18_000);
        window.reset_at_unix_seconds = Some(UnixSeconds::new(1010));
        window.server_offset_ms = Some(tau_proto::ServerOffsetMillis::new(offset));
        window.server_offset_observed_at_unix_ms = Some(UnixMillis::new(1_000_000));
        let hint = ProviderUsageHint::from_windows(provider("a"), &[window])
            .expect("nonempty calibrated window");
        assert_eq!(hint.remaining(expiry_ms - 1), Some(7000));
        assert_eq!(hint.remaining(expiry_ms), None);
    }
}

/// Per-provider atomic replacement survives independent writes and rejects bad
/// files.
#[test]
fn sidecar_roundtrip_and_corruption_are_best_effort() {
    let temp = tempfile::tempdir().expect("temp dir");
    assert!(ProviderUsageHint::read(temp.path(), &provider("a")).is_none());
    for used in [100, 200] {
        for name in ["a", "b"] {
            let hint = ProviderUsageHint::from_windows(provider(name), &[window(used, 18_000)])
                .expect("nonempty window");
            hint.write(temp.path()).expect("write sidecar");
            assert_eq!(
                ProviderUsageHint::read(temp.path(), &provider(name))
                    .expect("read saved sidecar")
                    .remaining(1_000_000),
                Some(10_000 - used)
            );
        }
    }
    let path = hint_path(temp.path(), &provider("a"));
    for content in [
        b"{".as_slice(),
        br#"{"version":99,"provider":"a","windows":[]}"#,
        br#"{"version":1,"provider":"b","windows":[]}"#,
    ] {
        std::fs::write(&path, content).expect("corrupt sidecar");
        assert!(ProviderUsageHint::read(temp.path(), &provider("a")).is_none());
    }
    std::fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).expect("oversized sidecar");
    assert!(ProviderUsageHint::read(temp.path(), &provider("a")).is_none());
    assert_eq!(
        ProviderUsageHint::read(temp.path(), &provider("b"))
            .expect("independent sidecar remains")
            .remaining(1_000_000),
        Some(9800)
    );
    let blocked = temp.path().join("file");
    std::fs::write(&blocked, b"not a directory").expect("block sidecar directory");
    assert!(
        ProviderUsageHint::from_windows(provider("a"), &[window(0, 1)])
            .expect("nonempty window")
            .write(&blocked)
            .is_err()
    );
}

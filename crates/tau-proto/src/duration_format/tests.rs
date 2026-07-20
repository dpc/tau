use super::*;

/// Reproduces the reported weekly usage-window delay as a readable multi-day
/// approximation instead of an opaque six-digit second count.
#[test]
fn formats_observed_provider_usage_window_delay() {
    assert_eq!(format_approximate_duration_secs(419_322), "4d 20h");
}

/// Protects second, minute, hour, day, rounding, and integer-limit boundaries
/// from unit conversion or overflow regressions.
#[test]
fn formats_duration_unit_boundaries() {
    for (seconds, expected) in [
        (0, "0s"),
        (59, "59s"),
        (60, "1m"),
        (61, "1m 1s"),
        (3_599, "59m 59s"),
        (3_600, "1h"),
        (3_630, "1h 1m"),
        (86_369, "23h 59m"),
        (86_370, "1d"),
        (86_400, "1d"),
        (u64::MAX, "213503982334601d 7h"),
    ] {
        assert_eq!(
            format_approximate_duration_secs(seconds),
            expected,
            "{seconds}"
        );
    }
}

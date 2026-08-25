use super::*;

/// Ensures omitted strict configuration uses the approved ten-per-hour
/// defaults rather than an unlimited or zero-value quota.
#[test]
fn default_is_ten_events_per_hour() {
    let limit = PostRateLimit::default();
    assert_eq!(limit.max_events.get(), 10);
    assert_eq!(limit.window_seconds.get(), 3_600);
}

/// Ensures a present configuration object fills each omitted field from the
/// approved defaults while still rejecting zero and unknown values.
#[test]
fn config_object_is_partial_strict_and_positive() {
    let empty: PostRateLimit = serde_json::from_value(serde_json::json!({})).expect("empty object");
    assert_eq!(empty.max_events.get(), 10);
    assert_eq!(empty.window_seconds.get(), 3_600);
    let only_max: PostRateLimit =
        serde_json::from_value(serde_json::json!({"max_events":2})).expect("partial maximum");
    assert_eq!(only_max.max_events.get(), 2);
    assert_eq!(only_max.window_seconds.get(), 3_600);
    let only_window: PostRateLimit =
        serde_json::from_value(serde_json::json!({"window_seconds":2})).expect("partial window");
    assert_eq!(only_window.max_events.get(), 10);
    assert_eq!(only_window.window_seconds.get(), 2);
    for invalid in [
        serde_json::json!({"max_events":0}),
        serde_json::json!({"window_seconds":0}),
        serde_json::json!({"extra":true}),
    ] {
        assert!(serde_json::from_value::<PostRateLimit>(invalid).is_err());
    }
}

/// Ensures expiry reopens the rolling window exactly at its boundary.
#[test]
fn expired_admission_reopens_the_window() {
    let limit: PostRateLimit = serde_json::from_value(serde_json::json!({
        "max_events": 1,
        "window_seconds": 10,
    }))
    .expect("test limit");
    let start = Instant::now();
    let mut window = PostRateLimitWindow::default();
    window.reserve_at(limit, start).expect("first reservation");
    assert!(
        window
            .reserve_at(limit, start + Duration::from_secs(9))
            .is_err()
    );
    assert!(
        window
            .reserve_at(limit, start + Duration::from_secs(10))
            .is_ok()
    );
}

/// Ensures an overfull quota derives retry from the max-events-th newest
/// timestamp, so configuration reductions do not advertise an early retry.
#[test]
fn overfull_quota_uses_the_quota_filling_threshold_for_retry() {
    let limit: PostRateLimit = serde_json::from_value(serde_json::json!({
        "max_events": 2,
        "window_seconds": 100,
    }))
    .expect("test limit");
    let start = Instant::now();
    let mut window = PostRateLimitWindow {
        admitted_at: VecDeque::from([
            start,
            start + Duration::from_secs(10),
            start + Duration::from_secs(20),
        ]),
    };
    assert_eq!(
        window
            .reserve_at(limit, start + Duration::from_secs(30))
            .expect_err("three retained attempts exceed two")
            .retry_after_seconds,
        80
    );
}

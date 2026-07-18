use super::*;

/// Text exactly at a field cap remains unchanged, while one extra character is
/// replaced by an ellipsis without exceeding that same cap.
#[test]
fn bounded_oauth_text_reserves_truncation_marker_inside_cap() {
    for cap in [MAX_OAUTH_ERROR_CODE_CHARS, MAX_OAUTH_ERROR_MESSAGE_CHARS] {
        let exact = "x".repeat(cap);
        assert_eq!(
            bounded_oauth_text(&exact, cap).as_deref(),
            Some(exact.as_str())
        );

        let oversized = "x".repeat(cap + 1);
        let bounded = bounded_oauth_text(&oversized, cap).expect("bounded text");
        assert_eq!(bounded.chars().count(), cap);
        assert!(bounded.ends_with('…'));
    }
}

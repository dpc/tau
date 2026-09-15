use super::*;

/// Diagnostic overflow must keep a bounded exact prefix and the current
/// rejection site, rather than failing provider work or pretending the prefix
/// is complete.
#[test]
fn overflow_preserves_latest_event_and_marks_gap() {
    let mut capture = ResponseCapture::selected(true).expect("selected");
    let prefix = "x".repeat(PREFIX_BYTES);
    capture.record(&prefix);
    capture.record("discarded middle");
    capture.record("{ malformed final event");
    assert_eq!(capture.raw_events, [prefix]);
    assert!(capture.truncated);
    assert_eq!(capture.received_events, 3);
    assert_eq!(
        capture.last_received_event.as_deref(),
        Some("{ malformed final event")
    );
    assert!(!capture.last_event_truncated);
}

/// Tiny events and unexpectedly oversized text cannot evade capture memory
/// bounds.
#[test]
fn event_count_and_last_event_are_bounded() {
    let mut capture = ResponseCapture::selected(true).expect("selected");
    for _ in 0..=PREFIX_EVENTS {
        capture.record("");
    }
    assert_eq!(capture.raw_events.len(), PREFIX_EVENTS);
    assert!(capture.truncated);
    capture.record(&"é".repeat(LAST_EVENT_BYTES));
    assert_eq!(
        capture.last_received_event.as_ref().expect("last").len(),
        LAST_EVENT_BYTES
    );
    assert!(capture.last_event_truncated);
}

/// Internal error formatting remains bounded and UTF-8 valid, even when
/// provider prose is much larger than the diagnostic allowance.
#[test]
fn internal_error_detail_is_bounded() {
    let mut detail = ErrorDetail::default();
    let error = LlmError::InvalidResponse("é".repeat(64 * 1024));
    assert!(std::fmt::write(&mut detail, format_args!("{error:?}")).is_err());
    assert!(detail.truncated);
    assert!(detail.text.len() <= ERROR_DETAIL_BYTES);
}

/// Disabled exact capture must allocate no accumulator.
#[test]
fn disabled_capture_is_absent() {
    assert!(ResponseCapture::selected(false).is_none());
}

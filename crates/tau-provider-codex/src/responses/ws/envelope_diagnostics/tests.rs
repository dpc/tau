use super::*;

/// Application receipt and consumption must remain distinct, and lifecycle
/// events must not invent semantic progress.
#[test]
fn application_and_semantic_observations_keep_distinct_clocks() {
    let mut diagnostics = EnvelopeDiagnostics::new("test", None, Duration::from_secs(900), None);
    let read_at = diagnostics.started_at + Duration::from_secs(1);
    let consumed_at = read_at + Duration::from_millis(75);
    diagnostics.record_frame(123, Some(read_at), consumed_at);
    diagnostics.record_disposition(ParsedEventDisposition::Recognized);
    assert_eq!(diagnostics.first_frame_read_at, Some(read_at));
    assert_eq!(diagnostics.last_frame_at, Some(consumed_at));
    assert_eq!(
        diagnostics.max_read_to_consume,
        Some(Duration::from_millis(75))
    );
    assert!(diagnostics.first_semantic_at.is_none());
    diagnostics.record_disposition(ParsedEventDisposition::Semantic);
    assert!(diagnostics.first_semantic_at.is_some());
    assert_eq!(diagnostics.semantic_event_count, 1);
    diagnostics.record_frame(456, None, consumed_at + Duration::from_secs(1));
    assert_eq!(diagnostics.first_frame_at, Some(consumed_at));
    assert_eq!(diagnostics.last_frame_read_at, Some(read_at));
    assert_eq!(diagnostics.frame_count, 2);
    assert_eq!(diagnostics.frame_bytes, 579);
}

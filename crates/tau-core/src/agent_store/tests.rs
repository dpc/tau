use super::*;

/// Ensures the writer rejects a record length that the matching loader would
/// reject, before opening or mutating the journal.
#[test]
fn write_record_limit_matches_read_limit() {
    let error = validate_record_length(Path::new("/not/opened/events.cbor"), MAX_RECORD_BYTES + 1)
        .expect_err("oversized record must be rejected");
    assert!(matches!(
        error,
        AgentStoreError::RecordTooLarge {
            record_length,
            maximum: MAX_RECORD_BYTES,
            ..
        } if record_length == MAX_RECORD_BYTES + 1
    ));
}

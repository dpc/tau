use std::path::Path;

use super::*;

/// Ensures the session reader and managed writer share the exact record bound.
#[test]
fn session_record_limit_accepts_exact_boundary() {
    validate_record_length(Path::new("/not/opened/events.cbor"), MAX_RECORD_BYTES)
        .expect("exact boundary must be accepted");
}

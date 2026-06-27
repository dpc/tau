use super::*;

/// Covers the fixture's opt-in gate so `TAU_VCR=off` or an empty value cannot
/// accidentally run a live provider test outside cassette mode.
#[test]
fn vcr_enabled_requires_active_vcr_mode() {
    assert!(!vcr_enabled(None, false).expect("unset VCR mode is valid"));
    assert!(!vcr_enabled(Some(""), false).expect("empty VCR mode is off"));
    assert!(!vcr_enabled(Some("off"), false).expect("explicit off VCR mode is valid"));
    assert!(vcr_enabled(Some("record-if-missing"), false).is_err());
    assert!(vcr_enabled(Some("record-if-missing"), true).expect("record mode is active"));
    assert!(vcr_enabled(Some("replay-only"), true).expect("replay mode is active"));
    assert!(vcr_enabled(Some("bad-mode"), true).is_err());
}

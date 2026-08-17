use super::*;

/// The bounded writer accepts exactly the configured ceiling and rejects the
/// first byte beyond it without retaining an oversized buffer.
#[test]
fn capped_serialization_accepts_cap_and_rejects_cap_plus_one() {
    let exact = "x".repeat(MAX_CAPTURE_BYTES - 2);
    assert_eq!(
        serialize_capped(&exact, MAX_CAPTURE_BYTES)
            .expect("quoted string fits exactly")
            .len(),
        MAX_CAPTURE_BYTES
    );
    let oversized = "x".repeat(MAX_CAPTURE_BYTES - 1);
    assert!(serialize_capped(&oversized, MAX_CAPTURE_BYTES).is_err());
}

/// Credential projection covers values and keys, removes opaque embedded image
/// URLs, and fails closed when projected keys collide.
#[test]
fn sanitization_is_recursive_and_collision_safe() {
    let mut value = serde_json::json!({
        "secret-key": "prefix secret suffix",
        "opaque": "provider body DATA:IMAGE/png;base64,canary",
        "nested": {"secret": "first", "[REDACTED]": "second"},
    });
    sanitize_capture_value(&mut value, "secret");
    assert_eq!(value["[REDACTED]-key"], "prefix [REDACTED] suffix");
    assert_eq!(value["opaque"], IMAGE_OMISSION);
    assert_eq!(
        value["nested"],
        serde_json::json!({"capture_omitted": "projected_key_collision"})
    );
}

/// Disabled capture must not clone or account raw provider events even when the
/// parser continues to process a successful response stream.
#[test]
fn disabled_capture_retains_no_raw_events() {
    let capture = DebugCapture::new(false);
    capture.record_event(
        &serde_json::json!({"type": "response.completed"}),
        r#"{"type":"response.completed"}"#,
    );
    let events = capture.events.lock().expect("event state");
    assert!(events.values.is_empty());
    assert_eq!(events.bytes, 0);
    assert!(!events.truncated);
}

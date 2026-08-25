use reqwest::header::{HeaderMap, HeaderValue};

use super::*;

/// Post-upgrade errors must retain bounded codes from every documented
/// envelope location so callers can distinguish retryable transport/service
/// failures from terminal request and context failures.
#[test]
fn terminal_error_extracts_status_and_nested_codes() {
    for (event, expected_status, expected_code) in [
        (
            serde_json::json!({
                "type": "error",
                "status": 401,
                "error": {"code": "invalid_api_key"}
            }),
            Some(401),
            "invalid_api_key",
        ),
        (
            serde_json::json!({
                "type": "response.failed",
                "response": {
                    "error": {"type": "server_error"},
                    "status": 500
                }
            }),
            Some(500),
            "server_error",
        ),
        (
            serde_json::json!({
                "type": "response.incomplete",
                "response": {
                    "error": {"code": "context_length_exceeded"}
                }
            }),
            None,
            "context_length_exceeded",
        ),
    ] {
        let Some(Error::Provider { status, code }) = provider_terminal_error(&event) else {
            panic!("terminal provider event");
        };
        assert_eq!(status, expected_status);
        assert_eq!(code.as_deref(), Some(expected_code));
    }
    let oversized = "x".repeat(256);
    let event = serde_json::json!({"type": "error", "code": oversized});
    let Some(Error::Provider {
        code: Some(code), ..
    }) = provider_terminal_error(&event)
    else {
        panic!("bounded provider code");
    };
    assert_eq!(code.len(), 128);
}

/// A server may not negotiate extensions or subprotocols the client did not
/// offer; accepting either would bypass tungstenite's normal handshake
/// validation because Tau performs the HTTP upgrade through reqwest.
#[test]
fn handshake_rejects_unsolicited_negotiation() {
    let key = HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ==");
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let mut headers = HeaderMap::new();
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    headers.insert(
        "sec-websocket-accept",
        HeaderValue::from_str(&accept).expect("accept header"),
    );
    assert!(websocket_headers_valid(&headers, &key));
    for name in ["sec-websocket-extensions", "sec-websocket-protocol"] {
        headers.insert(name, HeaderValue::from_static("unsolicited"));
        assert!(!websocket_headers_valid(&headers, &key));
        headers.remove(name);
    }
}

/// Per-event and cumulative bounds reject both one oversized frame and an
/// otherwise valid event that crosses the complete-response byte budget.
#[test]
fn response_byte_bounds_cover_frames_and_cumulative_streams() {
    assert!(checked_response_bytes(0, MAX_EVENT_BYTES + 1).is_none());
    assert!(checked_response_bytes(MAX_RESPONSE_BYTES, 1).is_none());
    assert_eq!(
        checked_response_bytes(MAX_RESPONSE_BYTES - 1, 1),
        Some(MAX_RESPONSE_BYTES)
    );
}

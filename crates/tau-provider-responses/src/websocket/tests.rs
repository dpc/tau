use reqwest::header::{HeaderMap, HeaderValue};
use tokio::runtime::Builder;
use tungstenite::protocol::frame::Frame;
use tungstenite::protocol::frame::coding::{Data, OpCode};

use super::*;
use crate::decoded_event::{reset_test_counts, test_counts};

/// Borrowed WebSocket lowering must preserve the exact bytes produced by the
/// former typed-body-to-`Value` transformation, including member order,
/// Unicode, cache controls, tools, and omission of the SSE-only stream flag.
#[test]
fn borrowed_request_envelope_matches_value_reference_bytes() {
    let mut body = RequestBody {
        model: "模型-🦀".to_owned(),
        input: vec![
            super::super::ResponsesInputItem::Json(serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "héllo"}],
            })),
            super::super::ResponsesInputItem::Raw(
                serde_json::value::RawValue::from_string(
                    r#"{"type":"reasoning","summary":[]}"#.to_owned(),
                )
                .expect("raw fixture"),
            ),
        ],
        stream: true,
        reasoning: super::super::Reasoning { effort: "high" },
        instructions: Some("system".to_owned()),
        prompt_cache_key: Some("tau:agent".to_owned()),
        prompt_cache_retention: Some("24h"),
        prompt_cache_options: Some(super::super::PromptCacheOptions {
            mode: "explicit",
            ttl: "30m",
        }),
        max_output_tokens: Some(4096),
        tools: vec![serde_json::json!({"type": "function", "name": "run"})],
        tool_choice: Some("auto".to_owned()),
    };
    let assert_matches_reference = |body: &RequestBody| {
        let mut reference = serde_json::to_value(body).expect("reference body");
        let object = reference.as_object_mut().expect("request object");
        object.remove("stream");
        object.insert(
            "type".to_owned(),
            Value::String("response.create".to_owned()),
        );
        assert_eq!(
            serde_json::to_vec(&WebSocketRequestBody::try_from(body).expect("borrowed envelope"),)
                .expect("borrowed envelope JSON"),
            serde_json::to_vec(&reference).expect("reference envelope"),
        );
    };

    assert_matches_reference(&body);
    body.tool_choice = None;
    assert_matches_reference(&body);
    body.tools.clear();
    assert_matches_reference(&body);
    body.max_output_tokens = None;
    assert_matches_reference(&body);
    body.prompt_cache_options = None;
    assert_matches_reference(&body);
    body.prompt_cache_retention = None;
    assert_matches_reference(&body);
    body.prompt_cache_key = None;
    assert_matches_reference(&body);
    body.instructions = None;
    assert_matches_reference(&body);
}

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

/// Provider terminal classification must precede strict sidecar indexing, as
/// it did before transports shared one semantic decode.
#[test]
fn terminal_error_with_duplicate_sidecars_keeps_provider_classification() {
    let raw = r#"{"type":"error","status":429,"code":"rate_limit","item":1,"item":2}"#;
    let decoded = decode_websocket_event(raw).expect("semantic provider error");
    let DecodedWebSocketEvent::ProviderError { error, .. } = decoded else {
        panic!("provider error must bypass sidecar indexing");
    };
    assert!(matches!(
        error,
        Error::Provider {
            status: Some(429),
            code: Some(ref code)
        } if code == "rate_limit"
    ));
}

/// The WebSocket production pre-parser shares one semantic decode with strict
/// sidecar indexing for ordinary assembler-bound events.
#[test]
fn websocket_preparser_decodes_event_once() {
    reset_test_counts();
    assert!(matches!(
        decode_websocket_event(r#"{"type":"response.heartbeat"}"#),
        Ok(DecodedWebSocketEvent::Apply(_))
    ));
    assert_eq!(test_counts(), (1, 1));
}

/// The WebSocket pre-parser must pass only the exact max-output incomplete
/// terminal to the shared assembler; unknown incomplete reasons remain errors.
#[test]
fn max_output_incomplete_bypasses_websocket_error_classification_only() {
    let max_output = serde_json::json!({
        "type": "response.incomplete",
        "incomplete_details": {"reason": "content_filter"},
        "response": {
            "incomplete_details": {"reason": "max_output_tokens"}
        }
    });
    assert!(provider_terminal_error(&max_output).is_none());

    let unknown = serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "incomplete_details": {"reason": "content_filter"}
        }
    });
    assert!(matches!(
        provider_terminal_error(&unknown),
        Some(Error::Provider { .. })
    ));

    let top_level_only = serde_json::json!({
        "type": "response.incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "response": {
            "incomplete_details": {"reason": "content_filter"}
        }
    });
    assert!(matches!(
        provider_terminal_error(&top_level_only),
        Some(Error::Provider { .. })
    ));
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

/// The socket construction seam must retain the existing inclusive frame and
/// fragmented-message limits rather than tungstenite's larger defaults.
#[test]
fn transport_config_uses_existing_event_limit_for_frames_and_messages() {
    let config = websocket_config();

    assert_eq!(config.max_frame_size, Some(MAX_EVENT_BYTES));
    assert_eq!(config.max_message_size, Some(MAX_EVENT_BYTES));
}

/// The production socket seam must reject a declared limit-plus-one frame with
/// tungstenite's capacity error before an application message can exist.
#[test]
fn configured_transport_rejects_oversized_frame_before_message_delivery() {
    assert_message_too_long(transport_error(vec![Message::Text(
        "x".repeat(MAX_EVENT_BYTES + 1).into(),
    )]));
}

/// The production socket seam must reject permitted fragments once their
/// aggregate crosses the limit, before delivering assembled application text.
#[test]
fn configured_transport_rejects_oversized_fragmented_aggregate_before_delivery() {
    let split = MAX_EVENT_BYTES / 2;
    let messages = vec![
        Message::Frame(Frame::message(
            vec![b'x'; split],
            OpCode::Data(Data::Text),
            false,
        )),
        Message::Frame(Frame::message(
            vec![b'x'; MAX_EVENT_BYTES + 1 - split],
            OpCode::Data(Data::Continue),
            true,
        )),
    ];

    assert_message_too_long(transport_error(messages));
}

fn transport_error(messages: Vec<Message>) -> tungstenite::Error {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(async move {
            let (client_io, server_io) = tokio::io::duplex(2 * MAX_EVENT_BYTES);
            let mut client = configured_websocket_stream(client_io).await;
            let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
            let send = async move {
                for message in messages {
                    server.send(message).await.expect("send test frame");
                }
            };
            let receive = async move {
                match client.next().await {
                    Some(Err(error)) => error,
                    Some(Ok(message)) => panic!("unexpected application message: {message:?}"),
                    None => panic!("test transport closed without a capacity error"),
                }
            };
            let ((), error) = tokio::join!(send, receive);
            error
        })
}

fn assert_message_too_long(error: tungstenite::Error) {
    assert!(matches!(
        error,
        tungstenite::Error::Capacity(tungstenite::error::CapacityError::MessageTooLong {
            size,
            max_size: MAX_EVENT_BYTES,
        }) if size == MAX_EVENT_BYTES + 1
    ));
}

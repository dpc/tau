use serde_json::{Value, json};
use tau_provider::retry_policy::{RetryClass, RetryDecision};

use super::*;
use crate::SemanticProgress;

fn capture_record(
    evidence: &AttemptFailureEvidence,
    enabled: bool,
    dispatches: u64,
    repair_used: bool,
) -> Option<Value> {
    let session_id = tau_proto::SessionId::parse("session-failure").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-failure").expect("agent");
    let context = tau_proto::PromptContext::default();
    let originator = tau_proto::PromptOriginator::User;
    let request = crate::Prompt {
        system_prompt: "",
        context: &context,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &originator,
        share_user_cache_key: false,
        session_id: &session_id,
        agent_id: &agent_id,
        debug_provider_requests: enabled,
    };
    let mut correlation = AttemptCaptureCorrelation::new(LogicalAttempt::new(4));
    for _ in 0..dispatches {
        let _dispatch = correlation.next_dispatch();
    }
    if repair_used {
        correlation.mark_repair_used();
    }
    let decision = RetryDecision::new(RetryClass::Unknown);
    let mut submitted = None;
    submit_capture_with(
        CaptureInput {
            agent_prompt_id: "prompt-failure",
            request: &request,
            decision: &decision,
            progress: SemanticProgress::Parsed,
            correlation: correlation.snapshot(),
            response_bytes_received: 417,
            evidence: Some(evidence),
            access_token: "loaded-secret",
            account_id: Some("account-secret"),
        },
        |capture| submitted = Some(capture),
    );
    submitted.map(|capture| serde_json::from_slice(capture.json()).expect("capture JSON"))
}

/// Regression: live retry detail must scrub controls, loaded secrets, and
/// credential-shaped substrings before crossing the Codex boundary.
#[test]
fn live_detail_scrubs_controls_secrets_and_token_shapes() {
    let evidence = AttemptFailureEvidence::provider(&json!({
        "type": "error",
        "code": "new",
        "message": "line\nsecret \u{202e} BearerToken sk-abcdefghijklmnop"
    }));
    let detail = evidence.live_detail("secret", None).expect("safe detail");
    assert_eq!(detail.as_str(), "[redacted]");
    for message in [
        "error=SK-abcdefghijklmnop",
        "authorization:Bearer abcdefghijklmnop",
        "authorization:Bearer\tabcdefghijklmnop",
        "authorization:Bearer\nabcdefghijklmnop",
        "api-key=abcdefghijklmnop",
        "failed (aaaa.bbbbbbbbbbbbbbbbbbbb.cccc)",
        "failed (aaaa.bbbbbbbbbbbbbbbbbbbb.cccc.)",
    ] {
        let evidence = AttemptFailureEvidence::provider(&json!({
            "type": "error",
            "message": message,
        }));
        assert_eq!(
            evidence
                .live_detail("unrelated", None)
                .expect("redacted detail")
                .as_str(),
            "[redacted]"
        );
    }
}

/// A secret that crosses the retained-prefix cap must not leave a safe-looking
/// credential prefix in live status.
#[test]
fn live_detail_redacts_provider_text_truncated_before_secret_scrubbing() {
    let access_token = "loaded-secret-crossing-the-cap";
    let message = format!("{}{}", "x".repeat(250), access_token);
    let evidence = AttemptFailureEvidence::provider(&json!({
        "type": "error",
        "message": message,
    }));

    assert_eq!(
        evidence
            .live_detail(access_token, None)
            .expect("redacted detail")
            .as_str(),
        "[redacted]"
    );
}

/// Overlapping loaded secrets must consume the longest match regardless of
/// which credential field owns the longer value.
#[test]
fn live_detail_redacts_longest_overlapping_loaded_secret() {
    let evidence = AttemptFailureEvidence::provider(&json!({
        "type": "error",
        "message": "id=secret-account",
    }));
    for (access_token, account_id) in [
        ("secret", Some("secret-account")),
        ("secret-account", Some("secret")),
    ] {
        assert_eq!(
            evidence
                .live_detail(access_token, account_id)
                .expect("redacted detail")
                .as_str(),
            "id=[redacted]"
        );
    }
}

/// Capture-disabled parsing must retain only bounded live evidence and skip the
/// persistent structural projection and exact full-message length walk.
#[test]
fn disabled_capture_stops_provider_projection_at_live_bound() {
    let evidence = AttemptFailureEvidence::provider_with_mode(
        &json!({
            "type": "error",
            "message": "x".repeat(65_536),
            "error": {"unknown": vec!["x"; 256]},
        }),
        ProviderEvidenceMode::LiveOnly,
    );
    let AttemptFailureEvidence::Provider(provider) = evidence else {
        panic!("provider evidence");
    };
    let message = provider.message.expect("bounded live message");
    assert_eq!(message.live_prefix.len(), 256);
    assert!(!message.live_prefix_complete);
    assert_eq!(message.inspected_unicode_scalars, 257);
    assert_eq!(message.inspected_utf8_bytes, 256);
    assert_eq!(provider.shape, None);
}

/// Regression: structural captures must replace unknown keys, sensitive
/// subtrees, and all provider-controlled scalar values.
#[test]
fn shape_replaces_values_unknown_keys_and_sensitive_subtrees() {
    let evidence = AttemptFailureEvidence::provider(&json!({
        "type": "error",
        "unknown-secret-key": "value",
        "authorization": {"nested": "credential"},
        "error": {"code": "bad", "message": "secret prose"}
    }));
    let AttemptFailureEvidence::Provider(provider) = evidence else {
        panic!("provider evidence");
    };
    assert_eq!(
        provider.shape,
        Some(json!({
            "<redacted-key>": "redacted",
            "error": {"code": "string", "message": "string"},
            "type": "string",
            "<field-3>": "string"
        }))
    );
}

/// Multiple sensitive keys deterministically retain one canonical marker and
/// use positional collision fallback without exposing either key.
#[test]
fn sensitive_shape_key_collision_is_deterministic() {
    let evidence = AttemptFailureEvidence::provider(&json!({
        "authorization": "one",
        "api_key": "two",
    }));
    let AttemptFailureEvidence::Provider(provider) = evidence else {
        panic!("provider evidence");
    };
    assert_eq!(
        provider.shape,
        Some(json!({
            "<redacted-key>": "redacted",
            "<field-1>": "redacted",
        }))
    );
    assert!(provider.shape_truncated);
}

/// Regression: the schema must correlate one logical attempt's transparent
/// repair with its exact second request dispatch without retaining prose.
#[test]
fn capture_uses_exact_attempt_dispatch_and_observed_provider_projection() {
    let session_id = tau_proto::SessionId::parse("session-failure").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-failure").expect("agent");
    let context = tau_proto::PromptContext::default();
    let originator = tau_proto::PromptOriginator::User;
    let request = crate::Prompt {
        system_prompt: "",
        context: &context,
        tools: &[],
        params: tau_proto::ModelParams::default(),
        tool_choice: tau_proto::ToolChoice::default(),
        compaction: None,
        originator: &originator,
        share_user_cache_key: false,
        session_id: &session_id,
        agent_id: &agent_id,
        debug_provider_requests: true,
    };
    let evidence = AttemptFailureEvidence::provider(&json!({
        "type": "response.failed",
        "request_id": "req_good",
        "response": {
            "id": "resp_good",
            "error": {
                "code": "new_code",
                "message": "secret prose is measured, never persisted"
            }
        }
    }));
    let mut correlation = AttemptCaptureCorrelation::new(LogicalAttempt::new(4));
    let _first = correlation.next_dispatch();
    let _second = correlation.next_dispatch();
    correlation.mark_repair_used();
    let decision = RetryDecision::new(RetryClass::Unknown);
    let mut submitted = None;
    submit_capture_with(
        CaptureInput {
            agent_prompt_id: "prompt-failure",
            request: &request,
            decision: &decision,
            progress: SemanticProgress::Parsed,
            correlation: correlation.snapshot(),
            response_bytes_received: 417,
            evidence: Some(&evidence),
            access_token: "loaded-secret",
            account_id: Some("account-secret"),
        },
        |capture| submitted = Some(capture),
    );
    let capture = submitted.expect("one capture");
    assert_eq!(
        capture.class(),
        tau_provider::debug_capture_writer::ProviderDebugCaptureClass::ResponsesAttemptFailure
    );
    let record: Value = serde_json::from_slice(capture.json()).expect("capture JSON");
    assert_eq!(
        record,
        json!({
            "schema_version": 1,
            "capture_kind": "provider_attempt_failure",
            "session_id": "session-failure",
            "agent_prompt_id": "prompt-failure",
            "logical_attempt": 4,
            "wire_dispatch_index": 2,
            "backend": {
                "kind": "responses",
                "transport_intent": "websocket",
                "transport_established": true,
            },
            "outcome": "retry_scheduled",
            "classification": {
                "category": "unknown",
                "retry_after_secs": null,
            },
            "wire": {
                "wire_dispatches": 2,
                "repair_used": true,
                "response_bytes_received": 417,
                "semantic_progress": "parsed",
            },
            "provider": {
                "terminal_event_type": "response.failed",
                "canonical_error_code": "new_code",
                "provider_request_id": "req_good",
                "provider_response_id": "resp_good",
                "message": {
                    "present": true,
                    "utf8_bytes": 41,
                    "unicode_scalars": 41,
                },
                "terminal_event_shape": {
                    "type": "string",
                    "request_id": "string",
                    "response": {
                        "id": "string",
                        "error": {
                            "code": "string",
                            "message": "string",
                        },
                    },
                },
            },
            "transport": null,
            "truncation": {
                "total": false,
                "shape": false,
                "identifiers": false,
            },
        })
    );
    let text = String::from_utf8(capture.json().to_vec()).expect("utf8 JSON");
    assert!(!text.contains("secret prose"));
}

/// Regression: close captures preserve code/phase/length facts while omitting
/// the provider-controlled reason and respecting capture eligibility.
#[test]
fn close_capture_preserves_facts_and_omits_reason_text() {
    let evidence = AttemptFailureEvidence::transport(
        TransportPhase::ResponseStream,
        true,
        TransportFailureKind::WebSocketTermination(WsTermination::CloseFrame {
            code: Some(1011),
            reason: Some("loaded-secret keepalive detail".to_owned()),
        }),
    );
    let record = capture_record(&evidence, true, 2, true).expect("capture");
    assert_eq!(record["provider"], Value::Null);
    assert_eq!(
        record["transport"],
        json!({
            "phase": "response_stream",
            "kind": "websocket_close",
            "ws_close_code": 1011,
            "ws_close_reason": {
                "present": true,
                "utf8_bytes": 30,
                "unicode_scalars": 30,
            },
            "clean_eof": false,
            "frame_bytes": null,
        })
    );
    assert!(!record.to_string().contains("keepalive detail"));
    assert!(capture_record(&evidence, false, 2, true).is_none());
}

/// Regression: upgrade failures have no fabricated failing dispatch index;
/// repair-upgrade failures retain only the one request actually sent.
#[test]
fn pre_upgrade_capture_distinguishes_zero_dispatch_from_repair_upgrade() {
    let evidence =
        AttemptFailureEvidence::upgrade(Some("req_upgrade"), TransportFailureKind::Upgrade);
    let initial = capture_record(&evidence, true, 0, false).expect("initial upgrade capture");
    assert_eq!(initial["wire_dispatch_index"], Value::Null);
    assert_eq!(initial["wire"]["wire_dispatches"], 0);
    assert_eq!(initial["wire"]["repair_used"], false);
    assert_eq!(initial["provider"]["provider_request_id"], "req_upgrade");

    let repair = capture_record(&evidence, true, 1, true).expect("repair upgrade capture");
    assert_eq!(repair["wire_dispatch_index"], Value::Null);
    assert_eq!(repair["wire"]["wire_dispatches"], 1);
    assert_eq!(repair["wire"]["repair_used"], true);
}

/// Regression: every closed transport phase/kind must serialize a stable local
/// label without falling back to raw library text.
#[test]
fn transport_capture_labels_cover_closed_failure_matrix() {
    let cases = [
        (
            TransportPhase::Send,
            TransportFailureKind::Send,
            true,
            json!({"phase":"send","kind":"websocket_send","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":null}),
        ),
        (
            TransportPhase::ResponseStream,
            TransportFailureKind::Read,
            true,
            json!({"phase":"response_stream","kind":"websocket_read","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":null}),
        ),
        (
            TransportPhase::ResponseStream,
            TransportFailureKind::Keepalive,
            true,
            json!({"phase":"response_stream","kind":"websocket_keepalive","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":null}),
        ),
        (
            TransportPhase::ResponseStream,
            TransportFailureKind::IdleTimeout,
            true,
            json!({"phase":"response_stream","kind":"response_idle_timeout","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":null}),
        ),
        (
            TransportPhase::ResponseStream,
            TransportFailureKind::WebSocketTermination(WsTermination::CleanEof),
            true,
            json!({"phase":"response_stream","kind":"clean_eof","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":true,"frame_bytes":null}),
        ),
        (
            TransportPhase::ResponseStream,
            TransportFailureKind::Frame(FrameFailure::new(FrameFailureKind::MalformedText, 19)),
            true,
            json!({"phase":"response_stream","kind":"malformed_text","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":19}),
        ),
        (
            TransportPhase::ResponseStream,
            TransportFailureKind::Frame(FrameFailure::new(FrameFailureKind::Binary, 23)),
            true,
            json!({"phase":"response_stream","kind":"binary_frame","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":23}),
        ),
        (
            TransportPhase::PreUpgrade,
            TransportFailureKind::Upgrade,
            false,
            json!({"phase":"pre_upgrade","kind":"websocket_upgrade","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":null}),
        ),
        (
            TransportPhase::PreUpgrade,
            TransportFailureKind::Outbound,
            false,
            json!({"phase":"pre_upgrade","kind":"outbound","ws_close_code":null,
                "ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
                "clean_eof":false,"frame_bytes":null}),
        ),
    ];
    for (phase, kind, established, expected) in cases {
        let evidence = AttemptFailureEvidence::transport(phase, established, kind);
        let record = capture_record(&evidence, true, 1, false).expect("transport capture");
        assert_eq!(record["transport"], expected);
        assert_eq!(record["backend"]["transport_established"], established);
    }
}

/// Regression: array and nesting limits must truncate deterministically at the
/// approved boundaries rather than growing capture memory.
#[test]
fn shape_bounds_truncate_depth_and_container_entries() {
    let oversized = Value::Array((0..129).map(|_| Value::Null).collect());
    let AttemptFailureEvidence::Provider(provider) = AttemptFailureEvidence::provider(&oversized)
    else {
        panic!("provider evidence");
    };
    assert!(provider.shape_truncated);
    assert_eq!(
        provider
            .shape
            .as_ref()
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(128)
    );

    let mut exact = Value::Null;
    for _ in 0..16 {
        exact = json!([exact]);
    }
    let AttemptFailureEvidence::Provider(provider) = AttemptFailureEvidence::provider(&exact)
    else {
        panic!("provider evidence");
    };
    assert!(!provider.shape_truncated);

    let deep = json!([exact]);
    let AttemptFailureEvidence::Provider(provider) = AttemptFailureEvidence::provider(&deep) else {
        panic!("provider evidence");
    };
    assert!(provider.shape_truncated);
}

/// Object-entry and global-node limits accept the exact cap and truncate cap+1.
#[test]
fn shape_object_and_node_boundaries_are_exact() {
    for (entries, expected_len, truncated) in [(128, 128, false), (129, 128, true)] {
        let fields = (0..entries)
            .map(|index| (format!("unknown-{index:03}"), Value::Null))
            .collect::<serde_json::Map<_, _>>();
        let AttemptFailureEvidence::Provider(provider) =
            AttemptFailureEvidence::provider(&Value::Object(fields))
        else {
            panic!("provider evidence");
        };
        assert_eq!(
            provider
                .shape
                .as_ref()
                .and_then(Value::as_object)
                .map(serde_json::Map::len),
            Some(expected_len)
        );
        assert_eq!(provider.shape_truncated, truncated);
    }

    fn node_count(value: &Value) -> usize {
        1 + match value {
            Value::Array(values) => values.iter().map(node_count).sum(),
            Value::Object(fields) => fields.values().map(node_count).sum(),
            _ => 0,
        }
    }
    for (leaves, truncated) in [(1_015, false), (1_016, true)] {
        let mut remaining = leaves;
        let branches = (0..8)
            .map(|_| {
                let count = remaining.min(127);
                remaining -= count;
                Value::Array(vec![Value::Null; count])
            })
            .collect();
        let value = Value::Array(branches);
        let AttemptFailureEvidence::Provider(provider) = AttemptFailureEvidence::provider(&value)
        else {
            panic!("provider evidence");
        };
        assert_eq!(provider.shape.as_ref().map(node_count), Some(1_024));
        assert_eq!(provider.shape_truncated, truncated);
    }
}

/// Key and identifier limits distinguish the exact scalar/byte caps from cap+1.
#[test]
fn key_and_identifier_boundaries_are_exact() {
    for (key, truncated) in [
        ("k".repeat(128), false),
        ("k".repeat(129), true),
        ("🦀".repeat(128), false),
        (format!("{}a", "🦀".repeat(128)), true),
    ] {
        let value = Value::Object([(key, Value::Null)].into_iter().collect());
        let AttemptFailureEvidence::Provider(provider) = AttemptFailureEvidence::provider(&value)
        else {
            panic!("provider evidence");
        };
        assert_eq!(provider.shape_truncated, truncated);
    }

    for (candidate, accepted) in [
        ("a".repeat(256), true),
        ("a".repeat(257), false),
        ("🦀".repeat(256), false),
        (format!("{}a", "🦀".repeat(256)), false),
    ] {
        let mut truncated = false;
        assert_eq!(
            validated_identifier(Some(&candidate), "other", None, &mut truncated).is_some(),
            accepted
        );
        assert_eq!(truncated, !accepted);
    }
}

/// Persisted lengths count multibyte text exactly while live text obeys both
/// scalar and byte caps.
#[test]
fn multibyte_lengths_and_live_detail_caps_are_exact() {
    let provider = AttemptFailureEvidence::provider(&json!({
        "type": "error",
        "message": "é🙂",
    }));
    let record = capture_record(&provider, true, 1, false).expect("provider capture");
    assert_eq!(
        record["provider"]["message"],
        json!({"present":true,"utf8_bytes":6,"unicode_scalars":2})
    );

    let close = AttemptFailureEvidence::transport(
        TransportPhase::ResponseStream,
        true,
        TransportFailureKind::WebSocketTermination(WsTermination::CloseFrame {
            code: Some(1000),
            reason: Some("é🙂".to_owned()),
        }),
    );
    let record = capture_record(&close, true, 1, false).expect("close capture");
    assert_eq!(
        record["transport"],
        json!({
            "phase": "response_stream",
            "kind": "websocket_close",
            "ws_close_code": 1000,
            "ws_close_reason": {
                "present": true,
                "utf8_bytes": 6,
                "unicode_scalars": 2,
            },
            "clean_eof": false,
            "frame_bytes": null,
        })
    );

    for (reason, expected_scalars, expected_bytes) in [
        ("🦀".repeat(256), 256, 1_024),
        ("🦀".repeat(257), 256, 1_024),
    ] {
        let detail = super::redacted_detail::sanitize_live_detail(&reason, "other", None)
            .expect("live detail");
        assert_eq!(detail.as_str().chars().count(), expected_scalars);
        assert_eq!(detail.as_str().len(), expected_bytes);
    }
}

/// Capture serialization accepts the exact 64-KiB cap, applies the shape
/// fallback at cap+1, and drops a record that remains oversized afterward.
#[test]
fn total_capture_byte_boundary_is_exact() {
    fn record_with_total_bytes(target: usize, shape: Value) -> Value {
        let mut record = json!({
            "provider": {"terminal_event_shape": shape},
            "truncation": {"total": false},
            "padding": "",
        });
        let baseline = serde_json::to_vec_pretty(&record).expect("baseline").len();
        record["padding"] = Value::String("x".repeat(target - baseline));
        assert_eq!(
            serde_json::to_vec_pretty(&record).expect("record").len(),
            target
        );
        record
    }

    let BoundedRecord::Ready { serialized: exact } =
        serialize_bounded_record(record_with_total_bytes(65_536, Value::Null))
            .expect("serialize exact cap")
    else {
        panic!("exact cap must be retained");
    };
    assert_eq!(exact.len(), 65_536);
    let BoundedRecord::Ready {
        serialized: fallback,
    } = serialize_bounded_record(record_with_total_bytes(
        65_537,
        Value::String("shape".to_owned()),
    ))
    .expect("serialize cap+1 fallback")
    else {
        panic!("shape fallback must fit");
    };
    let fallback: Value = serde_json::from_slice(&fallback).expect("fallback JSON");
    assert_eq!(fallback["provider"]["terminal_event_shape"], Value::Null);
    assert_eq!(fallback["truncation"]["total"], true);
    assert!(matches!(
        serialize_bounded_record(record_with_total_bytes(65_538, Value::Null))
            .expect("serialize oversized"),
        BoundedRecord::Oversized
    ));
}

/// Regression: persisted identifiers must reject loaded-secret containment and
/// bearer/JWT/API-key-shaped values.
#[test]
fn identifier_validation_rejects_loaded_and_token_shaped_values() {
    let mut truncated = false;
    assert_eq!(
        validated_identifier(Some("prefix-secret-suffix"), "secret", None, &mut truncated),
        None
    );
    assert!(truncated);
    let mut truncated = false;
    assert_eq!(
        validated_identifier(
            Some("aaa.bbbbbbbbbbbbbbbbbbbb.ccc"),
            "other",
            None,
            &mut truncated
        ),
        None
    );
    assert!(truncated);
    for shaped in [
        "prefix-sk-abcdefghijklmnop",
        "prefix-ApiKey-abcdefghijklmnop",
        "prefix-api-key-abcdefghijklmnop",
        "prefix.abcdefghijklmnopqrst.suffix",
        "prefix.abcdefghijklmnopqrst.suffix.",
    ] {
        let mut truncated = false;
        assert_eq!(
            validated_identifier(Some(shaped), "other", None, &mut truncated),
            None
        );
        assert!(truncated);
    }
}

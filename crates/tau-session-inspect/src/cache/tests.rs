//! Synthetic cache evidence fixtures; never provider traffic or private state.

use std::fs::File;
use std::io::Write as _;

use super::*;

/// Realized reads, including perfect hits, never invent an eligible ceiling.
#[test]
fn reads_do_not_synthesize_eligibility() {
    let perfect = metrics(100, 100, None);
    assert_eq!(perfect["share_of_input"], 1.0);
    assert_eq!(perfect["eligibility_evidence"], "unknown");
    assert!(perfect["eligibility_utilization"].is_null());
    assert!(metrics(0, 0, Some(0))["share_of_input"].is_null());
    assert!(metrics(0, 0, Some(0))["eligibility_utilization"].is_null());
}

/// Invalid counters stay invalid instead of being capped into credible
/// evidence.
#[test]
fn invalid_read_and_ceiling_evidence_is_not_repaired() {
    assert!(metrics(10, 11, None)["non_read_input"].is_null());
    assert_eq!(metrics(10, 11, None)["input_read_evidence"], "invalid");
    assert_eq!(metrics(10, 5, Some(4))["eligibility_evidence"], "invalid");
    assert_eq!(metrics(10, 5, Some(11))["eligibility_evidence"], "invalid");
    assert_eq!(metrics(10, 5, Some(10))["eligibility_utilization"], 0.5);
}

/// Writes one synthetic private zstd capture under the managed directory shape.
fn capture(root: &std::path::Path, name: &str, body: &[u8]) {
    std::fs::create_dir_all(root.join("provider")).expect("capture directory");
    let bytes = zstd::stream::encode_all(body, 3).expect("compress fixture");
    std::fs::write(root.join("provider").join(name), bytes).expect("write fixture");
}

/// Multiple same-prompt files remain file counts, never inferred attempt joins.
#[test]
fn legacy_files_have_explicit_partial_coverage_without_payload_export() {
    let root = tempfile::tempdir().expect("fixture root");
    let body = br#"{"session_id":"session","agent_prompt_id":"prompt","body":{"secret":"CREDENTIAL","previous_response_id":"PRIVATE_RESPONSE"}}"#;
    capture(root.path(), "1.json.zst", body);
    capture(root.path(), "2.json.zst", body);
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps["legacy_partial"], 2);
    let counts = inventory.prompts.values().next().expect("capture counts");
    assert_eq!(counts.request_files, 2);
    let output = serde_json::to_string(counts).expect("encode counts");
    assert!(!output.contains("CREDENTIAL"));
    assert!(!output.contains("PRIVATE_RESPONSE"));
}

/// Torn compression and compressed limits cannot silently turn into empty
/// success.
#[test]
fn torn_and_bounded_capture_files_are_counted_gaps() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(root.path(), "1.json.zst", br#"{"body":{}}"#);
    let path = root.path().join("provider/1.json.zst");
    let mut bytes = std::fs::read(&path).expect("read fixture");
    bytes.truncate(bytes.len() - 2);
    std::fs::write(&path, bytes).expect("truncate fixture");
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps["truncated_or_malformed_compression"], 1);
    let limits = CacheScanLimits {
        compressed_file_bytes: 1,
        ..Default::default()
    };
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &limits,
    );
    assert_eq!(inventory.gaps["compressed_capture_limit"], 1);
}

/// Unknown schemas are explicitly unsupported, never treated as legacy records.
#[test]
fn unsupported_schema_is_not_legacy_success() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(
        root.path(),
        "1.json.zst",
        br#"{"schema":"tau.cache_diagnostic","schema_version":99}"#,
    );
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps["unsupported_capture_schema"], 1);
    assert!(inventory.prompts.is_empty());
}

/// Duplicate fields cannot overwrite typed attribution or hidden nested
/// evidence.
#[test]
fn duplicate_json_keys_are_rejected_recursively() {
    for bytes in [
        br#"{"session_id":"one","session_id":"two"}"#.as_slice(),
        br#"{"body":{"a":1,"a":2}}"#.as_slice(),
    ] {
        assert!(serde_json::from_slice::<strict_json::StrictJson>(bytes).is_err());
    }
}

/// Decompression admission and cumulative budgets both expose partial results.
#[test]
fn decoded_and_total_limits_are_explicit() {
    let root = tempfile::tempdir().expect("fixture root");
    capture(root.path(), "1.json.zst", &[b' '; 1000]);
    capture(root.path(), "2.json.zst", &[b' '; 1000]);
    let limits = CacheScanLimits {
        decompressed_file_bytes: 10,
        total_decompressed_bytes: 10,
        ..Default::default()
    };
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session id"),
        &limits,
    );
    assert_eq!(inventory.gaps["decoded_or_memory_capture_limit"], 1);
    assert_eq!(inventory.gaps["cumulative_capture_limit"], 1);
}

/// Preflight rejects over-budget journals without attempting strict replay or
/// repair.
#[test]
fn journal_preflight_produces_partial_without_inspection_or_mutation() {
    let root = tempfile::tempdir().expect("fixture root");
    let dir = root.path().join("agents/agent");
    std::fs::create_dir_all(&dir).expect("journal directory");
    let path = dir.join("events.cbor");
    let mut file = File::create(&path).expect("journal fixture");
    file.write_all(&[0; 129]).expect("journal bytes");
    let options = CacheOptions {
        state_dir: root.path().into(),
        scope: CacheScope::Agent {
            agent_id: "agent".parse().expect("agent id"),
            include_descendants: false,
        },
        prompt: None,
        limits: CacheScanLimits {
            working_memory_bytes: 65536,
            ..Default::default()
        },
        producer_build: "synthetic-build".into(),
    };
    let report = read_cache_report(&options).expect("partial report");
    assert!(report.is_partial());
    assert!(!report.inspected);
    assert_eq!(report.gaps["journal_memory_preflight_limit"], 1);
    assert_eq!(
        std::fs::read(path).expect("unchanged journal"),
        vec![0; 129]
    );
    assert!(!dir.join("lock").exists());
}

/// Exact admission arithmetic is inclusive and saturates both multiplication
/// and sum.
#[test]
fn journal_preflight_boundary_and_overflow_cannot_wrap() {
    assert_eq!(journal_memory_charge(0, 128), 65536 / 4);
    assert!(journal_memory_charge(0, 129) > 65536 / 4);
    assert_eq!(journal_memory_charge(0, u64::MAX), u64::MAX);
    assert_eq!(journal_memory_charge(u64::MAX - 1, 1), u64::MAX);
}

/// Current producer envelopes remain recognized without exposing their raw
/// payloads.
#[test]
fn current_provider_capture_envelopes_are_inventory_not_unsupported_schema() {
    let root = tempfile::tempdir().expect("fixture root");
    // Shapes mirror current producer serializers, including nullable usage and
    // the separately existing versioned failure records (not new migrations).
    let envelopes = [
        json!({"backend":"chat_completions","transport":"http-sse","model":"model",
            "operation":"inference","logical_attempt":1,"wire_dispatch_index":1,
            "body":{"messages":[{"content":"PRIVATE"}]}}),
        json!({"backend":"responses","transport":"websocket","model":"model",
            "body":{"input":[]}}),
        json!({"backend":"chat_completions","transport":"http-sse","model":"model",
            "operation":"inference","logical_attempt":1,"wire_dispatch_index":1,
            "usage":null,"stop_reason":"end_turn","output_items":[],"raw_events":[]}),
        json!({"backend":"responses","transport":"http-sse","model":"model",
            "provider_response_id":"PRIVATE_ID","usage":null,"stop_reason":"end_turn",
            "response_bytes_received":10,"raw_events":[],"raw_events_truncated":false}),
        json!({"backend":{"kind":"responses","transport":"websocket"},
            "provider_response_id":"PRIVATE_ID","usage":null,
            "provider_response_finished":{},"provider_terminal_event":null}),
        json!({"backend":"chat_completions","transport":"http-sse","model":"model",
            "operation":"inference","logical_attempt":1,"wire_dispatch_index":1,
            "http_status":503,"body":"PRIVATE_ERROR"}),
        json!({"backend":"responses","transport":"http-sse","model":"model",
            "response_bytes_received":10,"error":{"kind":"http","body":"PRIVATE_ERROR"}}),
        current_attempt_failure(),
        current_compact_failure(),
    ];
    for (index, mut envelope) in envelopes.into_iter().enumerate() {
        envelope["session_id"] = "session".into();
        envelope["agent_prompt_id"] = "prompt".into();
        capture(
            root.path(),
            &format!("{index}.json.zst"),
            &serde_json::to_vec(&envelope).expect("encode envelope"),
        );
    }
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(inventory.gaps, BTreeMap::from([("legacy_partial", 9)]));
    let counts = inventory.prompts.values().next().expect("prompt inventory");
    assert_eq!(counts.request_files, 2);
    assert_eq!(counts.response_files, 3);
    assert_eq!(counts.failure_files, 4);
    assert!(
        !serde_json::to_string(counts)
            .expect("counts")
            .contains("PRIVATE")
    );
}

/// A current producer-exact finite attempt with no established transport
/// evidence.
fn current_attempt_failure() -> Value {
    json!({
        "schema_version":1,"capture_kind":"provider_attempt_failure",
        "session_id":"session","agent_prompt_id":"prompt",
        "operation":"inference","logical_attempt":1,"wire_dispatch_index":null,
        "backend":{"kind":"responses","transport_intent":"websocket","transport_established":false},
        "outcome":"retry_scheduled",
        "classification":{"category":"transport","retry_after_secs":null},
        "wire":{"wire_dispatches":0,"repair_used":false,"response_bytes_received":0,"semantic_progress":"none"},
        "provider":null,"transport":null,
        "truncation":{"total":false,"shape":false,"identifiers":false}
    })
}

/// A current producer-exact compact failure with complete empty decoded body
/// and no headers.
fn current_compact_failure() -> Value {
    json!({
        "schema_version":0,"capture_kind":"compact_http_failure",
        "session_id":"session","agent_prompt_id":"prompt",
        "operation":"compact","backend":{"kind":"responses","transport":"unary_http"},
        "http":{"status":503,"headers":{"content_type":null,"retry_after":null,
            "request_id":null,"openai_request_id":null,"x_request_id":null}},
        "body":{"decoded_bytes_received":0,"retained_bytes":0,"complete":true,
            "truncated":false,"redacted_prefix_truncated":false,
            "sha256_decoded_received":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "sha256_coverage":"complete_decoded_body","redacted_decoded_prefix_base64":""}
    })
}

/// Recognized discriminators do not make missing or wrong-type evidence
/// credible.
#[test]
fn malformed_current_failure_fields_are_partial_not_valid_inventory() {
    let root = tempfile::tempdir().expect("fixture root");
    let mut malformed = Vec::new();
    for base in [current_attempt_failure(), current_compact_failure()] {
        for name in base.as_object().expect("fixture object").keys() {
            if [
                "schema_version",
                "capture_kind",
                "session_id",
                "agent_prompt_id",
            ]
            .contains(&name.as_str())
            {
                continue;
            }
            let mut missing = base.clone();
            missing.as_object_mut().expect("object").remove(name);
            malformed.push(missing);
        }
    }
    for (mut value, pointer) in [
        (current_compact_failure(), "/body/decoded_bytes_received"),
        (current_compact_failure(), "/body/complete"),
        (current_compact_failure(), "/body/sha256_decoded_received"),
        (current_compact_failure(), "/http/headers"),
        (current_attempt_failure(), "/wire/response_bytes_received"),
        (current_attempt_failure(), "/wire/semantic_progress"),
        (current_attempt_failure(), "/backend/transport_established"),
        (
            current_attempt_failure(),
            "/classification/retry_after_secs",
        ),
        (current_attempt_failure(), "/truncation/total"),
    ] {
        *value.pointer_mut(pointer).expect("existing field") = json!([]);
        malformed.push(value);
    }
    let expected = malformed.len() as u64;
    for (index, value) in malformed.into_iter().enumerate() {
        capture(
            root.path(),
            &format!("{index}.json.zst"),
            &serde_json::to_vec(&value).expect("encode malformed"),
        );
    }
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(
        inventory.gaps,
        BTreeMap::from([("malformed_current_failure_capture", expected)])
    );
    assert!(inventory.prompts.is_empty());
}

/// Nullable provider/transport evidence is type-checked recursively, not
/// discarded unchecked.
#[test]
fn current_failure_nested_optional_shapes_are_validated() {
    let mut attempt = current_attempt_failure();
    attempt["provider"] = json!({"terminal_event_type":null,"canonical_error_code":null,
        "provider_request_id":"request","provider_response_id":null,
        "message":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
        "terminal_event_shape":null});
    attempt["transport"] = json!({"phase":"response_stream","kind":"websocket_close",
        "ws_close_code":1000,"ws_close_reason":{"present":false,"utf8_bytes":0,"unicode_scalars":0},
        "clean_eof":false,"frame_bytes":null});
    assert!(failure_shape::attempt(&attempt));
    attempt["provider"]["message"]["present"] = "false".into();
    assert!(!failure_shape::attempt(&attempt));

    let bytes = json!({"original_bytes":4,"retained_bytes":4,"truncated":false,
        "base64":"b29wcw==","utf8":"oops","original_unicode_scalars":4,"retained_unicode_scalars":4});
    let mut compact = current_compact_failure();
    compact["http"]["headers"]["request_id"] = bytes.clone();
    compact["body"]["parsed_error"] = json!({"code":bytes});
    assert!(failure_shape::compact(&compact));
    compact["body"]["parsed_error"]["code"]["retained_bytes"] = (-1).into();
    assert!(!failure_shape::compact(&compact));
}

/// Known discriminators at future versions remain unsupported rather than
/// malformed-current.
#[test]
fn future_failure_versions_are_not_current_shape_fallbacks() {
    let root = tempfile::tempdir().expect("fixture root");
    for (index, mut value) in [current_attempt_failure(), current_compact_failure()]
        .into_iter()
        .enumerate()
    {
        value["schema_version"] = 99.into();
        capture(
            root.path(),
            &format!("{index}.json.zst"),
            &serde_json::to_vec(&value).expect("future fixture"),
        );
    }
    let mut inventory = Inventory::default();
    inventory.scan(
        root.path(),
        &"session".parse().expect("session"),
        &CacheScanLimits::default(),
    );
    assert_eq!(
        inventory.gaps,
        BTreeMap::from([("unsupported_capture_schema", 2)])
    );
    assert!(inventory.prompts.is_empty());
}

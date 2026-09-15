use super::*;

/// Run the real response owner with an isolated in-memory provider stream.
fn captured_response(
    events: &[&str],
    mode: ResponseMode,
    enabled: bool,
) -> (Result<WsTurnResult, LlmError>, Vec<serde_json::Value>, bool) {
    let (mut conn, inbound, _outbound) = test_ws_conn();
    for event in events {
        inbound
            .send_blocking(InboundEvent::Event {
                text: (*event).to_owned().into(),
                read: None,
            })
            .expect("queue fake provider text");
    }
    let fixture = PromptFixture::new();
    let mut request = fixture.payload();
    request.debug_provider_requests = enabled;
    let diagnostic = CacheAttempt::new("capture-response", &request, 3, true).map(Arc::new);
    let mut correlation =
        path_crate_attempt_failure::AttemptCaptureCorrelation::new(crate::LogicalAttempt::new(3));
    correlation.diagnostic = diagnostic;
    let mut captures = Vec::new();
    let mut progress = false;
    let result = conn.run_response_with_capture_submit(
        &test_responses_config(),
        "capture-response",
        &request,
        Some(correlation.next_dispatch()),
        None,
        mode,
        &mut NeverAbort,
        &mut |_| {},
        &mut |state| progress |= state.has_semantic_progress(),
        |capture| {
            captures.push(serde_json::from_slice(capture.json()).expect("private JSON"));
        },
    );
    (result, captures, progress)
}

/// A native compact validation failure must retain the exact rejected event and
/// preceding accepted semantic item without converting failure into success.
#[test]
fn rejected_compact_event_survives_validation_exit() {
    let events = [
        r#"{ "type":"response.output_item.done", "output_index":0, "item":{ "type":"compaction", "encrypted_content":"private fake payload" } }"#,
        r#"{ "type" : "response.done", "response":{"id":"resp_rejected"} }"#,
    ];
    let (result, captures, progress) = captured_response(&events, ResponseMode::Compact, true);
    assert!(progress);
    let error = result.err().expect("unchanged compact rejection");
    assert!(matches!(error.root_error(), LlmError::InvalidResponse(_)));
    assert_eq!(error.retry_decision(), None);
    assert_eq!(captures.len(), 2);
    let request = &captures[0];
    let response = &captures[1];
    assert_eq!(response["record_kind"], "received_response");
    assert_eq!(response["raw_events"], serde_json::json!(events));
    assert_eq!(response["response_mode"], "compact");
    assert_eq!(response["logical_attempt"], 3);
    assert_eq!(response["wire_dispatch_index"], 1);
    assert_eq!(response["attempt_id"], request["attempt_id"]);
    assert_eq!(response["capture_complete"], false);
    assert_eq!(response["prefix_truncated"], false);
    assert_eq!(response["accepted_terminal"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("internal error")
            .contains("terminal_before_completed_item_or_unsupported_done")
    );
    assert!(
        !serde_json::to_string(response)
            .expect("JSON")
            .contains("test-token")
    );
}

/// Invalid JSON must be recorded before decoding and never disappear behind the
/// sanitized transport error, for both ordinary and compact streams.
#[test]
fn malformed_text_survives_decode_exit() {
    for mode in [ResponseMode::Ordinary, ResponseMode::Compact] {
        let events = ["{ not JSON \n exact bytes"];
        let (result, captures, progress) = captured_response(&events, mode, true);
        assert!(result.is_err());
        assert!(!progress);
        assert_eq!(captures[1]["raw_events"], serde_json::json!(events));
        assert_eq!(captures[1]["capture_complete"], false);
        assert_eq!(captures[1]["outcome"], "error");
        assert!(
            captures[1]["decode_error"]
                .as_str()
                .expect("original decoder error")
                .contains("line 1 column")
        );
    }
}

/// Successful compact output must retain original text and its terminal, rather
/// than dropping debug evidence while projecting the compact result and usage.
#[test]
fn successful_compact_response_retains_exact_terminal() {
    let events = [
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","encrypted_content":"opaque"}}"#,
        r#"{ "type":"response.completed", "response":{"id":"resp_compact","usage":{"input_tokens":4,"output_tokens":2}} }"#,
    ];
    let (result, captures, progress) = captured_response(&events, ResponseMode::Compact, true);
    let result = result.expect("valid compact output");
    assert!(progress);
    assert!(result.state.single_compaction_item().is_some());
    assert_eq!(
        result
            .state
            .usage()
            .expect("usage")
            .response_received_tokens,
        2
    );
    assert_eq!(captures[1]["raw_events"], serde_json::json!(events));
    assert_eq!(captures[1]["capture_complete"], true);
    assert_eq!(captures[1]["accepted_terminal"], true);
}

/// Prefix truncation must not alter decoding, semantic progress, or completion;
/// the latest event remains available after the bounded prefix fills.
#[test]
fn capture_truncation_does_not_change_compact_outcome() {
    let padding = format!(
        r#"{{"type":"unknown","padding":"{}"}}"#,
        "x".repeat(600_000)
    );
    let events = [
        padding.as_str(),
        padding.as_str(),
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"compaction","encrypted_content":"opaque"}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_compact"}}"#,
    ];
    let (result, captures, progress) = captured_response(&events, ResponseMode::Compact, true);
    assert!(result.is_ok());
    assert!(progress);
    assert_eq!(captures[1]["prefix_truncated"], true);
    assert_eq!(captures[1]["capture_complete"], false);
    assert_eq!(captures[1]["accepted_terminal"], true);
    assert_eq!(captures[1]["last_received_event"], events[3]);
    assert_eq!(captures[1]["last_received_event_index"], 4);
}

/// Disabling existing exact-capture policy suppresses both records, not
/// provider execution or its success result.
#[test]
fn disabled_response_capture_preserves_execution() {
    let (result, captures, _) = captured_response(
        &[r#"{"type":"response.completed","response":{"id":"resp_ok"}}"#],
        ResponseMode::Ordinary,
        false,
    );
    assert!(result.is_ok());
    assert!(captures.is_empty());
}

/// Provider-authored errors must survive event application failure unchanged in
/// private evidence instead of being replaced by a synthetic terminal report.
#[test]
fn backend_error_survives_event_application_exit() {
    let events = [
        r#"{ "type":"error", "error":{"code":"server_error","message":"exact fake backend failure"} }"#,
    ];
    let (result, captures, progress) = captured_response(&events, ResponseMode::Compact, true);
    let error = result.err().expect("backend error");
    assert!(error.retry_decision().is_some());
    assert!(!progress);
    assert_eq!(captures[1]["raw_events"], serde_json::json!(events));
    assert_eq!(captures[1]["capture_complete"], false);
    assert!(
        captures[1]["error"]
            .as_str()
            .expect("internal backend error")
            .contains("exact fake backend failure")
    );
}

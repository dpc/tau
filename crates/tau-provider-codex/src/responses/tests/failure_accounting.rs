use super::*;

/// Failure accounting must use the successful terminal's normalization without
/// accepting output, honoring end_turn=false, or changing error classification.
#[test]
fn failed_envelopes_preserve_accounting_without_completion_authority() {
    for kind in ["response.incomplete", "response.failed"] {
        for mode in [
            path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
            path_crate_attempt_failure::ProviderEvidenceMode::Persistent,
        ] {
            let event = serde_json::json!({
                "type": kind,
                "response": {
                    "id": "resp_failed",
                    "end_turn": false,
                    "error": {"code": "invalid_prompt"},
                    "incomplete_details": {"reason": "interrupted"},
                    "usage": {
                        "input_tokens": 100, "output_tokens": 7,
                        "input_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 9}
                    },
                    "output": [{"type": "function_call", "name": "must_not_run"}]
                }
            });
            let mut state = StreamState::new();
            state.provider_evidence_mode = mode;
            let error = apply_event(&mut state, &event, &mut |_| panic!("no output"))
                .expect_err("failure is not completion");
            assert!(error.retry_decision().is_none());
            assert_eq!(
                error.failure_kind(),
                Some(tau_proto::ProviderFailureKind::RequestRejected)
            );
            assert!(!state.requests_continuation());
            assert!(state.provider_terminal_event.is_none());
            assert!(state.response_id().is_none(), "no chain anchor");
            assert!(state.into_output_items().is_empty());
            let accounting = error.response_accounting().expect("accounting");
            assert_eq!(
                accounting.provider_response_id.as_deref(),
                Some("resp_failed")
            );
            let mut completed = StreamState::new();
            apply_terminal_event(&mut completed, &event);
            assert_eq!(accounting.usage, completed.usage());
            let usage = accounting.usage.as_ref().expect("usage");
            assert_eq!(usage.prompt_sent_tokens, 100);
            assert_eq!(usage.prompt_cached_tokens, 20);
            assert_eq!(usage.response_received_tokens, 7);
            assert_eq!(usage.cache.as_ref().expect("cache").write_tokens, Some(9));
        }
    }
}

/// Missing/malformed counters stay unknown, explicit zero remains observed,
/// and cache-write-only reports retain presence under the existing
/// normalization to total input (zero when unknown), rather than inventing
/// input tokens.
#[test]
fn failed_accounting_preserves_absence_zero_and_partial_usage() {
    for (usage, expected) in [
        (serde_json::Value::Null, None),
        (serde_json::json!({}), None),
        (
            serde_json::json!({"input_tokens": -1, "output_tokens": "7"}),
            None,
        ),
        (serde_json::json!({"output_tokens": 0}), Some((0, 0, None))),
        (serde_json::json!({"output_tokens": 7}), Some((0, 7, None))),
        (
            serde_json::json!({"input_tokens_details": {"cache_write_tokens": 3}}),
            Some((0, 0, Some(0))),
        ),
    ] {
        let event = serde_json::json!({
            "type": "response.incomplete", "response": {"id": 123, "usage": usage}
        });
        let error = response_incomplete_error(
            &event,
            path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
        );
        let accounting = error.response_accounting().expect("accounting");
        assert!(accounting.provider_response_id.is_none());
        assert_eq!(
            accounting.usage.as_ref().map(|usage| (
                usage.prompt_sent_tokens,
                usage.response_received_tokens,
                usage.cache.as_ref().and_then(|cache| cache.write_tokens),
            )),
            expected
        );
    }
}

/// Metadata never changes transient retry classification; revoking compact
/// capacity recovery after semantic output must retain the final accounting.
#[test]
fn failed_accounting_survives_policy_classification() {
    for code in ["server_error", "context_length_exceeded"] {
        let event = serde_json::json!({
            "type": "response.failed",
            "response": {"id": "resp_policy", "error": {"code": code},
                "usage": {"input_tokens": 11, "output_tokens": 2}}
        });
        let error = response_failed_error(
            &event,
            path_crate_attempt_failure::ProviderEvidenceMode::LiveOnly,
        );
        assert_eq!(error.retry_decision().is_some(), code == "server_error");
        let error = crate::CodexError::compact_terminal(error, crate::SemanticProgress::Parsed);
        let accounting = error.response_accounting().expect("accounting");
        assert_eq!(
            accounting.provider_response_id.as_deref(),
            Some("resp_policy")
        );
        assert_eq!(
            accounting
                .usage
                .as_ref()
                .expect("usage")
                .response_received_tokens,
            2
        );
        if code == "context_length_exceeded" {
            assert_eq!(
                error.failure_kind(),
                Some(tau_proto::ProviderFailureKind::Unknown)
            );
        }
    }
}

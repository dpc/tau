use tau_provider::retry_policy::{RetryClass, RetryDecision};

use super::*;

/// Semantic progress is monotonic across later empty observations, so
/// transparent transport work cannot make a replay-unsafe attempt look safe.
#[test]
fn semantic_progress_is_sticky() {
    let mut attempt =
        ProviderAttemptContext::new(AttemptOperation::Inference, LogicalAttempt::new(3));
    let mut progressed = StreamState::new();
    crate::responses::apply_event(
        &mut progressed,
        &serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": "msg",
            "output_index": 0,
            "delta": "accepted"
        }),
        &mut |_| {},
    )
    .expect("semantic delta");
    attempt.observe_stream(&progressed);
    attempt.observe_stream(&StreamState::new());
    assert_eq!(attempt.progress(), SemanticProgress::Parsed);
}

/// Correlation allocation retains logical and wire attempt numbering without
/// serving as provider-egress authority.
#[test]
fn correlation_allocation_tracks_wire_dispatch_index() {
    let mut attempt =
        ProviderAttemptContext::new(AttemptOperation::Compact, LogicalAttempt::new(3));
    assert_eq!(attempt.snapshot().wire_dispatches(), 0);
    let dispatch = attempt.correlation().next_dispatch();
    assert_eq!(dispatch.logical_attempt(), 3);
    assert_eq!(dispatch.wire_dispatch_index(), 1);
    assert_eq!(attempt.snapshot().wire_dispatches(), 1);
}

/// Compact finalization must serialize correlation-observed sticky progress,
/// cumulative bytes, and the real later attempt exactly once.
#[test]
fn compact_finalizer_emits_merged_attempt_observation_once() {
    let session_id = tau_proto::SessionId::parse("session-compact-finalizer").expect("session");
    let agent_id = tau_proto::AgentId::parse("agent-compact-finalizer").expect("agent");
    let context = tau_proto::PromptContext::default();
    let originator = tau_proto::PromptOriginator::User;
    let request = crate::Prompt {
        system_prompt: "",
        context: &context,
        hosted_tools: &[],
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
    let mut state = StreamState::new();
    state.record_transport_response_bytes(73);
    crate::responses::apply_event(
        &mut state,
        &serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "compaction", "id": "cmp_finalizer"}
        }),
        &mut |_| {},
    )
    .expect("compact item");
    let mut attempt =
        ProviderAttemptContext::new(AttemptOperation::Compact, LogicalAttempt::new(6));
    let dispatch = attempt.correlation().next_dispatch();
    assert_eq!(dispatch.wire_dispatch_index(), 1);
    attempt.correlation().observe_stream(&state);
    let decision = RetryDecision::new(RetryClass::Unknown);
    let evidence = AttemptFailureEvidence::provider(&serde_json::json!({
        "type": "response.failed",
        "error": {"code": "server_error"}
    }));
    let input = attempt
        .take_retry_failure(RetryFailureInput {
            agent_prompt_id: "prompt-compact-finalizer",
            request: &request,
            decision: &decision,
            evidence: Some(&evidence),
            access_token: "secret",
            account_id: None,
        })
        .expect("first finalization");
    let mut submitted = None;
    crate::attempt_failure::submit_capture_with(input, |capture| submitted = Some(capture));
    let record: serde_json::Value =
        serde_json::from_slice(submitted.expect("capture").json()).expect("capture JSON");
    assert_eq!(record["operation"], "compact");
    assert_eq!(record["logical_attempt"], 6);
    assert_eq!(record["wire_dispatch_index"], 1);
    assert_eq!(record["wire"]["response_bytes_received"], 73);
    assert_eq!(record["wire"]["semantic_progress"], "parsed");
    assert!(
        attempt
            .take_retry_failure(RetryFailureInput {
                agent_prompt_id: "prompt-compact-finalizer",
                request: &request,
                decision: &decision,
                evidence: Some(&evidence),
                access_token: "secret",
                account_id: None,
            })
            .is_none()
    );
}

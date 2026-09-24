//! Existing canonical Error semantics used by Grok's approved partial limits.

use super::*;

/// Error prose and usage survive live append and encoded cold replay without
/// opening tools or granting output-length recovery. Repeating the append on an
/// empty reconstructed tree checks the same fold, not harness crash recovery.
#[test]
fn partial_provider_error_preserves_prose_usage_without_tool_or_length_authority() {
    let mut live = AgentTree::from_events(agent_id(), &[]);
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 11,
        response_received_tokens: 7,
        ..Default::default()
    };
    let prose = ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: "partial prose".into(),
        }],
        phase: None,
        responses_raw_json: None,
    });
    let response = tau_proto::ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: "partial-limit".parse().expect("prompt"),
        agent_id: agent_id(),
        output_items: vec![prose.clone()],
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("provider stopped an incomplete response (max_time_limit)".into()),
        failure_kind: None,
        context_limit_telemetry: None,
        final_status_disposition: tau_proto::FinalStatusDisposition::Accepted,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: PromptOriginator::User,
        usage: Some(usage.clone()),
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: Some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::PublicResponses,
            base_url: "https://api.x.ai/v1".into(),
            transport: tau_proto::ProviderBackendTransport::HttpSse,
            stale_chain_fallback: false,
        }),
        provider_attempt: Default::default(),
        provider_response_id: Some("partial-id".into()),
        ws_pool_delta: None,
    };
    let record = PersistedAgentEvent {
        observation_id: tau_proto::ObservationId::from_bytes([1; 16]),
        seq: live.next_event_seq(),
        source: None,
        event: Event::ProviderResponseFinished(response),
        parent: AgentEventParent::InheritHead,
        fold_semantics: AgentJournalFoldSemantics::CommitOrder,
        recorded_at: UnixMicros::new(1),
    };
    live.apply_persisted_record(&record).expect("live terminal");
    let mut encoded = Vec::new();
    ciborium::into_writer(&vec![record.clone()], &mut encoded).expect("encode");
    let decoded: Vec<PersistedAgentEvent> =
        ciborium::from_reader(encoded.as_slice()).expect("decode");
    let cold = AgentTree::try_from_events(agent_id(), &decoded).expect("cold replay");
    let mut restarted = AgentTree::try_from_events(agent_id(), &[]).expect("restart cut");
    restarted
        .apply_persisted_record(&record)
        .expect("terminal after restart");
    for tree in [&live, &cold, &restarted] {
        assert!(!tree.has_open_foreground_tool_round());
        assert!(tree.output_length_continuation_recovery().is_none());
        assert!(tree.output_length_terminal_incomplete().is_none());
        let branch = tree.current_branch();
        assert!(matches!(branch.as_slice(),
            [AgentEntry::AssistantResponse { output_items, usage: actual, provider_response_id, .. }]
            if output_items == &vec![prose.clone()] && actual.as_ref() == Some(&usage)
                && provider_response_id.as_deref() == Some("partial-id")));
    }
    assert_eq!(live.current_branch(), cold.current_branch());
    assert_eq!(live.current_branch(), restarted.current_branch());
}

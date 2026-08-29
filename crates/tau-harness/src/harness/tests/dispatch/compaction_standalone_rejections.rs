//! Standalone compaction terminal rejection tests.

use super::*;

/// failure.
#[test]
fn standalone_rejections_do_not_mutate_context_or_compaction_authority() {
    fn valid_replacement() -> ContextItem {
        ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: "valid replacement".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })
    }

    fn replacement_with_invalid_provider_image() -> Vec<ContextItem> {
        let call_id = tau_proto::ToolCallId::from("call-invalid-image");
        vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: ToolName::new("read_image"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolResult(tau_proto::ToolResultItem {
                call_id,
                tool_type: tau_proto::ToolType::Function,
                status: tau_proto::ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Text(
                    "invalid image".to_owned(),
                )),
                presentation: Default::default(),
                provider_content: vec![tau_proto::ToolResultContentPart::Image(
                    tau_proto::ImageContent {
                        media_type: tau_proto::ImageMediaType::Png,
                        data: vec![1, 2, 3].into(),
                        width: 1,
                        height: 1,
                        detail: tau_proto::ImageDetail::High,
                    },
                )],
            }),
        ]
    }

    let cases = [
        (
            "provider error",
            Some("provider error"),
            None,
            tau_proto::ProviderStopReason::EndTurn,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::ProviderError,
        ),
        (
            "context failure",
            None,
            Some(tau_proto::ProviderFailureKind::ContextWindowExceeded),
            tau_proto::ProviderStopReason::EndTurn,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::ProviderError,
        ),
        (
            "request failure",
            None,
            Some(tau_proto::ProviderFailureKind::RequestRejected),
            tau_proto::ProviderStopReason::EndTurn,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::ProviderError,
        ),
        (
            "unknown failure",
            None,
            Some(tau_proto::ProviderFailureKind::Unknown),
            tau_proto::ProviderStopReason::EndTurn,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::ProviderError,
        ),
        (
            "non-terminal stop",
            None,
            None,
            tau_proto::ProviderStopReason::ToolCalls,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "length stop",
            None,
            None,
            tau_proto::ProviderStopReason::Length,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::OutputLengthExceeded,
        ),
        (
            "error stop",
            None,
            None,
            tau_proto::ProviderStopReason::Error,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "repetition stop",
            None,
            None,
            tau_proto::ProviderStopReason::RepetitionDetected,
            vec![valid_replacement()],
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "empty window",
            None,
            None,
            tau_proto::ProviderStopReason::EndTurn,
            Vec::new(),
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "malformed window",
            None,
            None,
            tau_proto::ProviderStopReason::EndTurn,
            vec![ContextItem::Message(MessageItem {
                role: ContextRole::User,
                content: Vec::new(),
                phase: None,
                responses_raw_json: None,
            })],
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "harness trigger",
            None,
            None,
            tau_proto::ProviderStopReason::EndTurn,
            vec![ContextItem::CompactionTrigger],
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "empty private local narrative",
            None,
            None,
            tau_proto::ProviderStopReason::EndTurn,
            vec![ContextItem::LocalCompactionNarrative(
                tau_proto::LocalCompactionNarrativeItem {
                    narrative: String::new(),
                },
            )],
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "multi-item private local narrative",
            None,
            None,
            tau_proto::ProviderStopReason::EndTurn,
            vec![
                ContextItem::LocalCompactionNarrative(tau_proto::LocalCompactionNarrativeItem {
                    narrative: "valid narrative".to_owned(),
                }),
                ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                    kind: tau_proto::ReasoningTextKind::Full,
                    text: "must not persist".to_owned(),
                }),
            ],
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
        (
            "invalid provider image",
            None,
            None,
            tau_proto::ProviderStopReason::EndTurn,
            replacement_with_invalid_provider_image(),
            tau_proto::StandaloneCompactionFailureReason::InvalidWindow,
        ),
    ];

    for (label, error, failure_kind, stop_reason, output_items, expected_reason) in cases {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("durable agent");
        let head = h.agent_runtime.agent_registry.agents[&cid].identity.head;
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(55);
        agent.execution.context_cached_tokens = Some(21);
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        agent.execution.context_usage_head = head;
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(&agent_id),
        );
        let compact = read_nth_prompt_created(&h, 0);
        let context_before = (
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_input_tokens,
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_cached_tokens,
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_usage_model
                .clone(),
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_usage_head,
            h.agent_runtime.agent_registry.agents[&cid]
                .execution
                .context_percent_used,
        );
        let head_before = h.agent_runtime.agent_registry.agents[&cid].identity.head;
        let stored_head_before = h
            .session_runtime
            .agent_store
            .agent(&agent_id)
            .expect("agent tree")
            .head();
        let billable_before = h.session_runtime.current_session_state.token_usage.total;
        let cache_deadline_before = h.provider_runtime.cache_residency.next_deadline();
        let (transaction_id, cut, resume_through) = match &h.agent_runtime.agent_registry.agents
            [&cid]
            .dispatch
            .activation_dispatch
        {
            ActivationDispatchState::Running {
                id,
                cut,
                resume_through,
                ..
            } => (id.clone(), *cut, *resume_through),
            state => panic!("expected running compaction, got {state:?}"),
        };

        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            agent_prompt_id: compact.agent_prompt_id.clone(),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items,
            stop_reason,
            error: error.map(str::to_owned),
            failure_kind,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            usage: Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: 10,
                prompt_cached_tokens: 100,
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: 2,
                stats: Default::default(),
            }),
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: Some(tau_proto::ProviderBackend {
                kind: tau_proto::ProviderBackendKind::PublicResponses,
                base_url: "https://example.invalid/v1".to_owned(),
                transport: tau_proto::ProviderBackendTransport::HttpSse,
                stale_chain_fallback: false,
            }),
            provider_attempt: Default::default(),
            provider_response_id: Some("resp-standalone-terminal".to_owned()),
            ws_pool_delta: None,
        })
        .unwrap_or_else(|error| panic!("{label}: {error}"));

        assert_eq!(
            (
                h.agent_runtime.agent_registry.agents[&cid]
                    .execution
                    .context_input_tokens,
                h.agent_runtime.agent_registry.agents[&cid]
                    .execution
                    .context_cached_tokens,
                h.agent_runtime.agent_registry.agents[&cid]
                    .execution
                    .context_usage_model
                    .clone(),
                h.agent_runtime.agent_registry.agents[&cid]
                    .execution
                    .context_usage_head,
                h.agent_runtime.agent_registry.agents[&cid]
                    .execution
                    .context_percent_used,
            ),
            context_before,
            "{label}"
        );
        assert_eq!(
            h.provider_runtime.cache_residency.next_deadline(),
            cache_deadline_before,
            "{label}"
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid].identity.head, head_before,
            "{label}"
        );
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent(&agent_id)
                .expect("agent tree")
                .head(),
            stored_head_before,
            "{label}"
        );
        assert_eq!(
            h.session_runtime
                .current_session_state
                .token_usage
                .total
                .sent_tokens,
            billable_before.sent_tokens.saturating_add(10),
            "{label}"
        );
        assert_eq!(
            h.session_runtime
                .current_session_state
                .token_usage
                .total
                .cached_tokens,
            billable_before.cached_tokens.saturating_add(10),
            "{label}"
        );
        assert_eq!(
            h.session_runtime
                .current_session_state
                .token_usage
                .total
                .received_tokens,
            billable_before.received_tokens.saturating_add(2),
            "{label}"
        );
        assert!(
            !h.provider_runtime
                .cache_residency
                .tracks_prompt(&compact.agent_prompt_id),
            "{label}"
        );
        assert!(
            !h.prompt_coordination
                .prompt_runtime
                .context_size_alerts
                .contains_key(&compact.agent_prompt_id),
            "{label}"
        );
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            ActivationDispatchState::None
        ));
        let suppressed = h
            .matching_durable_failed_recovery(
                agent_id.as_str(),
                &"test/model".into(),
                head.map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
            )
            .expect("terminal failure remains durable suppression authority");
        assert_eq!(suppressed.transaction_id, transaction_id);
        assert_eq!(suppressed.cut, cut);
        assert_eq!(suppressed.resume_through, resume_through);
        assert!(
            !event_log_events(&h)
                .iter()
                .any(|event| matches!(event, Event::AgentCompacted(_))),
            "{label}"
        );
        assert!(
            event_log_events(&h).iter().any(|event| {
                matches!(event, Event::AgentStandaloneCompactionFailed(failed)
                if failed.transaction_id == transaction_id
                    && failed.cut == cut
                    && failed.resume_through == resume_through
                    && failed.reason == expected_reason
                    && if label == "length stop" {
                        failed.incomplete_response.as_ref().is_some_and(|incomplete| {
                            incomplete.agent_prompt_id == compact.agent_prompt_id
                                && incomplete.usage.as_ref().is_some_and(|usage| {
                                    usage.prompt_sent_tokens == 10
                                        && usage.response_received_tokens == 2
                                })
                                && incomplete.provider_response_id.as_deref()
                                    == Some("resp-standalone-terminal")
                                 && incomplete.backend.kind
                                     == tau_proto::ProviderBackendKind::PublicResponses
                                && !incomplete.output_items.is_empty()
                        })
                    } else {
                        failed.incomplete_response.is_none()
                    })
            }),
            "{label}"
        );
        if label == "length stop" {
            let records = h
                .session_runtime
                .agent_store
                .agent_events(agent_id.as_str())
                .expect("durable events")
                .to_vec();
            let expected = records
                .iter()
                .find_map(|record| match &record.event {
                    Event::AgentStandaloneCompactionFailed(failed) => {
                        failed.incomplete_response.clone()
                    }
                    _ => None,
                })
                .expect("durable incomplete payload");
            let cold =
                tau_core::AgentTree::try_from_events(crate::parse_agent_id(&agent_id), &records)
                    .expect("cold replay must preserve the non-context incomplete terminal");
            let cold_failure = cold
                .unresolved_standalone_compaction_failure(
                    &"test/model".into(),
                    stored_head_before
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                )
                .expect("cold unresolved failure");
            assert_eq!(cold_failure.incomplete_response.as_ref(), Some(&expected));
        }
        h.shutdown().expect("shutdown");
    }
}

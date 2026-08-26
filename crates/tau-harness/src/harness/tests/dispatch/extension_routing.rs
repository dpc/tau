//! Tests for extension routing behavior.

use super::*;

/// A side agent that receives `agent.message_received` while its original turn
/// is in flight must process that internal message before teardown. Otherwise
/// the `PromptOriginator::Extension` completion path removes the side
/// conversation and drops the queued delivery.
#[test]
fn side_agent_drains_agent_message_before_extension_teardown() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let delegate_events = connect_test_tool(&mut h, "conn-delegate");
    let parent = ensure_test_user_agent(&mut h);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("delegate-call".into(), parent);

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-message".to_owned(),
            instruction: "side task".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("delegate-call".into()),
            task_name: None,
        },
    )
    .expect("query");

    let (side_spid, side_cid) = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid.as_str() != "default").then(|| (spid.clone(), prompt_cid.clone()))
        })
        .expect("side prompt id");
    let recipient_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&side_cid)
        .and_then(|conv| conv.identity.agent_id.clone())
        .expect("side agent id");

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("test-message")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "please include this".to_owned(),
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: side_spid.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "initial answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("conn-delegate"),
            query_id: "q-message".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side first response");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&side_cid),
        "side conversation must stay alive to process queued agent.message_received"
    );
    assert!(
        delegate_events
            .lock()
            .expect("delegate events")
            .iter()
            .all(|routed| {
                !matches!(
                    peel_inner_event(&routed.frame),
                    Some(Event::StartAgentResult(_))
                )
            }),
        "start result must wait until the message turn completes"
    );
    let message_spid = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(spid, prompt_cid)| {
            (prompt_cid == &side_cid && spid != &side_spid).then_some(spid.clone())
        })
        .expect("message prompt dispatched");
    let prompt = read_prompt_created(&h, &message_spid);
    let serialized = serde_json::to_string(&prompt.context.flatten()).expect("json");
    assert!(serialized.contains("please include this"));

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: message_spid,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "final answer".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: crate::test_extension_name("conn-delegate"),
            query_id: "q-message".to_owned(),
        },
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("side message response");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .contains_key(&side_cid),
        "tool-backed side conversation stays targetable after message turn"
    );
    let events = delegate_events.lock().expect("delegate events");
    let result = events
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q-message" => Some(result),
            _ => None,
        })
        .expect("query result routed");
    assert_eq!(result.text, "final answer");
    h.shutdown().expect("shutdown");
}

#[test]
fn agent_prompt_created_uses_refs_for_linear_extension() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "hello");
    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_provider_response_finished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: spid1.clone(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,

            content: vec![ContentPart::Text {
                text: "hi".to_owned(),
            }],

            phase: None,
            responses_raw_json: None,
        })],

        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                prompt_cache_read_ceiling_tokens: None,
                cache: None,
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("finish first");

    append_user_message_via_event(&mut h, "s1", "again");
    let spid2 = h.send_prompt_to_agent("s1");
    let raw2 = read_raw_prompt_created(&h, &spid2);
    let prompt2 = read_prompt_created(&h, &spid2);
    assert!(raw2.tools_ref.is_none());
    assert_eq!(raw2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(raw2.context.flatten(), prompt2.context.flatten());
    assert_eq!(prompt2.tools, prompt1.tools);

    h.shutdown().expect("shutdown");
}

/// Internal extension prompt submissions must stay hidden while still producing
/// harness-owned prompt facts for timer and other wakeup extensions.
#[test]
fn extension_internal_prompt_submit_request_routes_as_internal_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "utils-ext",
        "utils-ext",
        tau_proto::ClientKind::Tool,
    );
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.handle_extension_event(
        "utils-ext",
        TestProtocolItem::Event(Event::ExtInternalPromptSubmitRequest(
            tau_proto::ExtInternalPromptSubmitRequest {
                agent_id: agent_id.clone(),
                text: "timer fired".to_owned(),
                ctx_id: Some("timer:wake:1".to_owned()),
                activation_kind: None,
            },
        )),
    )
    .expect("submit internal prompt request");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt)
            if prompt.agent_id == agent_id
                && prompt.text == "timer fired"
                && prompt.message_class == tau_proto::PromptMessageClass::Internal
                && prompt.submission_source
                    == tau_proto::PromptSubmissionSource::Extension {
                        name: tau_proto::ExtensionName::parse("utils-ext")
                            .expect("configured extension name"),
                    }
                && prompt.ctx_id.as_deref() == Some("timer:wake:1")
    )));

    h.shutdown().expect("shutdown");
}

/// Bad extension prompt targets must be rejected with user-visible harness
/// notice and must not create durable prompt facts for arbitrary agent ids.
#[test]
fn extension_internal_prompt_submit_request_rejects_unknown_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "utils-ext",
        "utils-ext",
        tau_proto::ClientKind::Tool,
    );

    h.handle_extension_event(
        "utils-ext",
        TestProtocolItem::Event(Event::ExtInternalPromptSubmitRequest(
            tau_proto::ExtInternalPromptSubmitRequest {
                agent_id: tau_proto::AgentId::parse("missing-agent").expect("agent id"),
                text: "hello".to_owned(),
                ctx_id: None,
                activation_kind: None,
            },
        )),
    )
    .expect("reject prompt request");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(info) if info.message.contains("unknown or unloaded agent")
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(prompt) if prompt.text == "hello"
    )));

    h.shutdown().expect("shutdown");
}

/// Queued extension internal-prompt requests preserve their ctx_id when they
/// are folded as steering messages, giving timer restore replayable fired
/// evidence.
#[test]
fn queued_extension_internal_prompt_submit_request_preserves_ctx_id_when_steered() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "utils-ext",
        "utils-ext",
        tau_proto::ClientKind::Tool,
    );
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: test_agent_prompt_id("busy-prompt"),
        },
    );
    h.handle_extension_event(
        "utils-ext",
        TestProtocolItem::Event(Event::ExtInternalPromptSubmitRequest(
            tau_proto::ExtInternalPromptSubmitRequest {
                agent_id: agent_id.clone(),
                text: "timer fired".to_owned(),
                ctx_id: Some("timer:wake:1".to_owned()),
                activation_kind: None,
            },
        )),
    )
    .expect("queue internal prompt request");
    h.fold_pending_prompts_as_steered(&cid);

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.agent_id == agent_id
                && steered.text == "timer fired"
                && steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.submission_source
                    == tau_proto::PromptSubmissionSource::Extension {
                        name: tau_proto::ExtensionName::parse("utils-ext")
                            .expect("configured extension name"),
                    }
                && steered.ctx_id.as_deref() == Some("timer:wake:1")
    )));

    h.shutdown().expect("shutdown");
}

/// Queued extension internal-prompt requests must keep their own ctx_id values;
/// a single per-agent scratch slot would make later queued prompts overwrite
/// earlier request correlation ids.
#[test]
fn queued_extension_internal_prompt_submit_requests_preserve_individual_ctx_ids() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "utils-ext",
        "utils-ext",
        tau_proto::ClientKind::Tool,
    );
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.set_agent_turn_state(
        &cid,
        AgentTurnState::AgentThinking {
            agent_prompt_id: test_agent_prompt_id("busy-prompt"),
        },
    );

    for (text, ctx_id) in [("first", "ctx-1"), ("second", "ctx-2")] {
        h.handle_extension_event(
            "utils-ext",
            TestProtocolItem::Event(Event::ExtInternalPromptSubmitRequest(
                tau_proto::ExtInternalPromptSubmitRequest {
                    agent_id: agent_id.clone(),
                    text: text.to_owned(),
                    ctx_id: Some(ctx_id.to_owned()),
                    activation_kind: None,
                },
            )),
        )
        .expect("queue prompt request");
    }

    let queued_ctx_ids: Vec<_> = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .pending_prompts
        .iter()
        .map(|prompt| prompt.ctx_id.as_deref())
        .collect();
    assert_eq!(queued_ctx_ids, vec![Some("ctx-1"), Some("ctx-2")]);

    h.set_agent_turn_state(&cid, AgentTurnState::Idle);
    h.try_advance_queue();
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_id == agent_id && created.ctx_id.as_deref() == Some("ctx-1")
    )));

    let first = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_text_response(
        &first.agent_prompt_id,
        first.agent_id,
        "first done",
    ))
    .expect("durably finish first checkpointed dispatch");
    h.try_advance_queue();
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_id == agent_id && created.ctx_id.as_deref() == Some("ctx-2")
    )));

    h.shutdown().expect("shutdown");
}

/// Ensures the exact-text editor retains `replace` only for ext-shell routing
/// and reports, while the provider-visible request and terminal facts use
/// `edit` for both successful and failed calls.
#[test]
fn exact_text_edit_alias_preserves_canonical_and_extension_lifecycle_names() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let file = td.path().join("exact-text.txt");
    std::fs::write(&file, "before\n").expect("seed exact-text file");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);

    let exact_text_arguments = |old_text: &str| {
        CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text(file.display().to_string()),
            ),
            (
                CborValue::Text("edits".to_owned()),
                CborValue::Array(vec![CborValue::Map(vec![
                    (
                        CborValue::Text("oldText".to_owned()),
                        CborValue::Text(old_text.to_owned()),
                    ),
                    (
                        CborValue::Text("newText".to_owned()),
                        CborValue::Text("after\n".to_owned()),
                    ),
                ])]),
            ),
        ])
    };
    let dispatch_response =
        |h: &mut Harness, prompt_id: AgentPromptId, call_id: &str, arguments: CborValue| {
            h.handle_provider_response_finished(ProviderResponseFinished {
                automatic_compaction_decision: None,
                output_length_disposition: tau_proto::OutputLengthDisposition::None,
                estimated_api_cost_rates: None,
                estimated_api_cost_increment: None,
                agent_prompt_id: prompt_id,
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.into(),
                    name: ToolName::new("edit"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments,
                    raw_arguments_json: None,
                    responses_envelope: None,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                failure_kind: None,
                context_limit_telemetry: None,
                recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                usage: None,
                originator: tau_proto::PromptOriginator::User,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_attempt: Default::default(),
                provider_response_id: None,
                ws_pool_delta: None,
            })
            .expect("dispatch visible edit");
        };

    let success_prompt = test_agent_prompt_id("exact-text-success-prompt");
    seed_agent_thinking(&mut h, &cid, success_prompt.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(success_prompt.clone(), cid.clone());
    dispatch_response(
        &mut h,
        success_prompt,
        "exact-text-success",
        exact_text_arguments("before\n"),
    );
    let successful_report = drive_harness_until_extension_tool_report(&mut h, "exact-text-success");
    drive_harness_until_tool_turn_empty(&mut h);
    assert_eq!(
        std::fs::read_to_string(&file).expect("read changed file"),
        "after\n"
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(request)
            if request.call_id.as_str() == "exact-text-success"
                && request.tool_name.as_str() == "edit"
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolStarted(started)
            if started.call_id.as_str() == "exact-text-success"
                && started.tool_name.as_str() == "replace"
    )));
    assert!(matches!(
        successful_report,
        Event::ToolResultReported(result)
            if result.tool_name.as_str() == "replace"
    ));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "exact-text-success"
                && result.tool_name.as_str() == "edit"
    )));

    h.shutdown().expect("shutdown successful-call harness");

    let mut h = echo_harness(td.path().join("error-state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let error_prompt = test_agent_prompt_id("exact-text-error-prompt");
    seed_agent_thinking(&mut h, &cid, error_prompt.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(error_prompt.clone(), cid.clone());
    dispatch_response(
        &mut h,
        error_prompt,
        "exact-text-error",
        exact_text_arguments("missing"),
    );
    let error_report = drive_harness_until_extension_tool_report(&mut h, "exact-text-error");
    drive_harness_until_tool_turn_empty(&mut h);
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(request)
            if request.call_id.as_str() == "exact-text-error"
                && request.tool_name.as_str() == "edit"
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolStarted(started)
            if started.call_id.as_str() == "exact-text-error"
                && started.tool_name.as_str() == "replace"
    )));
    assert!(matches!(
        error_report,
        Event::ToolErrorReported(error)
            if error.tool_name.as_str() == "replace"
    ));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error)
            if error.call_id.as_str() == "exact-text-error"
                && error.tool_name.as_str() == "edit"
    )));

    h.shutdown().expect("shutdown");
}

/// Extensions must not forge harness-owned or otherwise non-extension-owned
/// facts through the generic fallback `emit` path.
#[test]
fn inbound_non_extension_owned_fallback_events_are_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    for forged in [
        Event::SessionStarted(tau_proto::SessionStarted {
            session_id: test_session_id("forged-session"),
            reason: tau_proto::SessionStartReason::New,
        }),
        Event::SessionShutdown(tau_proto::SessionShutdown {
            session_id: test_session_id("forged-session"),
        }),
        Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: test_session_id("forged-session"),
            agent_id: crate::parse_agent_id("forged-agent"),
            ephemeral: false,
        }),
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: test_session_id("forged-session"),
            agent_id: crate::parse_agent_id("forged-agent"),
        }),
        Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::default()),

            parent_agent: None,
            agent_id: crate::parse_agent_id("forged-agent"),
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
        Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
            query_id: "delegate-0".to_owned(),
            agent_id: crate::parse_agent_id("forged-agent"),
        }),
        Event::StartAgentResult(tau_proto::StartAgentResult {
            query_id: "delegate-0".to_owned(),
            text: "forged result".to_owned(),
            error: None,
        }),
        Event::ToolDelegateProgress(tau_proto::DelegateProgress {
            call_id: "delegate-call".into(),
            task_name: "forged task".to_owned(),
            agent_id: Some(crate::parse_agent_id("forged-agent")),
            role: Some("engineer".to_owned()),
            ctx_percent: None,
            ctx_input_tokens: None,
            ctx_window: None,
            tools_in_flight: 0,
            tools_total: 0,
            display: None,
        }),
        Event::ProviderCacheMissDiagnostic(tau_proto::ProviderCacheMissDiagnostic {
            agent_prompt_id: test_agent_prompt_id("forged-prompt"),
            model: "provider/model".into(),
            originator: tau_proto::PromptOriginator::User,
            tool_choice: tau_proto::ToolChoice::default(),
            ws_pool_delta: None,
            input_tokens: 1,
            cached_tokens: 0,
            previous_input_tokens: 1,
            cacheable_input_tokens: 1,
            corrected_cache_efficiency: 0.0,
        }),
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id: crate::parse_agent_id("forged-agent"),
            text: "forged submitted".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        }),
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            inference_activation: false,
            agent_id: crate::parse_agent_id("forged-agent"),
            text: "forged injected".to_owned(),
            message_class: tau_proto::PromptMessageClass::Internal,
        }),
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            self_compaction_terminal: None,
            inference_activation: true,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            agent_id: crate::parse_agent_id("forged-agent"),
            text: "forged steered".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            ctx_id: None,
        }),
    ] {
        let baseline_seq = h.runtime_io.event_log.next_seq();
        h.handle_extension_message(
            &crate::test_connection_id("extension"),
            TestMessage::Emit(tau_proto::Emit {
                event: Box::new(forged.clone()),
                persist: true,
            }),
        )
        .expect("extension emit");
        assert!(
            h.runtime_io.event_log.get_next_from(baseline_seq).is_none(),
            "forged {} must not be published",
            forged.name()
        );
    }

    assert!(h.session_runtime.store.session("forged-session").is_none());
    assert!(
        h.session_runtime
            .agent_store
            .agent_events("forged-agent")
            .expect("agent events")
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// A restored extension request without a reconstructible requester route must
/// remain inspectable but reject watch and message admission before provider
/// work.
#[test]
fn cold_restore_classifies_unroutable_extension_worker_as_unavailable() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let (worker_agent_id, parent_agent_id) = {
        let mut h = echo_harness(&sp).expect("start");
        h.config.selected_model = Some("test/model".into());
        let _extension = connect_test_tool(&mut h, "external-delegate");
        let parent_cid = ensure_test_user_agent(&mut h);
        let parent_agent_id = durable_agent_id_for_conversation(&h, &parent_cid);
        h.tool_routing
            .tool_runtime
            .tool_agents
            .insert("external-call".into(), parent_cid);
        let mut query = ext_query("external-query");
        query.tool_call_id = Some("external-call".into());
        h.handle_start_agent_request(&crate::test_connection_id("external-delegate"), query)
            .expect("start extension worker");
        let worker_cid = ext_query_cid(&h, "external-query").expect("worker");
        let worker_agent_id = durable_agent_id_for_conversation(&h, &worker_cid);
        h.handle_authenticated_ui_prompt_submitted(
            crate::harness::harness_connection_id(),
            UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: "keep coordinating".to_owned(),
                agent_id: parent_agent_id.clone(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            },
        )
        .expect("leave coordinator interrupted");
        h.shutdown().expect("shutdown interrupted worker");
        (worker_agent_id, parent_agent_id)
    };

    let mut resumed =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert!(
        !resumed
            .agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(worker_agent_id.as_str())
    );
    assert!(
        resumed
            .agent_runtime
            .agent_registry
            .restored_unavailable
            .contains_key(worker_agent_id.as_str())
    );
    assert_eq!(
        resumed.agent_message_recipient_status(worker_agent_id.as_str()),
        crate::harness::AgentMessageRecipientStatus::RestoredUnavailable
    );
    let watch_error = resumed
        .try_set_agent_watch(
            parent_agent_id.as_str(),
            worker_agent_id.as_str(),
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        )
        .expect_err("unavailable target must reject watch");
    assert!(watch_error.contains("cannot resume its pre-restart delegation"));
    let parent_cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(parent_agent_id.as_str())
        .cloned()
        .expect("parent route");
    let message_error = resumed
        .publish_agent_message_from_agent(
            &parent_cid,
            worker_agent_id.to_string(),
            "must not run".to_owned(),
        )
        .expect_err("unavailable target must reject message");
    assert!(message_error.contains("cannot resume its pre-restart delegation"));
    assert!(
        resumed
            .prompt_coordination
            .prompt_runtime
            .agents
            .values()
            .all(|cid| cid != &crate::parse_agent_id(worker_agent_id.as_str()))
    );
    let summaries =
        path_crate_internal_tools::InternalToolHost::new(&mut resumed).current_agent_summaries();
    assert!(summaries.iter().any(|summary| {
        summary.agent_id == worker_agent_id.as_str()
            && summary.state == path_crate_internal_tools::InternalAgentState::RestoredUnavailable
    }));
    resumed.remove_agent(&parent_cid);
    let summaries =
        path_crate_internal_tools::InternalToolHost::new(&mut resumed).current_agent_summaries();
    assert!(
        summaries
            .iter()
            .any(|summary| summary.agent_id == parent_agent_id.as_str()
                && summary.state == path_crate_internal_tools::InternalAgentState::Stopped)
    );
    resumed.shutdown().expect("shutdown resumed harness");
}

/// Pre-accounting journals can lack `AgentStarted.creator`; an extension
/// originator alone must never count as a reconstructible completion route.
#[test]
fn cold_restore_classifies_legacy_extension_worker_without_creator_as_unavailable() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let agent_id = tau_proto::AgentId::parse("legacy-extension-worker").expect("agent id");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let mut session_store = tau_core::SessionStore::open(&sessions_dir).expect("session store");
    session_store
        .record_session_meta("s1")
        .expect("session metadata");
    session_store
        .append_session_event(
            "s1",
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("legacy-init")
                    .expect("initialization id"),
                session_id: test_session_id("s1"),
                agent_id: agent_id.clone(),
                ephemeral: false,
            }),
        )
        .expect("membership");
    let mut agent_store = tau_core::AgentStore::open(sp.join("agents")).expect("agent store");
    for event in [
        Event::AgentStarted(tau_proto::AgentStarted {
            agent_id: agent_id.clone(),
            creator: None,
            parent_agent: None,
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id.clone(),
            text: "unfinished legacy extension work".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::Extension {
                name: crate::test_extension_name("legacy-extension"),
                query_id: "legacy-query".to_owned(),
            },
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    ] {
        agent_store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                event,
                tau_proto::UnixMicros::now(),
            )
            .expect("legacy event");
    }
    drop(agent_store);
    drop(session_store);

    let mut h = echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    assert!(
        !h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(agent_id.as_str())
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .restored_unavailable
            .contains_key(agent_id.as_str())
    );
    assert_eq!(
        h.agent_message_recipient_status(agent_id.as_str()),
        crate::harness::AgentMessageRecipientStatus::RestoredUnavailable
    );
    h.shutdown().expect("shutdown");
}

//! Dispatcher regressions for provider-visible automatic compaction projection.

use super::*;

fn append_projection_fixture_text(h: &mut Harness, cid: &AgentId, bytes: usize, marker: &str) {
    let agent_id = durable_agent_id_for_conversation(h, cid);
    h.publish_for_agent(
        cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: format!("{marker}:{}", "x".repeat(bytes)),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
}

fn set_projection_fixture_agents_message(h: &mut Harness, cid: &AgentId, message: String) {
    let agent_id = durable_agent_id_for_conversation(h, cid);
    h.publish_for_agent(
        cid,
        Event::AgentInitializationContextSet(tau_proto::AgentInitializationContextSet {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id,
            agent_initialization_id: tau_proto::AgentInitializationId::parse("init-byte-fit")
                .expect("test initialization id"),
            agents_message: Some(message),
            effective_skills: Vec::new(),
            agents_files: Vec::new(),
        }),
    );
}

/// Standalone prefix fitting must include the exact initialization block that
/// prompt materialization prepends before provider dispatch.
///
/// Both cases reproduce the incident's planner measurements and 4,304-byte
/// materialization increase. The corrected planner must retreat to a safe
/// closed prefix, include initialization exactly once, and dispatch rather
/// than synthesize a local context-window terminal.
#[test]
fn standalone_auto_compaction_fits_fully_materialized_incident_prefixes() {
    const PREFIX_BUDGET: u64 = 334_800;
    const INITIALIZATION_INCREASE: u64 = 4_304;

    for incident_planner_bytes in [334_055_u64, 333_918] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        append_projection_fixture_text(&mut h, &cid, 100, "safe-prefix");

        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        let safe_head = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("safe prefix head");
        let tree = h
            .session_runtime
            .agent_store
            .agent(&agent_id)
            .expect("agent tree");
        let safe_context = crate::prompt::assemble_prompt_context_prefix_from(
            tree,
            Some(safe_head),
            tau_proto::AgentHead::Node(safe_head),
        )
        .expect("safe context")
        .context;
        let empty_candidate = tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
            items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::User,
                content: vec![ContentPart::Text {
                    text: "incident-prefix:".to_owned(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        });
        let mut candidate_context = safe_context.clone();
        candidate_context.blocks.push(empty_candidate);
        let empty_candidate_bytes = u64::try_from(
            serde_json::to_vec(&candidate_context)
                .expect("serialize")
                .len(),
        )
        .expect("test length fits u64");
        let incident_payload_bytes = incident_planner_bytes
            .checked_sub(empty_candidate_bytes)
            .expect("incident target exceeds fixed context bytes");
        append_projection_fixture_text(
            &mut h,
            &cid,
            usize::try_from(incident_payload_bytes).expect("test length fits usize"),
            "incident-prefix",
        );
        let incident_head = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("incident prefix head");

        let tree = h
            .session_runtime
            .agent_store
            .agent(&agent_id)
            .expect("agent tree");
        let incident_context = crate::prompt::assemble_prompt_context_prefix_from(
            tree,
            Some(incident_head),
            tau_proto::AgentHead::Node(incident_head),
        )
        .expect("incident context")
        .context;
        assert_eq!(
            u64::try_from(
                serde_json::to_vec(&incident_context)
                    .expect("serialize")
                    .len()
            )
            .expect("test length fits u64"),
            incident_planner_bytes,
            "fixture must reproduce the pre-fix planner measurement"
        );

        let empty_initialization = tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
            items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::User,
                content: vec![ContentPart::Text {
                    text: String::new(),
                }],
                phase: None,
                responses_raw_json: None,
            })],
        });
        let mut with_empty_initialization = incident_context.clone();
        with_empty_initialization
            .blocks
            .insert(0, empty_initialization);
        let empty_initialization_increase = u64::try_from(
            serde_json::to_vec(&with_empty_initialization)
                .expect("serialize")
                .len(),
        )
        .expect("test length fits u64")
            - incident_planner_bytes;
        let agents_message_bytes = INITIALIZATION_INCREASE
            .checked_sub(empty_initialization_increase)
            .expect("initialization target exceeds fixed block bytes");
        set_projection_fixture_agents_message(
            &mut h,
            &cid,
            "a".repeat(usize::try_from(agents_message_bytes).expect("test length fits usize")),
        );

        let tree = h
            .session_runtime
            .agent_store
            .agent(&agent_id)
            .expect("agent tree");
        let mut materialized_incident = incident_context;
        materialized_incident.blocks.insert(
            0,
            crate::prompt::initialization_agents_context_block(tree)
                .expect("initialization context block"),
        );
        assert_eq!(
            u64::try_from(
                serde_json::to_vec(&materialized_incident)
                    .expect("serialize")
                    .len()
            )
            .expect("test length fits u64"),
            incident_planner_bytes + INITIALIZATION_INCREASE,
            "fixture must reproduce the adapter's fully materialized measurement"
        );

        {
            let info = h
                .provider_runtime
                .model_info
                .get_mut(&"test/model".into())
                .expect("test model");
            info.supports_compaction = false;
            info.supports_standalone_compaction = true;
            info.standalone_compaction_threshold = Some(1);
            info.standalone_compaction_prefix_budget = Some(PREFIX_BUDGET);
        }
        assert_eq!(
            h.fitting_automatic_compaction_cut(
                &agent_id,
                tau_proto::AgentHead::Node(incident_head),
                None,
                PREFIX_BUDGET,
            ),
            Some(tau_proto::AgentHead::Node(safe_head)),
            "fully materialized over-limit candidate must retreat"
        );

        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue".to_owned()))
            .expect("dispatch safe compaction prefix");
        let compact = read_nth_prompt_created(&h, 0);
        assert_eq!(
            compact.operation,
            tau_proto::PromptOperation::StandaloneCompaction
        );
        assert_eq!(
            compact
                .context
                .blocks
                .iter()
                .filter(|block| {
                    **block
                        == crate::prompt::initialization_agents_context_block(
                            h.session_runtime
                                .agent_store
                                .agent(&agent_id)
                                .expect("agent tree"),
                        )
                        .expect("initialization block")
                })
                .count(),
            1,
            "prompt materialization must include initialization exactly once"
        );
        let mut historical_context = compact.context.clone();
        let trailing = historical_context
            .blocks
            .pop()
            .expect("standalone compaction trigger block");
        assert_eq!(
            trailing,
            tau_proto::ContextBlock::UserInput(tau_proto::UserInputBlock {
                items: vec![ContextItem::CompactionTrigger],
            })
        );
        let historical_bytes = u64::try_from(
            serde_json::to_vec(&historical_context)
                .expect("serialize historical prefix")
                .len(),
        )
        .expect("test length fits u64");
        let planned_bytes = crate::prompt::active_prompt_prefix_json_measurements(
            h.session_runtime
                .agent_store
                .agent(&agent_id)
                .expect("agent tree"),
            Some(incident_head),
        )
        .expect("prefix measurements")
        .into_iter()
        .find_map(|(node_id, bytes)| (node_id == safe_head).then_some(bytes))
        .expect("selected safe-prefix measurement");
        assert_eq!(
            planned_bytes, historical_bytes,
            "planner and materialized adapter prefix must have identical request shape"
        );
        assert!(
            historical_bytes <= PREFIX_BUDGET,
            "the dispatched historical prefix must pass adapter admission"
        );
        h.shutdown().expect("shutdown");
    }
}

/// A same-model provider usage baseline must prevent the live incident's first
/// premature automatic compaction.
///
/// This mirrors IFku transaction ct-18's boundary: the old projection crossed
/// 334,800 by only 83 fake tokens while provider input was 57,552. Only the
/// provider-visible suffix after that exact head may be added.
#[test]
fn standalone_auto_compaction_uses_provider_baseline_for_initial_incident_window() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    append_projection_fixture_text(&mut h, &cid, 326_099, "baseline-prefix");
    let baseline_head = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("baseline head");
    {
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(57_552);
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_head = Some(baseline_head);
    }
    append_projection_fixture_text(&mut h, &cid, 4_324, "incident-suffix");
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let head = h.agent_runtime.agent_registry.agents[&cid].identity.head;
    let raw_projection =
        path_crate_harness::compaction_runtime::active_provider_window_projected_tokens(
            h.session_runtime
                .agent_store
                .agent(&agent_id)
                .expect("agent tree"),
            head,
            353_400,
        )
        .expect("fallback projection");
    assert_eq!(
        raw_projection, 334_883,
        "fixture must reproduce ct-18's 83-fake-token threshold crossing"
    );
    {
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.context_window = 353_400;
        info.standalone_compaction_threshold = Some(334_800);
        info.standalone_compaction_prefix_budget = Some(334_800);
        info.tags.push(tau_proto::ModelTag::new("shell:chatgpt"));
    }
    assert_eq!(
        h.automatic_compaction_projected_tokens(&cid, &"test/model".into()),
        Some(66_154),
        "provider baseline plus the exact new suffix matches the reconstructed incident projection"
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue".to_owned()))
        .expect("dispatch inference");
    assert_eq!(
        read_nth_prompt_created(&h, 0).operation,
        tau_proto::PromptOperation::Inference,
        "same-model baseline plus suffix remains far below the threshold"
    );
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentStandaloneCompactionStarted(_)
        )),
        0
    );
    h.shutdown().expect("shutdown");
}

/// A successful partial compaction must use its compacted-token baseline for
/// multipass admission instead of charging the opaque replacement twice.
///
/// This reproduces the incident shape: a byte-budget cut preserves a large
/// suffix and the replacement retains matching typed and raw opaque payloads.
/// Both the compacted-baseline path and the corrected from-scratch projection
/// remain below 334,800 and resume inference.
#[test]
fn standalone_auto_compaction_uses_compacted_baseline_for_opaque_multipass_window() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    append_projection_fixture_text(&mut h, &cid, 100_000, "compacted-prefix");
    append_projection_fixture_text(&mut h, &cid, 300_000, "preserved-suffix");
    {
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.context_window = 353_400;
        info.standalone_compaction_threshold = Some(334_800);
        info.standalone_compaction_prefix_budget = Some(334_800);
        info.tags.push(tau_proto::ModelTag::new("shell:chatgpt"));
    }
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("continue".to_owned()))
        .expect("start initial compaction");
    let compact = read_nth_prompt_created(&h, 0);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    let opaque_json = format!(
        r#"{{"type":"compaction","encrypted_content":"{}"}}"#,
        "z".repeat(17_800)
    );
    let opaque_value = CborValue::Map(vec![
        (
            CborValue::Text("type".to_owned()),
            CborValue::Text("compaction".to_owned()),
        ),
        (
            CborValue::Text("encrypted_content".to_owned()),
            CborValue::Text("z".repeat(17_800)),
        ),
    ]);
    let mut response = standalone_compaction_success_response(&compact, "unused");
    response.output_items = vec![
        ContextItem::Message(MessageItem {
            role: ContextRole::User,
            content: vec![ContentPart::Text {
                text: format!("client-retained:{}", "r".repeat(10_000)),
            }],
            phase: None,
            responses_raw_json: None,
        }),
        ContextItem::Compaction(tau_proto::OpaqueProviderItem::with_raw_json(
            opaque_value,
            opaque_json,
        )),
    ];
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some("test/model".into()),
        prompt_sent_tokens: 33_604,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 2_748,
        stats: Default::default(),
    });
    h.handle_provider_response_finished(response)
        .expect("accept compaction");

    let inference = read_nth_prompt_created(&h, 1);
    assert_eq!(
        inference.operation,
        tau_proto::PromptOperation::Inference,
        "compacted baseline plus preserved suffix must not start a rolling pass"
    );
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentStandaloneCompactionStarted(_)
        )),
        1,
        "successful partial compaction must not immediately retrigger"
    );
    let live_projection = h
        .automatic_compaction_projected_tokens(&cid, &"test/model".into())
        .expect("live compacted baseline projection");
    assert!(
        2_748 + 10_000 < live_projection,
        "client-retained replacement messages must remain charged: {live_projection}"
    );
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let from_scratch_projection =
        path_crate_harness::compaction_runtime::active_provider_window_projected_tokens(
            h.session_runtime
                .agent_store
                .agent(&agent_id)
                .expect("agent tree"),
            h.agent_runtime.agent_registry.agents[&cid].identity.head,
            353_400,
        )
        .expect("from-scratch projection");
    assert!(
        from_scratch_projection < 334_800,
        "from-scratch scheduling must also charge one opaque replay representation: {from_scratch_projection}"
    );
    h.shutdown().expect("shutdown");
    wait_for_session_unlock(&state, "s1");

    let mut restored =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    enable_remote_compaction_for_test_model(&mut restored);
    {
        let info = restored
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.context_window = 353_400;
        info.standalone_compaction_threshold = Some(334_800);
        info.standalone_compaction_prefix_budget = Some(334_800);
        info.tags.push(tau_proto::ModelTag::new("shell:chatgpt"));
    }
    let restored_cid = ensure_test_user_agent(&mut restored);
    let cold_projection = restored
        .automatic_compaction_projected_tokens(&restored_cid, &"test/model".into())
        .expect("cold compaction baseline projection");
    assert!(
        cold_projection < 334_800,
        "cold replay must retain the compacted baseline rather than retrigger: {cold_projection}"
    );
    assert_eq!(
        cold_projection, live_projection,
        "live and cold replay must charge the same ChatGPT-v2 retained prefix"
    );
    assert_eq!(
        restored
            .session_runtime
            .agent_store
            .agent(&agent_id)
            .expect("restored agent tree")
            .active_provider_compaction(
                restored.agent_runtime.agent_registry.agents[&restored_cid]
                    .identity
                    .head
            )
            .and_then(|compacted| compacted.compacted_input_tokens.as_ref())
            .map(|measurement| measurement.tokens),
        Some(2_748),
        "cold fold must recover the exact durable compacted-token baseline"
    );
    restored.shutdown().expect("shutdown restored harness");
}

/// Coarse estimated and zero compacted measurements must retain the
/// conservative full-window fallback instead of suppressing scheduling.
#[test]
fn automatic_compaction_rejects_inexact_or_zero_compacted_baselines() {
    use tau_proto::{CompactionTokenMeasurement, CompactionTokenProvenance};

    for measurement in [
        CompactionTokenMeasurement {
            tokens: 25_000,
            provenance: CompactionTokenProvenance::Estimated,
        },
        CompactionTokenMeasurement {
            tokens: 0,
            provenance: CompactionTokenProvenance::ProviderReported,
        },
    ] {
        assert_eq!(
            path_crate_harness::compaction_runtime::scheduling_compacted_input_tokens(&measurement),
            None
        );
    }
    assert_eq!(
        path_crate_harness::compaction_runtime::scheduling_compacted_input_tokens(
            &CompactionTokenMeasurement {
                tokens: 2_748,
                provenance: CompactionTokenProvenance::ProviderReported,
            }
        ),
        Some(2_748)
    );
}

/// Provider-reported usage already covers a message-only local-summary
/// replacement, while ChatGPT-v2 client-retained messages before its opaque
/// compaction item require a separate conservative charge.
#[test]
fn automatic_compaction_supplements_only_opaque_retained_prefixes() {
    let retained = ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text {
            text: "client retained".repeat(100),
        }],
        phase: None,
        responses_raw_json: None,
    });
    assert_eq!(
        path_crate_harness::compaction_runtime::projected_client_retained_replacement_tokens(
            std::slice::from_ref(&retained),
            true,
        ),
        Some(0),
        "provider-reported local-summary output must not be charged twice"
    );
    let opaque = ContextItem::Compaction(tau_proto::OpaqueProviderItem::new(CborValue::Map(
        Vec::new(),
    )));
    let mixed = [retained, opaque];
    assert_eq!(
        path_crate_harness::compaction_runtime::projected_client_retained_replacement_tokens(
            &mixed, false,
        ),
        Some(0),
        "ordinary provider-owned mixed output must not be charged twice"
    );
    assert!(
        path_crate_harness::compaction_runtime::projected_client_retained_replacement_tokens(
            &mixed, true,
        )
        .is_some_and(|tokens| 0 < tokens),
        "ChatGPT-v2's client-retained prefix must remain conservatively charged"
    );
}

/// Large duplicated tool payloads must not schedule automatic compaction when
/// their provider-visible projection remains below the token threshold.
///
/// Durable prompt JSON repeats the raw result representation and exceeds the
/// historical 334,800-byte false boundary here. The dispatcher must instead
/// use the active provider window's token projection, as telemetry does.
#[test]
fn standalone_auto_compaction_ignores_duplicated_raw_tool_payload_bytes() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.publish_for_agent(
        &cid,
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: crate::parse_agent_id(&agent_id),
            text: "a".repeat(5_000),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
    );
    let calls = (0..6)
        .map(|index| (format!("call-{index}"), format!("tool_{index}")))
        .collect::<Vec<_>>();
    seed_assistant_tool_round(
        &mut h,
        &cid,
        &calls
            .iter()
            .map(|(call_id, tool_name)| (call_id.as_str(), tool_name.as_str()))
            .collect::<Vec<_>>(),
    );
    for (call_id, tool_name) in &calls {
        h.publish_for_agent(
            &cid,
            Event::ProviderToolResult(ToolResult {
                presentation: Default::default(),
                call_id: call_id.clone().into(),
                tool_name: ToolName::new(tool_name),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("r".repeat(37_000)),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        );
    }
    let head = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("tool results head");
    let tree = h
        .session_runtime
        .agent_store
        .agent(&agent_id)
        .expect("agent tree");
    let active_bytes =
        serde_json::to_vec(&crate::prompt::assemble_prompt_context_from(tree, Some(head)).context)
            .expect("serialize active prompt")
            .len() as u64;
    let active_projection =
        path_crate_harness::compaction_runtime::active_provider_window_projected_tokens(
            tree,
            Some(head),
            1_000,
        )
        .expect("project active provider window");
    assert!(
        active_bytes > 334_800,
        "fixture must exceed the former serialized-byte scheduling boundary"
    );
    assert!(
        active_projection < 334_800,
        "fixture must retain a smaller provider-visible projection"
    );
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(334_800);
    assert!(
        !h.schedule_standalone_auto_compaction_for_activation(&cid, true, None),
        "raw durable bytes must not create an automatic compaction transaction"
    );
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.shutdown().expect("shutdown");
}

/// Outbound agent-message routing facts must not schedule compaction because
/// prompt assembly deliberately omits them from the receiving provider window.
///
/// This prevents large sent-message bodies after an exact usage head from
/// being charged as baseline suffix growth.
#[test]
fn standalone_auto_compaction_ignores_outbound_agent_message_payloads() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    {
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(100);
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_head = agent.identity.head;
    }
    h.publish_for_agent(
        &cid,
        Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse("large-outbound-message")
                .expect("test message id"),
            sender_id: agent_id.clone(),
            recipient: tau_proto::AgentMessageRecipient::Agent {
                agent_id: crate::parse_agent_id("recipient-agent"),
            },
            kind: tau_proto::AgentMessageKind::Message,
            message: "ignored outbound routing fact ".repeat(10_000),
        }),
    );
    let telemetry = h.prompt_context_limit_snapshot(
        &cid,
        &"test/model".into(),
        tau_proto::PromptOperation::Inference,
    );
    assert_eq!(
        telemetry.projected_input_tokens,
        Some(
            100 + path_crate_harness::context_limit_telemetry::context_projection_reserve(
                h.provider_runtime.model_info[&"test/model".into()].context_window,
            )
        ),
        "provider-visible projection must omit the outbound routing fact"
    );
    assert!(
        telemetry
            .transcript_delta_bytes
            .is_some_and(|bytes| bytes > 100_000),
        "exact durable suffix telemetry must retain the outbound routing fact"
    );
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(10_000);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("ordinary prompt".to_owned()))
        .expect("dispatch ordinary prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(prompt.operation, tau_proto::PromptOperation::Inference);
    assert!(
        !serde_json::to_string(&prompt.context)
            .expect("serialize provider context")
            .contains("ignored outbound routing fact")
    );
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStandaloneCompactionStarted(_)))
    );
    h.shutdown().expect("shutdown");
}

/// Raw Responses replay sidecars must not make eager-terminal or later
/// activation scheduling charge the same assistant payload twice.
#[test]
fn automatic_compaction_charges_one_assistant_replay_representation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let threshold = 250_000;
    {
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.context_window = 500_000;
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(threshold);
        info.standalone_compaction_prefix_budget = Some(500_000);
    }
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("one replay".to_owned()))
        .expect("dispatch inference");
    let prompt = read_nth_prompt_created(&h, 0);
    let payload = "z".repeat(170_000);
    let mut response = standalone_compaction_success_response(&prompt, &payload);
    let ContextItem::Message(message) = &mut response.output_items[0] else {
        unreachable!("response helper returns one message")
    };
    message.responses_raw_json = Some(format!(
        r#"{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{payload}"}}]}}"#
    ));
    h.handle_provider_response_finished(response)
        .expect("accept assistant response");

    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentStandaloneCompactionStarted(_)
        )),
        0,
        "eager terminal projection must remain below the threshold"
    );
    let later_projection = h
        .automatic_compaction_projected_tokens(&cid, &"test/model".into())
        .expect("later provider-window projection");
    assert!(
        later_projection < threshold,
        "later scheduling must also charge one replay representation: {later_projection}"
    );
    h.shutdown().expect("shutdown");
}

/// Automatic multipass admission must measure the replacement window and
/// remaining suffix from scratch at its exact token boundary.
///
/// A first calibration pass records the post-compaction projection. Replaying
/// the same pass one token below its threshold must resume inference directly;
/// equality must schedule exactly one continuation rather than comparing the
/// much larger durable prompt JSON or an older usage snapshot.
#[test]
fn standalone_auto_compaction_multipass_uses_exact_active_projection_boundary() {
    let start_first_pass = || {
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
        info.standalone_compaction_threshold = Some(500);
        info.standalone_compaction_prefix_budget = Some(1_000);
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        for marker in ["old-A", "old-B"] {
            h.publish_for_agent(
                &cid,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    inference_activation: false,
                    agent_id: agent_id.clone(),
                    text: format!("{marker}:{}", "x".repeat(600)),
                    trusted_internal_spans: Vec::new(),
                    message_class: tau_proto::PromptMessageClass::User,
                    internal_kind: None,
                    originator: tau_proto::PromptOriginator::User,
                    submission_source: Default::default(),
                    display_name: None,
                    ctx_id: None,
                }),
            );
        }
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("pending suffix".to_owned()))
            .expect("start bounded pass");
        let first = read_nth_prompt_created(&h, 0);
        (td, h, cid, first)
    };
    let finish_first_pass = |h: &mut Harness, first: AgentPromptCreated| {
        h.handle_provider_response_finished(provider_text_response(
            &first.agent_prompt_id,
            first.agent_id,
            "summary-one",
        ))
        .expect("finish first pass");
    };

    let (_calibration_dir, mut calibration, calibration_cid, calibration_first) =
        start_first_pass();
    calibration
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(0);
    finish_first_pass(&mut calibration, calibration_first);
    let calibration_agent_id = durable_agent_id_for_conversation(&calibration, &calibration_cid);
    let calibration_head = event_log_events(&calibration)
        .into_iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                ) =>
            {
                started.resume_through
            }
            _ => None,
        })
        .expect("calibration continuation resume head");
    let projection =
        path_crate_harness::compaction_runtime::active_provider_window_projected_tokens(
            calibration
                .session_runtime
                .agent_store
                .agent(&calibration_agent_id)
                .expect("agent tree"),
            calibration_head.as_option(),
            1_000,
        )
        .expect("post-compaction projection");
    assert!(
        projection > 0,
        "fixture must produce a nonempty active window"
    );
    calibration.shutdown().expect("shutdown calibration");

    let (_below_dir, mut below, _below_cid, below_first) = start_first_pass();
    below
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(projection + 1);
    finish_first_pass(&mut below, below_first);
    assert_eq!(
        read_nth_prompt_created(&below, 1).operation,
        tau_proto::PromptOperation::Inference,
        "one token below the active projection must not start a continuation"
    );
    assert_eq!(
        event_log_count(&below, |event| matches!(
            event,
            Event::AgentStandaloneCompactionStarted(_)
        )),
        1,
        "one-below admission must retain only the initial pass"
    );
    below.shutdown().expect("shutdown one-below");

    let (_equal_dir, mut equal, _equal_cid, equal_first) = start_first_pass();
    equal
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(projection);
    finish_first_pass(&mut equal, equal_first);
    assert_eq!(
        read_nth_prompt_created(&equal, 1).operation,
        tau_proto::PromptOperation::StandaloneCompaction,
        "the exact active projection boundary must continue compaction"
    );
    assert_eq!(
        event_log_count(&equal, |event| matches!(
            event,
            Event::AgentStandaloneCompactionStarted(_)
        )),
        2,
        "equality must schedule exactly one continuation"
    );
    equal.shutdown().expect("shutdown equality");
}

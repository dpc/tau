//! Dispatcher regressions for provider-visible automatic compaction projection.

use super::*;

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
/// This prevents large sent-message bodies from being charged as transcript
/// input after the scheduler switched from durable JSON bytes to active-window
/// projections.
#[test]
fn standalone_auto_compaction_ignores_outbound_agent_message_payloads() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
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

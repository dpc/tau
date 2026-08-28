//! Tests agent-scoped manual-compaction runtime correlation.

use super::*;

/// Equal target-local request ids must survive pending-to-accepted commit for
/// two agents and retain each original self-tool continuation independently.
#[test]
fn equal_local_model_request_ids_commit_and_continue_for_both_targets() {
    let (_td, mut h, _caller_cid, target_a_cid, _unused_call, target_a_id) =
        setup_manual_cross_compaction_test();
    let (target_b_cid, target_b_id) =
        install_manual_compaction_target(&mut h, "other-compaction-target");
    connect_test_tool(&mut h, "equal-model-acceptance");
    h.handle_extension_event(
        "equal-model-acceptance",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");

    let calls = [
        (target_a_cid.clone(), "call-equal-a", "call-equal-sibling-a"),
        (target_b_cid.clone(), "call-equal-b", "call-equal-sibling-b"),
    ];
    for (cid, compact_call_id, sibling_call_id) in &calls {
        seed_assistant_tool_round(
            &mut h,
            cid,
            &[(compact_call_id, "compact"), (sibling_call_id, "sibling")],
        );
        for (call_id, name) in [(*compact_call_id, "compact"), (*sibling_call_id, "sibling")] {
            h.tool_routing
                .tool_runtime
                .tool_agents
                .insert(call_id.into(), cid.clone());
            h.tool_routing.tool_runtime.pending_tools.insert(
                call_id.into(),
                PendingTool {
                    name: ToolName::new(name),
                    internal_name: ToolName::new(name),
                    tool_type: tau_proto::ToolType::Function,
                    allows_provider_image: false,
                },
            );
            h.tool_routing
                .tool_runtime
                .tool_turn
                .record_unqueued_in_flight(
                    cid.clone(),
                    call_id.into(),
                    ToolTurnCategories::default(),
                );
            h.prompt_coordination
                .prompt_runtime
                .tool_call_prompts
                .insert(call_id.into(), test_agent_prompt_id("sp-seeded-tools"));
        }
        h.request_agent_tool_compaction(
            cid,
            &AgentToolCall {
                call_ref: None,
                id: (*compact_call_id).into(),
                name: ToolName::new("compact"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            },
            ToolName::new("compact"),
            None,
        );
    }
    let staged = h
        .prompt_coordination
        .compaction_runtime
        .pending_manual_acceptances
        .values()
        .map(|pending| pending.request())
        .collect::<Vec<_>>();
    assert_eq!(staged.len(), 2);
    assert_eq!(staged[0].request_id, staged[1].request_id);

    for _ in 0..2 {
        h.handle_extension_event(
            "equal-model-acceptance",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit one acceptance");
    }
    assert_eq!(
        h.prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .len(),
        2
    );
    assert!(calls.iter().all(|(_, call_id, _)| {
        h.tool_routing
            .tool_runtime
            .tool_turn
            .is_backgrounded(&ToolCallId::from(*call_id))
    }));

    for (cid, _, sibling_call_id) in &calls {
        h.finish_prebuilt_internal_tool_result(ToolResult {
            presentation: Default::default(),
            call_id: (*sibling_call_id).into(),
            tool_name: ToolName::new("sibling"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        });
        assert!(
            h.prompt_coordination
                .compaction_runtime
                .active_manual_transactions
                .keys()
                .any(|transaction| transaction.belongs_to(cid))
        );
    }
    let starts = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    assert!(starts.iter().any(|started| {
        started.agent_id == target_a_id
            && matches!(
                &started.trigger,
                tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                    initiating_tool_call_id,
                    ..
                } if initiating_tool_call_id.as_str() == "call-equal-a"
            )
    }));
    assert!(starts.iter().any(|started| {
        started.agent_id == target_b_id
            && matches!(
                &started.trigger,
                tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                    initiating_tool_call_id,
                    ..
                } if initiating_tool_call_id.as_str() == "call-equal-b"
            )
    }));
}

/// Tearing down one append-rejected equal-id acceptance must preserve the
/// other target's intercepted publication and original call owner.
#[test]
fn equal_local_model_request_ids_teardown_preserves_other_retained_publication() {
    let (_td, mut h, caller_cid, target_a_cid, call_a, target_a_id) =
        setup_manual_cross_compaction_test();
    let (_target_b_cid, target_b_id) =
        install_manual_compaction_target(&mut h, "other-retained-target");
    let call_b = register_manual_cross_compaction_call(&mut h, &caller_cid, "call-retained-b");
    connect_test_tool(&mut h, "equal-retained-acceptance");
    h.handle_extension_event(
        "equal-retained-acceptance",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");
    h.request_agent_tool_compaction(
        &caller_cid,
        &call_a,
        ToolName::new("agent_compact"),
        Some(&target_a_id),
    );
    h.request_agent_tool_compaction(
        &caller_cid,
        &call_b,
        ToolName::new("agent_compact"),
        Some(&target_b_id),
    );
    let staged = h
        .prompt_coordination
        .compaction_runtime
        .pending_manual_acceptances
        .values()
        .map(|pending| pending.request())
        .collect::<Vec<_>>();
    assert_eq!(staged.len(), 2);
    assert_eq!(staged[0].request_id, staged[1].request_id);

    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "equal-retained-acceptance",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("retain rejected first acceptance");
    h.remove_agent_expected(&target_a_cid);
    assert_eq!(
        h.prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .values()
            .next()
            .expect("other target remains")
            .request()
            .target_agent_id,
        target_b_id
    );
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentManualCompactionRequested(request))
            if request.target_agent_id == target_b_id
    ));
    h.handle_extension_event(
        "equal-retained-acceptance",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit preserved acceptance");
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .model_tool_start_by_call(&call_b.id)
            .is_some()
    );
}

/// Equal target-local UI ids must keep separate requester ACK ownership through
/// each target's durable acceptance commit.
#[test]
fn equal_local_ui_request_ids_ack_each_target_requester() {
    let (_td, mut h, _caller_cid, _target_a_cid, _unused_call, target_a_id) =
        setup_manual_cross_compaction_test();
    let (_target_b_cid, target_b_id) = install_manual_compaction_target(&mut h, "other-ui-target");
    let frames_a = connect_test_client(&mut h, "equal-ui-a", tau_proto::ClientKind::Ui);
    let frames_b = connect_test_client(&mut h, "equal-ui-b", tau_proto::ClientKind::Ui);
    connect_test_tool(&mut h, "equal-ui-acceptance");
    h.handle_extension_event(
        "equal-ui-acceptance",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUESTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register UI acceptance interceptor");
    h.handle_compact_request(
        &crate::test_connection_id("equal-ui-a"),
        test_session_id("s1"),
        Some(target_a_id.as_str()),
    );
    h.handle_compact_request(
        &crate::test_connection_id("equal-ui-b"),
        test_session_id("s1"),
        Some(target_b_id.as_str()),
    );
    let staged = h
        .prompt_coordination
        .compaction_runtime
        .pending_manual_acceptances
        .values()
        .map(|pending| pending.request())
        .collect::<Vec<_>>();
    assert_eq!(staged.len(), 2);
    assert_eq!(staged[0].request_id, staged[1].request_id);
    assert_eq!(
        h.prompt_coordination
            .compaction_runtime
            .pending_ui_acknowledgements
            .len(),
        2
    );

    h.handle_extension_event(
        "equal-ui-acceptance",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit target A UI acceptance");
    assert!(frames_a.lock().expect("UI A frames").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::HarnessNotice(notice)) if notice.message == "compaction queued"
        )
    }));
    assert!(!frames_b.lock().expect("UI B frames").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::HarnessNotice(notice)) if notice.message == "compaction queued"
        )
    }));
    assert_eq!(
        h.prompt_coordination
            .compaction_runtime
            .pending_ui_acknowledgements
            .len(),
        1
    );

    h.handle_extension_event(
        "equal-ui-acceptance",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit target B UI acceptance");
    assert!(frames_b.lock().expect("UI B frames").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::HarnessNotice(notice)) if notice.message == "compaction queued"
        )
    }));
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .pending_ui_acknowledgements
            .is_empty()
    );
}

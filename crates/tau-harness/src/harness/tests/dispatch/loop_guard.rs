//! Tests for loop guard behavior.

use super::*;

#[test]
fn loop_guard_repeated_assistant_text_flows_through_provider_responses() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    for idx in 0..3 {
        let spid: AgentPromptId = test_agent_prompt_id(format!("sp-loop-text-{idx}"));
        seed_agent_thinking(&mut h, &cid, spid.as_str());
        h.prompt_coordination
            .prompt_runtime
            .agents
            .insert(spid.clone(), cid.clone());
        h.handle_provider_response_finished(provider_text_response(
            &spid,
            tau_proto::AgentId::parse("main").expect("agent id"),
            text,
        ))
        .expect("response handled");
    }

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.text.contains("Loop guard:")
    )));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );

    let spid = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|conv| conv.dispatch.in_flight_prompt.clone())
        .expect("loop breaker prompt dispatched");
    h.handle_provider_response_finished(provider_text_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
        text,
    ))
    .expect("response handled");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .execution
            .loop_guard
            .stop_automatic_continuation()
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

/// A loop-guard stop cannot discard the typed wake that owns already-committed
/// agent-message activation.
#[test]
fn loop_guard_block_preserves_canonical_agent_message_wake() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let recipient_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .execution
        .loop_guard
        .mark_cycle_blocked("cycle");
    seed_tools_running(&mut h, &cid, vec!["done-call".into()]);
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("loop-guard-agent-message")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&recipient_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "external message".to_owned(),
        }),
    );

    h.maybe_complete_agent_turn_for(&cid, "done-call");

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text.contains("external message")
    )));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(prompt)
            if prompt.agent_id.as_str() == recipient_id
                && prompt.context.flatten().iter().any(|item| {
                    text_part(item).is_some_and(|text| text.contains("external message"))
                })
    )));
}
#[test]
fn loop_guard_detects_repeated_assistant_text_once_then_blocks() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(
        conv.dispatch.pending_prompts[0]
            .text
            .contains("Loop guard:")
    );
    assert!(!conv.execution.loop_guard.stop_automatic_continuation());

    h.mark_loop_guard_breakers_dispatched(&cid);
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(conv.execution.loop_guard.stop_automatic_continuation());
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

#[test]
fn loop_guard_progress_reset_preserves_in_flight_argument_signatures() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for (idx, path) in ["a.txt", "b.txt", "c.txt"].into_iter().enumerate() {
        h.remember_tool_call_loop_signature(
            &cid,
            &crate::harness::AgentToolCall {
                call_ref: None,
                id: format!("call-{idx}").into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(path.to_owned()),
                )]),
            },
        );
    }

    h.reset_loop_guard_for_progress(&cid);

    for idx in 0..3 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "read", "generic failure"),
        );
    }

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn loop_guard_same_batch_failures_do_not_block_before_breaker_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..4 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "tool", &format!("failure {idx}")),
        );
    }

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(!conv.execution.loop_guard.stop_automatic_continuation());
}

#[test]
fn loop_guard_detects_abab_suffix() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    h.record_assistant_loop_signature(
        &cid,
        Some("First repeated long assistant state with no concrete action."),
    );
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("call-a", "read", "first distinct failure"),
    );
    h.record_assistant_loop_signature(
        &cid,
        Some("First repeated long assistant state with no concrete action."),
    );
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("call-b", "read", "first distinct failure"),
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(conv.dispatch.pending_prompts[0].text.contains("A/B/A/B"));
}

#[test]
fn loop_guard_resets_on_user_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";

    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.reset_loop_guard_for_progress(&cid);
    h.record_assistant_loop_signature(&cid, Some(text));

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(conv.dispatch.pending_prompts.is_empty());
}

#[test]
fn loop_guard_resets_on_agent_head_move() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("seed".to_owned()))
        .expect("publish seed");
    let node_id = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent")
        .identity
        .head
        .expect("agent head");
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&agent_id),
            head: tau_proto::AgentHead::Node(node_id),
        }),
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}
#[test]
fn loop_guard_detects_repeated_same_failing_tool_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..3 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "read", "missing file"),
        );
    }

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(
        conv.dispatch.pending_prompts[0]
            .text
            .contains("repeated identical failing tool call")
    );
}

#[test]
fn loop_guard_repeated_tool_failure_signature_includes_arguments() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for (idx, path) in ["a.txt", "b.txt"].into_iter().enumerate() {
        let call = crate::harness::AgentToolCall {
            call_ref: None,
            id: format!("call-{idx}").into(),
            name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text(path.to_owned()),
            )]),
        };
        h.remember_tool_call_loop_signature(&cid, &call);
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(call.id.as_str(), "read", "generic failure"),
        );
    }

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn loop_guard_production_tool_failure_signature_includes_arguments() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..3 {
        h.execute_agent_tool_call(
            &cid,
            &crate::harness::AgentToolCall {
                call_ref: None,
                id: format!("distinct-{idx}").into(),
                name: ToolName::new("missing_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text(format!("file-{idx}.txt")),
                )]),
            },
        )
        .expect("tool call handled");
    }
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .is_empty()
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text.contains("Loop guard:")
    )));

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    for idx in 0..3 {
        h.execute_agent_tool_call(
            &cid,
            &crate::harness::AgentToolCall {
                call_ref: None,
                id: format!("same-{idx}").into(),
                name: ToolName::new("missing_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("same.txt".to_owned()),
                )]),
            },
        )
        .expect("tool call handled");
    }
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSubmitted(submitted)
            if submitted.text.contains("repeated identical failing tool call")
    )));
}

#[test]
fn loop_guard_detects_consecutive_different_tool_failures() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);

    for idx in 0..4 {
        h.record_tool_failure_loop_signature(
            &cid,
            &loop_guard_tool_error(&format!("call-{idx}"), "tool", &format!("failure {idx}")),
        );
    }

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
    assert!(
        conv.dispatch.pending_prompts[0]
            .text
            .contains("several consecutive tool failures")
    );
}

#[test]
fn loop_guard_resets_on_successful_terminal_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("failed-call", "read", "missing file"),
    );

    h.publish_terminal_tool_result(
        Some(&cid),
        None,
        tau_proto::ToolResult {
            presentation: Default::default(),
            call_id: "ok-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.execution.loop_guard.consecutive_tool_failures(), 0);
    assert!(conv.dispatch.pending_prompts.is_empty());
}

#[test]
fn loop_guard_provider_repetition_response_queues_pivot_then_blocks() {
    // Provider-side stream repetition is trusted only as a loop-guard trigger:
    // the provider's error is display text, while the harness uses a fixed pivot
    // reason and blocks only after the breaker was tried.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);

    let spid: AgentPromptId = test_agent_prompt_id("sp-provider-repetition-1");
    seed_agent_thinking(&mut h, &cid, spid.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(spid.clone(), cid.clone());
    h.handle_provider_response_finished(provider_repetition_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
    ))
    .expect("response handled");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered)
            if steered.message_class == tau_proto::PromptMessageClass::Internal
                && steered.text.contains("provider detected a tight exact stream repetition")
    )));
    let spid = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .and_then(|conv| conv.dispatch.in_flight_prompt.clone())
        .expect("loop breaker prompt dispatched");
    h.handle_provider_response_finished(provider_repetition_response(
        &spid,
        tau_proto::AgentId::parse("main").expect("agent id"),
    ))
    .expect("response handled");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .execution
            .loop_guard
            .stop_automatic_continuation()
    );
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.level == tau_proto::NoticeLevel::Warning
                && notice.purpose == tau_proto::NoticePurpose::Alert
                && notice.message.contains("Loop guard stopped automatic continuation")
    )));
}

#[test]
fn loop_guard_resets_when_user_prompt_is_published() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("new direction".to_owned()))
        .expect("publish user prompt");

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

#[test]
fn loop_guard_reset_removes_pending_breaker_prompt() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );

    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user("new input".to_owned()))
        .expect("publish user prompt");

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

#[test]
fn loop_guard_resets_when_user_prompt_is_queued() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-loop-queued-reset"),
    };
    h.record_assistant_loop_signature(
        &cid,
        Some("I will keep trying the same plan without taking any tool action or making progress."),
    );

    let submission = h
        .submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            &agent_id,
            PendingPrompt::user("queued user input".to_owned()),
        )
        .expect("submit prompt");

    assert_eq!(submission, PromptSubmission::Queued);
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.dispatch.pending_prompts.len(), 1);
}

#[test]
fn loop_guard_queued_user_prompt_removes_pending_breaker() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("agent id")
        .to_owned();
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: test_agent_prompt_id("sp-loop-queued-breaker-reset"),
    };

    let submission = h
        .submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            &agent_id,
            PendingPrompt::user("queued user input".to_owned()),
        )
        .expect("submit prompt");

    assert_eq!(submission, PromptSubmission::Queued);
    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert!(
        !conv
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_loop_guard)
    );
}

/// Folding an ordinary queued user prompt removes a stale loop-guard pivot from
/// the same batch so only the user's real progress reaches the next prompt.
#[test]
fn loop_guard_folding_user_prompt_drops_stale_pivot_from_batch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    {
        let conv = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        conv.dispatch
            .pending_prompts
            .push_back(PendingPrompt::user("queued user input".to_owned()));
    }
    let text =
        "I will keep trying the same plan without taking any tool action or making progress.";
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));
    h.record_assistant_loop_signature(&cid, Some(text));

    h.fold_pending_prompts_as_steered(&cid);

    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::AgentPromptSteered(steered) if steered.text.contains("Loop guard:")
        )
    }));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptSteered(steered) if steered.text == "queued user input"
    )));
}
#[test]
fn loop_guard_resets_on_successful_background_tool_result() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    publish_test_tool_declaration(&mut h, &cid, "bg-call");
    h.record_tool_failure_loop_signature(
        &cid,
        &loop_guard_tool_error("failed-call", "read", "missing file"),
    );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("bg-call".into(), cid.clone());
    h.tool_routing.tool_runtime.pending_tools.insert(
        "bg-call".into(),
        PendingTool {
            name: ToolName::new("read"),
            internal_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );

    h.handle_background_tool_result(
        &crate::test_connection_id("conn-bg"),
        tau_proto::ToolResult {
            presentation: Default::default(),
            call_id: "bg-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("ok".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        },
    );

    let conv = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(conv.execution.loop_guard.consecutive_tool_failures(), 0);
    assert!(
        conv.dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_activating_background_completion),
        "committed background result should queue its completion prompt"
    );
}

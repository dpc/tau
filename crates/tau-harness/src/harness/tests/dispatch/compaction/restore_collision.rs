//! Cold-restore regressions for self/cross manual-compaction correlation.

use super::*;

/// Cold restore must not consume a cross-agent compaction result when its
/// target-local request ID collides with the caller's delivered self request.
#[test]
fn restored_cross_compaction_request_id_collision_preserves_result_for_wait() {
    assert_restored_cross_compaction_request_id_collision(false);
}

/// The same target-local request-ID collision must preserve a cross-agent
/// pre-start failure for exact wait consumption after cold restore.
#[test]
fn restored_failed_cross_compaction_request_id_collision_preserves_error_for_wait() {
    assert_restored_cross_compaction_request_id_collision(true);
}

fn assert_restored_cross_compaction_request_id_collision(pre_start_failure: bool) {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("harness");
    h.provider_runtime
        .model_info
        .get_mut(&"echo/model".into())
        .expect("echo model")
        .supports_standalone_compaction = true;
    let caller = ensure_test_user_agent(&mut h);
    let caller_id = durable_agent_id_for_conversation(&h, &caller);
    let (target, target_id) = install_manual_compaction_target(&mut h, "collision-target");
    let collision_index = h.agent_runtime.agent_registry.agents[&caller]
        .dispatch
        .next_prompt_index
        .max(
            h.agent_runtime.agent_registry.agents[&target]
                .dispatch
                .next_prompt_index,
        );
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&caller)
        .expect("caller")
        .dispatch
        .next_prompt_index = collision_index;
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&target)
        .expect("target")
        .dispatch
        .next_prompt_index = collision_index;

    let self_call_id = ToolCallId::from("call-collision-self");
    let self_prompt = start_seeded_self_compaction(&mut h, &caller, self_call_id.clone());
    let self_request = h
        .session_runtime
        .agent_store
        .agent(&caller_id)
        .expect("caller tree")
        .manual_compaction_recoveries()
        .into_iter()
        .find_map(|recovery| match recovery {
            tau_core::ManualCompactionRecovery::Started { requested, .. }
                if requested
                    .tool_source()
                    .is_some_and(|source| source.initiating_tool_call_id == self_call_id) =>
            {
                Some(requested)
            }
            _ => None,
        })
        .expect("self request");
    h.handle_provider_response_finished(provider_text_response(
        &self_prompt.agent_prompt_id,
        self_prompt.agent_id,
        "self collision summary",
    ))
    .expect("complete self compaction");

    if pre_start_failure {
        seed_reactive_compaction_prefix(&mut h, &target);
        h.provider_runtime
            .model_info
            .get_mut(&"echo/model".into())
            .expect("echo model")
            .standalone_compaction_prefix_budget = Some(tau_proto::ByteCount::new(1));
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&target)
            .expect("target")
            .dispatch
            .next_prompt_index = collision_index;
    }
    let cross_call = register_manual_cross_compaction_call(
        &mut h,
        &caller,
        if pre_start_failure {
            "call-collision-cross-failed"
        } else {
            "call-collision-cross-result"
        },
    );
    h.request_agent_tool_compaction(
        &caller,
        &cross_call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    let cross_recovery = h
        .session_runtime
        .agent_store
        .agent(target_id.as_str())
        .expect("target tree")
        .manual_compaction_recoveries()
        .into_iter()
        .find(|recovery| match recovery {
            tau_core::ManualCompactionRecovery::Started { requested, .. }
            | tau_core::ManualCompactionRecovery::Failed { requested, .. } => requested
                .tool_source()
                .is_some_and(|source| source.initiating_tool_call_id == cross_call.id),
            tau_core::ManualCompactionRecovery::Waiting(_) => false,
        })
        .expect("cross request");
    let cross_request = match &cross_recovery {
        tau_core::ManualCompactionRecovery::Started { requested, .. }
        | tau_core::ManualCompactionRecovery::Failed { requested, .. } => requested,
        tau_core::ManualCompactionRecovery::Waiting(_) => unreachable!("terminal cross request"),
    };
    assert_eq!(self_request.request_id, cross_request.request_id);
    assert_ne!(self_request.target_agent_id, cross_request.target_agent_id);
    assert_ne!(
        self_request.required_tool_source().initiating_tool_call_id,
        cross_request.required_tool_source().initiating_tool_call_id
    );
    let cross_request_id = cross_request.request_id.to_string();

    if pre_start_failure {
        assert!(matches!(
            cross_recovery,
            tau_core::ManualCompactionRecovery::Failed { .. }
        ));
    } else {
        assert!(matches!(
            cross_recovery,
            tau_core::ManualCompactionRecovery::Started { .. }
        ));
        let cross_prompt = event_log_events(&h)
            .into_iter()
            .filter_map(|event| match event {
                Event::AgentPromptCreated(prompt)
                    if prompt.agent_id == target_id
                        && prompt.operation == tau_proto::PromptOperation::StandaloneCompaction =>
                {
                    Some(prompt)
                }
                _ => None,
            })
            .next_back()
            .expect("cross compaction prompt");
        h.handle_provider_response_finished(provider_text_response(
            &cross_prompt.agent_prompt_id,
            cross_prompt.agent_id,
            "cross collision summary",
        ))
        .expect("complete cross compaction");
    }
    assert_eq!(
        durable_background_outcome_counts(&h, &caller_id, cross_call.id.as_str()),
        if pre_start_failure { (0, 1) } else { (1, 0) }
    );
    let expected_cross_error = pre_start_failure.then(|| {
        h.session_runtime
            .agent_store
            .snapshot_agent_events_for_test(&caller_id)
            .expect("caller events")
            .iter()
            .find_map(|record| match &record.event {
                Event::ToolBackgroundError(error) if error.call_id == cross_call.id => {
                    Some(error.message.clone())
                }
                _ => None,
            })
            .expect("cross background error")
    });
    let self_delivery_count = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(&caller_id)
        .expect("caller events")
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::AgentPromptSteered(steered)
                    if steered.self_compaction_terminal.as_ref().is_some_and(|terminal|
                        terminal.tool_call_id == self_call_id)
            )
        })
        .count();
    let target_start_count = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(target_id.as_str())
        .expect("target events")
        .iter()
        .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionStarted(_)))
        .count();
    drop(h);
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let resumed_caller = resumed
        .runtime_agent_id_for_target_agent(Some(&caller_id))
        .expect("resumed caller");
    assert!(!resumed.wait_completion_is_retained_for_test(&resumed_caller, &self_call_id));
    assert!(resumed.wait_completion_is_retained_for_test(&resumed_caller, &cross_call.id));
    assert_eq!(
        resumed
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(&caller_id)
            .expect("resumed caller events")
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentPromptSteered(steered)
                        if steered.self_compaction_terminal.as_ref().is_some_and(|terminal|
                            terminal.tool_call_id == self_call_id)
                )
            })
            .count(),
        self_delivery_count,
        "restore must not redeliver the self terminal"
    );
    assert_eq!(
        resumed
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(target_id.as_str())
            .expect("resumed target events")
            .iter()
            .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        target_start_count,
        "restore must not redispatch completed cross compaction work"
    );

    let wait = AgentToolCall {
        call_ref: None,
        id: ToolCallId::from(if pre_start_failure {
            "wait-collision-cross-failed"
        } else {
            "wait-collision-cross-result"
        }),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(cross_call.id.to_string()),
        )]),
    };
    seed_assistant_tool_round(&mut resumed, &resumed_caller, &[(wait.id.as_str(), "wait")]);
    seed_tools_running(&mut resumed, &resumed_caller, vec![wait.id.clone()]);
    resumed
        .handle_wait_tool_call(&resumed_caller, &wait, ToolName::new("wait"))
        .expect("exact wait");
    if pre_start_failure {
        assert!(event_log_contains_any_source(&resumed, |event| matches!(
            event,
            Event::ToolError(error)
                if error.call_id == wait.id
                    && Some(&error.message) == expected_cross_error.as_ref()
        )));
    } else {
        let wait_events = event_log_events(&resumed)
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::ToolResult(result) if result.call_id == wait.id
                ) || matches!(
                    event,
                    Event::ToolError(error) if error.call_id == wait.id
                )
            })
            .collect::<Vec<_>>();
        assert!(
            wait_events.iter().any(|event| matches!(
                event,
                Event::ToolResult(result)
                    if cbor_map_text(&result.result, "request_id")
                        == Some(cross_request_id.as_str())
                        && cbor_map_text(&result.result, "target_agent_id")
                            == Some(target_id.as_str())
                        && cbor_map_text(&result.result, "status") == Some("compacted")
            )),
            "{wait_events:#?}"
        );
    }
    assert!(!resumed.wait_completion_is_retained_for_test(&resumed_caller, &cross_call.id));
}

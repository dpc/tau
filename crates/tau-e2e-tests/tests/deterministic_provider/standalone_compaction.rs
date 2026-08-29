//! Standalone local-summary-compaction deterministic acceptance.

use tau_proto::{
    ContextItem, ContextRecoveryDisposition, Event, PromptOperation, ProviderFailureKind,
    StandaloneCompactionFailureReason, StandaloneCompactionTrigger,
};

use super::*;

/// Proves one canonical ordinary context-window rejection commits its recovery
/// plan before exactly one reactive opaque compaction and one replacement-based
/// inference continuation, without retrying the rejected prompt.
#[test]
fn deterministic_context_overflow_reactively_compacts_and_continues()
-> Result<(), Box<dyn std::error::Error>> {
    let pre_cut_body = "pre-cut ordinary body";
    let overflow_prompt = "ordinary prompt that overflows";
    let fixture = DeterministicFixture::new_v2(
        "deterministic_context_overflow_reactively_compacts_and_continues",
        &ScenarioV2::new(
            "reactive-opaque-context-overflow",
            vec![ScenarioLaneV2 {
                ctx_id: "reactive-overflow-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: pre_cut_body.to_owned(),
                        response: "established compactable history".to_owned(),
                    },
                    ScenarioActionV2::ContextOverflow {
                        user_text: overflow_prompt.to_owned(),
                        removed_user_text: pre_cut_body.to_owned(),
                        removed_assistant_text: "established compactable history".to_owned(),
                        failure_kind: ProviderFailureKind::ContextWindowExceeded,
                    },
                    ScenarioActionV2::ReactiveOpaqueCompaction {
                        removed_user_text: pre_cut_body.to_owned(),
                        removed_assistant_text: "established compactable history".to_owned(),
                        overflow_user_text: overflow_prompt.to_owned(),
                    },
                    ScenarioActionV2::ReactiveCompactedOpaqueText {
                        removed_user_text: pre_cut_body.to_owned(),
                        removed_assistant_text: "established compactable history".to_owned(),
                        overflow_user_text: overflow_prompt.to_owned(),
                        response: "recovered from opaque replacement".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("reactive-overflow");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent(&mut peer, "reactive-overflow-lane", pre_cut_body)?;
    let established = recv_until_finished(&mut peer)?;
    assert_assistant(&established.output_items, "established compactable history");
    submit_prompt(
        &mut peer,
        &established.agent_id,
        "reactive-overflow-prompt",
        overflow_prompt,
    )?;
    let rejected_prompt = recv_until_created(&mut peer, Some("reactive-overflow-prompt"))?;
    assert_eq!(rejected_prompt.operation, PromptOperation::Inference);
    let rejected = recv_until_finished_for(&mut peer, &rejected_prompt.agent_prompt_id)?;
    assert_eq!(rejected.agent_id, rejected_prompt.agent_id);
    assert!(rejected.output_items.is_empty());
    assert_eq!(
        rejected.failure_kind,
        Some(ProviderFailureKind::ContextWindowExceeded)
    );
    assert_eq!(
        rejected.recovery_disposition,
        ContextRecoveryDisposition::ReactiveCompactionPlanned
    );

    let started = recv_until_compaction_started(&mut peer)?;
    assert_eq!(started.agent_id, rejected_prompt.agent_id);
    assert_eq!(started.operation, PromptOperation::StandaloneCompaction);
    assert!(matches!(
        started.trigger,
        StandaloneCompactionTrigger::ReactiveContextOverflow {
            ref failed_agent_prompt_id
        } if failed_agent_prompt_id == &rejected_prompt.agent_prompt_id
    ));
    let compact_prompt = recv_until_compaction_prompt(&mut peer)?;
    assert_eq!(started.compact_prompt_id, compact_prompt.agent_prompt_id);
    let compacted = recv_until_compacted(&mut peer)?;
    assert_eq!(compacted.agent_id, rejected_prompt.agent_id);
    assert_eq!(
        compacted.transaction_id.as_ref(),
        Some(&started.transaction_id)
    );
    assert_eq!(compacted.cut.as_ref(), Some(&started.cut));
    assert_eq!(
        compacted.compact_prompt_id.as_ref(),
        Some(&compact_prompt.agent_prompt_id)
    );
    assert!(matches!(
        compacted.replacement_window.as_slice(),
        [ContextItem::Compaction(item)]
            if item.raw_json()
                == tau_e2e_tests::CANONICAL_OPAQUE_COMPACTION_JSON
    ));

    let continuation_prompt = recv_until_inference_prompt(&mut peer)?;
    assert_ne!(
        continuation_prompt.agent_prompt_id,
        rejected_prompt.agent_prompt_id
    );
    assert_ne!(
        continuation_prompt.agent_prompt_id,
        compact_prompt.agent_prompt_id
    );
    let continued = recv_until_finished_for(&mut peer, &continuation_prompt.agent_prompt_id)?;
    assert_assistant(&continued.output_items, "recovered from opaque replacement");
    disconnect_ui(&mut peer)?;
    server.finish()?;

    ReactiveRecovery {
        fixture: &fixture,
        established_prompt: &established.agent_prompt_id,
        rejected_prompt: &rejected_prompt,
        compact_prompt: &compact_prompt,
        continuation_prompt: &continuation_prompt,
        rejected: &rejected,
        started: &started,
        compacted: &compacted,
    }
    .assert_durable()?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves a manual standalone compaction durably preserves one canonical opaque
/// provider item across a clean restart, and the resumed next inference
/// receives that exact replacement rather than the removed transcript.
#[test]
fn deterministic_opaque_standalone_compaction_replays_after_clean_restart()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = DeterministicFixture::new_v2(
        "deterministic_opaque_standalone_compaction_replays_after_clean_restart",
        &ScenarioV2::new(
            "standalone-opaque-compaction-restart",
            vec![ScenarioLaneV2 {
                ctx_id: "opaque-compact-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: "establish opaque compactable history".to_owned(),
                        response: "initial opaque history".to_owned(),
                    },
                    ScenarioActionV2::StandaloneOpaqueCompaction,
                    ScenarioActionV2::CompactedOpaqueText {
                        user_text: "continue after opaque restart".to_owned(),
                        removed_user_text: "establish opaque compactable history".to_owned(),
                        response: "continued from opaque replacement".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket_a = fixture.socket_path("opaque-compact-boot-a");
    let server_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut peer_a = connect_ui(&socket_a)?;
    create_agent(
        &mut peer_a,
        "opaque-compact-lane",
        "establish opaque compactable history",
    )?;
    let first = recv_until_finished(&mut peer_a)?;
    request_compaction(&mut peer_a, &first.agent_id)?;
    let started = recv_until_compaction_started(&mut peer_a)?;
    let compact_prompt = recv_until_compaction_prompt(&mut peer_a)?;
    let compacted = recv_until_compacted(&mut peer_a)?;
    assert_eq!(
        compacted.compact_prompt_id.as_ref(),
        Some(&compact_prompt.agent_prompt_id)
    );
    assert_eq!(
        compacted.transaction_id.as_ref(),
        Some(&started.transaction_id)
    );
    assert_eq!(
        compacted.compact_prompt_id.as_ref(),
        Some(&compact_prompt.agent_prompt_id)
    );
    assert!(matches!(
        compacted.replacement_window.as_slice(),
        [ContextItem::Compaction(item)]
            if item.raw_json()
                == tau_e2e_tests::CANONICAL_OPAQUE_COMPACTION_JSON
    ));
    disconnect_ui(&mut peer_a)?;
    server_a.finish()?;
    assert_durable_opaque_compaction(&fixture, &first.agent_id)?;

    let socket_b = fixture.socket_path("opaque-compact-boot-b");
    let server_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut peer_b = connect_ui(&socket_b)?;
    recv_until_session_replay_complete(&mut peer_b)?;
    submit_prompt(
        &mut peer_b,
        &first.agent_id,
        "opaque-compact-continuation",
        "continue after opaque restart",
    )?;
    let continued = recv_until_finished(&mut peer_b)?;
    assert_assistant(&continued.output_items, "continued from opaque replacement");
    disconnect_ui(&mut peer_b)?;
    server_b.finish()?;
    assert_durable_opaque_compaction(&fixture, &first.agent_id)?;
    fixture.assert_consumed()?;
    Ok(())
}

/// A post-tool continuation owns one inferred-Done automatic decision, whose
/// opaque replacement survives a clean restart and excludes its source context
/// from the next ordinary inference.
#[test]
fn deterministic_post_tool_policy_compacts_and_replays_after_clean_restart()
-> Result<(), Box<dyn std::error::Error>> {
    const INITIAL: &str = "run the lifecycle tool and compact";
    const FOLLOW_UP: &str = "continue from the lifecycle replacement";
    const CALL_ID: &str = "lifecycle-call";
    let fixture = DeterministicFixture::new_dummy_tool_v2(
        "deterministic_post_tool_policy_compacts_and_replays_after_clean_restart",
        &ScenarioV2::new(
            "post-tool-outer-finish-policy-restart",
            vec![ScenarioLaneV2 {
                ctx_id: "post-tool-policy-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::DummyToolCall {
                        user_text: INITIAL.to_owned(),
                        call_id: CALL_ID.into(),
                    },
                    ScenarioActionV2::DummyToolResultWithUsage {
                        user_text: INITIAL.to_owned(),
                        call_id: CALL_ID.into(),
                        response: "post-tool lifecycle finished".to_owned(),
                    },
                    ScenarioActionV2::StandaloneOpaqueCompaction,
                    ScenarioActionV2::CompactedOpaqueCheckpoint {
                        removed_user_text: INITIAL.to_owned(),
                        response: "checkpointed lifecycle replacement".to_owned(),
                    },
                    ScenarioActionV2::Text {
                        user_text: FOLLOW_UP.to_owned(),
                        response: "continued from lifecycle replacement".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
        DUMMY_TOOL,
    )?;
    let config_path = fixture.config_dir().join("harness.yaml");
    let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    let main = config["agents"]["role_groups"]["e2e"]["roles"]["deterministic-e2e"]
        .as_object_mut()
        .ok_or("missing main role")?;
    main.insert(
        "inference_compaction".to_owned(),
        serde_json::Value::String("disabled".to_owned()),
    );
    main.insert(
        "compactions".to_owned(),
        serde_json::json!({
            "finish": {
                "threshold": 1000,
                "enable": true,
                "when": {
                    "at": "outer_turn_finished",
                    "statuses": ["done"]
                }
            }
        }),
    );
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
    let user_config_dir = fixture.root().join("xdg-config").join("tau");
    std::fs::create_dir_all(&user_config_dir)?;
    std::fs::write(user_config_dir.join("harness.yaml"), "{}\n")?;

    let socket_a = fixture.socket_path("post-tool-policy-a");
    let server_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut peer_a = connect_ui(&socket_a)?;
    create_agent(&mut peer_a, "post-tool-policy-lane", INITIAL)?;
    let continuation_prompt = recv_until_created(&mut peer_a, None)?;
    let finished = recv_until_finished_for(&mut peer_a, &continuation_prompt.agent_prompt_id)?;
    assert_assistant(&finished.output_items, "post-tool lifecycle finished");
    assert!(finished.automatic_compaction_decision.is_some());
    let started = recv_until_compaction_started(&mut peer_a)?;
    assert!(matches!(
        started.trigger,
        StandaloneCompactionTrigger::AutomaticPolicy { ref decision_id }
            if Some(decision_id)
                == finished
                    .automatic_compaction_decision
                    .as_ref()
                    .map(|decision| &decision.transaction_id)
    ));
    let compact_prompt = recv_until_compaction_prompt(&mut peer_a)?;
    let compacted = recv_until_compacted(&mut peer_a)?;
    assert_eq!(
        compacted.compact_prompt_id.as_ref(),
        Some(&compact_prompt.agent_prompt_id)
    );
    assert!(matches!(
        compacted.replacement_window.as_slice(),
        [ContextItem::Compaction(item)]
            if item.raw_json()
                == tau_e2e_tests::CANONICAL_OPAQUE_COMPACTION_JSON
    ));
    let checkpointed = recv_until_finished(&mut peer_a)?;
    assert_assistant(
        &checkpointed.output_items,
        "checkpointed lifecycle replacement",
    );
    disconnect_ui(&mut peer_a)?;
    server_a.finish()?;

    let snapshot_a = lifecycle_snapshot(&fixture, &continuation_prompt.agent_id)?;
    let socket_b = fixture.socket_path("post-tool-policy-b");
    let server_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut peer_b = connect_ui(&socket_b)?;
    recv_until_session_replay_complete(&mut peer_b)?;
    submit_prompt(
        &mut peer_b,
        &continuation_prompt.agent_id,
        "post-tool-policy-follow-up",
        FOLLOW_UP,
    )?;
    let follow_up = recv_until_finished(&mut peer_b)?;
    assert_assistant(
        &follow_up.output_items,
        "continued from lifecycle replacement",
    );
    disconnect_ui(&mut peer_b)?;
    server_b.finish()?;

    let snapshot_b = lifecycle_snapshot(&fixture, &continuation_prompt.agent_id)?;
    snapshot_b.require_prefix(&snapshot_a)?;
    let prompts = snapshot_b
        .agent_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentPromptStarted(started) => Some(started),
            _ => None,
        })
        .collect::<Vec<_>>();
    let outer_started = snapshot_b
        .agent_events
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentOuterTurnStarted(started) => Some(started),
            _ => None,
        })
        .ok_or("missing outer turn start")?;
    let decision_terminal = snapshot_b
        .agent_events
        .iter()
        .find_map(|record| match &record.event {
            Event::ProviderResponseFinished(response)
                if response.automatic_compaction_decision.is_some() =>
            {
                Some(response)
            }
            _ => None,
        })
        .ok_or("missing decision terminal")?;
    let initial_started = prompts
        .iter()
        .find(|started| started.agent_prompt_id == outer_started.agent_prompt_id)
        .ok_or("missing initial prompt start")?;
    let continuation_started = prompts
        .iter()
        .find(|started| {
            started.operation == PromptOperation::Inference
                && started.agent_prompt_id == decision_terminal.agent_prompt_id
                && started.outer_turn_id == initial_started.outer_turn_id
        })
        .ok_or("missing distinct post-tool continuation prompt")?;
    assert_ne!(
        initial_started.agent_prompt_id,
        continuation_started.agent_prompt_id
    );
    let count = |predicate: &dyn Fn(&Event) -> bool| {
        snapshot_b
            .agent_events
            .iter()
            .filter(|record| predicate(&record.event))
            .count()
    };
    assert_eq!(
        count(&|event| matches!(event, Event::AgentPromptStarted(_))),
        5,
        "initial, tool continuation, compact, checkpoint, and follow-up prompts are the complete lineage"
    );
    assert_eq!(
        count(&|event| matches!(
            event,
            Event::ProviderResponseFinished(response)
                if response.stop_reason == tau_proto::ProviderStopReason::ToolCalls
        )),
        1
    );
    assert_eq!(
        count(&|event| matches!(event, Event::ProviderToolResult(_))),
        1
    );
    assert_eq!(
        count(&|event| matches!(
            event,
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == continuation_started.agent_prompt_id
                    && response.automatic_compaction_decision.is_some()
        )),
        1
    );
    assert_eq!(
        count(
            &|event| matches!(event, Event::AgentOuterTurnFinished(finished)
            if Some(&finished.outer_turn_id) == initial_started.outer_turn_id.as_ref())
        ),
        1
    );
    assert_eq!(
        count(
            &|event| matches!(event, Event::AgentStandaloneCompactionStarted(value)
            if value.transaction_id == started.transaction_id)
        ),
        1
    );
    assert_eq!(
        count(&|event| matches!(event, Event::AgentCompacted(value)
            if value.transaction_id.as_ref() == Some(&started.transaction_id))),
        1
    );
    let position = |predicate: &dyn Fn(&Event) -> bool| {
        snapshot_b
            .agent_events
            .iter()
            .position(|record| predicate(&record.event))
            .expect("expected lifecycle record")
    };
    let decision_index = position(&|event| {
        matches!(
            event,
            Event::ProviderResponseFinished(response)
                if response.automatic_compaction_decision.as_ref().map(|decision| &decision.transaction_id)
                    == Some(&started.transaction_id)
        )
    });
    let finish_index = position(&|event| {
        matches!(
            event,
            Event::AgentOuterTurnFinished(finished)
                if finished.automatic_compaction_decision.as_ref() == Some(&started.transaction_id)
        )
    });
    let start_index = position(&|event| {
        matches!(
            event,
            Event::AgentStandaloneCompactionStarted(value)
                if value.transaction_id == started.transaction_id
        )
    });
    assert!(
        decision_index < finish_index && finish_index < start_index,
        "automatic lifecycle order changed"
    );
    assert_eq!(fixture.trace()?.matches(" matched ").count(), 5);
    let published = fixture.published_trace_events()?;
    let diagnostics = published
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::HarnessNotice(notice)
                    if matches!(
                        notice.level,
                        tau_proto::NoticeLevel::Warning | tau_proto::NoticeLevel::Critical
                    )
            ) || matches!(
                event,
                Event::ProviderResponseFinished(response) if response.failure_kind.is_some()
            ) || matches!(event, Event::AgentStandaloneCompactionFailed(_))
        })
        .collect::<Vec<_>>();
    assert!(
        diagnostics.is_empty(),
        "closed lifecycle emitted warning/failure frames: {diagnostics:?}"
    );
    assert_durable_opaque_compaction(&fixture, &continuation_prompt.agent_id)?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Loads the single-agent durable lifecycle snapshot and rejects persisted
/// provider failures or harness warnings in the closed success scenario.
fn lifecycle_snapshot(
    fixture: &DeterministicFixture,
    expected_agent_id: &tau_proto::AgentId,
) -> Result<tau_e2e_tests::DurableSnapshot, Box<dyn std::error::Error>> {
    let snapshot = tau_e2e_tests::DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    if snapshot.agent_id != *expected_agent_id
        || snapshot.agent_events.iter().any(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(response) if response.failure_kind.is_some()
            ) || matches!(&record.event, Event::HarnessNotice(_))
                || matches!(&record.event, Event::AgentStandaloneCompactionFailed(_))
        })
    {
        return Err("closed lifecycle scenario persisted a warning or failure".into());
    }
    Ok(snapshot)
}

/// Proves an explicitly opted-in deterministic model completes the real
/// standalone transaction: the fake receives its tool-free request, the
/// harness durably replaces the transcript, and the next user turn sees only
/// the replacement window.
#[test]
fn deterministic_standalone_compaction_replaces_transcript_and_continues()
-> Result<(), Box<dyn std::error::Error>> {
    let narrative = compact_narrative("completed initial work");
    let checkpoint = local_summary_checkpoint(&narrative);
    let fixture = DeterministicFixture::new_v2(
        "deterministic_standalone_compaction_replaces_transcript_and_continues",
        &ScenarioV2::new(
            "standalone-compaction-success",
            vec![ScenarioLaneV2 {
                ctx_id: "compact-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: "establish compactable history".to_owned(),
                        response: "initial history".to_owned(),
                    },
                    ScenarioActionV2::StandaloneCompaction {
                        narrative: narrative.clone(),
                    },
                    ScenarioActionV2::CompactedText {
                        user_text: "continue after compaction".to_owned(),
                        checkpoint: checkpoint.clone(),
                        removed_user_text: "establish compactable history".to_owned(),
                        response: "continued from replacement".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket_a = fixture.socket_path("compact-success-a");
    let server_a = spawn_daemon(&fixture, &socket_a, tau_harness::SessionLaunchStatus::New);
    let mut peer_a = connect_ui(&socket_a)?;
    create_agent(&mut peer_a, "compact-lane", "establish compactable history")?;
    let first = recv_until_finished(&mut peer_a)?;
    request_compaction(&mut peer_a, &first.agent_id)?;
    let started = recv_until_compaction_started(&mut peer_a)?;
    let compact_prompt = recv_until_compaction_prompt(&mut peer_a)?;
    assert_eq!(started.compact_prompt_id, compact_prompt.agent_prompt_id);
    let compacted = recv_until_compacted(&mut peer_a)?;
    assert_eq!(compacted.agent_id, first.agent_id);
    assert_eq!(
        compacted.compact_prompt_id.as_ref(),
        Some(&compact_prompt.agent_prompt_id)
    );
    assert_eq!(
        compacted.transaction_id.as_ref(),
        Some(&started.transaction_id)
    );
    assert_eq!(
        compacted.operation,
        Some(PromptOperation::StandaloneCompaction)
    );
    assert_eq!(
        compacted.replacement_window,
        vec![ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::User,
            content: vec![tau_proto::ContentPart::SyntheticCompactionSummary {
                text: checkpoint.clone(),
            }],
            phase: None,
            responses_raw_json: None,
        })]
    );
    disconnect_ui(&mut peer_a)?;
    server_a.finish()?;
    assert_durable_compaction(
        &fixture,
        &first.agent_id,
        &checkpoint,
        std::slice::from_ref(&compacted),
        &[],
    )?;

    let socket_b = fixture.socket_path("compact-success-b");
    let server_b = spawn_daemon(
        &fixture,
        &socket_b,
        tau_harness::SessionLaunchStatus::Resumed,
    );
    let mut peer_b = connect_ui(&socket_b)?;
    recv_until_session_replay_complete(&mut peer_b)?;
    submit_prompt(
        &mut peer_b,
        &first.agent_id,
        "compact-continuation",
        "continue after compaction",
    )?;
    let continued = recv_until_finished(&mut peer_b)?;
    assert_assistant(&continued.output_items, "continued from replacement");
    disconnect_ui(&mut peer_b)?;
    server_b.finish()?;
    assert_durable_compaction(&fixture, &first.agent_id, &checkpoint, &[compacted], &[])?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves terminal provider failure and targeted cancellation leave durable
/// standalone failure facts, after which a fresh explicit compaction can still
/// replace history and permit ordinary continuation.
#[test]
fn deterministic_standalone_compaction_failure_and_cancellation_remain_recoverable()
-> Result<(), Box<dyn std::error::Error>> {
    let narrative = compact_narrative("recovered after terminal boundaries");
    let checkpoint = local_summary_checkpoint(&narrative);
    let fixture = DeterministicFixture::new_v2(
        "deterministic_standalone_compaction_failure_and_cancellation_remain_recoverable",
        &ScenarioV2::new(
            "standalone-compaction-terminal-boundaries",
            vec![ScenarioLaneV2 {
                ctx_id: "compact-boundary-lane".to_owned(),
                actions: vec![
                    ScenarioActionV2::Text {
                        user_text: "create boundary history".to_owned(),
                        response: "boundary history".to_owned(),
                    },
                    ScenarioActionV2::StandaloneCompactionError {
                        failure_kind: ProviderFailureKind::RequestRejected,
                        error: "synthetic compactor rejection".to_owned(),
                    },
                    ScenarioActionV2::StandaloneCompactionHold { timeout_ms: 10_000 },
                    ScenarioActionV2::StandaloneCompaction {
                        narrative: narrative.clone(),
                    },
                    ScenarioActionV2::CompactedText {
                        user_text: "continue after boundary recovery".to_owned(),
                        checkpoint: checkpoint.clone(),
                        removed_user_text: "create boundary history".to_owned(),
                        response: "boundary recovery continued".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("compact-boundaries");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent(
        &mut peer,
        "compact-boundary-lane",
        "create boundary history",
    )?;
    let first = recv_until_finished(&mut peer)?;

    request_compaction(&mut peer, &first.agent_id)?;
    let rejected_started = recv_until_compaction_started(&mut peer)?;
    let rejected_prompt = recv_until_compaction_prompt(&mut peer)?;
    assert_eq!(
        rejected_started.compact_prompt_id,
        rejected_prompt.agent_prompt_id
    );
    let rejected = recv_until_compaction_failure(&mut peer)?;
    assert_eq!(
        rejected.reason,
        StandaloneCompactionFailureReason::ProviderError
    );
    assert_eq!(rejected.transaction_id, rejected_started.transaction_id);

    request_compaction(&mut peer, &first.agent_id)?;
    let cancelled_started = recv_until_compaction_started(&mut peer)?;
    let held = recv_until_compaction_prompt(&mut peer)?;
    assert_eq!(cancelled_started.compact_prompt_id, held.agent_prompt_id);
    cancel_prompt(&mut peer, &held)?;
    let cancelled = recv_until_compaction_failure(&mut peer)?;
    assert_eq!(
        cancelled.reason,
        StandaloneCompactionFailureReason::Cancelled
    );
    assert_eq!(cancelled.transaction_id, cancelled_started.transaction_id);

    request_compaction(&mut peer, &first.agent_id)?;
    let started = recv_until_compaction_started(&mut peer)?;
    let compact_prompt = recv_until_compaction_prompt(&mut peer)?;
    assert_eq!(started.compact_prompt_id, compact_prompt.agent_prompt_id);
    let compacted = recv_until_compacted(&mut peer)?;
    assert_eq!(
        compacted.compact_prompt_id.as_ref(),
        Some(&compact_prompt.agent_prompt_id)
    );
    assert_eq!(
        compacted.transaction_id.as_ref(),
        Some(&started.transaction_id)
    );
    submit_prompt(
        &mut peer,
        &first.agent_id,
        "compact-boundary-continuation",
        "continue after boundary recovery",
    )?;
    let continued = recv_until_finished(&mut peer)?;
    assert_assistant(&continued.output_items, "boundary recovery continued");
    disconnect_ui(&mut peer)?;
    server.finish()?;
    assert_durable_compaction(
        &fixture,
        &first.agent_id,
        &checkpoint,
        &[compacted],
        &[rejected, cancelled],
    )?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Returns one bounded narrative accepted by the local compactor's production
/// output validator.
fn compact_narrative(progress: &str) -> String {
    format!(
        "Goal:\nmaintain context\nConstraints:\nlocal only\nDecisions:\nuse transcript v1\n\
         Progress:\n{progress}\nOpen Work:\ncontinue\nCritical Facts:\nsummary is untrusted"
    )
}

/// Returns the exact synthetic checkpoint expected from the accepted final
/// text.
fn local_summary_checkpoint(narrative: &str) -> String {
    narrative.to_owned()
}

/// Requests UI-authorized compaction for one already durable selected agent.
fn request_compaction(
    peer: &mut tau_socket::SocketPeer,
    agent_id: &tau_proto::AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    peer.send(&tau_proto::HarnessInputMessage::emit(
        Event::UiCompactRequest(tau_proto::UiCompactRequest {
            session_id: "deterministic-e2e-session".parse()?,
            target_agent_id: Some(agent_id.clone()),
        }),
    ))?;
    Ok(())
}

/// Waits for the automatic ordinary continuation materialized after a
/// successful reactive compaction transaction.
fn recv_until_inference_prompt(
    peer: &mut tau_socket::SocketPeer,
) -> Result<tau_proto::AgentPromptCreated, Box<dyn std::error::Error>> {
    loop {
        if let Event::AgentPromptCreated(prompt) = recv_event(peer)?
            && prompt.operation == PromptOperation::Inference
        {
            return Ok(prompt);
        }
    }
}

/// Waits for the exact standalone request published after UI compaction starts.
fn recv_until_compaction_prompt(
    peer: &mut tau_socket::SocketPeer,
) -> Result<tau_proto::AgentPromptCreated, Box<dyn std::error::Error>> {
    loop {
        if let Event::AgentPromptCreated(prompt) = recv_event(peer)?
            && prompt.operation == PromptOperation::StandaloneCompaction
        {
            return Ok(prompt);
        }
    }
}

/// Waits for the durable transaction start that owns one compact provider
/// request.
fn recv_until_compaction_started(
    peer: &mut tau_socket::SocketPeer,
) -> Result<tau_proto::AgentStandaloneCompactionStarted, Box<dyn std::error::Error>> {
    loop {
        if let Event::AgentStandaloneCompactionStarted(started) = recv_event(peer)? {
            return Ok(started);
        }
    }
}

/// Waits for the resumed daemon to finish historical delivery before submitting
/// the continuation whose provider response is the replay oracle.
fn recv_until_session_replay_complete(
    peer: &mut tau_socket::SocketPeer,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        if matches!(recv_event(peer)?, Event::SessionReplayComplete(_)) {
            return Ok(());
        }
    }
}

/// Waits for the durable successful transcript replacement fact.
fn recv_until_compacted(
    peer: &mut tau_socket::SocketPeer,
) -> Result<tau_proto::AgentCompacted, Box<dyn std::error::Error>> {
    loop {
        if let Event::AgentCompacted(compacted) = recv_event(peer)? {
            return Ok(compacted);
        }
    }
}

/// Waits for one terminal standalone transaction failure fact.
fn recv_until_compaction_failure(
    peer: &mut tau_socket::SocketPeer,
) -> Result<tau_proto::AgentStandaloneCompactionFailed, Box<dyn std::error::Error>> {
    loop {
        if let Event::AgentStandaloneCompactionFailed(failed) = recv_event(peer)? {
            return Ok(failed);
        }
    }
}

/// Typed live facts that one reactive recovery must persist in causal order.
struct ReactiveRecovery<'a> {
    /// Fixture owning the durable journal.
    fixture: &'a DeterministicFixture,
    /// Setup inference prompt that creates the pre-cut round.
    established_prompt: &'a tau_proto::AgentPromptId,
    /// Failed ordinary prompt that authorizes recovery.
    rejected_prompt: &'a tau_proto::AgentPromptCreated,
    /// Standalone prompt owned by the reactive transaction.
    compact_prompt: &'a tau_proto::AgentPromptCreated,
    /// Automatic ordinary continuation after replacement.
    continuation_prompt: &'a tau_proto::AgentPromptCreated,
    /// Durable no-output context-window terminal.
    rejected: &'a tau_proto::ProviderResponseFinished,
    /// Durable reactive transaction start.
    started: &'a tau_proto::AgentStandaloneCompactionStarted,
    /// Durable opaque replacement outcome.
    compacted: &'a tau_proto::AgentCompacted,
}

impl ReactiveRecovery<'_> {
    /// Checks the durable transaction and ordered setup, rejected, compaction,
    /// and continuation prompt-start facts preserve one causal replacement
    /// flow.
    fn assert_durable(&self) -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = tau_e2e_tests::DurableSnapshot::load(
            self.fixture.harness_state_dir(),
            &"deterministic-e2e-session".parse()?,
        )?;
        let events = snapshot
            .agent_events
            .iter()
            .map(|record| &record.event)
            .collect::<Vec<_>>();
        let rejected_terminals = events
            .iter()
            .filter_map(|event| match event {
                Event::ProviderResponseFinished(value)
                    if value.agent_prompt_id == self.rejected_prompt.agent_prompt_id =>
                {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if rejected_terminals.as_slice() != [self.rejected.clone()] {
            return Err("durable ordinary overflow terminal was not unique".into());
        }
        let reactive_starts = events
            .iter()
            .filter_map(|event| match event {
                Event::AgentStandaloneCompactionStarted(value)
                    if value.agent_id == self.rejected_prompt.agent_id =>
                {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if reactive_starts.as_slice() != [self.started.clone()] {
            return Err("durable reactive compaction start was not unique".into());
        }
        let compacted_events = events
            .iter()
            .filter_map(|event| match event {
                Event::AgentCompacted(value) if value.agent_id == self.rejected_prompt.agent_id => {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if compacted_events.as_slice() != [self.compacted.clone()] {
            return Err("durable reactive replacement was not unique".into());
        }
        let checkpoints = events
            .iter()
            .filter_map(|event| match event {
                Event::AgentPromptStarted(value)
                    if value.agent_id == self.rejected_prompt.agent_id =>
                {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let checkpoint_pairs = checkpoints
            .iter()
            .map(|checkpoint| (&checkpoint.agent_prompt_id, checkpoint.operation))
            .collect::<Vec<_>>();
        let expected_checkpoint_pairs = [
            (self.established_prompt, PromptOperation::Inference),
            (
                &self.rejected_prompt.agent_prompt_id,
                PromptOperation::Inference,
            ),
            (
                &self.compact_prompt.agent_prompt_id,
                PromptOperation::StandaloneCompaction,
            ),
            (
                &self.continuation_prompt.agent_prompt_id,
                PromptOperation::Inference,
            ),
        ];
        if checkpoint_pairs != expected_checkpoint_pairs {
            return Err(
                "durable recovery checkpoints changed or retried the ordinary prompt".into(),
            );
        }
        let final_responses = events
            .iter()
            .filter_map(|event| match event {
                Event::ProviderResponseFinished(value)
                    if value.agent_prompt_id == self.continuation_prompt.agent_prompt_id =>
                {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if final_responses.len() != 1 {
            return Err("reactive recovery did not produce exactly one final response".into());
        }
        Ok(())
    }
}

/// Checks the authoritative journal retains only the expected replacement and
/// terminal facts after the daemon exits.
fn assert_durable_compaction(
    fixture: &DeterministicFixture,
    expected_agent_id: &tau_proto::AgentId,
    expected_checkpoint: &str,
    expected_compacted: &[tau_proto::AgentCompacted],
    expected_failures: &[tau_proto::AgentStandaloneCompactionFailed],
) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = tau_e2e_tests::DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    let compacted = snapshot
        .agent_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentCompacted(value) => Some(value),
            _ => None,
        })
        .cloned()
        .collect::<Vec<_>>();
    let failures = snapshot
        .agent_events
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentStandaloneCompactionFailed(value) => Some(value),
            _ => None,
        })
        .cloned()
        .collect::<Vec<_>>();
    if compacted != expected_compacted || failures != expected_failures {
        return Err(format!(
            "durable compaction outcomes differed: successes={compacted:?}, failures={failures:?}"
        )
        .into());
    }
    if compacted.iter().any(|value| {
        value.agent_id != *expected_agent_id
            || value.operation != Some(PromptOperation::StandaloneCompaction)
            || value.replacement_window.len() != 1
            || !value
                .replacement_window
                .iter()
                .filter_map(|item| match item {
                    tau_proto::ContextItem::Message(message) => Some(&message.content),
                    _ => None,
                })
                .flatten()
                .any(|part| {
                    matches!(
                        part,
                        tau_proto::ContentPart::SyntheticCompactionSummary { text }
                            if text == expected_checkpoint
                    )
                })
    }) {
        return Err("durable replacement window changed".into());
    }
    if failures
        .iter()
        .any(|value| value.agent_id != *expected_agent_id)
    {
        return Err("durable compaction failure targeted a different agent".into());
    }
    Ok(())
}

/// Checks that durable replay retains the fixed provider-owned raw syntax
/// without converting it into a summary message.
fn assert_durable_opaque_compaction(
    fixture: &DeterministicFixture,
    expected_agent_id: &tau_proto::AgentId,
) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = tau_e2e_tests::DurableSnapshot::load(
        fixture.harness_state_dir(),
        &"deterministic-e2e-session".parse()?,
    )?;
    let compacted = snapshot
        .agent_events
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentCompacted(value) if value.agent_id == *expected_agent_id => Some(value),
            _ => None,
        });
    let Some(compacted) = compacted else {
        return Err("durable AgentCompacted was not recorded".into());
    };
    if !matches!(
        compacted.replacement_window.as_slice(),
        [ContextItem::Compaction(item)]
            if item.raw_json()
                == tau_e2e_tests::CANONICAL_OPAQUE_COMPACTION_JSON
    ) {
        return Err("durable opaque replacement bytes changed".into());
    }
    Ok(())
}

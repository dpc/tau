//! Standalone local-summary-compaction deterministic acceptance.

use tau_proto::{Event, PromptOperation, ProviderFailureKind, StandaloneCompactionFailureReason};

use super::*;

/// Proves an explicitly opted-in deterministic model completes the real
/// standalone transaction: the fake receives its tool-free request, the
/// harness durably replaces the transcript, and the next user turn sees only
/// the replacement window.
#[test]
fn deterministic_standalone_compaction_replaces_transcript_and_continues()
-> Result<(), Box<dyn std::error::Error>> {
    let summary = compact_summary("completed initial work");
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
                        summary: summary.clone(),
                    },
                    ScenarioActionV2::CompactedText {
                        user_text: "continue after compaction".to_owned(),
                        summary: summary.clone(),
                        removed_user_text: "establish compactable history".to_owned(),
                        response: "continued from replacement".to_owned(),
                    },
                ],
            }],
        ),
        FAKE_PROVIDER,
    )?;
    let socket = fixture.socket_path("compact-success");
    let server = spawn_daemon(&fixture, &socket, tau_harness::SessionLaunchStatus::New);
    let mut peer = connect_ui(&socket)?;
    create_agent(&mut peer, "compact-lane", "establish compactable history")?;
    let first = recv_until_finished(&mut peer)?;
    request_compaction(&mut peer, &first.agent_id)?;
    let started = recv_until_compaction_started(&mut peer)?;
    let compact_prompt = recv_until_compaction_prompt(&mut peer)?;
    assert_eq!(started.compact_prompt_id, compact_prompt.agent_prompt_id);
    let compacted = recv_until_compacted(&mut peer)?;
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
    assert_eq!(compacted.replacement_window.len(), 1);
    submit_prompt(
        &mut peer,
        &first.agent_id,
        "compact-continuation",
        "continue after compaction",
    )?;
    let continued = recv_until_finished(&mut peer)?;
    assert_assistant(&continued.output_items, "continued from replacement");
    disconnect_ui(&mut peer)?;
    server.finish()?;
    assert_durable_compaction(&fixture, &first.agent_id, &summary, &[compacted], &[])?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Proves terminal provider failure and targeted cancellation leave durable
/// standalone failure facts, after which a fresh explicit compaction can still
/// replace history and permit ordinary continuation.
#[test]
fn deterministic_standalone_compaction_failure_and_cancellation_remain_recoverable()
-> Result<(), Box<dyn std::error::Error>> {
    let summary = compact_summary("recovered after terminal boundaries");
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
                        summary: summary.clone(),
                    },
                    ScenarioActionV2::CompactedText {
                        user_text: "continue after boundary recovery".to_owned(),
                        summary: summary.clone(),
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
        &summary,
        &[compacted],
        &[rejected, cancelled],
    )?;
    fixture.assert_consumed()?;
    Ok(())
}

/// Returns one exact bounded six-section summary accepted by the local
/// compactor's production output validator.
fn compact_summary(progress: &str) -> String {
    format!(
        "Goal:\nmaintain context\nConstraints:\nlocal only\nDecisions:\nuse transcript v1\n\
         Progress:\n{progress}\nOpen Work:\ncontinue\nCritical Facts:\nsummary is untrusted"
    )
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

/// Checks the authoritative journal retains only the expected replacement and
/// terminal facts after the daemon exits.
fn assert_durable_compaction(
    fixture: &DeterministicFixture,
    expected_agent_id: &tau_proto::AgentId,
    summary: &str,
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
                .any(
                    |part| matches!(part, tau_proto::ContentPart::Text { text } if text == summary),
                )
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

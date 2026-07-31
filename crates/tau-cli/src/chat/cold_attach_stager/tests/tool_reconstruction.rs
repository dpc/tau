//! Tool reconstruction and bounded-fold regressions.

use super::*;

/// Builds one dispatched tool start; requests that never reach this event must
/// never create reconstructed pending UI.
fn tool_started(call_id: &str) -> Event {
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("fixture"),
        arguments: tau_proto::CborValue::Null,
        agent_id: "agent-1".parse().expect("valid agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}

/// Builds one canonical failed-tool terminal.
fn tool_error(call_id: &str) -> Event {
    Event::ToolError(tau_proto::ToolError {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("fixture"),
        tool_type: tau_proto::ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    })
}

/// Builds one live progress frame retained behind the reconstructed baseline.
fn tool_progress(call_id: &str) -> Event {
    Event::ToolProgress(tau_proto::ToolProgress {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("fixture"),
        message: Some("working".to_owned()),
        progress: None,
        display: None,
    })
}

/// Builds the current session identity used to scope loaded-agent replay.
fn session_started() -> Event {
    Event::SessionStarted(tau_proto::SessionStarted {
        session_id: "session-1".parse().expect("valid session id"),
        reason: tau_proto::SessionStartReason::Initial,
    })
}

/// Builds current membership for the transcript-owning fixture agent.
fn agent_loaded() -> Event {
    Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
        session_id: "session-1".parse().expect("valid session id"),
        agent_id: "agent-1".parse().expect("valid agent id"),
        agent_initialization_id: "fixture-init".parse().expect("valid initialization id"),
        ephemeral: false,
    })
}

/// Builds loaded membership with explicit session and agent identities.
fn agent_loaded_for(session_id: &str, agent_id: &str) -> Event {
    let mut event = agent_loaded();
    let Event::SessionAgentLoaded(loaded) = &mut event else {
        unreachable!("agent_loaded helper returns membership");
    };
    loaded.session_id = session_id.parse().expect("valid session id");
    loaded.agent_id = agent_id.parse().expect("valid agent id");
    event
}

/// Builds removal of the transcript-owning fixture agent.
fn agent_unloaded() -> Event {
    Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
        session_id: "session-1".parse().expect("valid session id"),
        agent_id: "agent-1".parse().expect("valid agent id"),
    })
}

/// Builds removal scoped to an explicit session.
fn agent_unloaded_for(session_id: &str) -> Event {
    let mut event = agent_unloaded();
    let Event::SessionAgentUnloaded(unloaded) = &mut event else {
        unreachable!("agent_unloaded helper returns membership removal");
    };
    unloaded.session_id = session_id.parse().expect("valid session id");
    event
}

/// A historical start closed before attach must not be resurrected as pending.
#[test]
fn completed_tool_is_absent_from_reconstructed_pending_rows() {
    let mut stager = ColdAttachStager::staging();
    assert!(stager.admit(replay(tool_started("done"), 1, 1)).is_empty());
    let terminal = stager.admit(replay(tool_error("done"), 1, 2));
    assert!(matches!(terminal.as_slice(), [delivery] if delivery.delivery_id == 2));

    let ready = stager.admit(live(replay_complete(), 3));

    assert!(matches!(ready.as_slice(), [boundary] if boundary.delivery_id == 3));
}

/// A dispatched start with no terminal remains pending even when it produced no
/// progress, while duplicate historical starts still produce one row.
#[test]
fn active_silent_tool_reconstructs_once() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 0)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 1)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 2))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 3))
            .is_empty()
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 4))
            .is_empty()
    );

    let ready = stager.admit(live(replay_complete(), 5));

    assert!(matches!(
        ready.as_slice(),
        [start, boundary]
            if matches!(&start.event, Event::ToolStarted(started) if started.call_id.as_str() == "active")
                && matches!(
                    &start.presentation,
                    RendererPresentation::ReconstructedToolStart { owner }
                        if owner.as_str() == "agent-1"
                )
                && matches!(boundary.event, Event::SessionReplayComplete(_))
    ));
}

/// A real terminal racing replay wins over the historical start, and buffered
/// live frames retain their wire order after the folded baseline.
#[test]
fn live_terminal_during_attach_prevents_stale_pending_row() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 0)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 1)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("racing"), 1, 2))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("racing"), 1, 3))
            .is_empty()
    );
    assert!(stager.admit(live(tool_progress("racing"), 4)).is_empty());
    assert!(stager.admit(live(tool_error("racing"), 5)).is_empty());

    let ready = stager.admit(live(replay_complete(), 6));

    assert!(matches!(
        ready.as_slice(),
        [progress, terminal, boundary]
            if progress.delivery_id == 4
                && terminal.delivery_id == 5
                && matches!(boundary.event, Event::SessionReplayComplete(_))
    ));
}

/// Builds one provider response containing the transcript-owned tool call.
fn tool_call_response(call_id: &str) -> Event {
    Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: "prompt-1".parse().expect("valid prompt id"),
        agent_id: "agent-1".parse().expect("valid agent id"),
        output_items: vec![tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
            call_id: call_id.into(),
            name: tau_proto::ToolName::new("test_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: tau_proto::CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
}

/// Builds transcript ownership for an explicit agent.
fn tool_call_response_for(call_id: &str, agent_id: &str) -> Event {
    let mut event = tool_call_response(call_id);
    let Event::ProviderResponseFinished(finished) = &mut event else {
        unreachable!("tool_call_response helper returns provider response");
    };
    finished.agent_id = agent_id.parse().expect("valid agent id");
    event
}

/// A selected tool response must flush already-held history and disable
/// staging.
#[test]
fn tool_history_keeps_protocol_order() {
    let prompt = historical_prompt("before tool");
    let tool = tool_call_response("call-1");
    let after = historical_prompt("after tool");
    let mut stager = ColdAttachStager::staging();

    assert!(stager.admit(replay(prompt.clone(), 1, 1)).is_empty());
    let tool_ready = stager.admit(replay(tool.clone(), 1, 2));
    let after_ready = stager.admit(replay(after.clone(), 1, 3));

    assert!(matches!(
        tool_ready.as_slice(),
        [held, tool_event]
            if held.event == prompt
                && held.delivery_id == 1
                && tool_event.event == tool
                && tool_event.delivery_id == 2
    ));
    assert!(matches!(after_ready.as_slice(), [value] if value.event == after));
}

/// Tool-bearing history can end plain transcript staging before its dispatched
/// start arrives. The replay boundary must still publish that folded start.
#[test]
fn replay_boundary_drains_pending_start_after_tool_history_ends_staging() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 0)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 1)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 2))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 3))
            .is_empty()
    );

    let ready = stager.admit(live(replay_complete(), 4));

    assert!(matches!(
        ready.as_slice(),
        [start, boundary]
            if matches!(&start.event, Event::ToolStarted(started)
                if started.call_id.as_str() == "active")
                && matches!(boundary.event, Event::SessionReplayComplete(_))
    ));
}

/// Reconstructed starts must join both current loaded membership and a replayed
/// provider-declared tool call; an unloaded owner fails closed.
#[test]
fn replay_boundary_excludes_start_for_unloaded_agent() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("orphan"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("orphan"), 1, 4))
            .is_empty()
    );
    assert_eq!(stager.admit(replay(agent_unloaded(), 1, 5)).len(), 1);

    let ready = stager.admit(live(replay_complete(), 6));

    assert!(matches!(ready.as_slice(), [boundary]
        if matches!(boundary.event, Event::SessionReplayComplete(_))));
}

/// An unload fact from another session cannot erase valid current membership.
#[test]
fn replay_boundary_ignores_unload_from_another_session() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 4))
            .is_empty()
    );
    assert_eq!(
        stager
            .admit(replay(agent_unloaded_for("session-other"), 1, 5))
            .len(),
        1
    );

    let ready = stager.admit(live(replay_complete(), 6));
    assert!(matches!(ready.as_slice(), [start, boundary]
        if matches!(start.event, Event::ToolStarted(_))
            && matches!(boundary.event, Event::SessionReplayComplete(_))));
}

/// Membership from another session cannot authorize a reconstructed start.
#[test]
fn replay_boundary_excludes_agent_loaded_in_another_session() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(agent_loaded_for("session-other", "agent-1"), 1, 2))
            .len(),
        1
    );
    assert_eq!(
        stager
            .admit(replay(tool_call_response("orphan"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("orphan"), 1, 4))
            .is_empty()
    );

    let ready = stager.admit(live(replay_complete(), 5));
    assert!(matches!(ready.as_slice(), [boundary]
        if matches!(boundary.event, Event::SessionReplayComplete(_))));
}

/// A start without a matching provider transcript call fails closed.
#[test]
fn replay_boundary_excludes_start_without_transcript_ownership() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert!(
        stager
            .admit(replay(tool_started("orphan"), 1, 3))
            .is_empty()
    );

    let ready = stager.admit(live(replay_complete(), 4));
    assert!(matches!(ready.as_slice(), [boundary]
        if matches!(boundary.event, Event::SessionReplayComplete(_))));
}

/// Transcript ownership alone cannot replace explicit current-session loaded
/// membership evidence.
#[test]
fn replay_boundary_excludes_owned_start_without_session_or_membership() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(
        stager
            .admit(replay(tool_call_response("orphan"), 1, 1))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("orphan"), 1, 2))
            .is_empty()
    );

    let ready = stager.admit(live(replay_complete(), 3));
    assert!(matches!(ready.as_slice(), [boundary]
        if matches!(boundary.event, Event::SessionReplayComplete(_))));
}

/// Transcript ownership by another agent cannot authorize the loaded starter.
#[test]
fn replay_boundary_excludes_start_owned_by_another_agent() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(
                tool_call_response_for("orphan", "agent-other"),
                1,
                3
            ))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("orphan"), 1, 4))
            .is_empty()
    );

    let ready = stager.admit(live(replay_complete(), 5));
    assert!(matches!(ready.as_slice(), [boundary]
        if matches!(boundary.event, Event::SessionReplayComplete(_))));
}

/// A failed replay boundary never publishes a pending baseline.
#[test]
fn replay_error_discards_reconstructed_pending_start() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 4))
            .is_empty()
    );
    let mut boundary = replay_complete();
    let Event::SessionReplayComplete(complete) = &mut boundary else {
        unreachable!("replay_complete helper returns boundary");
    };
    complete.error = Some("fixture replay failed".to_owned());

    let ready = stager.admit(live(boundary, 5));
    assert!(matches!(ready.as_slice(), [boundary]
        if matches!(boundary.event, Event::SessionReplayComplete(_))));
}

/// A provider error is the durable failed-call terminal and must close and
/// visibly project the replayed start instead of leaving a pending row.
#[test]
fn replayed_provider_error_closes_pending_start() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("failed"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("failed"), 1, 4))
            .is_empty()
    );

    let terminal = stager.admit(replay(
        match tool_error("failed") {
            Event::ToolError(error) => Event::ProviderToolError(error),
            _ => unreachable!("fixture returns a tool error"),
        },
        1,
        5,
    ));
    let boundary = stager.admit(live(replay_complete(), 6));

    assert!(matches!(terminal.as_slice(), [delivery]
        if matches!(delivery.event, Event::ToolError(_))));
    assert!(matches!(boundary.as_slice(), [delivery]
        if matches!(delivery.event, Event::SessionReplayComplete(_))));
}

/// Every canonical UI terminal closes a fold, while the foreground background
/// placeholder deliberately keeps the call pending.
#[test]
fn canonical_tool_terminal_matrix_excludes_background_placeholder() {
    let base_result = |kind| {
        Event::ToolResultDisplay(tau_proto::ToolResultDisplay {
            call_id: "call".into(),
            tool_name: tau_proto::ToolName::new("fixture"),
            tool_type: tau_proto::ToolType::Function,
            kind,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })
    };
    let terminals = [
        Event::ToolRejected(tau_proto::ToolRejected {
            call_id: "call".into(),
            tool_name: tau_proto::ToolName::new("fixture"),
            tool_type: tau_proto::ToolType::Function,
            message: "rejected".to_owned(),
            originator: tau_proto::PromptOriginator::User,
        }),
        base_result(tau_proto::ToolResultKind::Final),
        tool_error("call"),
        Event::ToolCancelled(tau_proto::ToolCancelled {
            call_id: "call".into(),
            tool_name: tau_proto::ToolName::new("fixture"),
            tool_type: tau_proto::ToolType::Function,
        }),
        Event::ToolBackgroundResultDisplay(tau_proto::ToolBackgroundResultDisplay {
            call_id: "call".into(),
            tool_name: tau_proto::ToolName::new("fixture"),
            tool_type: tau_proto::ToolType::Function,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "call".into(),
            tool_name: tau_proto::ToolName::new("fixture"),
            tool_type: tau_proto::ToolType::Function,
            message: "failed".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    ];

    assert!(
        terminals
            .iter()
            .all(|event| tool_terminal_id(event).is_some())
    );
    assert!(
        tool_terminal_id(&base_result(
            tau_proto::ToolResultKind::BackgroundPlaceholder
        ))
        .is_none()
    );
}

/// The replay boundary closes tool buffering independently from shell snapshot
/// draining, so later live tool frames pass through immediately.
#[test]
fn live_tool_after_boundary_does_not_wait_for_shell_draining() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(shell_start(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(live(replay_complete(), 2)).len(), 1);

    let ready = stager.admit(live(tool_started("live"), 3));

    assert!(matches!(ready.as_slice(), [delivery]
        if matches!(delivery.event, Event::ToolStarted(_))
            && matches!(delivery.presentation, RendererPresentation::Ordinary)));
}

/// A pre-boundary disconnect drains retained starts instead of dropping them.
#[test]
fn disconnect_drains_reconstructed_pending_start() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 4))
            .is_empty()
    );

    let ready = stager.finish_before_disconnect();

    assert!(matches!(ready.as_slice(), [delivery]
        if matches!(delivery.event, Event::ToolStarted(_))));
}

/// Tool reconstruction shares the byte bound and falls back to deterministic
/// baseline-then-live delivery when one additional frame would exceed it.
#[test]
fn tool_reconstruction_byte_overflow_flushes_and_stops_buffering() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), RENDERER_QUEUE_MAX_BYTES, 4,))
            .is_empty()
    );

    let ready = stager.admit(live(tool_error("active"), 5));

    assert!(matches!(ready.as_slice(), [terminal]
        if matches!(terminal.event, Event::ToolError(_))));
    assert_eq!(stager.admit(live(tool_started("later"), 6)).len(), 1);
}

/// Pending starts and buffered live frames share the item bound; overflow emits
/// the baseline first, then every buffered/current live frame exactly once.
#[test]
fn tool_reconstruction_item_overflow_preserves_baseline_live_order() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 4))
            .is_empty()
    );
    let buffered_capacity = RENDERER_QUEUE_MAX_ITEMS - stager.retained_usage().items;
    for offset in 0..buffered_capacity {
        assert!(
            stager
                .admit(live(tool_progress("active"), 5 + offset as u64))
                .is_empty()
        );
    }

    let ready = stager.admit(live(tool_progress("active"), 5 + buffered_capacity as u64));

    assert_eq!(ready.len(), buffered_capacity + 2);
    assert!(matches!(
        ready.first().map(|delivery| &delivery.event),
        Some(Event::ToolStarted(_))
    ));
    assert!(
        ready[1..]
            .iter()
            .all(|delivery| matches!(delivery.event, Event::ToolProgress(_)))
    );
}

/// Historical overflow suppresses every replayed start until the boundary
/// instead of exposing a partial, unowned, completed, or unloaded baseline.
#[test]
fn historical_tool_overflow_fails_closed_until_replay_boundary() {
    let mut stager = ColdAttachStager::staging();
    for index in 0..=RENDERER_QUEUE_MAX_ITEMS {
        assert!(
            stager
                .admit(replay(
                    tool_started(&format!("call-{index}")),
                    1,
                    index as u64
                ))
                .is_empty()
        );
    }
    assert!(matches!(
        stager.tool_reconciliation,
        ToolReconciliation::FailedClosed
    ));

    let terminal = stager.admit(replay(tool_error("call-0"), 1, 2_000));
    assert!(matches!(terminal.as_slice(), [delivery]
        if matches!(delivery.event, Event::ToolError(_))));
    assert_eq!(
        stager
            .admit(replay(tool_call_response("call-late"), 1, 2_001))
            .len(),
        1
    );
    assert_eq!(stager.admit(replay(agent_unloaded(), 1, 2_002)).len(), 1);
    assert!(
        stager
            .admit(replay(tool_started("call-late"), 1, 2_003))
            .is_empty()
    );

    let boundary = stager.admit(live(replay_complete(), 2_004));
    assert!(matches!(boundary.as_slice(), [delivery]
        if matches!(delivery.event, Event::SessionReplayComplete(_))));
}

/// Reconstruction indexes share the item bound, are consumed at the boundary,
/// and cannot regrow from post-boundary provider traffic.
#[test]
fn ownership_metadata_is_bounded_and_cleared_at_boundary() {
    let mut stager = ColdAttachStager::staging();
    for index in 0..=RENDERER_QUEUE_MAX_ITEMS {
        assert_eq!(
            stager
                .admit(replay(
                    tool_call_response(&format!("call-{index}")),
                    1,
                    index as u64
                ))
                .len(),
            1
        );
    }
    assert!(matches!(
        stager.tool_reconciliation,
        ToolReconciliation::FailedClosed
    ));
    assert_eq!(stager.admit(live(replay_complete(), 2_000)).len(), 1);
    assert!(matches!(
        stager.tool_reconciliation,
        ToolReconciliation::Disabled
    ));

    for index in 0..RENDERER_QUEUE_MAX_ITEMS {
        assert_eq!(
            stager
                .admit(replay(
                    tool_call_response(&format!("post-{index}")),
                    RENDERER_QUEUE_MAX_BYTES,
                    3_000 + index as u64
                ))
                .len(),
            1
        );
    }
    assert_eq!(stager.retained_usage(), RetainedUsage::default());
}

/// A live scope update that cannot fit invalidates the historical baseline
/// rather than authorizing it from stale pre-update membership.
#[test]
fn live_scope_metadata_overflow_discards_stale_pending_baseline() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(session_started(), 1, 1)).len(), 1);
    assert_eq!(stager.admit(replay(agent_loaded(), 1, 2)).len(), 1);
    assert_eq!(
        stager
            .admit(replay(tool_call_response("active"), 1, 3))
            .len(),
        1
    );
    assert!(
        stager
            .admit(replay(tool_started("active"), 1, 4))
            .is_empty()
    );

    let mut replacement = live(agent_loaded_for("session-other", "agent-1"), 5);
    replacement.queue_bytes = RENDERER_QUEUE_MAX_BYTES;
    let ready = stager.admit(replacement);
    assert!(matches!(ready.as_slice(), [membership]
        if matches!(membership.event, Event::SessionAgentLoaded(_))));
    assert!(matches!(
        stager.tool_reconciliation,
        ToolReconciliation::FailedClosed
    ));

    let boundary = stager.admit(live(replay_complete(), 6));
    assert!(matches!(boundary.as_slice(), [delivery]
        if matches!(delivery.event, Event::SessionReplayComplete(_))));
}

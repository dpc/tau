//! Focused cold-attach staging tests.

mod tool_reconstruction;

use tau_cli_term::RendererDeliveryId;
use tau_proto::{Event, UnixMicros};

use super::{
    ColdAttachStager, RENDERER_QUEUE_MAX_BYTES, RENDERER_QUEUE_MAX_ITEMS, RendererDelivery,
    RendererPresentation, RetainedUsage, ShellStartPresentation, ToolReconciliation,
    renderer_event_from_delivery, tool_terminal_id,
};
use crate::event_renderer::selection_intent::UiTarget;

/// Builds one plain replayable transcript prompt.
fn historical_prompt(text: &str) -> Event {
    historical_prompt_for("agent-1", text)
}

/// Builds one plain replayable transcript prompt for a specific owner.
fn historical_prompt_for(agent_id: &str, text: &str) -> Event {
    Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
        literal: false,
        session_id: "session-1".parse().expect("valid session id"),
        text: text.to_owned(),
        agent_id: agent_id.parse().expect("valid agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })
}

/// Applies one staged delivery through the same presentation-specific renderer
/// entry point as the production renderer loop.
fn render_delivery(
    renderer: &mut crate::event_renderer::EventRenderer,
    delivery: RendererDelivery,
) {
    match delivery.presentation {
        RendererPresentation::Ordinary => renderer.handle_socket_delivery(
            &delivery.event,
            delivery.recorded_at,
            delivery.delivery_id,
        ),
        RendererPresentation::Replay => renderer.handle_replay_socket_delivery(
            &delivery.event,
            delivery.recorded_at,
            delivery.delivery_id,
        ),
        RendererPresentation::ColdAttachReplay => {
            renderer.handle_cold_attach_replay_socket_delivery(
                &delivery.event,
                delivery.recorded_at,
                delivery.delivery_id,
            );
        }
        RendererPresentation::ReconstructedToolStart { .. } => {
            renderer.handle_cold_attach_replay_socket_delivery(
                &delivery.event,
                delivery.recorded_at,
                delivery.delivery_id,
            );
        }
        RendererPresentation::FinishAttach { agent_id } => {
            renderer.handle_attach_replay_complete_socket_delivery(
                &delivery.event,
                agent_id.as_deref(),
                delivery.recorded_at,
                delivery.delivery_id,
            );
        }
        RendererPresentation::StandaloneShellTerminal => {
            unreachable!("selection regression does not stage shell terminals")
        }
    }
}

/// Wraps a replay event with deterministic queue metadata.
fn replay(event: Event, queue_bytes: usize, delivery_id: u64) -> RendererDelivery {
    RendererDelivery {
        abandoned_shell_starts: Vec::new(),
        event: Box::new(event),
        replay: true,
        recorded_at: UnixMicros::new(1),
        queue_bytes,
        delivery_id: RendererDeliveryId::new(delivery_id),
        presentation: RendererPresentation::Ordinary,
    }
}

/// Wraps a live event with deterministic queue metadata.
fn live(event: Event, delivery_id: u64) -> RendererDelivery {
    RendererDelivery {
        abandoned_shell_starts: Vec::new(),
        event: Box::new(event),
        replay: false,
        recorded_at: UnixMicros::new(2),
        queue_bytes: 1,
        delivery_id: RendererDeliveryId::new(delivery_id),
        presentation: RendererPresentation::Ordinary,
    }
}

/// Returns the allocation identity that must survive renderer admission.
fn event_allocation(event: &Event) -> *const Event {
    std::ptr::from_ref(event)
}

/// Socket conversion keeps the decoded event box for ordinary delivery and
/// still discards replay-only terminal side effects before staging.
#[test]
fn socket_conversion_preserves_event_allocation_and_filters_replay_side_effects() {
    let ordinary =
        tau_proto::EventDelivery::live(UnixMicros::new(9), historical_prompt("ordinary"));
    let ordinary_allocation = event_allocation(ordinary.event.as_ref());
    let ordinary = renderer_event_from_delivery(ordinary, 42, RendererDeliveryId::new(7))
        .expect("ordinary delivery");
    assert_eq!(
        event_allocation(ordinary.event.as_ref()),
        ordinary_allocation
    );

    let side_effect = tau_proto::EventDelivery::replay(
        UnixMicros::new(10),
        Event::TermBell(tau_proto::TermBell {}),
    );
    assert!(renderer_event_from_delivery(side_effect, 1, RendererDeliveryId::new(8)).is_none());
}

/// Cold-attach staging retains a decoded transcript box and releases that same
/// allocation at its replay boundary.
#[test]
fn cold_attach_staging_preserves_event_allocations() {
    let mut stager = ColdAttachStager::staging();
    let transcript =
        tau_proto::EventDelivery::replay(UnixMicros::new(1), historical_prompt("held"));
    let transcript_allocation = event_allocation(transcript.event.as_ref());
    let transcript = renderer_event_from_delivery(transcript, 1, RendererDeliveryId::new(1))
        .expect("transcript delivery");
    assert!(stager.admit(transcript).is_empty());

    let ready = stager.admit(live(replay_complete(), 2));
    assert_eq!(
        event_allocation(ready[0].event.as_ref()),
        transcript_allocation
    );
}

/// Replay normalization changes the event kind in its existing decoded box so
/// cold-attach filtering does not replace the allocation.
#[test]
fn replay_tool_normalization_preserves_event_allocation() {
    let mut stager = ColdAttachStager::pass_through();
    let delivery = replay(
        Event::ProviderToolError(tau_proto::ToolError {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("fixture"),
            tool_type: tau_proto::ToolType::Function,
            message: "historical failure".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,
            display: None,
        }),
        1,
        1,
    );
    let allocation = event_allocation(delivery.event.as_ref());
    let ready = stager.admit(delivery);
    assert!(matches!(ready[0].event.as_ref(), Event::ToolError(_)));
    assert_eq!(event_allocation(ready[0].event.as_ref()), allocation);
}

/// Builds the catch-up boundary used to publish the folded pending baseline.
fn replay_complete() -> Event {
    Event::SessionReplayComplete(tau_proto::SessionReplayComplete {
        session_id: "session-1".parse().expect("valid session id"),
        error: None,
    })
}

/// Builds one per-agent replay terminal with an optional incompatibility error.
fn agent_replay_complete(agent_id: &str, error: Option<&str>) -> Event {
    Event::AgentReplayComplete(tau_proto::AgentReplayComplete {
        agent_id: agent_id.parse().expect("valid agent id"),
        session_id: Some("session-1".parse().expect("valid session id")),
        error: error.map(str::to_owned),
    })
}

/// Builds one current resumed-runtime snapshot for attach selection.
fn agent_stats(agent_id: &str) -> Event {
    Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: "session-1".parse().expect("valid session id"),
        agent_id: agent_id.parse().expect("valid agent id"),
        work_status: Default::default(),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
    })
}

/// Builds one replayed tool terminal that ends plain transcript staging before
/// the later attach-selection facts arrive.
fn historical_tool_error() -> Event {
    Event::ToolError(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: "historical-call".into(),
        tool_name: tau_proto::ToolName::new("fixture"),
        tool_type: tau_proto::ToolType::Function,
        message: "historical failure".to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    })
}

/// A terminal observed before a duplicate reconstructed start must keep that
/// completed call out of the pending attach baseline.
#[test]
fn late_duplicate_start_after_terminal_stays_settled() {
    let mut stager = ColdAttachStager::staging();
    let terminal = Event::ToolError(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: "settled-call".into(),
        tool_name: tau_proto::ToolName::new("fixture"),
        tool_type: tau_proto::ToolType::Function,
        message: "done".to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    });
    assert_eq!(stager.admit(replay(terminal, 1, 1)).len(), 1);
    let start = Event::ToolStarted(tau_proto::ToolStarted {
        invocation_policy: Default::default(),
        call_id: "settled-call".into(),
        tool_name: tau_proto::ToolName::new("fixture"),
        arguments: tau_proto::CborValue::Null,
        agent_id: "agent-1".parse().expect("valid agent id"),
        originator: tau_proto::PromptOriginator::User,
    });
    assert!(stager.admit(replay(start, 1, 2)).is_empty());
    let ready = stager.admit(live(replay_complete(), 3));
    assert!(
        !ready.iter().any(|delivery| {
            matches!(
                delivery.event.as_ref(),
                Event::ToolStarted(started) if started.call_id.as_str() == "settled-call"
            )
        }),
        "settled duplicate start must not reappear at attach completion"
    );
}

/// Attach must select the only agent whose journal replay succeeded and whose
/// current runtime snapshot proves it is actually restored, even after
/// tool-bearing history ends plain transcript staging.
#[test]
fn selects_unique_successful_runtime_agent_at_replay_boundary() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(replay(historical_tool_error(), 1, 1)).len(), 1);
    assert_eq!(
        stager
            .admit(live(
                agent_replay_complete("legacy-agent", Some("incompatible journal")),
                2,
            ))
            .len(),
        1
    );
    assert_eq!(
        stager
            .admit(live(agent_replay_complete("restored-agent", None), 3))
            .len(),
        1
    );
    assert_eq!(
        stager.admit(live(agent_stats("restored-agent"), 4)).len(),
        1
    );

    let ready = stager.admit(live(replay_complete(), 5));

    assert!(matches!(
        ready.last().map(|delivery| &delivery.presentation),
        Some(RendererPresentation::FinishAttach {
            agent_id: Some(agent_id),
        }) if agent_id.as_str() == "restored-agent"
    ));
}

/// Attach-selection metadata shares the cold-attach item/byte budget, fails
/// closed on overflow, and releases retained entries on disconnect.
#[test]
fn bounds_and_releases_attach_selection_metadata() {
    let mut overflowing = ColdAttachStager::staging();
    assert_eq!(
        overflowing
            .admit(replay(
                agent_replay_complete("agent-a", None),
                RENDERER_QUEUE_MAX_BYTES,
                1,
            ))
            .len(),
        1
    );
    assert_eq!(
        overflowing.retained_usage(),
        RetainedUsage {
            items: 1,
            bytes: RENDERER_QUEUE_MAX_BYTES,
        }
    );
    assert_eq!(
        overflowing
            .admit(replay(agent_stats("agent-a"), 1, 2))
            .len(),
        1
    );
    assert_eq!(overflowing.retained_usage(), RetainedUsage::default());
    let boundary = overflowing.admit(live(replay_complete(), 3));
    assert!(matches!(
        boundary.last().map(|delivery| &delivery.presentation),
        Some(RendererPresentation::FinishAttach { agent_id: None })
    ));

    let mut disconnected = ColdAttachStager::staging();
    assert_eq!(
        disconnected
            .admit(replay(agent_replay_complete("agent-a", None), 7, 4))
            .len(),
        1
    );
    assert_eq!(disconnected.retained_usage().items, 1);
    assert!(disconnected.finish_before_disconnect().is_empty());
    assert_eq!(disconnected.retained_usage(), RetainedUsage::default());
}

/// Attach must keep the no-agent state honest when multiple successfully
/// replayed agents have current runtimes instead of choosing arbitrarily.
#[test]
fn leaves_ambiguous_runtime_agents_unselected() {
    let mut stager = ColdAttachStager::staging();
    for (delivery_id, agent_id) in [(1, "agent-a"), (2, "agent-b")] {
        assert_eq!(
            stager
                .admit(live(agent_replay_complete(agent_id, None), delivery_id))
                .len(),
            1
        );
        assert_eq!(
            stager
                .admit(live(agent_stats(agent_id), delivery_id + 2))
                .len(),
            1
        );
    }

    let ready = stager.admit(live(replay_complete(), 5));

    assert!(matches!(
        ready.last().map(|delivery| &delivery.presentation),
        Some(RendererPresentation::FinishAttach { agent_id: None })
    ));
}

/// Live prompt traffic racing catch-up remains display-only until the attach
/// boundary resolves an empty candidate set to overview.
#[test]
fn live_prompt_before_empty_boundary_cannot_claim_initial_selection() {
    let (_term, handle, _vt) = crate::tests::setup(100, 24);
    let mut renderer = crate::tests::marker_test_renderer(handle);
    let selected = renderer.current_agent_state();
    let mut stager = ColdAttachStager::staging();

    let ready = stager.admit(live(
        historical_prompt_for("live-agent", "racing live prompt"),
        1,
    ));
    assert!(matches!(
        ready.as_slice(),
        [delivery]
            if matches!(
                delivery.presentation,
                RendererPresentation::ColdAttachReplay
            )
    ));
    for delivery in ready {
        render_delivery(&mut renderer, delivery);
    }
    assert!(
        selected
            .lock()
            .expect("selection intent")
            .selected_agent_id()
            .is_none()
    );

    for delivery in stager.admit(live(replay_complete(), 2)) {
        render_delivery(&mut renderer, delivery);
    }
    let intent = selected.lock().expect("selection intent");
    assert!(intent.selected_agent_id().is_none());
    assert!(matches!(intent.target(), UiTarget::Overview));
}

/// Transcript rows for multiple valid restored agents must remain display-only
/// until the boundary rejects the ambiguous automatic selection.
#[test]
fn ambiguous_attach_transcript_rows_remain_unselected_in_renderer() {
    let (_term, handle, _vt) = crate::tests::setup(100, 24);
    let mut renderer = crate::tests::marker_test_renderer(handle);
    let selected = renderer.current_agent_state();
    let mut stager = ColdAttachStager::staging();
    for (delivery_id, agent_id) in [(1, "agent-a"), (2, "agent-b")] {
        assert!(
            stager
                .admit(replay(
                    historical_prompt_for(agent_id, agent_id),
                    1,
                    delivery_id,
                ))
                .is_empty()
        );
        for delivery in stager.admit(live(agent_replay_complete(agent_id, None), delivery_id + 2)) {
            render_delivery(&mut renderer, delivery);
        }
        for delivery in stager.admit(live(agent_stats(agent_id), delivery_id + 4)) {
            render_delivery(&mut renderer, delivery);
        }
    }
    for delivery in stager.admit(live(replay_complete(), 7)) {
        render_delivery(&mut renderer, delivery);
    }

    assert!(
        selected
            .lock()
            .expect("selection intent")
            .as_ref()
            .is_none()
    );
}

/// An early historical row absent from the current runtime cannot preempt the
/// different agent selected by the successful-replay/runtime intersection.
#[test]
fn non_runtime_replay_row_cannot_preempt_unique_boundary_candidate() {
    let (_term, handle, _vt) = crate::tests::setup(100, 24);
    let mut renderer = crate::tests::marker_test_renderer(handle);
    let selected = renderer.current_agent_state();
    let mut stager = ColdAttachStager::staging();
    assert!(
        stager
            .admit(replay(
                historical_prompt_for("historical-agent", "old"),
                1,
                1,
            ))
            .is_empty()
    );
    for (delivery_id, event) in [
        (2, agent_replay_complete("historical-agent", None)),
        (3, agent_replay_complete("restored-agent", None)),
        (4, agent_stats("restored-agent")),
    ] {
        for delivery in stager.admit(live(event, delivery_id)) {
            render_delivery(&mut renderer, delivery);
        }
    }
    for delivery in stager.admit(live(replay_complete(), 5)) {
        render_delivery(&mut renderer, delivery);
    }

    assert_eq!(
        selected.lock().expect("selection intent").as_deref(),
        Some("restored-agent")
    );
}

/// Builds one correlated user-shell start.
fn shell_start() -> Event {
    Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: "session-1".parse().expect("valid session id"),
        command_id: tau_proto::ShellCommandId::parse("shell-1").expect("valid command id"),
        command: "fixture-block".to_owned(),
        include_in_context: true,
        target_agent_id: Some("agent-1".parse().expect("valid agent id")),
    })
}

/// Live starts observed before Subscribe win over the replay snapshot, and the
/// terminal releases the bounded correlation entry for a future lifecycle
/// without replacing either delivery's event box.
#[test]
fn deduplicates_live_shell_start_before_replay_snapshot() {
    let mut stager = ColdAttachStager::staging();
    let shell = live(shell_start(), 1);
    let shell_allocation = event_allocation(shell.event.as_ref());
    let ready = stager.admit(shell);
    assert_eq!(event_allocation(ready[0].event.as_ref()), shell_allocation);

    let finished = Event::ShellCommandFinished(tau_proto::ShellCommandFinished {
        command_id: tau_proto::ShellCommandId::parse("shell-1").expect("valid command id"),
        session_id: "session-1".parse().expect("valid session id"),
        command: "fixture-block".to_owned(),
        include_in_context: true,
        target_agent_id: Some("agent-1".parse().expect("valid agent id")),
        output: "done".to_owned(),
        exit_code: None,
        cancelled: true,
    });
    let historical_delivery = replay(finished.clone(), 1, 2);
    let historical_allocation = event_allocation(historical_delivery.event.as_ref());
    let historical = stager.admit(historical_delivery);
    assert_eq!(
        event_allocation(historical[0].event.as_ref()),
        historical_allocation
    );
    assert!(matches!(
        historical[0].event.as_ref(),
        Event::ShellCommandFinished(old)
            if old.command_id.as_str() == "shell-1"
                && old.command == "fixture-block"
                && old.cancelled
    ));
    assert!(matches!(
        historical[0].presentation,
        RendererPresentation::StandaloneShellTerminal
    ));
    assert!(stager.admit(replay(shell_start(), 1, 3)).is_empty());

    assert_eq!(stager.admit(live(finished, 3)).len(), 1);
    assert_eq!(stager.admit(live(shell_start(), 4)).len(), 1);
}

/// Replay completion explicitly abandons a pre-boundary start that the running
/// snapshot did not confirm, so a completed `!!` cannot leave an orphan row.
#[test]
fn replay_boundary_abandons_unconfirmed_shell_start() {
    let mut stager = ColdAttachStager::pass_through();
    assert_eq!(stager.admit(live(shell_start(), 1)).len(), 1);
    let boundary = Event::SessionReplayComplete(tau_proto::SessionReplayComplete {
        session_id: "session-1".parse().expect("valid session id"),
        error: None,
    });

    let ready = stager.admit(replay(boundary, 1, 2));

    assert!(matches!(
        ready.as_slice(),
        [delivery]
            if delivery.abandoned_shell_starts
                == [ShellStartPresentation {
                    command_id: tau_proto::ShellCommandId::parse("shell-1")
                        .expect("valid command id"),
                    target_agent_id: Some("agent-1".parse().expect("valid agent id")),
                }]
    ));
    assert_eq!(stager.admit(live(shell_start(), 3)).len(), 1);
}

/// Replayed terminal-output events must never repeat old terminal side effects.
#[test]
fn drops_replayed_terminal_output_events() {
    let recorded_at = UnixMicros::new(123);
    for event in [
        Event::TermBell(tau_proto::TermBell {}),
        Event::Osc1337SetUserVar(tau_proto::Osc1337SetUserVar {
            name: "status".to_owned(),
            value: "ready".to_owned(),
        }),
    ] {
        let delivery = tau_proto::EventDelivery::replay(recorded_at, event);
        assert!(renderer_event_from_delivery(delivery, 1, RendererDeliveryId::new(7)).is_none());
    }
}

/// Live terminal-output events must retain metadata through staging conversion.
#[test]
fn keeps_live_terminal_output_events() {
    let recorded_at = UnixMicros::new(123);
    for event in [
        Event::TermBell(tau_proto::TermBell {}),
        Event::Osc1337SetUserVar(tau_proto::Osc1337SetUserVar {
            name: "status".to_owned(),
            value: "ready".to_owned(),
        }),
    ] {
        let delivery = tau_proto::EventDelivery::live(recorded_at, event.clone());
        let rendered = renderer_event_from_delivery(delivery, 1, RendererDeliveryId::new(7))
            .expect("live terminal output is retained");
        assert_eq!(rendered.event.as_ref(), &event);
        assert_eq!(rendered.recorded_at, recorded_at);
        assert!(!rendered.replay);
        assert_eq!(rendered.queue_bytes, 1);
        assert_eq!(rendered.delivery_id, RendererDeliveryId::new(7));
    }
}

/// Cold attach must place state before history and pass live boundary traffic.
#[test]
fn places_state_before_history_and_live_after_boundary() {
    let session_id: tau_proto::SessionId = "session-1".parse().expect("valid session id");
    let agent_id = "agent-1".parse().expect("valid agent id");
    let prompt = Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
        literal: false,
        session_id: session_id.clone(),
        text: "historical prompt".to_owned(),
        agent_id,
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    });
    let state = Event::ExtensionReady(tau_proto::ExtensionReady {
        extension_name: "test-extension".parse().expect("valid extension name"),
        instance_id: 1.into(),
        pid: Some(123),
    });
    let boundary = Event::SessionReplayComplete(tau_proto::SessionReplayComplete {
        session_id,
        error: None,
    });
    let live = Event::TermBell(tau_proto::TermBell {});
    let delivery = |event, replay, delivery_id: u64| RendererDelivery {
        abandoned_shell_starts: Vec::new(),
        event: Box::new(event),
        replay,
        recorded_at: UnixMicros::new(1),
        queue_bytes: 1,
        delivery_id: RendererDeliveryId::new(delivery_id),
        presentation: RendererPresentation::Ordinary,
    };
    let mut stager = ColdAttachStager::staging();

    assert!(stager.admit(delivery(prompt.clone(), true, 1)).is_empty());
    let ready = stager.admit(delivery(state.clone(), true, 2));
    assert!(matches!(ready.as_slice(), [value] if value.event.as_ref() == &state));
    let ready = stager.admit(delivery(boundary.clone(), false, 3));
    assert!(matches!(
        ready.as_slice(),
        [history, complete]
            if history.event.as_ref() == &prompt
                && history.delivery_id == RendererDeliveryId::new(1)
                && complete.event.as_ref() == &boundary
                && complete.delivery_id == RendererDeliveryId::new(3)
    ));
    let ready = stager.admit(delivery(live.clone(), false, 4));
    assert!(matches!(ready.as_slice(), [value] if value.event.as_ref() == &live));
}

/// Remote termination must release staged rows before disconnect admission.
#[test]
fn drains_history_before_disconnect() {
    let event = historical_prompt("history");
    let mut stager = ColdAttachStager::staging();
    assert!(
        stager
            .admit(RendererDelivery {
                abandoned_shell_starts: Vec::new(),
                event: Box::new(event.clone()),
                replay: true,
                recorded_at: UnixMicros::new(1),
                queue_bytes: 2,
                delivery_id: RendererDeliveryId::new(9),
                presentation: RendererPresentation::Ordinary,
            })
            .is_empty()
    );

    let drained = stager.finish_before_disconnect();
    assert!(
        matches!(drained.as_slice(), [value] if value.event.as_ref() == &event && value.delivery_id == RendererDeliveryId::new(9))
    );
}

/// Disabled staging must preserve exact delivery order and correlation.
#[test]
fn pass_through_preserves_protocol_order() {
    let first = historical_prompt("first");
    let second = historical_prompt("second");
    let mut stager = ColdAttachStager::pass_through();

    let first_ready = stager.admit(replay(first.clone(), 1, 1));
    let second_ready = stager.admit(replay(second.clone(), 1, 2));

    assert!(
        matches!(first_ready.as_slice(), [value] if value.event.as_ref() == &first && value.delivery_id == RendererDeliveryId::new(1))
    );
    assert!(
        matches!(second_ready.as_slice(), [value] if value.event.as_ref() == &second && value.delivery_id == RendererDeliveryId::new(2))
    );
}

/// Item overflow must flush retained history in relative order and stop
/// staging.
#[test]
fn item_overflow_flushes_and_stops_staging() {
    let mut stager = ColdAttachStager::staging();
    let state = Event::ExtensionReady(tau_proto::ExtensionReady {
        extension_name: "test-extension".parse().expect("valid extension name"),
        instance_id: 1.into(),
        pid: None,
    });
    assert!(
        stager
            .admit(replay(historical_prompt("held"), 1, 0))
            .is_empty()
    );
    let state_ready = stager.admit(replay(state.clone(), 1, 8_000));
    assert!(matches!(state_ready.as_slice(), [value] if value.event.as_ref() == &state));
    for index in 1..RENDERER_QUEUE_MAX_ITEMS {
        assert!(
            stager
                .admit(replay(historical_prompt("held"), 1, index as u64))
                .is_empty()
        );
    }

    let ready = stager.admit(replay(historical_prompt("overflow"), 1, 9_999));

    assert_eq!(ready.len(), RENDERER_QUEUE_MAX_ITEMS + 1);
    assert!(
        ready
            .iter()
            .take(RENDERER_QUEUE_MAX_ITEMS)
            .enumerate()
            .all(|(index, delivery)| delivery.delivery_id == RendererDeliveryId::new(index as u64))
    );
    assert_eq!(
        ready.last().expect("overflow row").delivery_id,
        RendererDeliveryId::new(9_999)
    );
    let subsequent = stager.admit(replay(historical_prompt("subsequent"), 1, 10_000));
    assert!(
        matches!(subsequent.as_slice(), [value] if value.delivery_id == RendererDeliveryId::new(10_000))
    );
}

/// Byte overflow must use the same bounded flush transition as item overflow.
#[test]
fn byte_overflow_flushes_and_stops_staging() {
    let mut stager = ColdAttachStager::staging();
    assert!(
        stager
            .admit(replay(historical_prompt("held"), 1, 1))
            .is_empty()
    );
    let state = Event::ExtensionReady(tau_proto::ExtensionReady {
        extension_name: "test-extension".parse().expect("valid extension name"),
        instance_id: 1.into(),
        pid: None,
    });
    let state_ready = stager.admit(replay(state.clone(), 1, 8_000));
    assert!(matches!(state_ready.as_slice(), [value] if value.event.as_ref() == &state));

    let ready = stager.admit(replay(
        historical_prompt("overflow"),
        RENDERER_QUEUE_MAX_BYTES,
        2,
    ));

    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].delivery_id, RendererDeliveryId::new(1));
    assert_eq!(ready[1].delivery_id, RendererDeliveryId::new(2));
    let subsequent = stager.admit(replay(historical_prompt("subsequent"), 1, 3));
    assert!(
        matches!(subsequent.as_slice(), [value] if value.delivery_id == RendererDeliveryId::new(3))
    );
}

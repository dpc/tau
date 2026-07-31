//! Focused cold-attach staging tests.

mod tool_reconstruction;

use tau_proto::{Event, UnixMicros};

use super::{
    ColdAttachStager, RENDERER_QUEUE_MAX_BYTES, RENDERER_QUEUE_MAX_ITEMS, RendererDelivery,
    RendererPresentation, RetainedUsage, ShellStartPresentation, ToolReconciliation,
    renderer_event_from_delivery, tool_terminal_id,
};

/// Builds one plain replayable transcript prompt.
fn historical_prompt(text: &str) -> Event {
    Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
        literal: false,
        session_id: "session-1".parse().expect("valid session id"),
        text: text.to_owned(),
        agent_id: "agent-1".parse().expect("valid agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })
}

/// Wraps a replay event with deterministic queue metadata.
fn replay(event: Event, queue_bytes: usize, delivery_id: u64) -> RendererDelivery {
    RendererDelivery {
        abandoned_shell_starts: Vec::new(),
        event,
        replay: true,
        recorded_at: UnixMicros::new(1),
        queue_bytes,
        delivery_id,
        presentation: RendererPresentation::Ordinary,
    }
}

/// Wraps a live event with deterministic queue metadata.
fn live(event: Event, delivery_id: u64) -> RendererDelivery {
    RendererDelivery {
        abandoned_shell_starts: Vec::new(),
        event,
        replay: false,
        recorded_at: UnixMicros::new(2),
        queue_bytes: 1,
        delivery_id,
        presentation: RendererPresentation::Ordinary,
    }
}

/// Builds the catch-up boundary used to publish the folded pending baseline.
fn replay_complete() -> Event {
    Event::SessionReplayComplete(tau_proto::SessionReplayComplete {
        session_id: "session-1".parse().expect("valid session id"),
        error: None,
    })
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
/// terminal releases the bounded correlation entry for a future lifecycle.
#[test]
fn deduplicates_live_shell_start_before_replay_snapshot() {
    let mut stager = ColdAttachStager::staging();
    assert_eq!(stager.admit(live(shell_start(), 1)).len(), 1);

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
    let historical = stager.admit(replay(finished.clone(), 1, 2));
    assert!(matches!(
        historical.as_slice(),
        [RendererDelivery {
            event: Event::ShellCommandFinished(old),
            ..
        }] if old.command_id.as_str() == "shell-1"
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
        assert!(renderer_event_from_delivery(delivery, 1, 7).is_none());
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
        let rendered =
            renderer_event_from_delivery(delivery, 1, 7).expect("live terminal output is retained");
        assert_eq!(rendered.event, event);
        assert_eq!(rendered.recorded_at, recorded_at);
        assert!(!rendered.replay);
        assert_eq!(rendered.queue_bytes, 1);
        assert_eq!(rendered.delivery_id, 7);
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
    let delivery = |event, replay, delivery_id| RendererDelivery {
        abandoned_shell_starts: Vec::new(),
        event,
        replay,
        recorded_at: UnixMicros::new(1),
        queue_bytes: 1,
        delivery_id,
        presentation: RendererPresentation::Ordinary,
    };
    let mut stager = ColdAttachStager::staging();

    assert!(stager.admit(delivery(prompt.clone(), true, 1)).is_empty());
    let ready = stager.admit(delivery(state.clone(), true, 2));
    assert!(matches!(ready.as_slice(), [value] if value.event == state));
    let ready = stager.admit(delivery(boundary.clone(), false, 3));
    assert!(matches!(
        ready.as_slice(),
        [history, complete]
            if history.event == prompt
                && history.delivery_id == 1
                && complete.event == boundary
                && complete.delivery_id == 3
    ));
    let ready = stager.admit(delivery(live.clone(), false, 4));
    assert!(matches!(ready.as_slice(), [value] if value.event == live));
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
                event: event.clone(),
                replay: true,
                recorded_at: UnixMicros::new(1),
                queue_bytes: 2,
                delivery_id: 9,
                presentation: RendererPresentation::Ordinary,
            })
            .is_empty()
    );

    let drained = stager.finish_before_disconnect();
    assert!(
        matches!(drained.as_slice(), [value] if value.event == event && value.delivery_id == 9)
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
        matches!(first_ready.as_slice(), [value] if value.event == first && value.delivery_id == 1)
    );
    assert!(
        matches!(second_ready.as_slice(), [value] if value.event == second && value.delivery_id == 2)
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
    assert!(matches!(state_ready.as_slice(), [value] if value.event == state));
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
            .all(|(index, delivery)| delivery.delivery_id == index as u64)
    );
    assert_eq!(ready.last().expect("overflow row").delivery_id, 9_999);
    let subsequent = stager.admit(replay(historical_prompt("subsequent"), 1, 10_000));
    assert!(matches!(subsequent.as_slice(), [value] if value.delivery_id == 10_000));
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
    assert!(matches!(state_ready.as_slice(), [value] if value.event == state));

    let ready = stager.admit(replay(
        historical_prompt("overflow"),
        RENDERER_QUEUE_MAX_BYTES,
        2,
    ));

    assert_eq!(ready.len(), 2);
    assert_eq!(ready[0].delivery_id, 1);
    assert_eq!(ready[1].delivery_id, 2);
    let subsequent = stager.admit(replay(historical_prompt("subsequent"), 1, 3));
    assert!(matches!(subsequent.as_slice(), [value] if value.delivery_id == 3));
}

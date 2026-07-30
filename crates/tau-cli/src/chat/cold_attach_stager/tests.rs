//! Focused cold-attach staging tests.

use tau_proto::{Event, UnixMicros};

use super::{
    ColdAttachStager, RENDERER_QUEUE_MAX_BYTES, RENDERER_QUEUE_MAX_ITEMS, RendererDelivery,
    renderer_event_from_delivery,
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
        event,
        replay: true,
        recorded_at: UnixMicros::new(1),
        queue_bytes,
        delivery_id,
    }
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
        event,
        replay,
        recorded_at: UnixMicros::new(1),
        queue_bytes: 1,
        delivery_id,
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
                event: event.clone(),
                replay: true,
                recorded_at: UnixMicros::new(1),
                queue_bytes: 2,
                delivery_id: 9,
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

/// A selected tool response must flush already-held history and disable
/// staging.
#[test]
fn tool_history_keeps_protocol_order() {
    let prompt = historical_prompt("before tool");
    let tool = Event::ProviderResponseFinished(tau_proto::ProviderResponseFinished {
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: "prompt-1".parse().expect("valid prompt id"),
        agent_id: "agent-1".parse().expect("valid agent id"),
        output_items: vec![tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
            call_id: "call-1".into(),
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
    });
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

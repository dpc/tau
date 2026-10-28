//! Manual compaction queue projection from the same typed facts used on attach.

use super::*;

/// Builds a UI-owned intent without manufacturing a tool invocation.
fn requested(target: &str) -> AgentManualCompactionRequested {
    AgentManualCompactionRequested {
        target_agent_id: agent_id(target),
        source: tau_proto::ManualCompactionSource::UiCompact {
            ui_compact: tau_proto::UiManualCompactionSource {
                eligible_automatic_transaction_id: None,
                target_role: "default".to_owned(),
            },
        },
        ..self_compaction_requested("cr-ui-private", "unused")
    }
}

/// Live delivery replaces one deduplicated queued marker with private
/// compactor progress and completion, without retained acceptance chatter.
#[test]
fn ui_compaction_queue_retires_on_start() {
    {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.switch_agent(agent_id("main"));
        let request = requested("main");
        let deliver = |renderer: &mut EventRenderer, event: Event| {
            renderer.handle(&event);
        };
        for _ in 0..2 {
            deliver(
                &mut renderer,
                Event::AgentManualCompactionRequested(request.clone()),
            );
        }
        sync(&handle);
        let queued = vt.screen_text(100).join("\n");
        assert_eq!(queued.matches("compact (queued)").count(), 1, "{queued}");
        assert!(!queued.contains("cr-ui-private"), "{queued}");
        assert!(!queued.contains("accepted"), "{queued}");
        renderer.toggle_verbose_mode();
        sync(&handle);
        assert!(vt.screen_contains(100, "compact (queued)"));
        let mut start = standalone_compaction_started("ct-ui", "ap-ui");
        start.trigger = tau_proto::StandaloneCompactionTrigger::ManualUi {
            request_id: request.request_id,
        };
        deliver(
            &mut renderer,
            Event::AgentStandaloneCompactionStarted(start),
        );
        deliver(
            &mut renderer,
            Event::AgentPromptStarted(standalone_compaction_prompt_started("ap-ui")),
        );
        sync(&handle);
        assert!(!vt.screen_contains(100, "(queued)"));
        deliver(
            &mut renderer,
            Event::AgentStandaloneCompactionFailed(AgentStandaloneCompactionFailed {
                agent_id: agent_id("main"),
                transaction_id: tau_proto::CompactionTransactionId::parse("ct-ui")
                    .expect("transaction id"),
                cut: tau_proto::AgentHead::Root,
                reason: tau_proto::StandaloneCompactionFailureReason::Cancelled,
                resume_through: None,
                context_retreat: None,
                output_length_continuation: None,
                incomplete_response: None,
            }),
        );
        sync(&handle);
        assert!(!vt.screen_contains(100, "(queued)"));
        assert!(!vt.screen_contains(100, "cr-ui-private"));
    }
}

/// Equal request IDs on different targets cannot clear each other's markers;
/// hidden cancellation and automatic satisfaction must fold before selection.
#[test]
fn hidden_compaction_queue_terminals_are_target_owned() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));
    for target in ["main", "worker"] {
        renderer.handle(&Event::AgentManualCompactionRequested(requested(target)));
    }
    renderer.handle(&Event::AgentManualCompactionRequestFailed(
        tau_proto::AgentManualCompactionRequestFailed {
            target_agent_id: agent_id("worker"),
            request_id: requested("worker").request_id,
            reason: tau_proto::ManualCompactionRequestFailureReason::Cancelled,
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "compact (queued)"));
    assert!(!vt.screen_contains(100, "cancelled"));
    renderer.switch_agent(agent_id("worker"));
    sync(&handle);
    assert!(!vt.screen_contains(100, "(queued)"));
    assert!(vt.screen_contains(100, "cancelled"));
    renderer.handle(&Event::AgentManualCompactionRequestSatisfied(
        tau_proto::AgentManualCompactionRequestSatisfied {
            target_agent_id: agent_id("main"),
            request_id: requested("main").request_id,
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-auto")
                .expect("transaction id"),
        },
    ));
    renderer.switch_agent(agent_id("main"));
    sync(&handle);
    assert!(!vt.screen_contains(100, "(queued)"));
    assert!(!vt.screen_contains(100, "cr-ui-private"));
}

/// Cold attach must publish only the final queue state at its replay boundary:
/// pending intent stays visible, while started, failed, and satisfied intent
/// does not.
#[test]
fn cold_attach_compaction_queue_reconstructs_at_replay_boundary() {
    let request = requested("main");
    let mut start = standalone_compaction_started("ct-attach", "ap-attach");
    start.trigger = tau_proto::StandaloneCompactionTrigger::ManualUi {
        request_id: request.request_id.clone(),
    };
    for terminal in [
        None,
        Some(Event::AgentStandaloneCompactionStarted(start)),
        Some(Event::AgentManualCompactionRequestFailed(
            tau_proto::AgentManualCompactionRequestFailed {
                request_id: request.request_id.clone(),
                target_agent_id: agent_id("main"),
                reason: tau_proto::ManualCompactionRequestFailureReason::ModelChanged,
            },
        )),
        Some(Event::AgentManualCompactionRequestSatisfied(
            tau_proto::AgentManualCompactionRequestSatisfied {
                request_id: request.request_id.clone(),
                target_agent_id: agent_id("main"),
                transaction_id: tau_proto::CompactionTransactionId::parse("ct-auto")
                    .expect("transaction id"),
            },
        )),
    ] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle_cold_attach_replay_socket_delivery(
            &Event::AgentManualCompactionRequested(request.clone()),
            tau_proto::UnixMicros::new(1),
            tau_cli_term::RendererDeliveryId::new(1),
        );
        if let Some(event) = &terminal {
            renderer.handle_cold_attach_replay_socket_delivery(
                event,
                tau_proto::UnixMicros::new(2),
                tau_cli_term::RendererDeliveryId::new(2),
            );
        }
        renderer.handle_attach_agent_selection_socket_delivery(
            &Event::SessionReplayComplete(tau_proto::SessionReplayComplete {
                main_agent_id: None,
                session_id: test_session_id("s1"),
                error: None,
            }),
            &agent_id("main"),
            tau_proto::UnixMicros::new(3),
            tau_cli_term::RendererDeliveryId::new(3),
        );
        sync(&handle);
        assert_eq!(
            vt.screen_contains(100, "compact (queued)"),
            terminal.is_none()
        );
        assert_eq!(
            vt.screen_contains(100, "model changed"),
            matches!(terminal, Some(Event::AgentManualCompactionRequestFailed(_))),
        );
        assert!(!vt.screen_contains(100, "cr-ui-private"));
        assert!(!vt.screen_contains(100, "accepted compaction request"));
    }
}

use super::*;
use crate::harness::tests::dispatch::{final_tool_result, setup_routed_test_tool_call, tool_error};

/// Collect terminal reports and projections for one call in commit order.
fn committed_terminal_events(
    harness: &Harness,
    call_id: &str,
) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        let event_call_id = match &entry.event {
            Event::ToolResultReported(result)
            | Event::ToolResult(result)
            | Event::ProviderToolResult(result) => Some(&result.call_id),
            Event::ToolErrorReported(error)
            | Event::ToolError(error)
            | Event::ProviderToolError(error) => Some(&error.call_id),
            Event::ToolCancelledReported(cancelled) | Event::ToolCancelled(cancelled) => {
                Some(&cancelled.call_id)
            }
            Event::ToolBackgroundResult(result) => Some(&result.call_id),
            Event::ToolBackgroundError(error) => Some(&error.call_id),
            _ => None,
        };
        if event_call_id.is_some_and(|id| id.as_str() == call_id) {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// Register one exact interceptor for the supplied terminal event names.
fn intercept_terminal_names(harness: &mut Harness, names: Vec<tau_proto::EventName>) {
    connect_test_tool(harness, "terminal-interceptor");
    harness
        .handle_extension_event(
            "terminal-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: names.into_iter().map(EventSelector::Exact).collect(),
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register terminal interceptor");
}

/// Resolve the currently parked terminal interception action.
fn reply(harness: &mut Harness, action: InterceptAction) {
    harness
        .handle_extension_event(
            "terminal-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply { action })),
        )
        .expect("resolve terminal interception");
}

/// A rewritten result report commits before validation. Canonical UI/provider
/// projections use the harness source and reject both rewrite and Drop actions.
#[test]
fn result_report_replacement_drives_protected_canonical_projections() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("result-replaced", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![
            tau_proto::EventName::TOOL_RESULT_REPORTED,
            tau_proto::EventName::TOOL_RESULT,
            tau_proto::EventName::PROVIDER_TOOL_RESULT,
        ],
    );

    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "result-replaced",
                "owned_tool",
                "original",
            ))),
        )
        .expect("park result report");
    assert!(harness.tool_agents.contains_key("result-replaced"));
    assert!(committed_terminal_events(&harness, "result-replaced").is_empty());

    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ToolResultReported(
            final_tool_result("result-replaced", "owned_tool", "replacement"),
        )))),
    );
    assert!(
        !harness.tool_agents.contains_key("result-replaced"),
        "cleanup runs only after the report commits and canonical publication begins"
    );
    assert!(matches!(
        harness
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::ToolResult(_))
    ));

    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ToolResult(final_tool_result(
            "result-replaced",
            "owned_tool",
            "forged canonical",
        ))))),
    );
    assert!(matches!(
        harness
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::ProviderToolResult(_))
    ));
    reply(&mut harness, InterceptAction::Drop);

    assert!(matches!(
        committed_terminal_events(&harness, "result-replaced").as_slice(),
        [
            (Some(report_source), Event::ToolResultReported(report)),
            (Some(result_source), Event::ToolResult(result)),
            (Some(provider_source), Event::ProviderToolResult(provider)),
        ] if report_source == "conn-owner"
            && result_source == HARNESS_CONNECTION_ID
            && provider_source == HARNESS_CONNECTION_ID
            && matches!(&report.result, CborValue::Text(text) if text == "replacement")
            && result == report
            && provider == report
    ));
}

/// Dropping a mutable result report prevents cleanup and every canonical
/// projection.
#[test]
fn dropped_result_report_has_no_downstream_effect() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("result-dropped", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_RESULT_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "result-dropped",
                "owned_tool",
                "drop",
            ))),
        )
        .expect("park result report");
    reply(&mut harness, InterceptAction::Drop);

    assert!(committed_terminal_events(&harness, "result-dropped").is_empty());
    assert!(harness.tool_agents.contains_key("result-dropped"));
    assert_eq!(
        harness
            .pending_tool_providers
            .get("result-dropped")
            .map(tau_proto::ConnectionId::as_str),
        Some("conn-owner")
    );
}

/// A rewritten error report drives canonical failure projections, whose Drop
/// and rewrite actions cannot change the accepted failure.
#[test]
fn error_report_replacement_drives_protected_canonical_projections() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("error-replaced", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![
            tau_proto::EventName::TOOL_ERROR_REPORTED,
            tau_proto::EventName::TOOL_ERROR,
            tau_proto::EventName::PROVIDER_TOOL_ERROR,
        ],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolErrorReported(tool_error(
                "error-replaced",
                "owned_tool",
                "original",
            ))),
        )
        .expect("park error report");
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ToolErrorReported(tool_error(
            "error-replaced",
            "owned_tool",
            "replacement",
        ))))),
    );
    reply(&mut harness, InterceptAction::Drop);
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ProviderToolError(tool_error(
            "error-replaced",
            "owned_tool",
            "forged provider error",
        ))))),
    );

    assert!(matches!(
        committed_terminal_events(&harness, "error-replaced").as_slice(),
        [
            (Some(report_source), Event::ToolErrorReported(report)),
            (Some(error_source), Event::ToolError(error)),
            (Some(provider_source), Event::ProviderToolError(provider)),
        ] if report_source == "conn-owner"
            && error_source == HARNESS_CONNECTION_ID
            && provider_source == HARNESS_CONNECTION_ID
            && report.message == "replacement"
            && error == report
            && provider == report
    ));
}

/// Dropping a mutable error report leaves the routed call live.
#[test]
fn dropped_error_report_has_no_downstream_effect() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("error-dropped", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_ERROR_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolErrorReported(tool_error(
                "error-dropped",
                "owned_tool",
                "drop",
            ))),
        )
        .expect("park error report");
    reply(&mut harness, InterceptAction::Drop);

    assert!(committed_terminal_events(&harness, "error-dropped").is_empty());
    assert!(harness.tool_agents.contains_key("error-dropped"));
}

/// A rewritten cancellation report drives one protected harness-sourced
/// foreground cancellation.
#[test]
fn cancellation_report_replacement_drives_protected_canonical_fact() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("cancel-replaced", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![
            tau_proto::EventName::TOOL_CANCELLED_REPORTED,
            tau_proto::EventName::TOOL_CANCELLED,
        ],
    );
    let cancellation = |tool_name: &str| {
        Event::ToolCancelledReported(tau_proto::ToolCancelled {
            call_id: "cancel-replaced".into(),
            tool_name: tau_proto::ToolName::new(tool_name),
            tool_type: tau_proto::ToolType::Function,
        })
    };
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(cancellation("forged_original")),
        )
        .expect("park cancellation report");
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(cancellation("forged_replacement")))),
    );
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ToolCancelled(
            tau_proto::ToolCancelled {
                call_id: "cancel-replaced".into(),
                tool_name: tau_proto::ToolName::new("forged_canonical"),
                tool_type: tau_proto::ToolType::Function,
            },
        )))),
    );

    assert!(matches!(
        committed_terminal_events(&harness, "cancel-replaced").as_slice(),
        [
            (Some(report_source), Event::ToolCancelledReported(report)),
            (Some(canonical_source), Event::ToolCancelled(canonical)),
        ] if report_source == "conn-owner"
            && canonical_source == HARNESS_CONNECTION_ID
            && report.tool_name.as_str() == "forged_replacement"
            && canonical.tool_name.as_str() == "owned_tool"
    ));
}

/// Dropping a mutable cancellation report leaves the routed call live.
#[test]
fn dropped_cancellation_report_has_no_downstream_effect() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("cancel-dropped", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_CANCELLED_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolCancelledReported(tau_proto::ToolCancelled {
                call_id: "cancel-dropped".into(),
                tool_name: tau_proto::ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
            })),
        )
        .expect("park cancellation report");
    reply(&mut harness, InterceptAction::Drop);

    assert!(committed_terminal_events(&harness, "cancel-dropped").is_empty());
    assert!(harness.tool_agents.contains_key("cancel-dropped"));
}

/// Direct canonical spoofing, a non-Tool/Core report, and an unknown call all
/// fail at their respective authority/validation boundaries.
#[test]
fn terminal_report_authority_and_route_validation_fail_closed() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("authority-call", "owned_tool");
    connect_ready_configured_extension(
        &mut harness,
        "provider-peer",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .handle_extension_event(
            "provider-peer",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "authority-call",
                "owned_tool",
                "wrong kind",
            ))),
        )
        .expect("reject wrong-kind report");
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResult(final_tool_result(
                "authority-call",
                "owned_tool",
                "forged canonical",
            ))),
        )
        .expect("reject direct canonical result");
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "unknown-call",
                "owned_tool",
                "unknown",
            ))),
        )
        .expect("commit unknown report");

    assert!(committed_terminal_events(&harness, "authority-call").is_empty());
    assert!(matches!(
        committed_terminal_events(&harness, "unknown-call").as_slice(),
        [(Some(source), Event::ToolResultReported(_))] if source == "conn-owner"
    ));
    assert!(harness.tool_agents.contains_key("authority-call"));
}

/// A parked stale configured generation may commit its report but cannot close
/// the current generation's route.
#[test]
fn stale_parked_generation_cannot_publish_terminal_canonical_fact() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("stale-result", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_RESULT_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "stale-result",
                "owned_tool",
                "stale",
            ))),
        )
        .expect("park stale result report");
    harness
        .extensions
        .entries
        .get_mut("conn-owner")
        .expect("owner generation")
        .instance_id = tau_proto::ExtensionInstanceId::new(43);
    reply(&mut harness, InterceptAction::Pass(None));

    assert!(matches!(
        committed_terminal_events(&harness, "stale-result").as_slice(),
        [(Some(source), Event::ToolResultReported(_))] if source == "conn-owner"
    ));
    assert!(harness.tool_agents.contains_key("stale-result"));
}

/// Disconnecting the captured source while its report is parked prevents
/// terminal state mutation even if interception later passes the report.
#[test]
fn disconnected_parked_source_cannot_publish_terminal_canonical_fact() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("disconnected-result", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_RESULT_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "disconnected-result",
                "owned_tool",
                "stale",
            ))),
        )
        .expect("park disconnected result report");
    harness
        .extensions
        .entries
        .get_mut("conn-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Disconnected;
    reply(&mut harness, InterceptAction::Pass(None));

    assert!(matches!(
        committed_terminal_events(&harness, "disconnected-result").as_slice(),
        [(Some(source), Event::ToolResultReported(_))] if source == "conn-owner"
    ));
    assert!(harness.tool_agents.contains_key("disconnected-result"));
}

/// Pre-Ready terminal reports remain ordinary retained operational messages
/// with exact encoded-byte charging before activation drains them.
#[test]
fn pre_ready_terminal_report_preserves_retained_byte_accounting() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("retained-result", "owned_tool");
    harness
        .extensions
        .entries
        .get_mut("conn-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Handshaking;
    let report = Event::ToolResultReported(final_tool_result(
        "retained-result",
        "owned_tool",
        "retained",
    ));
    let expected_bytes = Harness::encoded_emit_size(&report, false);
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(report),
                persist: false,
            })),
        )
        .expect("retain pre-Ready report");
    let stage = &harness.extensions.activation_staging["conn-owner"];
    assert_eq!(stage.retained_message_count, 1);
    assert_eq!(stage.retained_message_bytes, expected_bytes);
    assert!(committed_terminal_events(&harness, "retained-result").is_empty());

    harness
        .handle_extension_message("conn-owner", TestMessage::Ready(Default::default()))
        .expect("activate and drain report");
    assert!(matches!(
        committed_terminal_events(&harness, "retained-result").as_slice(),
        [
            (_, Event::ToolResultReported(_)),
            (Some(result_source), Event::ToolResult(_)),
            (Some(provider_source), Event::ProviderToolResult(_)),
        ] if result_source == HARNESS_CONNECTION_ID
            && provider_source == HARNESS_CONNECTION_ID
    ));
}

/// Result, error, and cancellation reports for backgrounded calls preserve the
/// existing real-background completion projections and never close provider
/// transcript state a second time.
#[test]
fn backgrounded_terminal_reports_preserve_background_completion_behavior() {
    for (suffix, report, expected_error) in [
        (
            "result",
            Event::ToolResultReported(final_tool_result("background-result", "owned_tool", "done")),
            None,
        ),
        (
            "error",
            Event::ToolErrorReported(tool_error("background-error", "owned_tool", "failed")),
            Some("failed"),
        ),
        (
            "cancel",
            Event::ToolCancelledReported(tau_proto::ToolCancelled {
                call_id: "background-cancel".into(),
                tool_name: tau_proto::ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
            }),
            Some("Tool cancelled"),
        ),
    ] {
        let call_id = format!("background-{suffix}");
        let (_tmp, mut harness) = setup_routed_test_tool_call(&call_id, "owned_tool");
        assert!(harness.tool_turn.mark_backgrounded(&call_id.clone().into()));
        harness
            .handle_extension_event("conn-owner", TestProtocolItem::Event(report))
            .expect("commit background terminal report");
        let events = committed_terminal_events(&harness, &call_id);
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| matches!(
                    event,
                    Event::ToolResult(_)
                        | Event::ToolError(_)
                        | Event::ToolCancelled(_)
                        | Event::ProviderToolResult(_)
                        | Event::ProviderToolError(_)
                ))
                .count(),
            0,
            "background completion must not emit a second foreground terminal"
        );
        match expected_error {
            None => assert!(events.iter().any(|(source, event)| {
                source.as_deref() == Some(HARNESS_CONNECTION_ID)
                    && matches!(
                        event,
                        Event::ToolBackgroundResult(result)
                            if matches!(&result.result, CborValue::Text(text) if text == "done")
                    )
            })),
            Some(message) => assert!(events.iter().any(|(source, event)| {
                source.as_deref() == Some(HARNESS_CONNECTION_ID)
                    && matches!(
                        event,
                        Event::ToolBackgroundError(error) if error.message == message
                    )
            })),
        }
        assert!(!harness.tool_agents.contains_key(call_id.as_str()));
    }
}

/// A duplicate report remains an observable peer report, but completed-call
/// tracking prevents a second canonical result/provider projection.
#[test]
fn duplicate_result_report_cannot_repeat_terminal_cleanup_or_projection() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("duplicate-result", "owned_tool");
    let report =
        Event::ToolResultReported(final_tool_result("duplicate-result", "owned_tool", "done"));
    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report.clone()))
        .expect("first report");
    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report))
        .expect("duplicate report");

    let events = committed_terminal_events(&harness, "duplicate-result");
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(event, Event::ToolResultReported(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(
                event,
                Event::ToolResult(_) | Event::ProviderToolResult(_)
            ))
            .count(),
        2,
        "one UI result plus one provider result"
    );
}

//! Contract tests for `SPEC-ui-prompt-draft-and-focus-events`.

use super::*;
use crate::event_log as path_crate_event_log;

/// Build one contentful prompt-draft liveness observation.
fn draft(text: &str) -> Event {
    Event::UiPromptDraft(tau_proto::UiPromptDraft {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        target_agent_id: None,
        text: Some(text.to_owned()),
    })
}

/// Build one terminal-focus liveness observation.
fn focus(focused: bool) -> Event {
    Event::UiFocusChanged(tau_proto::UiFocusChanged {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        focused,
    })
}

/// Return whether one source committed a matching event.
fn source_committed(h: &Harness, source: &str, predicate: impl Fn(&Event) -> bool) -> bool {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source) && predicate(&entry.event) {
            return true;
        }
    }
    false
}

/// Subscribe one peer to live UI liveness observations.
fn connect_liveness_observer(h: &mut Harness, id: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(h, id, tau_proto::ClientKind::Tool);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id(id),
            Vec::new(),
            vec![
                EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
                EventSelector::Exact(tau_proto::EventName::UI_FOCUS_CHANGED),
            ],
        )
        .expect("subscribe to UI liveness events");
    sink
}

/// An attached socket UI may publish draft and focus observations, which reach
/// subscribers as live deliveries with the UI's run-local source.
#[test]
fn attached_socket_ui_can_publish_liveness_events() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_test_client_with_origin(
        &mut h,
        "ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let observer = connect_liveness_observer(&mut h, "observer");

    for event in [draft("typing"), focus(true)] {
        h.handle_client_event_inner_with_persist(
            &crate::test_connection_id("ui"),
            event,
            Some(true),
        )
        .expect("publish UI liveness event");
    }

    let routed = observer.lock().expect("observer");
    let mut delivered = routed.iter().filter_map(|routed| match &routed.frame {
        HarnessOutputMessage::Deliver(delivery)
            if matches!(
                delivery.event.as_ref(),
                Event::UiPromptDraft(_) | Event::UiFocusChanged(_)
            ) =>
        {
            Some((routed.source_id.as_deref(), delivery))
        }
        _ => None,
    });
    for expected_name in [
        tau_proto::EventName::UI_PROMPT_DRAFT,
        tau_proto::EventName::UI_FOCUS_CHANGED,
    ] {
        let (source, delivery) = delivered.next().expect("liveness delivery");
        assert_eq!(source, Some("ui"));
        assert!(!delivery.replay);
        assert_eq!(delivery.event.name(), expected_name);
    }
    assert!(delivered.next().is_none());
}

/// Dedicated external-message, non-UI, missing, and disconnected socket peers
/// cannot publish UI liveness events.
#[test]
fn other_client_sources_cannot_publish_liveness_events() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");

    connect_test_client_with_origin(
        &mut h,
        "external",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    h.peer_messaging
        .external_message_peers
        .insert(crate::test_connection_id("external"));
    h.handle_client_event_inner(&crate::test_connection_id("external"), draft("denied"))
        .expect("reject external-message peer");
    assert!(!source_committed(&h, "external", |_| true));

    connect_test_client_with_origin(
        &mut h,
        "socket-tool",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    h.handle_client_event_inner(&crate::test_connection_id("socket-tool"), focus(true))
        .expect("reject non-UI socket");
    assert!(!source_committed(&h, "socket-tool", |_| true));

    h.handle_client_event_inner(&crate::test_connection_id("missing"), draft("missing"))
        .expect("reject missing client");
    assert!(!source_committed(&h, "missing", |_| true));

    connect_test_client_with_origin(
        &mut h,
        "disconnected",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    h.runtime_io
        .bus
        .disconnect(&crate::test_connection_id("disconnected"));
    h.handle_client_event_inner(&crate::test_connection_id("disconnected"), focus(false))
        .expect("reject disconnected client");
    assert!(!source_committed(&h, "disconnected", |_| true));
}

/// Configured and unconfigured extension-path peers have no authority for
/// attached-UI liveness event names, including configured UI and Core kinds.
#[test]
fn extensions_cannot_publish_ui_liveness_events() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let kinds = [
        tau_proto::ClientKind::Provider,
        tau_proto::ClientKind::Tool,
        tau_proto::ClientKind::Action,
        tau_proto::ClientKind::Ui,
        tau_proto::ClientKind::Core,
        tau_proto::ClientKind::External,
    ];

    for (index, kind) in kinds.into_iter().enumerate() {
        let source = format!("configured-{index}");
        connect_ready_configured_extension(&mut h, &source, &source, kind);
        h.handle_extension_event_inner(&crate::test_connection_id(&source), draft(&source))
            .expect("reject configured extension draft");
        h.handle_extension_event_inner(&crate::test_connection_id(&source), focus(true))
            .expect("reject configured extension focus");
        assert!(!source_committed(&h, &source, |_| true));
    }

    connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner(&crate::test_connection_id("unconfigured"), draft("denied"))
        .expect("reject unconfigured extension");
    assert!(!source_committed(&h, "unconfigured", |_| true));
}

/// The real CLI-style durable-bit request remains interceptor-visible; drop
/// prevents draft publication and same-name replacement publishes only focus.
#[test]
fn interception_preserves_false_metadata_and_controls_publication() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_test_client_with_origin(
        &mut h,
        "ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let observer = connect_liveness_observer(&mut h, "observer");
    let interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
                EventSelector::Exact(tau_proto::EventName::UI_FOCUS_CHANGED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_client_message(
        &crate::test_connection_id("ui"),
        HarnessInputMessage::Emit(tau_proto::Emit::new(draft("drop"))),
    )
    .expect("park draft");
    assert!(
        interceptor
            .lock()
            .expect("interceptor")
            .iter()
            .any(|routed| matches!(
                &routed.frame,
                HarnessOutputMessage::InterceptRequest(request)
                    if request.persist
                        && matches!(request.event.as_ref(), Event::UiPromptDraft(_))
            ))
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop draft");

    h.handle_client_message(
        &crate::test_connection_id("ui"),
        HarnessInputMessage::Emit(tau_proto::Emit::new(focus(false))),
    )
    .expect("park focus");
    assert!(
        interceptor
            .lock()
            .expect("interceptor")
            .iter()
            .rev()
            .any(|routed| matches!(
                &routed.frame,
                HarnessOutputMessage::InterceptRequest(request)
                    if request.persist
                        && matches!(request.event.as_ref(), Event::UiFocusChanged(_))
            ))
    );
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(focus(true)))),
        })),
    )
    .expect("replace focus");

    assert!(!source_committed(&h, "ui", |event| {
        matches!(event, Event::UiPromptDraft(_))
    }));
    assert!(source_committed(&h, "ui", |event| {
        matches!(event, Event::UiFocusChanged(change) if change.focused)
    }));
    let routed = observer.lock().expect("observer");
    let delivered: Vec<_> = routed
        .iter()
        .filter_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery)
                if matches!(
                    delivery.event.as_ref(),
                    Event::UiPromptDraft(_) | Event::UiFocusChanged(_)
                ) =>
            {
                Some((routed.source_id.as_deref(), delivery))
            }
            _ => None,
        })
        .collect();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, Some("ui"));
    assert!(!delivered[0].1.replay);
    assert_eq!(delivered[0].1.event.as_ref(), &focus(true));
}

/// Both caller persistence values deliver live, while neither event family has
/// semantic historical catch-up.
#[test]
fn liveness_events_are_no_store_for_both_persistence_values() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_test_client_with_origin(
        &mut h,
        "ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let live = connect_liveness_observer(&mut h, "live");

    h.handle_client_event_inner_with_persist(
        &crate::test_connection_id("ui"),
        draft("false"),
        Some(true),
    )
    .expect("publish persist=true draft");
    h.handle_client_event_inner_with_persist(
        &crate::test_connection_id("ui"),
        focus(true),
        Some(false),
    )
    .expect("publish persist=false focus");
    assert_eq!(
        live.lock()
            .expect("live observer")
            .iter()
            .filter(|routed| matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(
                            delivery.event.as_ref(),
                            Event::UiPromptDraft(_) | Event::UiFocusChanged(_)
                        )
            ))
            .count(),
        2
    );

    let historical = connect_test_client(&mut h, "historical", tau_proto::ClientKind::Ui);
    h.complete_subscription(
        &crate::test_connection_id("historical"),
        vec![
            EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT),
            EventSelector::Exact(tau_proto::EventName::UI_FOCUS_CHANGED),
        ],
        Vec::new(),
    )
    .expect("request liveness history");
    assert!(
        historical
            .lock()
            .expect("historical")
            .iter()
            .all(|routed| !matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if matches!(
                        delivery.event.as_ref(),
                        Event::UiPromptDraft(_) | Event::UiFocusChanged(_)
                    )
            ))
    );
}

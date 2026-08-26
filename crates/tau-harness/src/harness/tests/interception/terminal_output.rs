//! Contract tests for `SPEC-terminal-output-side-effect-events`.

use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Build one terminal bell side-effect event.
fn bell() -> Event {
    Event::TermBell(tau_proto::TermBell {})
}

/// Build one observable terminal user-variable side-effect event.
fn user_var(value: &str) -> Event {
    Event::Osc1337SetUserVar(tau_proto::Osc1337SetUserVar {
        name: "tau-test".to_owned(),
        value: value.to_owned(),
    })
}

/// Return whether one source committed an event matching the predicate.
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

/// Connect one socket-origin UI peer for direct client-intake testing.
fn connect_socket_ui(h: &mut Harness, id: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.runtime_io.bus.connect(Connection::new(
        PendingConnectionMetadata {
            id: Some(crate::test_connection_id(id)),
            name: crate::test_extension_name(id),
            kind: tau_proto::ClientKind::Ui,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

/// Subscribe one observer to live terminal-output delivery.
fn connect_terminal_observer(h: &mut Harness, id: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(h, id, tau_proto::ClientKind::Ui);
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id(id),
            Vec::new(),
            vec![
                EventSelector::Exact(tau_proto::EventName::TERM_BELL),
                EventSelector::Exact(tau_proto::EventName::TERM_OSC1337_SET_USER_VAR),
            ],
        )
        .expect("subscribe to terminal output");
    sink
}

/// Every configured extension kind, including configured Core, retains
/// terminal-output authorship without a capability.
#[test]
fn every_configured_extension_kind_can_publish_terminal_output() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let observer = connect_terminal_observer(&mut h, "observer");
    let kinds = [
        tau_proto::ClientKind::Provider,
        tau_proto::ClientKind::Tool,
        tau_proto::ClientKind::Action,
        tau_proto::ClientKind::Ui,
        tau_proto::ClientKind::Core,
        tau_proto::ClientKind::External,
    ];

    for (index, kind) in kinds.into_iter().enumerate() {
        let source = format!("terminal-source-{index}");
        let stable_name = format!("stable-terminal-source-{index}");
        connect_ready_configured_extension(&mut h, &source, &stable_name, kind);
        h.handle_extension_event_inner(&crate::test_connection_id(&source), bell())
            .expect("publish bell");
        h.handle_extension_event_inner(&crate::test_connection_id(&source), user_var(&source))
            .expect("publish user variable");
        assert!(source_committed(&h, &source, |event| {
            matches!(event, Event::TermBell(_))
        }));
        assert!(source_committed(&h, &source, |event| {
            matches!(
                event,
                Event::Osc1337SetUserVar(value) if value.value == source
            )
        }));
        let routed = observer.lock().expect("observer");
        assert!(routed.iter().any(|routed| {
            routed.source_id.as_deref() == Some(source.as_str())
                && matches!(
                    &routed.frame,
                    HarnessOutputMessage::Deliver(delivery)
                        if !delivery.replay && matches!(delivery.event.as_ref(), Event::TermBell(_))
                )
        }));
        assert!(routed.iter().any(|routed| {
            routed.source_id.as_deref() == Some(source.as_str())
                && matches!(
                    &routed.frame,
                    HarnessOutputMessage::Deliver(delivery)
                        if !delivery.replay
                            && matches!(
                                delivery.event.as_ref(),
                                Event::Osc1337SetUserVar(value) if value.value == source
                            )
                )
        }));
    }
}

/// Unconfigured and disconnected extension peers cannot commit terminal-output
/// events through the legacy extension fallback.
#[test]
fn unconfigured_and_disconnected_extensions_cannot_publish_terminal_output() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");

    connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner(&crate::test_connection_id("unconfigured"), bell())
        .expect("reject unconfigured bell");
    assert!(!source_committed(&h, "unconfigured", |event| {
        matches!(event, Event::TermBell(_))
    }));

    connect_ready_configured_extension(
        &mut h,
        "disconnected",
        "disconnected",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("disconnected")
        .expect("configured entry")
        .state = path_crate_extension::ExtensionState::Disconnected;
    h.handle_extension_event_inner(
        &crate::test_connection_id("disconnected"),
        user_var("stale"),
    )
    .expect("reject disconnected user variable");
    assert!(!source_committed(&h, "disconnected", |event| {
        matches!(event, Event::Osc1337SetUserVar(_))
    }));
}

/// A harness-attached socket UI may publish live terminal output, while a
/// dedicated external-message socket peer may not.
#[test]
fn attached_ui_has_authority_but_external_message_peer_does_not() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");

    connect_socket_ui(&mut h, "ui");
    h.handle_client_event_inner_with_persist(&crate::test_connection_id("ui"), bell(), Some(true))
        .expect("publish UI bell");
    assert!(source_committed(&h, "ui", |event| {
        matches!(event, Event::TermBell(_))
    }));

    connect_socket_ui(&mut h, "external");
    h.peer_messaging
        .external_message_peers
        .insert(crate::test_connection_id("external"));
    h.handle_client_event_inner_with_persist(
        &crate::test_connection_id("external"),
        user_var("denied"),
        Some(true),
    )
    .expect("reject external user variable");
    assert!(!source_committed(&h, "external", |event| {
        matches!(event, Event::Osc1337SetUserVar(_))
    }));
}

/// Ordinary interception may drop a terminal side-effect event before it
/// commits and reaches subscribing UIs.
#[test]
fn interception_drop_prevents_terminal_output_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "publisher",
        "publisher",
        tau_proto::ClientKind::Action,
    );
    let observer = connect_terminal_observer(&mut h, "observer");
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TERM_BELL)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner(&crate::test_connection_id("publisher"), bell())
        .expect("park bell");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop bell");

    assert!(!source_committed(&h, "publisher", |event| {
        matches!(event, Event::TermBell(_))
    }));
    assert!(observer.lock().expect("observer").iter().all(|routed| {
        routed.source_id.as_deref() != Some("publisher")
            || !matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if matches!(delivery.event.as_ref(), Event::TermBell(_))
            )
    }));
}

/// Same-name interceptor replacement retains the authenticated source and
/// commits only the replacement payload.
#[test]
fn interception_replacement_retains_source_and_replaces_payload() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "publisher",
        "stable-publisher",
        tau_proto::ClientKind::Core,
    );
    let observer = connect_terminal_observer(&mut h, "observer");
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TERM_OSC1337_SET_USER_VAR,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("publisher"),
        user_var("original"),
        Some(false),
    )
    .expect("park user variable");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(user_var("replacement")))),
        })),
    )
    .expect("replace user variable");

    assert!(source_committed(&h, "publisher", |event| {
        matches!(
            event,
            Event::Osc1337SetUserVar(value) if value.value == "replacement"
        )
    }));
    assert!(!source_committed(&h, "publisher", |event| {
        matches!(
            event,
            Event::Osc1337SetUserVar(value) if value.value == "original"
        )
    }));
    let deliveries: Vec<_> = observer
        .lock()
        .expect("observer")
        .iter()
        .filter_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery)
                if matches!(delivery.event.as_ref(), Event::Osc1337SetUserVar(_)) =>
            {
                Some((
                    routed.source_id.clone(),
                    delivery.replay,
                    delivery.event.as_ref().clone(),
                ))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        deliveries,
        [(
            Some(
                tau_proto::ConnectionId::parse("publisher")
                    .expect("test connection id must satisfy the identifier grammar")
            ),
            false,
            user_var("replacement"),
        )]
    );
}

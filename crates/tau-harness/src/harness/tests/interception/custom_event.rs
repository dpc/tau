//! Contract tests for `SPEC-custom-extension-events`.

use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Build one custom event with an extension-owned nested name.
fn custom(name: &str, payload: &str) -> Event {
    Event::ExtensionEvent(
        tau_proto::CustomEvent::try_new(
            name.parse().expect("custom event name"),
            Some(
                "s1".parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
            ),
            CborValue::Text(payload.to_owned()),
        )
        .expect("extension-owned custom event"),
    )
}

/// Return whether one source committed a matching custom event.
fn source_committed(
    h: &Harness,
    source: &str,
    predicate: impl Fn(&tau_proto::CustomEvent) -> bool,
) -> bool {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source)
            && let Event::ExtensionEvent(event) = &entry.event
            && predicate(event)
        {
            return true;
        }
    }
    false
}

/// Subscribe one observer to exact and prefix custom-event names.
fn connect_custom_observer(
    h: &mut Harness,
    id: &str,
    selectors: Vec<EventSelector>,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(h, id, tau_proto::ClientKind::Tool);
    h.bus
        .set_subscriptions(&crate::test_connection_id(id), Vec::new(), selectors)
        .expect("subscribe to custom events");
    sink
}

/// Every configured extension kind may publish extension-owned events without
/// a capability, and subscribers retain the run-local source.
#[test]
fn every_configured_extension_kind_can_publish_custom_events() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let observer = connect_custom_observer(
        &mut h,
        "observer",
        vec![EventSelector::Prefix("demo.".to_owned())],
    );
    let kinds = [
        tau_proto::ClientKind::Provider,
        tau_proto::ClientKind::Tool,
        tau_proto::ClientKind::Action,
        tau_proto::ClientKind::Ui,
        tau_proto::ClientKind::Core,
        tau_proto::ClientKind::External,
    ];

    for (index, kind) in kinds.into_iter().enumerate() {
        let source = format!("custom-source-{index}");
        let stable_name = format!("stable-custom-source-{index}");
        let name = format!("demo.event_{index}");
        connect_ready_configured_extension(&mut h, &source, &stable_name, kind);
        h.handle_extension_event_inner(&crate::test_connection_id(&source), custom(&name, &source))
            .expect("publish configured custom event");
        assert!(source_committed(&h, &source, |event| {
            event.name().to_string() == name && event.payload() == &CborValue::Text(source.clone())
        }));
        assert!(observer.lock().expect("observer").iter().any(|routed| {
            routed.source_id.as_deref() == Some(source.as_str())
                && matches!(
                    &routed.frame,
                    HarnessOutputMessage::Deliver(delivery)
                        if !delivery.replay
                            && matches!(
                                delivery.event.as_ref(),
                                Event::ExtensionEvent(event)
                                    if event.name().to_string() == name
                            )
                )
        }));
    }
}

/// Unconfigured and disconnected extension peers cannot reach custom-event
/// publication through the legacy generic fallback.
#[test]
fn unconfigured_and_disconnected_extensions_cannot_publish_custom_events() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");

    connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner(
        &crate::test_connection_id("unconfigured"),
        custom("demo.unconfigured", &crate::test_connection_id("denied")),
    )
    .expect("reject unconfigured event");
    assert!(!source_committed(&h, "unconfigured", |_| true));

    connect_ready_configured_extension(
        &mut h,
        "disconnected",
        "stable-disconnected",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("disconnected")
        .expect("configured entry")
        .state = path_crate_extension::ExtensionState::Disconnected;
    h.handle_extension_event_inner(
        &crate::test_connection_id("disconnected"),
        custom("demo.disconnected", &crate::test_connection_id("denied")),
    )
    .expect("reject disconnected event");
    assert!(!source_committed(&h, "disconnected", |_| true));
}

/// Only an attached socket peer with harness-assigned UI identity may publish
/// custom events through client intake.
#[test]
fn attached_ui_can_publish_but_other_socket_peers_cannot() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");

    connect_test_client_with_origin(
        &mut h,
        "ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    h.handle_client_event_inner_with_persist(
        &crate::test_connection_id("ui"),
        custom("demo.ui", &crate::test_connection_id("accepted")),
        Some(true),
    )
    .expect("publish UI custom event");
    assert!(source_committed(&h, "ui", |event| {
        event.name().to_string() == "demo.ui"
    }));

    connect_test_client_with_origin(
        &mut h,
        "external",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    h.external_message_peers
        .insert(crate::test_connection_id("external"));
    h.handle_client_event_inner(
        &crate::test_connection_id("external"),
        custom("demo.external", &crate::test_connection_id("denied")),
    )
    .expect("reject external-message peer");
    assert!(!source_committed(&h, "external", |_| true));

    connect_test_client_with_origin(
        &mut h,
        "socket-tool",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    h.handle_client_event_inner(
        &crate::test_connection_id("socket-tool"),
        custom("demo.socket_tool", &crate::test_connection_id("denied")),
    )
    .expect("reject non-UI socket");
    assert!(!source_committed(&h, "socket-tool", |_| true));
}

/// Interception drop prevents both custom-event commit and ordinary subscriber
/// delivery.
#[test]
fn interception_drop_prevents_custom_event_publication() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "publisher",
        "stable-publisher",
        tau_proto::ClientKind::Action,
    );
    let observer = connect_custom_observer(
        &mut h,
        "observer",
        vec![EventSelector::Exact(
            "demo.drop".parse().expect("event name"),
        )],
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                "demo.drop".parse().expect("event name"),
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner(
        &crate::test_connection_id("publisher"),
        custom("demo.drop", &crate::test_connection_id("original")),
    )
    .expect("park custom event");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop custom event");

    assert!(!source_committed(&h, "publisher", |_| true));
    assert!(observer.lock().expect("observer").iter().all(|routed| {
        !matches!(
            &routed.frame,
            HarnessOutputMessage::Deliver(delivery)
                if matches!(delivery.event.as_ref(), Event::ExtensionEvent(_))
        )
    }));
}

/// Same-name replacement may alter the opaque payload while retaining source
/// and publishing only the replacement to exact subscribers.
#[test]
fn same_name_replacement_changes_payload_and_retains_source() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "publisher",
        "stable-publisher",
        tau_proto::ClientKind::Core,
    );
    let event_name: tau_proto::EventName = "demo.replace".parse().expect("event name");
    let observer = connect_custom_observer(
        &mut h,
        "observer",
        vec![EventSelector::Exact(event_name.clone())],
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(event_name)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("publisher"),
        custom("demo.replace", "original"),
        Some(false),
    )
    .expect("park custom event");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(custom("demo.replace", "replacement")))),
        })),
    )
    .expect("replace custom event");

    let deliveries: Vec<_> = observer
        .lock()
        .expect("observer")
        .iter()
        .filter_map(|routed| match &routed.frame {
            HarnessOutputMessage::Deliver(delivery) => {
                let Event::ExtensionEvent(event) = delivery.event.as_ref() else {
                    return None;
                };
                Some((
                    routed.source_id.clone(),
                    delivery.replay,
                    event.payload().clone(),
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
            CborValue::Text("replacement".to_owned()),
        )]
    );
    assert!(source_committed(&h, "publisher", |event| {
        event.payload() == &CborValue::Text("replacement".to_owned())
    }));
}

/// A replacement with another nested name is invalid, so the original custom
/// event remains the committed and delivered observation.
#[test]
fn different_name_replacement_preserves_original_event() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "publisher",
        "stable-publisher",
        tau_proto::ClientKind::Tool,
    );
    let observer = connect_custom_observer(
        &mut h,
        "observer",
        vec![EventSelector::Exact(
            "demo.original".parse().expect("event name"),
        )],
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                "demo.original".parse().expect("event name"),
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner(
        &crate::test_connection_id("publisher"),
        custom("demo.original", &crate::test_connection_id("original")),
    )
    .expect("park original");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(custom("demo.other", "replacement")))),
        })),
    )
    .expect("reject renamed replacement");

    assert!(source_committed(&h, "publisher", |event| {
        event.name().to_string() == "demo.original"
            && event.payload() == &CborValue::Text("original".to_owned())
    }));
    assert!(!source_committed(&h, "publisher", |event| {
        event.name().to_string() == "demo.other"
    }));
    assert!(observer.lock().expect("observer").iter().any(|routed| {
        routed.source_id.as_deref() == Some("publisher")
            && matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(
                            delivery.event.as_ref(),
                            Event::ExtensionEvent(event)
                                if event.name().to_string() == "demo.original"
                                    && event.payload()
                                        == &CborValue::Text("original".to_owned())
                        )
            )
    }));
}

/// Both caller persistence values commit live but custom events never appear in
/// semantic historical catch-up.
#[test]
fn custom_events_are_live_only_for_both_persistence_values() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "publisher",
        "stable-publisher",
        tau_proto::ClientKind::Provider,
    );
    let live = connect_custom_observer(
        &mut h,
        "live",
        vec![EventSelector::Prefix("demo.".to_owned())],
    );
    let interceptor = connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                "demo.persistence".parse().expect("event name"),
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    for persist in [false, true] {
        h.handle_extension_event_inner_with_persist(
            &crate::test_connection_id("publisher"),
            custom("demo.persistence", &persist.to_string()),
            Some(persist),
        )
        .expect("publish custom event");
        assert!(
            interceptor
                .lock()
                .expect("interceptor")
                .iter()
                .rev()
                .any(|routed| {
                    matches!(
                        &routed.frame,
                        HarnessOutputMessage::InterceptRequest(request)
                            if request.persist == persist
                                && matches!(
                                    request.event.as_ref(),
                                    Event::ExtensionEvent(event)
                                        if event.name().to_string() == "demo.persistence"
                                )
                    )
                })
        );
        h.handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("pass custom event");
    }
    assert_eq!(
        live.lock()
            .expect("live observer")
            .iter()
            .filter(|routed| matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(delivery.event.as_ref(), Event::ExtensionEvent(_))
            ))
            .count(),
        2
    );

    let historical = connect_test_client(&mut h, "historical", tau_proto::ClientKind::Ui);
    h.complete_subscription(
        &crate::test_connection_id("historical"),
        vec![EventSelector::Prefix("demo.".to_owned())],
        Vec::new(),
    )
    .expect("request custom-event history");
    assert!(
        historical
            .lock()
            .expect("historical observer")
            .iter()
            .all(|routed| {
                !matches!(
                    &routed.frame,
                    HarnessOutputMessage::Deliver(delivery)
                        if matches!(delivery.event.as_ref(), Event::ExtensionEvent(_))
                )
            })
    );
}

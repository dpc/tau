//! Contract tests for `SPEC-extension-notice-requests`.

use super::*;

fn notice_request(message: &str, level: tau_proto::NoticeLevel) -> HarnessInputMessage {
    HarnessInputMessage::ExtensionNoticeRequest(tau_proto::ExtensionNoticeRequest {
        message: message.to_owned(),
        level,
    })
}

fn connect_notice_observer(
    harness: &mut Harness,
    connection_id: &str,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(harness, connection_id, tau_proto::ClientKind::Ui);
    harness
        .bus
        .set_subscriptions(
            &crate::test_connection_id(connection_id),
            Vec::new(),
            vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
        )
        .expect("subscribe to notices");
    sink
}

fn connect_handshaking_configured_extension(
    harness: &mut Harness,
    connection_id: &str,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_ready_configured_extension(
        harness,
        connection_id,
        connection_id,
        tau_proto::ClientKind::Tool,
    );
    harness
        .extensions
        .entries
        .get_mut(connection_id)
        .expect("configured extension")
        .state = crate::extension::ExtensionState::Handshaking;
    harness.extensions.ready_received.remove(connection_id);
    sink
}

fn committed_extension_notices(harness: &Harness) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut notices = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches!(
            &entry.event,
            Event::HarnessNotice(notice)
                if notice.kind == tau_proto::notice_kind::EXTENSION_NOTICE
        ) {
            notices.push((entry.source, entry.event));
        }
    }
    notices
}

/// Every configured extension kind can request a routine notice, while the
/// harness fixes authorship and caps critical severity.
#[test]
fn every_configured_extension_kind_gets_harness_authored_sanitized_output() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let observer = connect_notice_observer(&mut harness, "observer");
    let kinds = [
        tau_proto::ClientKind::Provider,
        tau_proto::ClientKind::Tool,
        tau_proto::ClientKind::Action,
        tau_proto::ClientKind::Ui,
        tau_proto::ClientKind::Core,
        tau_proto::ClientKind::External,
    ];
    let kind_count = kinds.len();

    for (index, kind) in kinds.into_iter().enumerate() {
        let source = format!("notice-source-{index}");
        connect_ready_configured_extension(&mut harness, &source, &source, kind);
        harness
            .handle_extension_message(
                &crate::test_connection_id(&source),
                notice_request(&format!("notice-{index}"), tau_proto::NoticeLevel::Critical),
            )
            .expect("request notice");
    }

    let notices = committed_extension_notices(&harness);
    assert_eq!(notices.len(), kind_count);
    for (index, (source, event)) in notices.into_iter().enumerate() {
        assert_eq!(source.as_deref(), Some(HARNESS_CONNECTION_ID));
        assert!(matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.message == format!("notice-{index}")
                    && notice.level == tau_proto::NoticeLevel::Warning
                    && !notice.always_show
        ));
    }
    let delivered = observer.lock().expect("observer");
    assert_eq!(
        delivered
            .iter()
            .filter(|routed| {
                routed.source_id.as_deref() == Some(HARNESS_CONNECTION_ID)
                    && matches!(
                        &routed.frame,
                        HarnessOutputMessage::Deliver(delivery)
                            if !delivery.replay
                                && matches!(
                                    delivery.event.as_ref(),
                                    Event::HarnessNotice(notice)
                                        if notice.kind
                                            == tau_proto::notice_kind::EXTENSION_NOTICE
                                )
                    )
            })
            .count(),
        kind_count
    );
}

/// The dedicated input is exact configured-extension authority. Socket,
/// unconfigured, and disconnected peers receive no output or diagnostic.
#[test]
fn unauthorized_notice_requests_are_silently_denied() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let observer = connect_notice_observer(&mut harness, "observer");

    connect_test_tool(&mut harness, "unconfigured");
    harness
        .handle_extension_message(
            &crate::test_connection_id("unconfigured"),
            notice_request("unconfigured", tau_proto::NoticeLevel::Info),
        )
        .expect("silently deny unconfigured request");

    connect_ready_configured_extension(
        &mut harness,
        "disconnected",
        "disconnected",
        tau_proto::ClientKind::Core,
    );
    harness
        .extensions
        .entries
        .get_mut("disconnected")
        .expect("configured entry")
        .state = crate::extension::ExtensionState::Disconnected;
    harness
        .handle_extension_message(
            &crate::test_connection_id("disconnected"),
            notice_request("disconnected", tau_proto::NoticeLevel::Info),
        )
        .expect("silently deny disconnected request");
    assert_eq!(
        harness.extensions.entries["disconnected"]
            .protocol_io
            .cumulative_stats()
            .uplink["message.extension_notice_request"]
            .count,
        1
    );

    connect_test_client_with_origin(
        &mut harness,
        "socket-ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    harness
        .handle_client_message(
            &crate::test_connection_id("socket-ui"),
            notice_request("socket", tau_proto::NoticeLevel::Info),
        )
        .expect("silently deny socket request");

    assert!(committed_extension_notices(&harness).is_empty());
    assert!(observer.lock().expect("observer").iter().all(|routed| {
        !matches!(
            &routed.frame,
            HarnessOutputMessage::Deliver(delivery)
                if matches!(
                    delivery.event.as_ref(),
                    Event::HarnessNotice(notice)
                        if notice.kind == tau_proto::notice_kind::EXTENSION_NOTICE
                )
        )
    }));
}

/// Legacy peer-authored harness notices no longer reach the fallback publisher.
#[test]
fn legacy_extension_notice_emit_is_denied() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let observer = connect_notice_observer(&mut harness, "observer");
    connect_ready_configured_extension(
        &mut harness,
        "publisher",
        "publisher",
        tau_proto::ClientKind::Core,
    );

    harness
        .handle_extension_message(
            &crate::test_connection_id("publisher"),
            HarnessInputMessage::emit_with_persist(
                Event::HarnessNotice(tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                    message: "legacy".to_owned(),
                    level: tau_proto::NoticeLevel::Info,
                    always_show: false,
                }),
                false,
            ),
        )
        .expect("deny legacy Emit");

    assert!(committed_extension_notices(&harness).is_empty());
    assert!(observer.lock().expect("observer").is_empty());
}

/// The harness-authored output remains ordinary transient publication:
/// interception can rewrite its message but not harness-owned fields.
#[test]
fn output_uses_ordinary_transient_interception_and_broadcast() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "publisher",
        "publisher",
        tau_proto::ClientKind::Tool,
    );
    let observer = connect_notice_observer(&mut harness, "observer");
    let interceptor = connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness
        .handle_extension_message(
            &crate::test_connection_id("publisher"),
            notice_request("original", tau_proto::NoticeLevel::Debug),
        )
        .expect("request notice");
    let intercept = interceptor
        .lock()
        .expect("interceptor")
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::InterceptRequest(request) => Some(request.clone()),
            _ => None,
        })
        .expect("notice intercept request");
    assert!(!intercept.persist);
    assert!(matches!(
        intercept.event.as_ref(),
        Event::HarnessNotice(notice)
            if notice.kind == tau_proto::notice_kind::EXTENSION_NOTICE
                && notice.message == "original"
                && notice.level == tau_proto::NoticeLevel::Debug
                && !notice.always_show
    ));

    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                    tau_proto::HarnessNotice {
                        kind: "spoofed.kind".to_owned(),
                        message: "rewritten".to_owned(),
                        level: tau_proto::NoticeLevel::Critical,
                        always_show: true,
                    },
                )))),
            })),
        )
        .expect("replace notice message");

    assert!(matches!(
        committed_extension_notices(&harness).as_slice(),
        [(Some(source), Event::HarnessNotice(notice))]
            if source.as_str() == HARNESS_CONNECTION_ID
                && notice.kind == tau_proto::notice_kind::EXTENSION_NOTICE
                && notice.message == "rewritten"
                && notice.level == tau_proto::NoticeLevel::Debug
                && !notice.always_show
    ));
    assert!(observer.lock().expect("observer").iter().any(|routed| {
        routed.source_id.as_deref() == Some(HARNESS_CONNECTION_ID)
            && matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(
                            delivery.event.as_ref(),
                            Event::HarnessNotice(notice) if notice.message == "rewritten"
                        )
            )
    }));
}

/// Notice requests retain operational activation ordering and byte/count quota
/// accounting.
#[test]
fn pre_ready_request_is_quota_charged_and_released_only_after_activation() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_handshaking_configured_extension(&mut harness, "publisher");
    let request = notice_request("after Ready", tau_proto::NoticeLevel::Info);
    let expected_bytes = tau_proto::encode_message_to_vec(&request)
        .expect("encode request")
        .len();

    harness
        .handle_extension_message(&crate::test_connection_id("publisher"), request)
        .expect("defer request");
    assert_eq!(
        harness.extensions.entries["publisher"]
            .protocol_io
            .cumulative_stats()
            .uplink["message.extension_notice_request"]
            .count,
        1
    );
    let stage = &harness.extensions.activation_staging["publisher"];
    assert_eq!(stage.retained_message_count, 1);
    assert_eq!(stage.retained_message_bytes, expected_bytes);
    assert!(committed_extension_notices(&harness).is_empty());

    harness
        .handle_extension_message(
            &crate::test_connection_id("publisher"),
            TestMessage::Ready(Default::default()),
        )
        .expect("activate publisher");
    assert!(matches!(
        committed_extension_notices(&harness).as_slice(),
        [(_, Event::HarnessNotice(notice))] if notice.message == "after Ready"
    ));
    let stats = harness.extensions.entries["publisher"]
        .protocol_io
        .cumulative_stats();
    assert_eq!(
        stats.uplink["message.extension_notice_request"].count, 1,
        "activation release must not meter an in-memory frame twice"
    );
    assert_eq!(
        stats.uplink["message.extension_notice_request"].bytes,
        u64::try_from(expected_bytes).expect("encoded request size fits u64")
    );
}

/// Disconnect before activation release drops the retained request.
#[test]
fn pre_ready_request_is_dropped_on_disconnect() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_handshaking_configured_extension(&mut harness, "disconnecting");
    harness
        .handle_extension_message(
            &crate::test_connection_id("disconnecting"),
            notice_request("must be dropped", tau_proto::NoticeLevel::Info),
        )
        .expect("defer disconnecting request");
    harness.handle_disconnect(&crate::test_connection_id("disconnecting"));
    assert!(
        !committed_extension_notices(&harness)
            .iter()
            .any(|(_, event)| {
                matches!(event, Event::HarnessNotice(notice) if notice.message == "must be dropped")
            })
    );
}

/// A request remains legal after this peer sends Ready while another extension
/// holds the initial global barrier, and all retained requests preserve ingress
/// order across extension queues.
#[test]
fn ready_received_requests_wait_for_global_barrier_in_wire_order() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.initial_extension_tool_preflight_complete = false;
    connect_handshaking_configured_extension(&mut harness, "first");
    connect_handshaking_configured_extension(&mut harness, "blocker");

    harness
        .handle_extension_message(
            &crate::test_connection_id("first"),
            notice_request("first-before-ready", tau_proto::NoticeLevel::Info),
        )
        .expect("stage first request");
    harness
        .handle_extension_message(
            &crate::test_connection_id("blocker"),
            notice_request("blocker-before-ready", tau_proto::NoticeLevel::Info),
        )
        .expect("stage blocker request");
    harness
        .handle_extension_message(
            &crate::test_connection_id("first"),
            TestMessage::Ready(Default::default()),
        )
        .expect("first Ready remains behind blocker");
    assert!(harness.extensions.ready_received.contains("first"));
    harness
        .handle_extension_message(
            &crate::test_connection_id("first"),
            notice_request("first-after-ready", tau_proto::NoticeLevel::Info),
        )
        .expect("stage legal post-Ready request");
    assert!(committed_extension_notices(&harness).is_empty());

    harness
        .handle_extension_message(
            &crate::test_connection_id("blocker"),
            TestMessage::Ready(Default::default()),
        )
        .expect("release global barrier");

    let messages = committed_extension_notices(&harness)
        .into_iter()
        .map(|(_, event)| match event {
            Event::HarnessNotice(notice) => notice.message,
            _ => unreachable!("helper filters extension notices"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            "first-before-ready",
            "blocker-before-ready",
            "first-after-ready"
        ]
    );
    assert_eq!(
        harness.extensions.entries["first"]
            .protocol_io
            .cumulative_stats()
            .uplink["message.extension_notice_request"]
            .count,
        2
    );
    assert_eq!(
        harness.extensions.entries["blocker"]
            .protocol_io
            .cumulative_stats()
            .uplink["message.extension_notice_request"]
            .count,
        1
    );
}

/// Dedicated notice requests preserve normal configured-extension phase
/// validation before their operational activation staging.
#[test]
fn pre_hello_request_follows_protocol_failure_path() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    harness.initial_extension_tool_preflight_complete = true;
    connect_ready_configured_extension(
        &mut harness,
        "spawning-requester",
        "spawning-requester",
        tau_proto::ClientKind::Tool,
    );
    harness
        .extensions
        .entries
        .get_mut("spawning-requester")
        .expect("spawning requester")
        .state = crate::extension::ExtensionState::Spawning;

    harness
        .handle_extension_message(
            &crate::test_connection_id("spawning-requester"),
            notice_request("too early", tau_proto::NoticeLevel::Info),
        )
        .expect("isolate out-of-phase requester");

    let entry = &harness.extensions.entries["spawning-requester"];
    assert_eq!(
        entry.protocol_io.cumulative_stats().uplink["message.extension_notice_request"].count,
        1
    );
    assert_eq!(entry.state, crate::extension::ExtensionState::Disconnected);
    assert!(
        harness
            .bus
            .connection(&crate::test_connection_id("spawning-requester"))
            .is_none()
    );
    assert!(committed_extension_notices(&harness).is_empty());
}

/// Once inline handling creates the harness-authored publication, disconnecting
/// the requesting extension does not cancel an output parked at an interceptor.
#[test]
fn requester_disconnect_does_not_cancel_parked_harness_output() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "publisher",
        "publisher",
        tau_proto::ClientKind::Tool,
    );
    let observer = connect_notice_observer(&mut harness, "observer");
    let _interceptor = connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness
        .handle_extension_message(
            &crate::test_connection_id("publisher"),
            notice_request("survives disconnect", tau_proto::NoticeLevel::Info),
        )
        .expect("request notice");
    harness.handle_disconnect(&crate::test_connection_id("publisher"));
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("pass parked output");

    assert!(
        committed_extension_notices(&harness)
            .iter()
            .any(|(source, event)| {
                source.as_deref() == Some(HARNESS_CONNECTION_ID)
                    && matches!(
                        event,
                        Event::HarnessNotice(notice) if notice.message == "survives disconnect"
                    )
            })
    );
    assert!(observer.lock().expect("observer").iter().any(|routed| {
        routed.source_id.as_deref() == Some(HARNESS_CONNECTION_ID)
            && matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if matches!(
                        delivery.event.as_ref(),
                        Event::HarnessNotice(notice) if notice.message == "survives disconnect"
                    )
            )
    }));
}

/// An interceptor may drop the ordinary harness-authored live output.
#[test]
fn interceptor_can_drop_requested_notice_output() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "publisher",
        "publisher",
        tau_proto::ClientKind::Tool,
    );
    let observer = connect_notice_observer(&mut harness, "observer");
    let _interceptor = connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");

    harness
        .handle_extension_message(
            &crate::test_connection_id("publisher"),
            notice_request("drop me", tau_proto::NoticeLevel::Info),
        )
        .expect("request notice");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("drop output");

    assert!(committed_extension_notices(&harness).is_empty());
    assert!(observer.lock().expect("observer").is_empty());
}

/// Debug JSONL records the raw point-to-point request and later event
/// publication as distinct records with distinct names.
#[test]
fn debug_jsonl_keeps_request_and_published_output_separate() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "publisher",
        "publisher",
        tau_proto::ClientKind::Tool,
    );
    let debug_path = harness
        .enable_debug_log(&temp.path().join("debug"))
        .expect("enable debug log");
    let event = HarnessEvent::from_connection_for_test(
        crate::test_connection_id("publisher"),
        notice_request("debug separation", tau_proto::NoticeLevel::Debug),
    );
    harness.log_event(&event);
    let mut served_clients = 0;
    let mut exit_on_disconnect = false;
    let mut ever_attached = false;
    harness
        .handle_runtime_event(
            event,
            &mut served_clients,
            &mut exit_on_disconnect,
            &mut ever_attached,
        )
        .expect("handle notice request");

    let entries = std::fs::read_to_string(debug_path)
        .expect("read debug log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("debug JSON"))
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["event_name"], "<message>");
    assert_eq!(entries[0]["event"]["message"], "extension_notice_request");
    assert_eq!(
        entries[0]["event"]["payload"]["message"],
        "debug separation"
    );
    assert_eq!(entries[1]["event_name"], "harness.notice");
}

/// Routine notice output is not current state and never replays to a late
/// historical subscriber.
#[test]
fn routine_notice_output_is_not_replayed() {
    let temp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "publisher",
        "publisher",
        tau_proto::ClientKind::Tool,
    );
    harness
        .handle_extension_message(
            &crate::test_connection_id("publisher"),
            notice_request("live only", tau_proto::NoticeLevel::Warning),
        )
        .expect("request notice");
    assert!(harness.replayable_harness_notices.iter().all(|notice| {
        notice.kind != tau_proto::notice_kind::EXTENSION_NOTICE || notice.message != "live only"
    }));

    let late = connect_test_client(&mut harness, "late", tau_proto::ClientKind::Ui);
    harness.replay_harness_notice(
        &crate::test_connection_id("late"),
        &[EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
    );
    assert!(late.lock().expect("late").iter().all(|routed| {
        !matches!(
            &routed.frame,
            HarnessOutputMessage::Deliver(delivery)
                if delivery.replay
                    && matches!(
                        delivery.event.as_ref(),
                        Event::HarnessNotice(notice) if notice.message == "live only"
                    )
        )
    }));
}

use super::*;

fn action_schema(action_id: &str) -> tau_actions::ActionSchema {
    tau_actions::ActionSchema {
        version: tau_actions::ACTION_SCHEMA_VERSION,
        roots: vec![tau_actions::ActionCommand {
            name: "/email".to_owned(),
            description: "Email approvals".to_owned(),
            action_id: None,
            args: Vec::new(),
            children: vec![tau_actions::ActionCommand {
                name: "list".to_owned(),
                description: "List approvals".to_owned(),
                action_id: Some(action_id.to_owned()),
                args: Vec::new(),
                children: Vec::new(),
            }],
        }],
    }
}

fn publish_action_schema(h: &mut Harness, source_id: &str, action_id: &str) {
    h.handle_extension_event(
        source_id,
        TestProtocolItem::Event(Event::ActionSchemaPublished(
            tau_proto::ActionSchemaPublished {
                extension_name: "spoofed".into(),
                instance_id: 99.into(),
                schema: action_schema(action_id),
            },
        )),
    )
    .expect("schema publish should be handled");
}

fn action_invoke(invocation_id: &str, extension_name: &str) -> tau_proto::ActionInvoke {
    tau_proto::ActionInvoke {
        invocation_id: invocation_id.into(),
        session_id: "s1".into(),
        extension_name: extension_name.into(),
        instance_id: 0.into(),
        action_id: "email.list".to_owned(),
        raw_line: "/email list".to_owned(),
        argv: Vec::new(),
        arguments: CborValue::Map(Vec::new()),
    }
}

fn action_result(invocation_id: &str, text: &str) -> tau_proto::ActionResult {
    tau_proto::ActionResult {
        invocation_id: invocation_id.into(),
        action_id: "email.list".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: text.to_owned(),
        },
    }
}

fn subscribe_to_actions(h: &mut Harness, client_id: &str) {
    h.bus
        .set_subscriptions(client_id, vec![EventSelector::Prefix("action.".to_owned())])
        .expect("subscribe to action events");
}

fn drain_sink(sink: &Arc<Mutex<Vec<RoutedFrame>>>) {
    sink.lock().expect("sink").clear();
}

#[test]
fn action_schema_publish_is_owner_stamped_and_broadcast() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let _extension = connect_test_client(&mut h, "email-ext", tau_proto::ClientKind::Tool);
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    subscribe_to_actions(&mut h, "ui");

    publish_action_schema(&mut h, "email-ext", "email.list");

    let events = ui.lock().expect("ui sink");
    let routed = events
        .iter()
        .find(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionSchemaPublished(_))
            )
        })
        .expect("schema event should be delivered");
    assert_eq!(routed.source_id.as_deref(), Some("email-ext"));
    match peel_inner_event(&routed.frame) {
        Some(Event::ActionSchemaPublished(published)) => {
            assert_eq!(
                published.extension_name,
                tau_proto::ExtensionName::from("email-ext")
            );
            assert_eq!(
                published.instance_id,
                tau_proto::ExtensionInstanceId::from(0)
            );
            assert_eq!(
                published
                    .schema
                    .executable_action_ids()
                    .expect("schema should be valid"),
                vec!["email.list".to_owned()]
            );
        }
        _ => unreachable!("matched above"),
    }
    assert!(h.action_registry.has_schema_for_connection("email-ext"));
}

#[test]
fn action_schema_replays_to_late_action_subscriber() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let _extension = connect_test_client(&mut h, "email-ext", tau_proto::ClientKind::Tool);
    publish_action_schema(&mut h, "email-ext", "email.list");
    let late_ui = connect_test_client(&mut h, "late-ui", tau_proto::ClientKind::Ui);

    h.replay_harness_notice("late-ui", &[EventSelector::Prefix("action.".to_owned())]);

    let events = late_ui.lock().expect("late ui sink");
    assert!(events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionSchemaPublished(published))
            if published.extension_name.as_str() == "email-ext"
    )));
}

#[test]
fn action_invoke_routes_to_owner_and_result_returns_only_to_requester() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_test_client(&mut h, "email-ext", tau_proto::ClientKind::Tool);
    let _spoof = connect_test_client(&mut h, "spoof-ext", tau_proto::ClientKind::Tool);
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    let other_ui = connect_test_client(&mut h, "other-ui", tau_proto::ClientKind::Ui);
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&ui);
    drain_sink(&other_ui);

    h.handle_client_event_inner(
        "ui",
        Event::ActionInvoke(action_invoke("action-1", "email-ext")),
    )
    .expect("invoke should be handled");

    let extension_events = extension.lock().expect("extension sink");
    let routed_invoke = extension_events
        .iter()
        .find(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionInvoke(_))
            )
        })
        .expect("invoke should be sent to owner");
    assert_eq!(routed_invoke.source_id.as_deref(), Some("ui"));
    drop(extension_events);

    h.handle_extension_event(
        "spoof-ext",
        TestProtocolItem::Event(Event::ActionResult(action_result("action-1", "spoofed"))),
    )
    .expect("spoofed result should be handled and discarded");
    assert!(ui.lock().expect("ui sink").is_empty());

    h.handle_extension_event(
        "email-ext",
        TestProtocolItem::Event(Event::ActionResult(action_result("action-1", "ok"))),
    )
    .expect("result should be handled");

    let ui_events = ui.lock().expect("ui sink");
    assert!(ui_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionResult(result))
            if result.invocation_id.as_str() == "action-1"
    )));
    assert!(other_ui.lock().expect("other ui sink").is_empty());
}

#[test]
fn duplicate_action_invocation_id_cannot_steal_result_routing() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_test_client(&mut h, "email-ext", tau_proto::ClientKind::Tool);
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    let other_ui = connect_test_client(&mut h, "other-ui", tau_proto::ClientKind::Ui);
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&ui);
    drain_sink(&other_ui);

    h.handle_client_event_inner(
        "ui",
        Event::ActionInvoke(action_invoke("shared-action", "email-ext")),
    )
    .expect("first invoke should be handled");
    h.handle_client_event_inner(
        "other-ui",
        Event::ActionInvoke(action_invoke("shared-action", "email-ext")),
    )
    .expect("duplicate invoke should be rejected");

    let extension_events = extension.lock().expect("extension sink");
    assert_eq!(
        extension_events
            .iter()
            .filter(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionInvoke(_))
            ))
            .count(),
        1,
        "duplicate invocation id must not be forwarded to the provider"
    );
    drop(extension_events);
    let other_events = other_ui.lock().expect("other ui sink");
    assert!(other_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionError(error))
            if error.invocation_id.as_str() == "shared-action"
                && error.message.contains("duplicate")
    )));
    drop(other_events);
    drain_sink(&ui);
    drain_sink(&other_ui);

    h.handle_extension_event(
        "email-ext",
        TestProtocolItem::Event(Event::ActionResult(action_result("shared-action", "ok"))),
    )
    .expect("original result should be handled");

    let ui_events = ui.lock().expect("ui sink");
    assert!(ui_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionResult(result)) if result.invocation_id.as_str() == "shared-action"
    )));
    assert!(other_ui.lock().expect("other ui sink").is_empty());
}

#[test]
fn action_invoke_rejects_non_ui_source_wrong_session_and_invalid_arguments() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_test_client(&mut h, "email-ext", tau_proto::ClientKind::Tool);
    let tool_client = connect_test_client(&mut h, "tool-client", tau_proto::ClientKind::Tool);
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&tool_client);
    drain_sink(&ui);

    h.handle_client_event_inner(
        "tool-client",
        Event::ActionInvoke(action_invoke("tool-action", "email-ext")),
    )
    .expect("non-ui invoke should be handled as rejection");
    assert!(tool_client.lock().expect("tool sink").iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionError(error))
            if error.invocation_id.as_str() == "tool-action" && error.message.contains("only UI")
    )));
    assert!(extension.lock().expect("extension sink").is_empty());

    let mut wrong_session = action_invoke("wrong-session", "email-ext");
    wrong_session.session_id = "other-session".into();
    h.handle_client_event_inner("ui", Event::ActionInvoke(wrong_session))
        .expect("wrong-session invoke should be handled as rejection");
    assert!(ui.lock().expect("ui sink").iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionError(error))
            if error.invocation_id.as_str() == "wrong-session"
                && error.message.contains("current session")
    )));
    assert!(extension.lock().expect("extension sink").is_empty());
    drain_sink(&ui);

    let mut invalid = action_invoke("bad-args", "email-ext");
    invalid.raw_line = "/email list unexpected".to_owned();
    invalid.argv = vec!["unexpected".to_owned()];
    h.handle_client_event_inner("ui", Event::ActionInvoke(invalid))
        .expect("invalid invoke should be handled as rejection");
    assert!(ui.lock().expect("ui sink").iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionError(error))
            if error.invocation_id.as_str() == "bad-args"
                && error.message.contains("invalid action invocation")
    )));
    assert!(extension.lock().expect("extension sink").is_empty());
}

#[test]
fn action_provider_disconnect_unregisters_and_fails_pending_invocations() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_test_client(&mut h, "email-ext", tau_proto::ClientKind::Tool);
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&ui);

    h.handle_client_event_inner(
        "ui",
        Event::ActionInvoke(action_invoke("action-2", "email-ext")),
    )
    .expect("invoke should be handled");
    drain_sink(&ui);

    h.handle_disconnect("email-ext");

    assert!(!h.action_registry.has_schema_for_connection("email-ext"));
    let ui_events = ui.lock().expect("ui sink");
    assert!(ui_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionError(error))
            if error.invocation_id.as_str() == "action-2"
    )));
}

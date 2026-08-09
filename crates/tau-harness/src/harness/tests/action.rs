use super::*;
use crate::extension::ExtensionState;

fn action_schema(action_id: &str) -> tau_actions::ActionSchema {
    tau_actions::ActionSchema {
        version: tau_actions::ACTION_SCHEMA_VERSION,
        roots: vec![tau_actions::ActionCommand {
            name: ":email".to_owned(),
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
        TestProtocolItem::Event(Event::ActionSchemaDeclared(
            tau_proto::ActionSchemaDeclared {
                schema: action_schema(action_id),
            },
        )),
    )
    .expect("schema publish should be handled");
}

/// Connect a configured Tool peer with explicit Action-provider authority.
fn connect_action_provider(h: &mut Harness, name: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(h, name, tau_proto::ClientKind::Tool);
    mark_connected_test_extension_configured(h, name, name, tau_proto::ClientKind::Tool);
    let entry = h
        .extensions
        .entries
        .get_mut(name)
        .expect("configured Action provider");
    entry.instance_id = 0.into();
    entry.peer_capabilities = [tau_proto::PeerCapability::ActionProvider]
        .into_iter()
        .collect();
    sink
}

fn action_invoke(invocation_id: &str, extension_name: &str) -> tau_proto::ActionInvoke {
    tau_proto::ActionInvoke {
        invocation_id: test_action_invocation_id(invocation_id),
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        extension_name: crate::test_extension_name(extension_name),
        instance_id: 0.into(),
        action_id: "email.list".to_owned(),
        raw_line: ":email list".to_owned(),
        argv: Vec::new(),
        arguments: CborValue::Map(Vec::new()),
    }
}

fn action_result(invocation_id: &str, text: &str) -> tau_proto::ActionResult {
    tau_proto::ActionResult {
        invocation_id: test_action_invocation_id(invocation_id),
        action_id: "email.list".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: text.to_owned(),
        },
    }
}

fn subscribe_to_actions(h: &mut Harness, client_id: &str) {
    h.bus
        .set_subscriptions(
            &crate::test_connection_id(client_id),
            Vec::new(),
            vec![EventSelector::Prefix("action.".to_owned())],
        )
        .expect("subscribe to action events");
}

fn drain_sink(sink: &Arc<Mutex<Vec<RoutedFrame>>>) {
    sink.lock().expect("sink").clear();
}

#[test]
fn action_schema_publish_is_owner_stamped_and_broadcast() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let _extension = connect_action_provider(&mut h, "email-ext");
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
    assert_eq!(
        routed.source_id.as_deref(),
        Some(crate::harness::harness_connection_id().as_str())
    );
    match peel_inner_event(&routed.frame) {
        Some(Event::ActionSchemaPublished(published)) => {
            assert_eq!(
                published.extension_name,
                tau_proto::ExtensionName::parse("email-ext")
                    .expect("test extension name must satisfy the identifier grammar")
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
    assert!(
        h.action_registry
            .has_schema_for_connection(&crate::test_connection_id("email-ext"))
    );
    let declaration_index = events
        .iter()
        .position(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionSchemaDeclared(_))
            )
        })
        .expect("peer declaration must commit and broadcast");
    let canonical_index = events
        .iter()
        .position(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionSchemaPublished(_))
            )
        })
        .expect("canonical schema must publish");
    assert!(declaration_index < canonical_index);
}

/// Ensures kind alone cannot grant Action authority without the explicit
/// capability.
#[test]
fn action_schema_declaration_requires_action_provider_capability() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let _extension = connect_ready_configured_extension(
        &mut h,
        "plain-tool",
        "plain-tool",
        tau_proto::ClientKind::Tool,
    );
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    subscribe_to_actions(&mut h, "ui");

    publish_action_schema(&mut h, "plain-tool", "email.list");

    assert!(
        !h.action_registry
            .has_schema_for_connection(&crate::test_connection_id("plain-tool"))
    );
    assert!(ui.lock().expect("ui sink").is_empty());
}

/// Ensures complete snapshots replace atomically, empty snapshots withdraw, and
/// identical declarations do not republish canonical state.
#[test]
fn action_schema_snapshots_replace_withdraw_and_deduplicate() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let _extension = connect_action_provider(&mut h, "email-ext");
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    subscribe_to_actions(&mut h, "ui");

    publish_action_schema(&mut h, "email-ext", "email.list");
    publish_action_schema(&mut h, "email-ext", "email.list");
    let canonical_count = ui
        .lock()
        .expect("ui sink")
        .iter()
        .filter(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionSchemaPublished(_))
            )
        })
        .count();
    assert_eq!(canonical_count, 1);

    h.handle_extension_event(
        "email-ext",
        TestProtocolItem::Event(Event::ActionSchemaDeclared(
            tau_proto::ActionSchemaDeclared {
                schema: tau_actions::ActionSchema {
                    version: tau_actions::ACTION_SCHEMA_VERSION,
                    roots: Vec::new(),
                },
            },
        )),
    )
    .expect("empty snapshot should withdraw Actions");

    assert!(
        h.action_registry
            .schema_for_connection(&crate::test_connection_id("email-ext"))
            .is_some_and(|snapshot| snapshot.schema.roots.is_empty())
    );
    assert!(
        h.action_registry
            .route_action_invoke(&action_invoke("withdrawn", "email-ext"))
            .is_err()
    );
}

/// Ensures dropping a parked startup Action snapshot releases its activation
/// reservation and lets the extension become ready without publishing state.
#[test]
fn dropped_startup_action_schema_releases_activation_reservation() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let _extension = connect_action_provider(&mut h, "email-ext");
    h.extensions
        .entries
        .get_mut("email-ext")
        .expect("Action provider")
        .state = ExtensionState::Handshaking;
    let _interceptor = connect_test_client(&mut h, "interceptor", tau_proto::ClientKind::Tool);
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::ACTION_SCHEMA_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    publish_action_schema(&mut h, "email-ext", "email.list");
    h.handle_extension_message(
        &crate::test_connection_id("email-ext"),
        TestMessage::Ready(Default::default()),
    )
    .expect("Ready waits for parked declaration");

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop startup declaration");

    assert_eq!(
        h.extensions.entries["email-ext"].state,
        ExtensionState::Ready
    );
    assert!(
        !h.extensions
            .pending_action_schema_declarations
            .contains_key("email-ext")
    );
    assert!(
        !h.action_registry
            .has_schema_for_connection(&crate::test_connection_id("email-ext"))
    );
}

/// Ensures interception replacement bytes are recharged and an oversized
/// startup Action snapshot fails closed without wedging its reservation.
#[test]
fn oversized_startup_action_schema_replacement_releases_reservation() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    h.initial_extension_tool_preflight_complete = false;
    let _extension = connect_action_provider(&mut h, "email-ext");
    h.extensions
        .entries
        .get_mut("email-ext")
        .expect("Action provider")
        .state = ExtensionState::Handshaking;
    let _interceptor = connect_test_client(&mut h, "interceptor", tau_proto::ClientKind::Tool);
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::ACTION_SCHEMA_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    publish_action_schema(&mut h, "email-ext", "email.list");
    let mut replacement = action_schema("email.list");
    replacement.roots[0].description = "x".repeat(crate::harness::MAX_EXTENSION_ACTIVATION_BYTES);

    let error = h
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(Event::ActionSchemaDeclared(
                    tau_proto::ActionSchemaDeclared {
                        schema: replacement,
                    },
                )))),
            })),
        )
        .expect_err("oversized replacement must fail required startup");

    assert!(error.to_string().contains("activation staging exceeds"));
    assert!(
        !h.extensions
            .pending_action_schema_declarations
            .contains_key("email-ext")
    );
    let stage = &h.extensions.activation_staging["email-ext"];
    assert_eq!(stage.retained_message_count, 0);
    assert_eq!(stage.retained_message_bytes, 0);
}

#[test]
fn action_schema_replays_to_late_action_subscriber() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let _extension = connect_action_provider(&mut h, "email-ext");
    publish_action_schema(&mut h, "email-ext", "email.list");
    let late_ui = connect_test_client(&mut h, "late-ui", tau_proto::ClientKind::Ui);

    h.replay_harness_notice(
        &crate::test_connection_id("late-ui"),
        &[EventSelector::Prefix("action.".to_owned())],
    );

    let events = late_ui.lock().expect("late ui sink");
    let replayed = events
        .iter()
        .find(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionSchemaPublished(published))
                    if published.extension_name.as_str() == "email-ext"
            )
        })
        .expect("late subscriber must receive current Action schema");
    assert_eq!(
        replayed.source_id.as_deref(),
        Some(crate::harness::harness_connection_id().as_str())
    );
}

#[test]
fn action_invoke_routes_to_owner_and_result_returns_only_to_requester() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_action_provider(&mut h, "email-ext");
    let _spoof = connect_test_client(&mut h, "spoof-ext", tau_proto::ClientKind::Tool);
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    let other_ui = connect_test_client(&mut h, "other-ui", tau_proto::ClientKind::Ui);
    subscribe_to_actions(&mut h, "other-ui");
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&ui);
    drain_sink(&other_ui);

    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
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
        TestProtocolItem::Event(Event::ActionResultReported(action_result(
            "action-1", "spoofed",
        ))),
    )
    .expect("spoofed result should be handled and discarded");
    assert!(ui.lock().expect("ui sink").is_empty());

    h.handle_extension_event(
        "email-ext",
        TestProtocolItem::Event(Event::ActionResultReported(action_result("action-1", "ok"))),
    )
    .expect("result should be handled");

    let ui_events = ui.lock().expect("ui sink");
    let canonical_result = ui_events
        .iter()
        .find(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::ActionResult(result))
                    if result.invocation_id.as_str() == "action-1"
            )
        })
        .expect("requester must receive canonical result");
    assert_eq!(
        canonical_result.source_id.as_deref(),
        Some(crate::harness::harness_connection_id().as_str())
    );
    let observer_events = other_ui.lock().expect("other ui sink");
    assert!(observer_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionResultReported(result))
            if result.invocation_id.as_str() == "action-1"
    )));
    assert!(!observer_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionResult(result))
            if result.invocation_id.as_str() == "action-1"
    )));
}

/// A dedicated cross-harness peer has a positive RPC allowlist, so generic
/// emission cannot fall through to either UI state changes or Action routing.
#[test]
fn external_message_peer_cannot_reach_ui_or_action_handlers() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_action_provider(&mut h, "email-ext");
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    let _external =
        connect_test_client(&mut h, "external-message", tau_proto::ClientKind::External);
    let external_id = crate::test_connection_id("external-message");
    h.handle_client_message(
        &external_id,
        tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: crate::test_extension_name(
                crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME,
            ),
            client_kind: tau_proto::ClientKind::External,
            expected_session_id: None,
            capabilities: Default::default(),
        }),
    )
    .expect("dedicated peer hello");
    let selected_role = h.selected_role.clone();

    for event in [
        Event::UiRoleSelect(tau_proto::UiRoleSelect {
            role: "engineer-senior".to_owned(),
        }),
        Event::ActionInvoke(action_invoke("external-action", "email-ext")),
    ] {
        h.handle_client_message(
            &external_id,
            tau_proto::HarnessInputMessage::Emit(tau_proto::Emit {
                event: Box::new(event),
                persist: false,
            }),
        )
        .expect("denied generic emission");
    }

    assert_eq!(h.selected_role, selected_role);
    assert!(extension.lock().expect("extension sink").is_empty());
}

#[test]
fn duplicate_action_invocation_id_cannot_steal_result_routing() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_action_provider(&mut h, "email-ext");
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    let other_ui = connect_test_client(&mut h, "other-ui", tau_proto::ClientKind::Ui);
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&ui);
    drain_sink(&other_ui);

    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
        Event::ActionInvoke(action_invoke("shared-action", "email-ext")),
    )
    .expect("first invoke should be handled");
    h.handle_client_event_inner(
        &crate::test_connection_id("other-ui"),
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
        TestProtocolItem::Event(Event::ActionResultReported(action_result(
            "shared-action",
            "ok",
        ))),
    )
    .expect("original result should be handled");

    let ui_events = ui.lock().expect("ui sink");
    assert!(ui_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionResult(result)) if result.invocation_id.as_str() == "shared-action"
    )));
    drop(ui_events);
    assert!(other_ui.lock().expect("other ui sink").is_empty());

    drain_sink(&extension);
    drain_sink(&ui);
    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
        Event::ActionInvoke(action_invoke("shared-action", "email-ext")),
    )
    .expect("completed invocation id reuse should be rejected");
    assert!(extension.lock().expect("extension sink").is_empty());
    assert!(ui.lock().expect("ui sink").iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionError(error))
            if error.invocation_id.as_str() == "shared-action"
                && error.message.contains("duplicate")
    )));
}

#[test]
fn action_invoke_rejects_non_ui_source_wrong_session_and_invalid_arguments() {
    let temp = TempDir::new().expect("temp dir");
    let mut h = quiet_provider_harness(temp.path()).expect("harness");
    let extension = connect_action_provider(&mut h, "email-ext");
    let tool_client = connect_test_client(&mut h, "tool-client", tau_proto::ClientKind::Tool);
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&tool_client);
    drain_sink(&ui);

    h.handle_client_event_inner(
        &crate::test_connection_id("tool-client"),
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
    wrong_session.session_id = "other-session"
        .parse::<tau_proto::SessionId>()
        .expect("known-safe SessionId must be valid");
    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
        Event::ActionInvoke(wrong_session),
    )
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
    invalid.raw_line = ":email list unexpected".to_owned();
    invalid.argv = vec!["unexpected".to_owned()];
    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
        Event::ActionInvoke(invalid),
    )
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
    let extension = connect_action_provider(&mut h, "email-ext");
    let ui = connect_test_client(&mut h, "ui", tau_proto::ClientKind::Ui);
    publish_action_schema(&mut h, "email-ext", "email.list");
    drain_sink(&extension);
    drain_sink(&ui);

    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
        Event::ActionInvoke(action_invoke("action-2", "email-ext")),
    )
    .expect("invoke should be handled");
    drain_sink(&ui);

    h.handle_disconnect(&crate::test_connection_id("email-ext"));

    assert!(
        !h.action_registry
            .has_schema_for_connection(&crate::test_connection_id("email-ext"))
    );
    let ui_events = ui.lock().expect("ui sink");
    assert!(ui_events.iter().any(|routed| matches!(
        peel_inner_event(&routed.frame),
        Some(Event::ActionError(error))
            if error.invocation_id.as_str() == "action-2"
    )));
}

/// Builds a validated action invocation id used by this test module.
fn test_action_invocation_id(value: impl AsRef<str>) -> tau_proto::ActionInvocationId {
    tau_proto::ActionInvocationId::parse(value.as_ref())
        .expect("test identifier must satisfy its grammar")
}

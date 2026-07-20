use super::*;

/// Construct one test tool specification.
fn declaration_tool_spec(name: &str, description: &str) -> tau_proto::ToolSpec {
    tau_proto::ToolSpec {
        name: tau_proto::ToolName::new(name),
        model_visible_name: None,
        description: Some(description.to_owned()),
        parameters: None,
        format: None,
        tool_type: tau_proto::ToolType::Function,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support: None,
        examples: Vec::new(),
    }
}

/// Wrap one test tool in its peer-authored declaration event.
fn tool_registration_declaration(name: &str, description: &str) -> Event {
    Event::ToolRegistrationDeclared(tau_proto::ToolRegistrationDeclared {
        tool: declaration_tool_spec(name, description),
        tool_group: None,
        prompt_fragment: None,
    })
}

/// Collect committed lifecycle declarations and canonical tool state for a
/// selected tool.
fn committed_tool_lifecycle_events(
    h: &Harness,
    tool_name: &str,
) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        let relevant = match &entry.event {
            Event::ToolRegistrationDeclared(declaration) => {
                declaration.tool.name.as_str() == tool_name
            }
            Event::ToolUnregistrationDeclared(declaration) => {
                declaration.tool_name.as_str() == tool_name
            }
            Event::ToolRegister(register) => register.tool.name.as_str() == tool_name,
            Event::ToolUnregister(unregister) => unregister.tool_name.as_str() == tool_name,
            _ => false,
        };
        if relevant {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// Exact interception can drop a tool declaration before it creates canonical
/// state or mutates the registry.
#[test]
fn dropping_tool_registration_declaration_prevents_canonical_state() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_REGISTRATION_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        tool_registration_declaration("declared_drop", "original"),
        Some(true),
    )
    .expect("declare tool");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop declaration");

    assert!(h.registry.providers_for("declared_drop").is_empty());
    assert!(committed_tool_lifecycle_events(&h, "declared_drop").is_empty());
}

/// A pre-Ready tool declaration parked in interception retains its activation
/// quota and blocks activation until the committed declaration is staged.
#[test]
fn parked_startup_tool_declaration_blocks_ready_until_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("tool-provider")
        .expect("tool provider")
        .state = crate::extension::ExtensionState::Handshaking;
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_REGISTRATION_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event(
        "tool-provider",
        TestProtocolItem::Event(tool_registration_declaration("declared_startup", "startup")),
    )
    .expect("park declaration");
    h.handle_extension_message("tool-provider", TestMessage::Ready(Default::default()))
        .expect("Ready waits");
    assert_eq!(
        h.extensions.entries["tool-provider"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions
            .pending_tool_lifecycle_declarations
            .get("tool-provider"),
        Some(&1)
    );

    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit declaration and activate");

    assert_eq!(
        h.extensions.entries["tool-provider"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(!h.registry.providers_for("declared_startup").is_empty());
    assert!(matches!(
        committed_tool_lifecycle_events(&h, "declared_startup").as_slice(),
        [
            (_, Event::ToolRegistrationDeclared(_)),
            (Some(source), Event::ToolRegister(_)),
        ] if source == HARNESS_CONNECTION_ID
    ));
}

/// A committed pre-Ready unregistration cancels the source's own staged
/// registration without exposing intermediate canonical or registry state.
#[test]
fn startup_register_then_unregister_exposes_no_intermediate_tool_state() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("tool-provider")
        .expect("tool provider")
        .state = crate::extension::ExtensionState::Handshaking;
    h.handle_extension_event(
        "tool-provider",
        TestProtocolItem::Event(tool_registration_declaration(
            "declared_cancelled",
            "cancelled before Ready",
        )),
    )
    .expect("stage registration");
    h.handle_extension_event(
        "tool-provider",
        TestProtocolItem::Event(Event::ToolUnregistrationDeclared(
            tau_proto::ToolUnregistrationDeclared {
                tool_name: tau_proto::ToolName::new("declared_cancelled"),
            },
        )),
    )
    .expect("cancel staged registration");
    h.handle_extension_message("tool-provider", TestMessage::Ready(Default::default()))
        .expect("activate empty stage");

    assert!(h.registry.providers_for("declared_cancelled").is_empty());
    let events = committed_tool_lifecycle_events(&h, "declared_cancelled");
    assert!(matches!(
        events.as_slice(),
        [
            (_, Event::ToolRegistrationDeclared(_)),
            (_, Event::ToolUnregistrationDeclared(_)),
        ]
    ));
}

/// A required initial tool's oversized interception replacement propagates the
/// startup-fatal activation quota error and releases its reservation.
#[test]
fn required_intercepted_tool_replacement_overflow_fails_startup() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    h.initial_extension_tool_preflight_complete = false;
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("tool-provider")
        .expect("tool provider")
        .state = crate::extension::ExtensionState::Handshaking;
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_REGISTRATION_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event(
        "tool-provider",
        TestProtocolItem::Event(tool_registration_declaration(
            "declared_small",
            "small declaration",
        )),
    )
    .expect("park small declaration");

    let error = h
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(tool_registration_declaration(
                    "declared_oversized",
                    &"x".repeat(super::super::super::MAX_EXTENSION_ACTIVATION_BYTES),
                )))),
            })),
        )
        .expect_err("required tool overflow must fail startup");

    assert!(error.to_string().contains("activation staging exceeds"));
    assert_eq!(
        h.extensions.entries["tool-provider"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert!(
        !h.extensions
            .pending_tool_lifecycle_declarations
            .contains_key("tool-provider")
    );
    let stage = &h.extensions.activation_staging["tool-provider"];
    assert_eq!(stage.retained_message_count, 0);
    assert_eq!(stage.retained_message_bytes, 0);
    assert!(h.registry.providers_for("declared_oversized").is_empty());
    assert!(
        committed_tool_lifecycle_events(&h, "declared_oversized")
            .iter()
            .all(|(_, event)| !matches!(event, Event::ToolRegister(_)))
    );
}

/// Resolving a parked tool declaration as the last initial-barrier blocker
/// propagates required-required collision failure without partial activation.
#[test]
fn intercepted_tool_resolution_propagates_initial_tool_collision() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    h.initial_extension_tool_preflight_complete = false;
    for source in ["required-a", "required-b"] {
        connect_ready_configured_extension(&mut h, source, source, tau_proto::ClientKind::Tool);
        h.extensions
            .entries
            .get_mut(source)
            .expect("required tool")
            .state = crate::extension::ExtensionState::Handshaking;
    }
    h.handle_extension_event(
        "required-a",
        TestProtocolItem::Event(tool_registration_declaration(
            "declared_collision",
            "owner A",
        )),
    )
    .expect("stage first owner");
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_REGISTRATION_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event(
        "required-b",
        TestProtocolItem::Event(tool_registration_declaration(
            "declared_collision",
            "owner B",
        )),
    )
    .expect("park second owner");
    for source in ["required-a", "required-b"] {
        h.handle_extension_message(source, TestMessage::Ready(Default::default()))
            .expect("Ready waits on parked declaration");
    }

    let error = h
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect_err("required collision must fail startup");
    assert!(error.to_string().contains("required extensions"));
    assert!(h.registry.providers_for("declared_collision").is_empty());
    assert_eq!(
        h.extensions.entries["required-a"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions.entries["required-b"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert!(
        committed_tool_lifecycle_events(&h, "declared_collision")
            .iter()
            .all(|(_, event)| !matches!(event, Event::ToolRegister(_)))
    );
}

/// Disconnecting a generation while its declaration is parked preserves the
/// committed observation but prevents stale registry and canonical state.
#[test]
fn parked_tool_declaration_cannot_mutate_after_disconnect() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_REGISTRATION_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        tool_registration_declaration("declared_stale", "stale"),
        Some(true),
    )
    .expect("park declaration");
    h.handle_disconnect("tool-provider");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale declaration");

    assert!(h.registry.providers_for("declared_stale").is_empty());
    assert!(matches!(
        committed_tool_lifecycle_events(&h, "declared_stale").as_slice(),
        [(_, Event::ToolRegistrationDeclared(_))]
    ));
}

/// A same-name interception replacement is revalidated after commit and drives
/// harness-authored canonical state with stable publisher provenance.
#[test]
fn replaced_tool_registration_declaration_drives_canonical_state() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Core,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_REGISTRATION_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        tool_registration_declaration("declared_replace", "original"),
        Some(true),
    )
    .expect("declare tool");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(tool_registration_declaration(
                "declared_replace",
                "replacement",
            )))),
        })),
    )
    .expect("replace declaration");

    let events = committed_tool_lifecycle_events(&h, "declared_replace");
    assert!(matches!(
        events.as_slice(),
        [
            (Some(declaration_source), Event::ToolRegistrationDeclared(declaration)),
            (Some(canonical_source), Event::ToolRegister(register)),
        ] if declaration_source == "tool-provider"
            && canonical_source == HARNESS_CONNECTION_ID
            && declaration.tool.description.as_deref() == Some("replacement")
            && register.tool.description.as_deref() == Some("replacement")
            && register.publisher_extension_id.as_str() == "configured-tool"
            && register.publisher_instance_id == tau_proto::ExtensionInstanceId::new(42)
    ));
    assert_eq!(
        h.registry.providers_for("declared_replace")[0]
            .tool
            .description
            .as_deref(),
        Some("replacement")
    );
}

/// Assigned-prefix validation runs on the committed interception replacement,
/// not on the declaration originally admitted.
#[test]
fn replaced_tool_registration_is_revalidated_against_assigned_prefix() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("tool-provider")
        .expect("tool provider")
        .tool_prefix = Some(tau_proto::ToolNamePrefix::parse("work").expect("prefix"));
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_REGISTRATION_DECLARED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        tool_registration_declaration("work_valid", "valid"),
        Some(true),
    )
    .expect("declare tool");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(tool_registration_declaration(
                "outside_prefix",
                "invalid replacement",
            )))),
        })),
    )
    .expect("commit invalid replacement");

    assert!(h.registry.providers_for("work_valid").is_empty());
    assert!(h.registry.providers_for("outside_prefix").is_empty());
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("assigned tool_prefix `work`")
                && notice.message.contains("outside_prefix")
    )));
    assert!(
        committed_tool_lifecycle_events(&h, "outside_prefix")
            .iter()
            .all(|(_, event)| !matches!(event, Event::ToolRegister(_)))
    );
}

/// A tool-prefix interceptor sees both declaration and canonical state, but
/// cannot drop or rewrite the harness-authored canonical event.
#[test]
fn tool_prefix_interception_protects_canonical_registration() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        tool_registration_declaration("declared_protected", "protected"),
        Some(true),
    )
    .expect("declare tool");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit declaration");
    let Some(Event::ToolRegister(mut forged)) = h
        .pending_intercept
        .as_ref()
        .map(|pending| pending.event.clone())
    else {
        panic!("expected canonical tool registration");
    };
    forged.tool.description = Some("forged canonical replacement".to_owned());
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ToolRegister(forged)))),
        })),
    )
    .expect("canonical rewrite is rejected");

    assert!(!h.registry.providers_for("declared_protected").is_empty());
    assert!(matches!(
        committed_tool_lifecycle_events(&h, "declared_protected").as_slice(),
        [
            (_, Event::ToolRegistrationDeclared(_)),
            (Some(source), Event::ToolRegister(register)),
        ] if source == HARNESS_CONNECTION_ID
            && register.tool.description.as_deref() == Some("protected")
    ));

    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        tool_registration_declaration("declared_drop_protected", "drop protected"),
        Some(true),
    )
    .expect("declare second tool");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit second declaration");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("canonical drop is overridden");
    assert!(matches!(
        committed_tool_lifecycle_events(&h, "declared_drop_protected").as_slice(),
        [
            (_, Event::ToolRegistrationDeclared(_)),
            (_, Event::ToolRegister(_)),
        ]
    ));
}

/// Explicit unregistration mutates only the declaring owner's state, publishes
/// canonical withdrawal under the harness source, and diagnoses repeats.
#[test]
fn committed_tool_unregistration_enforces_ownership_and_canonicalizes() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        tool_registration_declaration("declared_withdraw", "registered"),
        Some(true),
    )
    .expect("register tool");
    let declaration = Event::ToolUnregistrationDeclared(tau_proto::ToolUnregistrationDeclared {
        tool_name: tau_proto::ToolName::new("declared_withdraw"),
    });
    connect_ready_configured_extension(
        &mut h,
        "other-tool-provider",
        "other-configured-tool",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner_with_transient(
        "other-tool-provider",
        declaration.clone(),
        Some(true),
    )
    .expect("reject non-owner unregistration");
    assert!(!h.registry.providers_for("declared_withdraw").is_empty());
    h.handle_extension_event_inner_with_transient("tool-provider", declaration.clone(), Some(true))
        .expect("unregister tool");
    h.handle_extension_event_inner_with_transient("tool-provider", declaration, Some(true))
        .expect("repeat unknown unregistration");

    assert!(h.registry.providers_for("declared_withdraw").is_empty());
    let events = committed_tool_lifecycle_events(&h, "declared_withdraw");
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(event, Event::ToolUnregister(_)))
            .count(),
        1
    );
    assert!(events.iter().any(|(source, event)| {
        source.as_deref() == Some(HARNESS_CONNECTION_ID)
            && matches!(
                event,
                Event::ToolUnregister(unregister)
                    if unregister.publisher_extension_id.as_str() == "configured-tool"
                        && unregister.publisher_instance_id
                            == tau_proto::ExtensionInstanceId::new(42)
            )
    }));
    let rejections = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(notice)
                if notice.message.contains("Rejected tool unregistration")
                    && notice.message.contains("declared_withdraw") =>
            {
                Some(notice.message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(rejections.len(), 2);
    assert!(
        rejections
            .iter()
            .any(|message| message.contains("other-tool-provider"))
    );
    assert!(
        rejections
            .iter()
            .any(|message| message.contains("tool-provider"))
    );
}

/// Canonical withdrawals are immutable and must-pass under both replacement
/// and Drop interception attempts.
#[test]
fn canonical_tool_unregistration_is_immutable_and_must_pass() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "tool-provider",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    for tool_name in ["withdraw_rewrite", "withdraw_drop"] {
        h.handle_extension_event_inner_with_transient(
            "tool-provider",
            tool_registration_declaration(tool_name, tool_name),
            Some(true),
        )
        .expect("register tool");
    }
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_UNREGISTER)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        Event::ToolUnregistrationDeclared(tau_proto::ToolUnregistrationDeclared {
            tool_name: tau_proto::ToolName::new("withdraw_rewrite"),
        }),
        Some(true),
    )
    .expect("declare first withdrawal");
    let Some(Event::ToolUnregister(mut forged)) = h
        .pending_intercept
        .as_ref()
        .map(|pending| pending.event.clone())
    else {
        panic!("expected canonical withdrawal");
    };
    forged.tool_name = tau_proto::ToolName::new("forged_withdrawal");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::ToolUnregister(forged)))),
        })),
    )
    .expect("reject canonical withdrawal rewrite");

    h.handle_extension_event_inner_with_transient(
        "tool-provider",
        Event::ToolUnregistrationDeclared(tau_proto::ToolUnregistrationDeclared {
            tool_name: tau_proto::ToolName::new("withdraw_drop"),
        }),
        Some(true),
    )
    .expect("declare second withdrawal");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("override canonical withdrawal Drop");

    for tool_name in ["withdraw_rewrite", "withdraw_drop"] {
        assert!(h.registry.providers_for(tool_name).is_empty());
        assert!(matches!(
            committed_tool_lifecycle_events(&h, tool_name).as_slice(),
            [
                (_, Event::ToolRegistrationDeclared(_)),
                (_, Event::ToolRegister(_)),
                (_, Event::ToolUnregistrationDeclared(_)),
                (Some(source), Event::ToolUnregister(unregister)),
            ] if source == HARNESS_CONNECTION_ID
                && unregister.tool_name.as_str() == tool_name
                && unregister.publisher_extension_id.as_str() == "configured-tool"
        ));
    }
    assert!(committed_tool_lifecycle_events(&h, "forged_withdrawal").is_empty());
}

/// Configured Provider/Action peers and unconfigured Tool peers cannot author
/// declarations, and even an authorized Tool peer cannot author canonical
/// state.
#[test]
fn tool_declaration_and_canonical_authorship_fail_closed() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    for (source, tool_name, kind) in [
        (
            "provider-peer",
            "provider_peer",
            tau_proto::ClientKind::Provider,
        ),
        ("action-peer", "action_peer", tau_proto::ClientKind::Action),
    ] {
        connect_ready_configured_extension(&mut h, source, source, kind);
        h.handle_extension_event_inner_with_transient(
            source,
            tool_registration_declaration(tool_name, "forged"),
            Some(true),
        )
        .expect("reject unauthorized declaration");
        assert!(h.registry.providers_for(source).is_empty());
    }
    connect_test_tool(&mut h, "unconfigured-tool");
    h.handle_extension_event_inner_with_transient(
        "unconfigured-tool",
        tool_registration_declaration("unconfigured_tool", "forged"),
        Some(true),
    )
    .expect("reject unconfigured declaration");

    connect_ready_configured_extension(
        &mut h,
        "authorized-tool",
        "authorized-tool",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner_with_transient(
        "authorized-tool",
        Event::ToolRegister(tau_proto::ToolRegister {
            publisher_extension_id: "forged".into(),
            publisher_instance_id: 999.into(),
            tool: declaration_tool_spec("forged_canonical", "forged"),
            tool_group: None,
            prompt_fragment: None,
        }),
        Some(true),
    )
    .expect("reject peer canonical state");
    h.handle_extension_event_inner_with_transient(
        "authorized-tool",
        Event::ToolUnregister(tau_proto::ToolUnregister {
            publisher_extension_id: "forged".into(),
            publisher_instance_id: 999.into(),
            tool_name: tau_proto::ToolName::new("forged_canonical_unregistration"),
        }),
        Some(true),
    )
    .expect("reject peer canonical withdrawal");

    for name in [
        "provider_peer",
        "action_peer",
        "unconfigured_tool",
        "forged_canonical",
        "forged_canonical_unregistration",
    ] {
        assert!(h.registry.providers_for(name).is_empty());
        assert!(committed_tool_lifecycle_events(&h, name).is_empty());
    }
}

//! Contract tests for
//! `SPEC-session-discovery-declarations-and-readiness`.

use super::*;

/// Build one skill declaration whose name and description are easy to inspect.
fn skill(name: &str, description: &str) -> Event {
    Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
        name: name.into(),
        description: description.to_owned(),
        file_path: format!("/tmp/{name}.md").into(),
        add_to_prompt: true,
        user_invocable: true,
        disable_model_invocation: false,
        argument_hint: None,
    })
}

/// Register one interceptor for all four session-discovery event names.
fn connect_session_discovery_interceptor(h: &mut Harness) {
    connect_test_tool(h, "session-discovery-interceptor");
    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(
                    tau_proto::EventName::EXTENSION_SESSION_CONTEXT_PROVIDER_REGISTER,
                ),
                EventSelector::Exact(tau_proto::EventName::EXTENSION_SKILL_AVAILABLE),
                EventSelector::Exact(tau_proto::EventName::EXTENSION_AGENTS_MD_AVAILABLE),
                EventSelector::Exact(tau_proto::EventName::EXTENSION_SESSION_CONTEXT_READY),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
}

/// Return whether one source committed an event matching the predicate.
fn source_committed(h: &Harness, source: &str, predicate: impl Fn(&Event) -> bool) -> bool {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source) && predicate(&entry.event) {
            return true;
        }
    }
    false
}

/// Count committed harness notices so dropped declarations can prove they
/// produce no downstream diagnostics.
fn harness_notice_count(h: &Harness) -> usize {
    event_log_events(h)
        .into_iter()
        .filter(|event| matches!(event, Event::HarnessNotice(_)))
        .count()
}

/// A dropped skill declaration must remain absent from both the committed
/// stream and the harness-owned winner projection.
#[test]
fn dropped_skill_declaration_has_no_projection_or_diagnostic() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Action,
    );
    connect_session_discovery_interceptor(&mut h);
    let notices_before = harness_notice_count(&h);

    h.handle_extension_event_inner("discovery-owner", skill("drop-me", "drop me"))
        .expect("park skill");
    assert!(!h.discovered_skills.contains_key("drop-me"));
    assert_eq!(harness_notice_count(&h), notices_before);
    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop skill");

    assert!(!h.discovered_skills.contains_key("drop-me"));
    assert!(!source_committed(&h, "discovery-owner", |event| {
        matches!(event, Event::ExtSkillAvailable(skill) if skill.name == "drop-me")
    }));
    assert_eq!(harness_notice_count(&h), notices_before);
}

/// Only a same-name replacement may commit and replace an AGENTS.md source/path
/// slot after interception.
#[test]
fn agents_replacement_projects_only_committed_payload() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Provider,
    );
    let _agent = ensure_test_user_agent(&mut h);
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: "discovery-owner".into(),
        file_path: "/repo/AGENTS.md".into(),
        content: "PREVIOUS".to_owned(),
    });
    let injected_before = loaded_agent_events(&h, "s1")
        .into_iter()
        .filter(|event| matches!(event, Event::AgentUserMessageInjected(_)))
        .count();
    connect_session_discovery_interceptor(&mut h);
    let agents = |content: &str| {
        Event::ExtAgentsMdAvailable(tau_proto::ExtAgentsMdAvailable {
            file_path: "/repo/AGENTS.md".into(),
            content: content.to_owned(),
        })
    };

    h.handle_extension_event_inner("discovery-owner", agents("ORIGINAL"))
        .expect("park AGENTS declaration");
    assert_eq!(h.discovered_agents_files.len(), 1);
    assert_eq!(h.discovered_agents_files[0].content, "PREVIOUS");
    assert_eq!(
        loaded_agent_events(&h, "s1")
            .into_iter()
            .filter(|event| matches!(event, Event::AgentUserMessageInjected(_)))
            .count(),
        injected_before
    );
    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(agents("REPLACEMENT")))),
        })),
    )
    .expect("replace AGENTS declaration");

    assert_eq!(h.discovered_agents_files.len(), 1);
    assert_eq!(h.discovered_agents_files[0].content, "REPLACEMENT");
    assert!(source_committed(&h, "discovery-owner", |event| {
        matches!(event, Event::ExtAgentsMdAvailable(agents) if agents.content == "REPLACEMENT")
    }));
    let persisted_injection = h
        .store
        .session("s1")
        .expect("session")
        .loaded_agents()
        .into_iter()
        .flat_map(|agent_id| {
            h.agent_store
                .agent_events(agent_id.as_str())
                .expect("agent events")
        })
        .find(|entry| {
            matches!(
                &entry.event,
                Event::AgentUserMessageInjected(injected)
                    if injected.text.contains("REPLACEMENT")
            )
        })
        .expect("durable replacement injection");
    assert_eq!(persisted_injection.source, None);
}

/// A parked skill emitted before readiness must settle before the later
/// readiness acknowledgement can commit and complete session initialization.
#[test]
fn parked_skill_prevents_readiness_overtake() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_session_discovery_interceptor(&mut h);
    h.initialized_sessions.remove("s1");
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: [tau_proto::ConnectionId::from("discovery-owner")]
            .into_iter()
            .collect(),
    };

    h.handle_extension_event_inner("discovery-owner", skill("ordered", "ordered"))
        .expect("park skill");
    h.handle_extension_event_inner(
        "discovery-owner",
        Event::ExtensionSessionContextReady(tau_proto::ExtensionSessionContextReady {
            session_id: "s1".into(),
        }),
    )
    .expect("queue readiness");
    assert!(matches!(
        h.turn_state,
        TurnState::InitializingSession { .. }
    ));
    assert!(!source_committed(&h, "discovery-owner", |event| {
        matches!(event, Event::ExtensionSessionContextReady(_))
    }));

    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit skill");
    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit readiness");

    assert!(h.discovered_skills.contains_key("ordered"));
    assert!(h.initialized_sessions.contains("s1"));
}

/// A pre-Ready registration reservation must keep the peer handshaking until
/// the committed declaration has staged its membership.
#[test]
fn parked_registration_blocks_ready_activation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("discovery-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Handshaking;
    connect_session_discovery_interceptor(&mut h);

    h.handle_extension_event(
        "discovery-owner",
        TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        )),
    )
    .expect("park registration");
    h.handle_extension_message("discovery-owner", TestMessage::Ready(Default::default()))
        .expect("record Ready");

    assert_eq!(
        h.extensions.entries["discovery-owner"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions
            .pending_session_discovery_declarations
            .get("discovery-owner"),
        Some(&1)
    );
    assert!(
        !h.session_context_providers
            .contains(&tau_proto::ConnectionId::from("discovery-owner"))
    );

    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit registration");

    assert_eq!(
        h.extensions.entries["discovery-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(
        h.session_context_providers
            .contains(&tau_proto::ConnectionId::from("discovery-owner"))
    );
}

/// Session discovery declarations that already committed into a handshaking
/// extension's activation stage keep their admission generation, so a later
/// `Ready` cannot install old provider or skill state after rollover.
#[test]
fn rollover_rejects_already_staged_discovery_declarations_on_ready() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("discovery-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Handshaking;

    h.handle_extension_event(
        "discovery-owner",
        TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        )),
    )
    .expect("stage provider registration");
    h.handle_extension_event(
        "discovery-owner",
        TestProtocolItem::Event(skill("stale-skill", "old-session")),
    )
    .expect("stage skill");
    assert_eq!(
        h.extensions.activation_staging["discovery-owner"].retained_message_count,
        2
    );

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    h.handle_extension_message("discovery-owner", TestMessage::Ready(Default::default()))
        .expect("activate owner");

    assert!(source_committed(&h, "discovery-owner", |event| {
        matches!(event, Event::ExtensionSessionContextProviderRegister(_))
    }));
    assert!(source_committed(&h, "discovery-owner", |event| {
        matches!(event, Event::ExtSkillAvailable(skill) if skill.name == "stale-skill")
    }));
    assert!(
        !h.session_context_providers
            .contains(&tau_proto::ConnectionId::from("discovery-owner"))
    );
    assert!(!h.discovered_skills.contains_key("stale-skill"));
    assert!(
        !h.extensions
            .activation_staging
            .contains_key("discovery-owner")
    );
}

/// Session readiness admitted before Ready keeps its original session
/// generation, so releasing activation after rollover cannot finish the
/// replacement session even when the payload names it.
#[test]
fn pre_ready_session_readiness_after_rollover_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("discovery-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Handshaking;
    h.handle_extension_event(
        "discovery-owner",
        TestProtocolItem::Event(Event::ExtensionSessionContextReady(
            tau_proto::ExtensionSessionContextReady {
                session_id: "replacement".into(),
            },
        )),
    )
    .expect("defer readiness before Ready");

    h.switch_session("replacement".into(), tau_proto::SessionStartReason::New)
        .expect("switch session");
    h.turn_state = TurnState::InitializingSession {
        session_id: "replacement".into(),
        reason: tau_proto::SessionStartReason::New,
        waiting_on: [tau_proto::ConnectionId::from("discovery-owner")]
            .into_iter()
            .collect(),
    };
    h.handle_extension_message("discovery-owner", TestMessage::Ready(Default::default()))
        .expect("activate owner");

    assert!(source_committed(&h, "discovery-owner", |event| {
        matches!(
            event,
            Event::ExtensionSessionContextReady(ready)
                if ready.session_id.as_str() == "replacement"
        )
    }));
    assert!(matches!(
        &h.turn_state,
        TurnState::InitializingSession {
            session_id,
            waiting_on,
            ..
        } if session_id.as_str() == "replacement"
            && waiting_on.contains("discovery-owner")
    ));
}

/// Dropping a pre-Ready session-discovery declaration must release its count
/// and byte reservation so a recorded Ready can activate the peer.
#[test]
fn dropped_startup_registration_releases_reservation_and_ready() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("discovery-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Handshaking;
    connect_session_discovery_interceptor(&mut h);
    h.handle_extension_event(
        "discovery-owner",
        TestProtocolItem::Event(Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        )),
    )
    .expect("park registration");
    h.handle_extension_message("discovery-owner", TestMessage::Ready(Default::default()))
        .expect("record Ready");

    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop registration");

    assert_eq!(
        h.extensions.entries["discovery-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(
        !h.extensions
            .pending_session_discovery_declarations
            .contains_key("discovery-owner")
    );
    assert!(
        !h.session_context_providers
            .contains(&tau_proto::ConnectionId::from("discovery-owner"))
    );
    assert!(
        !h.extensions
            .activation_staging
            .contains_key("discovery-owner")
    );
}

/// An oversized same-name AGENTS.md replacement must fail required startup,
/// settle its reservation, and leave no file projection.
#[test]
fn oversized_startup_agents_replacement_fails_without_projection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    h.initial_extension_tool_preflight_complete = false;
    connect_ready_configured_extension(
        &mut h,
        "discovery-owner",
        "configured-discovery-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("discovery-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Handshaking;
    connect_session_discovery_interceptor(&mut h);
    let agents = |content: String| {
        Event::ExtAgentsMdAvailable(tau_proto::ExtAgentsMdAvailable {
            file_path: "/repo/AGENTS.md".into(),
            content,
        })
    };
    h.handle_extension_event(
        "discovery-owner",
        TestProtocolItem::Event(agents("small".to_owned())),
    )
    .expect("park AGENTS declaration");

    let error = h
        .handle_extension_event(
            "session-discovery-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(agents(
                    "x".repeat(super::super::super::MAX_EXTENSION_ACTIVATION_BYTES),
                )))),
            })),
        )
        .expect_err("oversized replacement must fail required startup");

    assert!(error.to_string().contains("activation staging exceeds"));
    assert!(h.discovered_agents_files.is_empty());
    assert!(
        !h.extensions
            .pending_session_discovery_declarations
            .contains_key("discovery-owner")
    );
    let stage = &h.extensions.activation_staging["discovery-owner"];
    assert_eq!(stage.retained_message_count, 0);
    assert_eq!(stage.retained_message_bytes, 0);
}

/// Every configured client kind may publish discovery observations, while a
/// Hello kind claim without a configured entry grants no authority.
#[test]
fn all_configured_kinds_have_discovery_authority_but_unconfigured_peer_does_not() {
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
        h.handle_extension_event_inner(&source, skill(&format!("kind-{index}"), "kind"))
            .expect("publish configured skill");
        assert!(
            h.discovered_skills
                .contains_key(format!("kind-{index}").as_str())
        );
    }

    connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner("unconfigured", skill("spoofed", "spoofed"))
        .expect("reject unconfigured skill");
    assert!(!h.discovered_skills.contains_key("spoofed"));
}

/// Registration participates in the session barrier only for live non-socket
/// Tool subscribers whose exact or prefix selector matches `session.started`.
#[test]
fn session_wait_set_preserves_tool_and_selector_asymmetry() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cases = [
        (
            "exact-tool",
            tau_proto::ClientKind::Tool,
            EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
            true,
        ),
        (
            "prefix-tool",
            tau_proto::ClientKind::Tool,
            EventSelector::Prefix("session.".to_owned()),
            true,
        ),
        (
            "wrong-prefix-tool",
            tau_proto::ClientKind::Tool,
            EventSelector::Prefix("agent.".to_owned()),
            false,
        ),
        (
            "provider-prefix",
            tau_proto::ClientKind::Provider,
            EventSelector::Prefix("session.".to_owned()),
            false,
        ),
    ];
    for (source, kind, selector, _) in &cases {
        connect_ready_configured_extension(&mut h, source, source, kind.clone());
        h.handle_extension_event(
            source,
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![selector.clone()],
            })),
        )
        .expect("subscribe");
        h.handle_extension_event_inner(
            source,
            Event::ExtensionSessionContextProviderRegister(
                tau_proto::ExtensionSessionContextProviderRegister {},
            ),
        )
        .expect("register session provider");
    }
    connect_ready_configured_extension(
        &mut h,
        "socket-tool",
        "socket-tool",
        tau_proto::ClientKind::Tool,
    );
    h.bus.disconnect("socket-tool");
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: "socket-tool".into(),
            name: "socket-tool".to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(TestSink {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    ));
    h.handle_extension_event(
        "socket-tool",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Prefix("session.".to_owned())],
        })),
    )
    .expect("subscribe socket tool");
    h.handle_extension_event_inner(
        "socket-tool",
        Event::ExtensionSessionContextProviderRegister(
            tau_proto::ExtensionSessionContextProviderRegister {},
        ),
    )
    .expect("reject socket registration");

    let waiting = h.session_init_provider_ids();
    for (source, _, _, expected) in cases {
        assert_eq!(
            waiting.contains(&tau_proto::ConnectionId::from(source)),
            expected,
            "unexpected wait participation for {source}"
        );
    }
    assert!(!waiting.contains(&tau_proto::ConnectionId::from("socket-tool")));
    assert!(
        !h.session_context_providers
            .contains(&tau_proto::ConnectionId::from("socket-tool"))
    );
}

/// An unregistered readiness event remains committed and observable but cannot
/// remove another source from the captured wait set.
#[test]
fn unregistered_readiness_is_observable_without_releasing_barrier() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "observer",
        "configured-observer",
        tau_proto::ClientKind::External,
    );
    h.initialized_sessions.remove("s1");
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: [tau_proto::ConnectionId::from("actual-waiter")]
            .into_iter()
            .collect(),
    };

    h.handle_extension_event_inner(
        "observer",
        Event::ExtensionSessionContextReady(tau_proto::ExtensionSessionContextReady {
            session_id: "s1".into(),
        }),
    )
    .expect("publish readiness");

    assert!(matches!(
        h.turn_state,
        TurnState::InitializingSession { .. }
    ));
    assert!(source_committed(&h, "observer", |event| {
        matches!(event, Event::ExtensionSessionContextReady(_))
    }));
}

/// A declaration parked across publisher disconnect may commit as a stale
/// observation but cannot repopulate discovery for the disconnected generation.
#[test]
fn disconnected_generation_cannot_project_parked_skill() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "old-owner",
        "stable-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("old-owner")
        .expect("old owner")
        .state = crate::extension::ExtensionState::Handshaking;
    connect_session_discovery_interceptor(&mut h);
    h.handle_extension_event(
        "old-owner",
        TestProtocolItem::Event(skill("stale-skill", "stale")),
    )
    .expect("park skill");
    assert_eq!(
        h.extensions
            .pending_session_discovery_declarations
            .get("old-owner"),
        Some(&1)
    );

    h.handle_disconnect("old-owner");
    assert!(
        !h.extensions
            .pending_session_discovery_declarations
            .contains_key("old-owner")
    );
    connect_ready_configured_extension(
        &mut h,
        "new-owner",
        "stable-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("new-owner")
        .expect("new owner")
        .state = crate::extension::ExtensionState::Handshaking;
    h.handle_extension_event(
        "new-owner",
        TestProtocolItem::Event(skill("stale-skill", "current")),
    )
    .expect("queue current skill");
    h.handle_extension_message("new-owner", TestMessage::Ready(Default::default()))
        .expect("record successor Ready");
    let successor_stage = &h.extensions.activation_staging["new-owner"];
    let successor_count = successor_stage.retained_message_count;
    let successor_bytes = successor_stage.retained_message_bytes;
    assert_eq!(
        h.extensions
            .pending_session_discovery_declarations
            .get("new-owner"),
        Some(&1)
    );
    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale skill");

    assert!(source_committed(&h, "old-owner", |event| {
        matches!(event, Event::ExtSkillAvailable(skill) if skill.name == "stale-skill")
    }));
    assert!(!h.discovered_skills.contains_key("stale-skill"));
    assert_eq!(
        h.extensions.entries["new-owner"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert_eq!(
        h.extensions
            .pending_session_discovery_declarations
            .get("new-owner"),
        Some(&1)
    );
    assert_eq!(
        h.extensions.activation_staging["new-owner"].retained_message_count,
        successor_count
    );
    assert_eq!(
        h.extensions.activation_staging["new-owner"].retained_message_bytes,
        successor_bytes
    );
    h.handle_extension_event(
        "session-discovery-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit current skill");
    assert_eq!(
        h.extensions.entries["new-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert_eq!(h.discovered_skills["stale-skill"].description, "current");
}

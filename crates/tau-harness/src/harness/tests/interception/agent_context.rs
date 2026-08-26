//! Contract tests for `SPEC-per-agent-context-declarations-and-readiness`.

use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Build one per-agent context value with an easily inspected payload.
fn context(agent_id: &str, value: &str) -> Event {
    Event::ExtAgentContextPublish(tau_proto::ExtAgentContextPublish {
        session_id: tau_proto::SessionId::parse("s1").expect("known-safe SessionId must be valid"),
        agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
            .expect("test identifier must be valid"),

        agent_id: tau_proto::AgentId::parse(agent_id).expect("agent id"),
        key: "test".into(),
        value: tau_proto::AgentContextValue(serde_json::json!(value)),
    })
}

/// Register one interceptor for the complete per-agent context event family.
fn connect_agent_context_interceptor(h: &mut Harness) {
    connect_test_tool(h, "agent-context-interceptor");
    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::EXTENSION_CONTEXT_PROVIDER_REGISTER),
                EventSelector::Exact(tau_proto::EventName::EXTENSION_AGENT_CONTEXT_PUBLISH),
                EventSelector::Exact(tau_proto::EventName::EXTENSION_CONTEXT_READY),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
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

/// Dropping a value declaration must prevent both stream observation and prompt
/// projection mutation.
#[test]
fn dropped_context_value_has_no_projection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Action,
    );
    connect_agent_context_interceptor(&mut h);
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");

    h.handle_extension_event_inner(
        &crate::test_connection_id("context-owner"),
        context("agent-1", &crate::test_connection_id("dropped")),
    )
    .expect("park context");
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id)),
        serde_json::json!({})
    );
    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop context");

    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id)),
        serde_json::json!({})
    );
    assert!(!source_committed(&h, "context-owner", |event| {
        matches!(event, Event::ExtAgentContextPublish(_))
    }));
}

/// A same-name interceptor replacement must become the committed observation
/// but an uncorrelated unloaded target cannot mutate prompt projection.
#[test]
fn uncorrelated_context_replacement_is_observable_but_not_projected() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Provider,
    );
    connect_agent_context_interceptor(&mut h);
    let agent_id = tau_proto::AgentId::parse("agent-unloaded").expect("agent id");

    h.handle_extension_event_inner(
        &crate::test_connection_id("context-owner"),
        context("agent-unloaded", &crate::test_connection_id("original")),
    )
    .expect("park context");
    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(context("agent-unloaded", "replacement")))),
        })),
    )
    .expect("replace context");

    let projected = h
        .prompt_coordination
        .context_discovery
        .agent_context
        .template_value(Some(&agent_id))
        .to_string();
    assert!(!projected.contains("replacement"));
    assert!(!projected.contains("original"));
    assert!(source_committed(&h, "context-owner", |event| {
        matches!(
            event,
            Event::ExtAgentContextPublish(publish)
                if publish.value.0 == serde_json::json!("replacement")
        )
    }));
}

/// FIFO generic publication must settle a context value before the later
/// readiness acknowledgement can release its per-agent dispatch barrier.
#[test]
fn parked_context_value_prevents_readiness_overtake() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_agent_context_interceptor(&mut h);
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        [tau_proto::ConnectionId::parse("context-owner")
            .expect("test connection id must satisfy the identifier grammar")]
        .into_iter()
        .collect(),
    );

    h.handle_extension_event_inner(
        &crate::test_connection_id("context-owner"),
        context("agent-1", &crate::test_connection_id("ordered")),
    )
    .expect("park context");
    h.handle_extension_event_inner(
        &crate::test_connection_id("context-owner"),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
        }),
    )
    .expect("queue readiness");
    assert!(
        h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent_id)
    );

    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit context value");
    assert!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id))
            .to_string()
            .contains("ordered")
    );
    assert!(
        h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent_id),
        "readiness must remain effect-free while its own interception is pending"
    );
    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit readiness");

    assert!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id))
            .to_string()
            .contains("ordered")
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent_id)
    );
}

/// Configured client kind alone cannot bypass initialization correlation.
#[test]
fn configured_kinds_cannot_publish_context_for_arbitrary_agents() {
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
        let agent = format!("agent-{}", index + 10);
        connect_ready_configured_extension(&mut h, &source, &source, kind);
        h.handle_extension_event_inner(
            &crate::test_connection_id(&source),
            context(&agent, &source),
        )
        .expect("publish configured context");
        assert_eq!(
            h.prompt_coordination
                .context_discovery
                .agent_context
                .template_value(Some(&tau_proto::AgentId::parse(&agent).expect("agent id"))),
            serde_json::json!({})
        );
        assert!(source_committed(&h, &source, |event| {
            matches!(
                event,
                Event::ExtAgentContextPublish(publish)
                    if publish.value.0 == serde_json::json!(source)
            )
        }));
    }

    connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner(
        &crate::test_connection_id("unconfigured"),
        context("agent-99", &crate::test_connection_id("spoofed")),
    )
    .expect("reject unconfigured context");
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(
                &tau_proto::AgentId::parse("agent-99").expect("agent id")
            )),
        serde_json::json!({})
    );
    assert!(!source_committed(&h, "unconfigured", |event| {
        matches!(event, Event::ExtAgentContextPublish(_))
    }));

    connect_ready_configured_extension(
        &mut h,
        "socket-origin",
        "socket-origin",
        tau_proto::ClientKind::Tool,
    );
    h.runtime_io
        .bus
        .disconnect(&crate::test_connection_id("socket-origin"));
    h.runtime_io.bus.connect(Connection::new(
        PendingConnectionMetadata {
            id: Some(crate::test_connection_id("socket-origin")),
            name: crate::test_extension_name("socket-origin"),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(TestSink {
            events: Arc::new(Mutex::new(Vec::new())),
        }),
    ));
    h.handle_extension_event_inner(
        &crate::test_connection_id("socket-origin"),
        context("agent-100", &crate::test_connection_id("socket")),
    )
    .expect("reject socket-origin context");
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(
                &tau_proto::AgentId::parse("agent-100").expect("agent id")
            )),
        serde_json::json!({})
    );
    assert!(!source_committed(&h, "socket-origin", |event| {
        matches!(event, Event::ExtAgentContextPublish(_))
    }));
}

/// A pre-Ready registration reservation must hold activation until interception
/// commits and stages provider membership.
#[test]
fn parked_registration_blocks_ready_activation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("context-owner")
        .expect("owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    connect_agent_context_interceptor(&mut h);

    h.handle_extension_event(
        "context-owner",
        TestProtocolItem::Event(Event::ExtensionContextProviderRegister(
            tau_proto::ExtensionContextProviderRegister {},
        )),
    )
    .expect("park registration");
    h.handle_extension_message(
        &crate::test_connection_id("context-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("record Ready");
    assert_eq!(
        h.extensions.entries["context-owner"].state,
        crate::extension::ExtensionState::Handshaking
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .agent_context_providers
            .contains(
                &tau_proto::ConnectionId::parse("context-owner")
                    .expect("test connection id must satisfy the identifier grammar")
            ),
        "registration must remain effect-free while interception is pending"
    );
    assert_eq!(
        h.extensions
            .pending_agent_context_declarations
            .get("context-owner"),
        Some(&1)
    );

    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit registration");
    assert_eq!(
        h.extensions.entries["context-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(
        h.prompt_coordination
            .context_discovery
            .agent_context_providers
            .contains(
                &tau_proto::ConnectionId::parse("context-owner")
                    .expect("test connection id must satisfy the identifier grammar")
            )
    );
}

/// Rollover commits a parked pre-Ready registration only as a stale
/// observation, releases its nonzero activation reservation, and lets the
/// already-recorded Ready complete without installing provider membership.
#[test]
fn rollover_releases_stale_context_registration_reservation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("context-owner")
        .expect("owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    connect_agent_context_interceptor(&mut h);
    h.handle_extension_event(
        "context-owner",
        TestProtocolItem::Event(Event::ExtensionContextProviderRegister(
            tau_proto::ExtensionContextProviderRegister {},
        )),
    )
    .expect("park registration");
    h.handle_extension_message(
        &crate::test_connection_id("context-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("record Ready");
    let stage = &h.extensions.activation_staging["context-owner"];
    assert_eq!(stage.retained_message_count, 1);
    assert!(stage.retained_message_bytes > 0);

    h.switch_session(
        "replacement"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");

    assert!(source_committed(&h, "context-owner", |event| {
        matches!(event, Event::ExtensionContextProviderRegister(_))
    }));
    assert_eq!(
        h.extensions.entries["context-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(
        !h.extensions
            .pending_agent_context_declarations
            .contains_key("context-owner")
    );
    assert!(
        !h.extensions
            .activation_staging
            .contains_key("context-owner")
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .agent_context_providers
            .contains(
                &tau_proto::ConnectionId::parse("context-owner")
                    .expect("test connection id must satisfy the identifier grammar")
            )
    );
}

/// Session-bound context declarations that already committed into a
/// handshaking extension's activation stage retain their admission generation,
/// so later `Ready` cannot project them into a replacement session.
#[test]
fn rollover_rejects_already_staged_context_declarations_on_ready() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Core,
    );
    h.extensions
        .entries
        .get_mut("context-owner")
        .expect("owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    let agent_id = tau_proto::AgentId::parse("stale-context-agent").expect("agent id");

    h.handle_extension_event(
        "context-owner",
        TestProtocolItem::Event(Event::ExtensionContextProviderRegister(
            tau_proto::ExtensionContextProviderRegister {},
        )),
    )
    .expect("stage provider registration");
    h.handle_extension_event(
        "context-owner",
        TestProtocolItem::Event(context(agent_id.as_str(), "old-session")),
    )
    .expect("stage context value");
    assert_eq!(
        h.extensions.activation_staging["context-owner"].retained_message_count,
        2
    );

    h.switch_session(
        "replacement"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    h.handle_extension_message(
        &crate::test_connection_id("context-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("activate owner");

    assert!(source_committed(&h, "context-owner", |event| {
        matches!(event, Event::ExtensionContextProviderRegister(_))
    }));
    assert!(source_committed(&h, "context-owner", |event| {
        matches!(event, Event::ExtAgentContextPublish(publish) if publish.agent_id == agent_id)
    }));
    assert!(
        !h.prompt_coordination
            .context_discovery
            .agent_context_providers
            .contains(
                &tau_proto::ConnectionId::parse("context-owner")
                    .expect("test connection id must satisfy the identifier grammar")
            )
    );
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id)),
        serde_json::json!({})
    );
    assert!(
        !h.extensions
            .activation_staging
            .contains_key("context-owner")
    );
}

/// Dropping a pre-Ready value declaration must release its activation charge
/// and allow an already-recorded Ready to activate without projecting the
/// value.
#[test]
fn dropped_startup_context_releases_reservation_and_ready() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("context-owner")
        .expect("owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    connect_agent_context_interceptor(&mut h);

    h.handle_extension_event_inner(
        &crate::test_connection_id("context-owner"),
        context("agent-1", &crate::test_connection_id("dropped")),
    )
    .expect("park context");
    h.handle_extension_message(
        &crate::test_connection_id("context-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("record Ready");
    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop context");

    assert_eq!(
        h.extensions.entries["context-owner"].state,
        crate::extension::ExtensionState::Ready
    );
    assert!(
        !h.extensions
            .pending_agent_context_declarations
            .contains_key("context-owner")
    );
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(
                &tau_proto::AgentId::parse("agent-1").expect("agent id")
            )),
        serde_json::json!({})
    );
}

/// Per-agent readiness admitted before Ready keeps its original session
/// generation, so releasing activation after rollover cannot clear a
/// replacement-session dispatch barrier even when the payload names it.
#[test]
fn pre_ready_context_readiness_after_rollover_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("context-owner")
        .expect("owner")
        .state = path_crate_extension::ExtensionState::Handshaking;
    let agent_id = tau_proto::AgentId::parse("replacement-agent").expect("agent id");
    h.handle_extension_event(
        "context-owner",
        TestProtocolItem::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: "replacement"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                agent_id: agent_id.clone(),
            },
        )),
    )
    .expect("defer readiness before Ready");

    h.switch_session(
        "replacement"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        [tau_proto::ConnectionId::parse("context-owner")
            .expect("test connection id must satisfy the identifier grammar")]
        .into_iter()
        .collect(),
    );
    h.handle_extension_message(
        &crate::test_connection_id("context-owner"),
        TestMessage::Ready(Default::default()),
    )
    .expect("activate owner");

    assert!(source_committed(&h, "context-owner", |event| {
        matches!(
            event,
            Event::ExtensionContextReady(ready)
                if ready.session_id.as_str() == "replacement"
                    && ready.agent_id == agent_id
        )
    }));
    assert_eq!(
        test_agent_context_waits(&h, &agent_id),
        Some(
            &[tau_proto::ConnectionId::parse("context-owner")
                .expect("test connection id must satisfy the identifier grammar")]
            .into_iter()
            .collect()
        )
    );
}

/// Per-agent readiness releases only its exact correlated initialization.
#[test]
fn context_ready_does_not_release_session_wait() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Tool,
    );
    let source = tau_proto::ConnectionId::parse("context-owner")
        .expect("test connection id must satisfy the identifier grammar");
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    h.prompt_coordination
        .context_discovery
        .initialized_sessions
        .remove("s1");
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: [source.clone()].into_iter().collect(),
    };
    set_test_agent_context_wait(&mut h, agent_id.clone(), [source].into_iter().collect());

    h.handle_extension_event_inner(
        &crate::test_connection_id("context-owner"),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
        }),
    )
    .expect("publish readiness");

    assert!(
        !h.prompt_coordination
            .context_discovery
            .initialized_sessions
            .contains("s1")
    );
    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent_id)
    );
}

/// A dropped or wrong-session readiness observation must not release either
/// compatibility wait.
#[test]
fn dropped_and_mismatched_context_ready_are_effect_free() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "context-owner",
        "configured-context-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_agent_context_interceptor(&mut h);
    let source = tau_proto::ConnectionId::parse("context-owner")
        .expect("test connection id must satisfy the identifier grammar");
    let agent_id = tau_proto::AgentId::parse("agent-1").expect("agent id");
    h.session_runtime.turn_state = TurnState::InitializingSession {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: [source.clone()].into_iter().collect(),
    };
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        [source.clone()].into_iter().collect(),
    );
    let ready = |session_id: &str| {
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                .expect("test identifier must be valid"),

            session_id: session_id
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
        })
    };

    h.handle_extension_event_inner(&crate::test_connection_id("context-owner"), ready("s1"))
        .expect("park readiness");
    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop readiness");
    assert!(
        h.prompt_coordination.context_discovery.pending_agents[&agent_id]
            .waiting_on
            .contains(&source)
    );
    assert!(matches!(
        &h.session_runtime.turn_state,
        TurnState::InitializingSession { waiting_on, .. } if waiting_on.contains(&source)
    ));

    h.handle_extension_event_inner(
        &crate::test_connection_id("context-owner"),
        ready("other-session"),
    )
    .expect("park mismatched readiness");
    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit mismatched readiness");
    assert!(
        h.prompt_coordination.context_discovery.pending_agents[&agent_id]
            .waiting_on
            .contains(&source)
    );
    assert!(matches!(
        &h.session_runtime.turn_state,
        TurnState::InitializingSession { waiting_on, .. } if waiting_on.contains(&source)
    ));
}

/// A parked old-generation declaration may commit for observation after
/// disconnect but cannot mutate the successor or recreate removed context.
#[test]
fn disconnected_generation_cannot_project_parked_context() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "old-owner",
        "stable-owner",
        tau_proto::ClientKind::Tool,
    );
    connect_agent_context_interceptor(&mut h);
    h.handle_extension_event_inner(
        &crate::test_connection_id("old-owner"),
        context("agent-1", &crate::test_connection_id("stale")),
    )
    .expect("park stale context");
    h.handle_disconnect(&crate::test_connection_id("old-owner"));
    connect_ready_configured_extension(
        &mut h,
        "new-owner",
        "stable-owner",
        tau_proto::ClientKind::Tool,
    );

    h.handle_extension_event(
        "agent-context-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale context");

    assert!(source_committed(&h, "old-owner", |event| {
        matches!(event, Event::ExtAgentContextPublish(_))
    }));
    assert_eq!(
        h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(
                &tau_proto::AgentId::parse("agent-1").expect("agent id")
            )),
        serde_json::json!({})
    );
    assert!(
        !h.extensions
            .pending_agent_context_declarations
            .contains_key("new-owner")
    );
}

/// Disconnecting an active interceptor must remove its context before passing a
/// parked readiness event that can synchronously freeze a prompt snapshot.
#[test]
fn interceptor_disconnect_removes_context_before_readiness_dispatch() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    h.config.selected_model = Some("test/model".into());
    connect_ready_configured_extension(&mut h, "waiter", "waiter", tau_proto::ClientKind::Tool);
    h.handle_extension_message(
        &crate::test_connection_id("waiter"),
        TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
        }),
    )
    .expect("subscribe waiter");
    h.handle_extension_event_inner(
        &crate::test_connection_id("waiter"),
        Event::ExtensionContextProviderRegister(tau_proto::ExtensionContextProviderRegister {}),
    )
    .expect("register waiter");
    connect_ready_configured_extension(
        &mut h,
        "stale-owner",
        "stale-owner",
        tau_proto::ClientKind::Action,
    );
    h.handle_extension_event(
        "stale-owner",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::EXTENSION_CONTEXT_READY,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept readiness");
    connect_ready_configured_extension(
        &mut h,
        "fragment-owner",
        "fragment-owner",
        tau_proto::ClientKind::Core,
    );
    h.handle_extension_event_inner(
        &crate::test_connection_id("fragment-owner"),
        Event::ExtPromptFragmentPublish(tau_proto::ExtPromptFragmentPublish {
            fragment: tau_proto::PromptFragment::new(
                "stale-context-check",
                tau_proto::PromptPriority::new(50),
                "{{#each agent_context.test}}{{value}}{{/each}}",
            ),
        }),
    )
    .expect("publish stable fragment");

    h.dispatch_user_prompt(
        "s1".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        "dispatch after readiness".to_owned(),
    )
    .expect("dispatch prompt");
    let agent_id = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find_map(|agent| agent.identity.agent_id.clone())
        .map(|agent_id| tau_proto::AgentId::parse(&agent_id).expect("agent id"))
        .expect("loaded agent");
    let initialization_id = h.prompt_coordination.context_discovery.pending_agents[&agent_id]
        .initialization_id
        .clone();
    let correlated_context = |value: &str| {
        Event::ExtAgentContextPublish(tau_proto::ExtAgentContextPublish {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
            agent_initialization_id: initialization_id.clone(),
            key: "test".into(),
            value: tau_proto::AgentContextValue(serde_json::json!(value)),
        })
    };
    h.handle_extension_event_inner(
        &crate::test_connection_id("fragment-owner"),
        correlated_context("SAFE CONTEXT"),
    )
    .expect("publish stable context");
    h.handle_extension_event_inner(
        &crate::test_connection_id("stale-owner"),
        correlated_context("STALE DISCONNECTING CONTEXT"),
    )
    .expect("publish stale context");
    h.handle_extension_event_inner(
        &crate::test_connection_id("waiter"),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            agent_initialization_id: initialization_id,

            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id.clone(),
        }),
    )
    .expect("park waiter readiness");
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    assert!(
        h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent_id)
    );

    h.handle_disconnect(&crate::test_connection_id("stale-owner"));

    assert!(
        !h.prompt_coordination
            .context_discovery
            .pending_agents
            .contains_key(&agent_id)
    );
    assert!(h.runtime_io.publication.pending_intercept.is_none());
    assert!(
        h.runtime_io.publication.idle_dispatches.is_empty(),
        "readiness resolution must drain deferred prompt dispatch"
    );
    let prompt = read_nth_prompt_created(&h, 0);
    assert!(prompt.system_prompt.contains("SAFE CONTEXT"));
    assert!(!prompt.system_prompt.contains("STALE DISCONNECTING CONTEXT"));
    assert!(
        !h.prompt_coordination
            .context_discovery
            .agent_context
            .template_value(Some(&agent_id))
            .to_string()
            .contains("STALE DISCONNECTING CONTEXT")
    );
}

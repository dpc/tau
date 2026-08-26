//! Contract tests for `SPEC-agent-metadata-requests-and-canonical-facts`.

use super::*;
use crate::event_log as path_crate_event_log;

/// Build one metadata-set request with an optional commit correlation.
fn set_request(agent_id: &tau_proto::AgentId, value: &str, mutation_id: Option<&str>) -> Event {
    Event::AgentMetadataSetRequest(tau_proto::AgentMetadataSet {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("ext_test_value"),
        value: CborValue::Text(value.to_owned()),
        mutation_id: mutation_id
            .map(|id| tau_proto::AgentMetadataMutationId::parse(id).expect("valid mutation id")),
        inheritable: true,
    })
}

/// Build one metadata-unset request for the test-owned key.
fn unset_request(agent_id: &tau_proto::AgentId) -> Event {
    Event::AgentMetadataUnsetRequest(tau_proto::AgentMetadataUnset {
        agent_id: agent_id.clone(),
        key: tau_proto::AgentMetadataKey::new("ext_test_value"),
    })
}

/// Collect metadata request and canonical commits with their delivery sources.
fn metadata_commits(h: &Harness) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut commits = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if matches!(
            entry.event,
            Event::AgentMetadataSetRequest(_)
                | Event::AgentMetadataUnsetRequest(_)
                | Event::AgentMetadataSet(_)
                | Event::AgentMetadataUnset(_)
        ) {
            commits.push((entry.source, entry.event));
        }
    }
    commits
}

/// A configured extension request commits before a separate harness-authored
/// canonical fact carrying the validated payload.
#[test]
fn configured_extension_request_precedes_canonical_fact() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Action,
    );

    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("requester"),
        set_request(&agent_id, "accepted", None),
        Some(true),
    )
    .expect("publish metadata request");
    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        unset_request(&agent_id),
    )
    .expect("publish metadata unset request");

    let commits = metadata_commits(&h);
    assert!(matches!(
        commits.as_slice(),
        [
            (Some(request_source), Event::AgentMetadataSetRequest(request)),
            (Some(canonical_source), Event::AgentMetadataSet(canonical)),
            (Some(unset_request_source), Event::AgentMetadataUnsetRequest(unset_request)),
            (Some(unset_canonical_source), Event::AgentMetadataUnset(unset_canonical)),
        ] if request_source.as_str() == "requester"
            && canonical_source.as_str() == HARNESS_CONNECTION_ID
            && request == canonical
            && unset_request_source.as_str() == "requester"
            && unset_canonical_source.as_str() == HARNESS_CONNECTION_ID
            && unset_request == unset_canonical
    ));
}

/// A metadata request active in interception at rollover keeps its raw
/// observation but cannot mutate replacement-session metadata from stale
/// admission.
#[test]
fn rollover_metadata_request_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Action,
    );
    connect_test_tool(&mut h, "metadata-interceptor");
    h.handle_extension_event(
        "metadata-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET_REQUEST,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
    h.handle_extension_event_inner_with_persist(
        &crate::test_connection_id("requester"),
        set_request(&agent_id, "stale rollover", None),
        Some(true),
    )
    .expect("park metadata request");

    h.switch_session(
        "replacement"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");

    assert!(matches!(
        metadata_commits(&h).as_slice(),
        [(Some(source), Event::AgentMetadataSetRequest(request))]
            if source == "requester"
                && request.value == CborValue::Text("stale rollover".to_owned())
    ));
}

/// Configured extensions and attached socket UI peers have request authority,
/// while unconfigured/non-UI peers and peer-authored canonical facts do not.
#[test]
fn metadata_authority_is_exact_and_canonical_facts_are_harness_owned() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());

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
        let value = format!("configured-{index}");
        connect_ready_configured_extension(&mut h, &source, &source, kind);
        h.handle_extension_event_inner(
            &crate::test_connection_id(&source),
            set_request(&agent_id, &value, None),
        )
        .expect("configured extension request");
    }

    connect_test_client_with_origin(
        &mut h,
        "ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
        set_request(&agent_id, &crate::test_connection_id("ui"), None),
    )
    .expect("attached UI request");

    connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner(
        &crate::test_connection_id("unconfigured"),
        set_request(&agent_id, &crate::test_connection_id("unconfigured"), None),
    )
    .expect("reject unconfigured request");
    connect_test_client_with_origin(
        &mut h,
        "socket-tool",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    h.handle_client_event_inner(
        &crate::test_connection_id("socket-tool"),
        set_request(&agent_id, &crate::test_connection_id("socket"), None),
    )
    .expect("reject non-UI socket");
    h.handle_extension_event_inner(
        &crate::test_connection_id("configured-4"),
        Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
            agent_id,
            key: tau_proto::AgentMetadataKey::new("ext_test_value"),
            value: CborValue::Text("spoofed canonical".to_owned()),
            mutation_id: None,
            inheritable: true,
        }),
    )
    .expect("reject canonical spoof");

    let commits = metadata_commits(&h);
    for index in 0..6 {
        let expected = format!("configured-{index}");
        assert!(commits.iter().any(|(source, event)| {
            source.as_deref() == Some(expected.as_str())
                && matches!(event, Event::AgentMetadataSetRequest(set)
                    if matches!(&set.value, CborValue::Text(value) if value == &expected))
        }));
    }
    assert!(commits.iter().any(|(source, event)| {
        source.as_deref() == Some("ui")
            && matches!(event, Event::AgentMetadataSetRequest(set)
                if matches!(&set.value, CborValue::Text(value) if value == "ui"))
    }));
    assert!(commits.iter().any(|(source, event)| {
        source.as_deref() == Some(HARNESS_CONNECTION_ID)
            && matches!(event, Event::AgentMetadataSet(set)
                if matches!(&set.value, CborValue::Text(value) if value == "ui"))
    }));
    assert!(!commits.iter().any(|(_, event)| {
        matches!(
            event,
            Event::AgentMetadataSetRequest(set) | Event::AgentMetadataSet(set)
                if matches!(&set.value, CborValue::Text(value)
                    if value == "unconfigured" || value == "socket" || value == "spoofed canonical")
        )
    }));
}

/// A request parked across publisher disconnect remains observable but cannot
/// mutate metadata for a stale configured-extension generation.
#[test]
fn stale_extension_request_has_no_canonical_successor() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET_REQUEST,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        set_request(&agent_id, &crate::test_connection_id("stale"), None),
    )
    .expect("park request");
    h.handle_disconnect(&crate::test_connection_id("requester"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale request");

    let commits = metadata_commits(&h);
    assert!(
        commits
            .iter()
            .any(|(_, event)| matches!(event, Event::AgentMetadataSetRequest(_)))
    );
    assert!(
        !commits
            .iter()
            .any(|(_, event)| matches!(event, Event::AgentMetadataSet(_)))
    );
}

/// A disconnected UI cannot turn a previously parked request into a canonical
/// mutation after it ceases to be attached.
#[test]
fn disconnected_ui_request_has_no_canonical_successor() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());
    connect_test_client_with_origin(
        &mut h,
        "ui",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET_REQUEST,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_client_event_inner(
        &crate::test_connection_id("ui"),
        set_request(&agent_id, &crate::test_connection_id("stale-ui"), None),
    )
    .expect("park UI request");
    h.runtime_io
        .bus
        .disconnect(&crate::test_connection_id("ui"));
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale UI request");

    let commits = metadata_commits(&h);
    assert!(
        commits
            .iter()
            .any(|(_, event)| matches!(event, Event::AgentMetadataSetRequest(_)))
    );
    assert!(
        !commits
            .iter()
            .any(|(_, event)| matches!(event, Event::AgentMetadataSet(_)))
    );
}

/// Invalid metadata requests remain committed observations but preserve the
/// established silent-rejection behavior by producing no canonical successor.
#[test]
fn invalid_request_commits_without_canonical_successor() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Provider,
    );
    let unknown = tau_proto::AgentId::parse("unknown-agent").expect("agent id");

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        set_request(&unknown, &crate::test_connection_id("invalid"), None),
    )
    .expect("commit invalid request");

    let commits = metadata_commits(&h);
    assert!(matches!(
        commits.as_slice(),
        [(Some(source), Event::AgentMetadataSetRequest(_))]
            if source.as_str() == "requester"
    ));
}

/// An invalid uncorrelated interception replacement commits as the observed
/// request and fails only at downstream validation; the original is not
/// applied.
#[test]
fn invalid_request_replacement_commits_without_original_mutation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Action,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET_REQUEST,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        set_request(&agent_id, &crate::test_connection_id("original"), None),
    )
    .expect("park request");
    let replacement = tau_proto::AgentMetadataSet {
        agent_id: tau_proto::AgentId::parse("unknown-agent").expect("agent id"),
        key: tau_proto::AgentMetadataKey::new("ext_test_value"),
        value: CborValue::Text("replacement".to_owned()),
        mutation_id: None,
        inheritable: true,
    };
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::AgentMetadataSetRequest(
                replacement.clone(),
            )))),
        })),
    )
    .expect("replace request");

    let commits = metadata_commits(&h);
    assert!(matches!(
        commits.as_slice(),
        [(Some(source), Event::AgentMetadataSetRequest(committed))]
            if source.as_str() == "requester" && committed == &replacement
    ));
}

/// A correlated request cannot be dropped or retargeted by interception, so
/// core-shell-style setters still receive the matching canonical commit echo.
#[test]
fn tokened_request_preserves_identity_and_reaches_canonical_echo() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let agent_id = tau_proto::AgentId::parse("metadata-agent").expect("agent id");
    h.agent_runtime
        .agent_registry
        .session_loaded
        .insert(agent_id.clone());
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Core,
    );
    connect_test_tool(&mut h, "interceptor");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_METADATA_SET_REQUEST,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        set_request(&agent_id, "original", Some("mutation-1")),
    )
    .expect("park tokened request");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("attempt request drop");

    let commits = metadata_commits(&h);
    assert!(commits.iter().any(|(_, event)| matches!(
        event,
        Event::AgentMetadataSet(set)
            if set.agent_id == agent_id
                && set.key.as_str() == "ext_test_value"
                && set.inheritable
                && set.mutation_id.as_ref().is_some_and(|id| id.as_str() == "mutation-1")
    )));

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        set_request(&agent_id, "replace-original", Some("mutation-2")),
    )
    .expect("park replacement request");
    h.handle_extension_event(
        "interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::AgentMetadataSetRequest(
                tau_proto::AgentMetadataSet {
                    agent_id: tau_proto::AgentId::parse("retargeted").expect("agent id"),
                    key: tau_proto::AgentMetadataKey::new("retargeted-key"),
                    value: CborValue::Text("rewritten".to_owned()),
                    mutation_id: Some(
                        tau_proto::AgentMetadataMutationId::parse("retargeted-mutation")
                            .expect("mutation id"),
                    ),
                    inheritable: false,
                },
            )))),
        })),
    )
    .expect("replace tokened request");
    let commits = metadata_commits(&h);
    assert!(commits.iter().any(|(_, event)| matches!(
        event,
        Event::AgentMetadataSet(set)
            if set.agent_id == agent_id
                && set.key.as_str() == "ext_test_value"
                && matches!(&set.value, CborValue::Text(value) if value == "rewritten")
                && set.inheritable
                && set.mutation_id.as_ref().is_some_and(|id| id.as_str() == "mutation-2")
    )));
}

//! Contract tests for `SPEC-internal-prompt-submit-requests`.

use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Build one internal-prompt request with visible text and correlation.
fn request(agent_id: &tau_proto::AgentId, text: &str) -> Event {
    Event::ExtInternalPromptSubmitRequest(tau_proto::ExtInternalPromptSubmitRequest {
        agent_id: agent_id.clone(),
        text: text.to_owned(),
        ctx_id: Some(format!("ctx:{text}")),
        activation_kind: None,
    })
}

/// Register one interceptor for internal-prompt requests.
fn connect_internal_prompt_interceptor(h: &mut Harness) {
    connect_test_tool(h, "internal-prompt-interceptor");
    h.handle_extension_event(
        "internal-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::EXTENSION_INTERNAL_PROMPT_SUBMIT_REQUEST,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
}

/// Return whether one source committed an event matching the predicate.
fn source_committed(h: &Harness, source: &str, predicate: impl Fn(&Event) -> bool) -> bool {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if entry.source.as_deref() == Some(source) && predicate(&entry.event) {
            return true;
        }
    }
    false
}

/// Return whether any committed event matches the predicate.
fn event_log_contains_any_source(h: &Harness, predicate: impl Fn(&Event) -> bool) -> bool {
    event_log_events(h).iter().any(predicate)
}

/// Return the first committed event matching the predicate.
fn first_committed_matching(
    h: &Harness,
    predicate: impl Fn(&Event) -> bool,
) -> crate::event_log::LogEntry {
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if predicate(&entry.event) {
            return entry;
        }
    }
    panic!("matching committed event");
}

/// Interception drop must prevent both the raw observation and harness-owned
/// prompt submission.
#[test]
fn dropped_request_does_not_submit_prompt() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    connect_internal_prompt_interceptor(&mut h);

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request(&agent_id, &crate::test_connection_id("drop-me")),
    )
    .expect("park request");
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "drop me")
    }));
    h.handle_extension_event(
        "internal-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop request");

    assert!(!source_committed(&h, "requester", |event| {
        matches!(event, Event::ExtInternalPromptSubmitRequest(_))
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "drop me")
    }));
}

/// A same-name replacement is revalidated and only its committed payload may
/// become the hidden harness-owned prompt fact.
#[test]
fn replacement_submits_only_committed_payload() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Provider,
    );
    connect_internal_prompt_interceptor(&mut h);

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request(&agent_id, &crate::test_connection_id("original")),
    )
    .expect("park request");
    h.handle_extension_event(
        "internal-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(request(&agent_id, "replacement")))),
        })),
    )
    .expect("replace request");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::ExtInternalPromptSubmitRequest(request)
                if request.text == "replacement"
        )
    }));
    assert!(event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::AgentPromptSubmitted(prompt)
                if prompt.text == "replacement"
                    && prompt.message_class == tau_proto::PromptMessageClass::Internal
                    && prompt.ctx_id.as_deref() == Some("ctx:replacement")
        )
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "original")
    }));
    let raw = first_committed_matching(
        &h,
        |event| matches!(event, Event::ExtInternalPromptSubmitRequest(request) if request.text == "replacement"),
    );
    let canonical = first_committed_matching(
        &h,
        |event| matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "replacement"),
    );
    assert_eq!(raw.source.as_deref(), Some("requester"));
    assert_eq!(canonical.source, None);
    assert!(raw.seq < canonical.seq);
}

/// Invalid targets remain committed request observations, then produce the
/// existing diagnostic without a canonical prompt fact.
#[test]
fn invalid_target_commits_before_rejection_diagnostic() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Core,
    );
    let missing = tau_proto::AgentId::parse("missing-agent").expect("agent id");

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request(&missing, &crate::test_connection_id("invalid")),
    )
    .expect("commit invalid request");

    assert!(source_committed(&h, "requester", |event| {
        matches!(event, Event::ExtInternalPromptSubmitRequest(request) if request.text == "invalid")
    }));
    assert!(event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.message.contains("unknown or unloaded agent")
        )
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "invalid")
    }));
    let raw = first_committed_matching(
        &h,
        |event| matches!(event, Event::ExtInternalPromptSubmitRequest(request) if request.text == "invalid"),
    );
    let rejection = first_committed_matching(&h, |event| {
        matches!(
            event,
            Event::HarnessNotice(notice)
                if notice.message.contains("unknown or unloaded agent")
        )
    });
    assert_eq!(raw.source.as_deref(), Some("requester"));
    assert_eq!(rejection.source.as_deref(), Some(HARNESS_CONNECTION_ID));
    assert!(raw.seq < rejection.seq);
}

/// Every configured client kind retains request authority while an unconfigured
/// or socket-origin peer cannot even commit the raw request.
#[test]
fn configured_kinds_have_authority_but_unconfigured_and_socket_do_not() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
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
        let text = format!("request-{index}");
        connect_ready_configured_extension(&mut h, &source, &source, kind);
        h.handle_extension_event_inner(
            &crate::test_connection_id(&source),
            request(&agent_id, &text),
        )
        .expect("publish configured request");
        assert!(source_committed(&h, &source, |event| {
            matches!(
                event,
                Event::ExtInternalPromptSubmitRequest(request) if request.text == text
            )
        }));
    }

    connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner(
        &crate::test_connection_id("unconfigured"),
        request(&agent_id, &crate::test_connection_id("spoofed")),
    )
    .expect("reject unconfigured request");
    assert!(!source_committed(&h, "unconfigured", |event| {
        matches!(event, Event::ExtInternalPromptSubmitRequest(_))
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "spoofed")
    }));

    connect_ready_configured_extension(
        &mut h,
        "socket-origin",
        "socket-origin",
        tau_proto::ClientKind::Tool,
    );
    h.bus
        .disconnect(&crate::test_connection_id("socket-origin"));
    h.bus.connect(Connection::new(
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
        request(&agent_id, &crate::test_connection_id("socket")),
    )
    .expect("reject socket request");
    assert!(!source_committed(&h, "socket-origin", |event| {
        matches!(event, Event::ExtInternalPromptSubmitRequest(_))
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "socket")
    }));
}

/// A request parked across disconnect can remain an observation but cannot
/// submit work for a stale connection generation.
#[test]
fn disconnected_generation_cannot_submit_parked_request() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    connect_ready_configured_extension(
        &mut h,
        "old-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    connect_internal_prompt_interceptor(&mut h);
    h.handle_extension_event_inner(
        &crate::test_connection_id("old-requester"),
        request(&agent_id, &crate::test_connection_id("stale-request")),
    )
    .expect("park request");

    h.handle_disconnect(&crate::test_connection_id("old-requester"));
    connect_ready_configured_extension(
        &mut h,
        "new-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event(
        "internal-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale request");

    assert!(source_committed(&h, "old-requester", |event| {
        matches!(event, Event::ExtInternalPromptSubmitRequest(_))
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::AgentPromptSubmitted(prompt) if prompt.text == "stale request"
        )
    }));
}

/// Internal-prompt requests are operational traffic: a handshaking source
/// cannot commit or submit them until Ready activates the source.
#[test]
fn pre_ready_request_waits_behind_activation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("requester")
        .expect("requester")
        .state = path_crate_extension::ExtensionState::Handshaking;

    h.handle_extension_event(
        "requester",
        TestProtocolItem::Event(request(&agent_id, "after Ready")),
    )
    .expect("defer request");
    assert!(!source_committed(&h, "requester", |event| {
        matches!(event, Event::ExtInternalPromptSubmitRequest(_))
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "after Ready")
    }));

    h.handle_extension_message(
        &crate::test_connection_id("requester"),
        TestMessage::Ready(Default::default()),
    )
    .expect("activate requester");

    assert!(source_committed(&h, "requester", |event| {
        matches!(event, Event::ExtInternalPromptSubmitRequest(request) if request.text == "after Ready")
    }));
    assert!(event_log_contains_any_source(&h, |event| {
        matches!(event, Event::AgentPromptSubmitted(prompt) if prompt.text == "after Ready")
    }));
}

/// A request admitted before Ready retains its old session generation through
/// activation staging, so Ready after rollover commits only the raw
/// observation.
#[test]
fn pre_ready_request_after_rollover_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("requester")
        .expect("requester")
        .state = path_crate_extension::ExtensionState::Handshaking;
    h.handle_extension_event(
        "requester",
        TestProtocolItem::Event(request(&agent_id, "stale after Ready")),
    )
    .expect("defer request before Ready");

    h.switch_session(
        "replacement"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    h.handle_extension_message(
        &crate::test_connection_id("requester"),
        TestMessage::Ready(Default::default()),
    )
    .expect("activate requester");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::ExtInternalPromptSubmitRequest(request)
                if request.text == "stale after Ready"
        )
    }));
    assert!(!event_log_contains_any_source(&h, |event| {
        matches!(
            event,
            Event::AgentPromptSubmitted(prompt) if prompt.text == "stale after Ready"
        )
    }));
}

//! Contract tests for `SPEC-start-agent-requests`.

use super::*;

/// Build one start-agent request with observable correlation and instruction.
fn request(query_id: &str, instruction: &str) -> StartAgentRequest {
    StartAgentRequest {
        query_id: query_id.to_owned(),
        instruction: instruction.to_owned(),
        role: Some("engineer".to_owned()),
        input_stats: tau_proto::ToolUseStats::default(),
        tool_call_id: None,
        task_name: Some(format!("task {query_id}")),
        parent_agent: None,
    }
}

/// Wrap one start-agent fixture for generic event intake.
fn request_event(query_id: &str, instruction: &str) -> Event {
    Event::StartAgentRequest(request(query_id, instruction))
}

/// Register one exact-name interceptor for start-agent requests.
fn connect_start_agent_interceptor(h: &mut Harness) {
    connect_test_tool(h, "start-agent-interceptor");
    h.handle_extension_event(
        "start-agent-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_START_REQUEST,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register interceptor");
}

/// Return whether one source committed a matching event.
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

/// Return the first committed event matching a predicate.
fn first_committed_matching(
    h: &Harness,
    predicate: impl Fn(&Event) -> bool,
) -> crate::event_log::LogEntry {
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if predicate(&entry.event) {
            return entry;
        }
    }
    panic!("matching committed event");
}

/// Count side agents carrying one extension query correlation.
fn query_agent_count(h: &Harness, query_id: &str) -> usize {
    h.agents
        .values()
        .filter(|agent| {
            matches!(
                &agent.originator,
                tau_proto::PromptOriginator::Extension {
                    query_id: candidate,
                    ..
                } if candidate == query_id
            )
        })
        .count()
}

/// Return a directed start-agent result from a connected requester's sink.
fn directed_result(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    query_id: &str,
) -> Option<tau_proto::StartAgentResult> {
    sink.lock()
        .expect("sink")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == query_id => {
                Some(result.clone())
            }
            _ => None,
        })
}

/// Return a directed start-agent acceptance from a connected requester's sink.
fn directed_acceptance(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    query_id: &str,
) -> Option<tau_proto::StartAgentAccepted> {
    sink.lock()
        .expect("sink")
        .iter()
        .find_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentAccepted(accepted)) if accepted.query_id == query_id => {
                Some(accepted.clone())
            }
            _ => None,
        })
}

/// Dropping the raw request must prevent observation, acceptance, and agent
/// work.
#[test]
fn dropped_request_has_no_start_agent_effect() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    connect_start_agent_interceptor(&mut h);

    h.handle_extension_event_inner("requester", request_event("q-drop", "drop me"))
        .expect("park request");
    h.handle_extension_event(
        "start-agent-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop request");

    assert!(!source_committed(&h, "requester", |event| {
        matches!(event, Event::StartAgentRequest(request) if request.query_id == "q-drop")
    }));
    assert_eq!(query_agent_count(&h, "q-drop"), 0);
    assert!(directed_acceptance(&sink, "q-drop").is_none());
    assert!(directed_result(&sink, "q-drop").is_none());
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(event, Event::StartAgentAccepted(accepted) if accepted.query_id == "q-drop")
    }));
}

/// A same-name replacement drives correlation, instruction, acceptance, and
/// agent creation only after the replacement commits.
#[test]
fn replacement_payload_commits_before_acceptance_and_agent_creation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Provider,
    );
    connect_start_agent_interceptor(&mut h);

    h.handle_extension_event_inner("requester", request_event("q-original", "original"))
        .expect("park request");
    h.handle_extension_event(
        "start-agent-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(request_event(
                "q-replacement",
                "replacement",
            )))),
        })),
    )
    .expect("replace request");

    let raw = first_committed_matching(&h, |event| {
        matches!(
            event,
            Event::StartAgentRequest(request)
                if request.query_id == "q-replacement" && request.instruction == "replacement"
        )
    });
    let accepted = first_committed_matching(&h, |event| {
        matches!(
            event,
            Event::StartAgentAccepted(accepted) if accepted.query_id == "q-replacement"
        )
    });
    assert_eq!(raw.source.as_deref(), Some("requester"));
    assert_eq!(
        accepted.source.as_deref(),
        Some(crate::harness::HARNESS_CONNECTION_ID)
    );
    assert!(raw.seq < accepted.seq);
    assert_eq!(query_agent_count(&h, "q-original"), 0);
    assert_eq!(query_agent_count(&h, "q-replacement"), 1);
    assert!(directed_acceptance(&sink, "q-replacement").is_some());
}

/// Invalid role selection remains a committed request observation followed by
/// the existing requester-directed terminal error and no accepted identity.
#[test]
fn invalid_role_commits_before_directed_rejection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Tool,
    );
    let mut invalid = request("q-invalid-role", "invalid");
    invalid.role = Some("missing-role".to_owned());

    h.handle_extension_event_inner("requester", Event::StartAgentRequest(invalid))
        .expect("process invalid request");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::StartAgentRequest(request) if request.query_id == "q-invalid-role"
        )
    }));
    let result = directed_result(&sink, "q-invalid-role").expect("directed rejection");
    assert!(result.error.is_some());
    assert_eq!(query_agent_count(&h, "q-invalid-role"), 0);
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::StartAgentAccepted(accepted) if accepted.query_id == "q-invalid-role"
        )
    }));
}

/// Invalid parent correlation is evaluated only after the raw request commits
/// and retains the existing requester-directed failure behavior.
#[test]
fn invalid_parent_commits_before_directed_rejection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Core,
    );
    let mut invalid = request("q-invalid-parent", "invalid");
    invalid.parent_agent =
        Some(tau_proto::AgentId::parse("missing-parent").expect("parent agent id"));

    h.handle_extension_event_inner("requester", Event::StartAgentRequest(invalid))
        .expect("process invalid request");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::StartAgentRequest(request) if request.query_id == "q-invalid-parent"
        )
    }));
    let result = directed_result(&sink, "q-invalid-parent").expect("directed rejection");
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not loaded"))
    );
    assert_eq!(query_agent_count(&h, "q-invalid-parent"), 0);
}

/// Every configured extension kind has request authority, while unconfigured
/// and socket-origin peers cannot commit the raw request or receive outcomes.
#[test]
fn configured_kinds_have_authority_but_unconfigured_and_socket_do_not() {
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
        let query_id = format!("q-kind-{index}");
        connect_ready_configured_extension(&mut h, &source, &source, kind);
        let mut invalid = request(&query_id, "authority");
        invalid.role = Some("missing-role".to_owned());
        h.handle_extension_event_inner(&source, Event::StartAgentRequest(invalid))
            .expect("publish configured request");
        assert!(source_committed(&h, &source, |event| {
            matches!(
                event,
                Event::StartAgentRequest(request) if request.query_id == query_id
            )
        }));
    }

    let unconfigured = connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner("unconfigured", request_event("q-unconfigured", "spoofed"))
        .expect("reject unconfigured");
    assert!(!source_committed(&h, "unconfigured", |event| {
        matches!(event, Event::StartAgentRequest(_))
    }));
    assert!(directed_result(&unconfigured, "q-unconfigured").is_none());

    let socket_sink = connect_ready_configured_extension(
        &mut h,
        "socket-origin",
        "socket-origin",
        tau_proto::ClientKind::Tool,
    );
    h.bus.disconnect("socket-origin");
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: "socket-origin".into(),
            name: "socket-origin".to_owned(),
            kind: tau_proto::ClientKind::Tool,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(TestSink {
            events: Arc::clone(&socket_sink),
        }),
    ));
    h.handle_extension_event_inner("socket-origin", request_event("q-socket", "socket"))
        .expect("reject socket");
    assert!(!source_committed(&h, "socket-origin", |event| {
        matches!(event, Event::StartAgentRequest(_))
    }));
    assert!(directed_result(&socket_sink, "q-socket").is_none());
}

/// A parked request may commit after its source disconnects, but the stale
/// generation cannot accept, reject, rebind, or create an agent.
#[test]
fn stale_generation_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let _old_sink = connect_ready_configured_extension(
        &mut h,
        "old-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    connect_start_agent_interceptor(&mut h);
    h.handle_extension_event_inner("old-requester", request_event("q-stale", "stale"))
        .expect("park request");

    h.handle_disconnect("old-requester");
    let new_sink = connect_ready_configured_extension(
        &mut h,
        "new-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event(
        "start-agent-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit stale request");

    assert!(source_committed(&h, "old-requester", |event| {
        matches!(event, Event::StartAgentRequest(request) if request.query_id == "q-stale")
    }));
    assert_eq!(query_agent_count(&h, "q-stale"), 0);
    assert!(directed_acceptance(&new_sink, "q-stale").is_none());
    assert!(directed_result(&new_sink, "q-stale").is_none());
    assert!(!event_log_events(&h).iter().any(|event| {
        matches!(
            event,
            Event::StartAgentAccepted(accepted) if accepted.query_id == "q-stale"
        )
    }));
}

/// A request parked in interception cannot create, reject, or rebind work after
/// the harness switches away from the session that admitted it.
#[test]
fn stale_session_request_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    connect_start_agent_interceptor(&mut h);
    connect_test_tool(&mut h, "rollover-request-blocker");
    h.handle_extension_event(
        "rollover-request-blocker",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register rollover blocker");
    h.publish_event(None, draft_event("block deferred start request"));
    h.handle_extension_event_inner(
        "requester",
        request_event("q-stale-session", "session A work"),
    )
    .expect("defer request behind parked observation");

    h.switch_session(
        "s2".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    h.switch_session(
        "s1".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::Resume,
    )
    .expect("return to original session id");
    h.handle_extension_event(
        "start-agent-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("consume stale-session interceptor reply");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::StartAgentRequest(request) if request.query_id == "q-stale-session"
        )
    }));
    assert_eq!(h.current_session_id.as_str(), "s1");
    assert_eq!(query_agent_count(&h, "q-stale-session"), 0);
    assert!(directed_acceptance(&sink, "q-stale-session").is_none());
    assert!(directed_result(&sink, "q-stale-session").is_none());
}

/// A pre-Ready request retains its original admission session while deferred,
/// so switching sessions before Ready cannot retarget the work into the new
/// session.
#[test]
fn pre_ready_request_keeps_original_admission_session() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.extensions
        .entries
        .get_mut("requester")
        .expect("requester")
        .state = crate::extension::ExtensionState::Handshaking;

    h.handle_extension_event(
        "requester",
        TestProtocolItem::Event(request_event("q-pre-ready-session", "session A work")),
    )
    .expect("defer request");
    assert!(!source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::StartAgentRequest(request) if request.query_id == "q-pre-ready-session"
        )
    }));

    h.switch_session(
        "s2".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    h.handle_extension_message("requester", TestMessage::Ready(Default::default()))
        .expect("activate requester");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::StartAgentRequest(request) if request.query_id == "q-pre-ready-session"
        )
    }));
    assert_eq!(h.current_session_id.as_str(), "s2");
    assert_eq!(query_agent_count(&h, "q-pre-ready-session"), 0);
    assert!(directed_acceptance(&sink, "q-pre-ready-session").is_none());
    assert!(directed_result(&sink, "q-pre-ready-session").is_none());
}

/// Repeating one stable publisher/query pair reuses the active side agent and
/// rebinds its directed result route to the latest live connection.
#[test]
fn active_duplicate_rebinds_without_creating_another_agent() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let old_sink = connect_ready_configured_extension(
        &mut h,
        "old-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner("old-requester", request_event("q-duplicate", "first"))
        .expect("start first request");
    let side_cid = h
        .agents
        .iter()
        .find_map(|(cid, agent)| {
            matches!(
                &agent.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. }
                    if query_id == "q-duplicate"
            )
            .then(|| cid.clone())
        })
        .expect("side agent");
    let side_agent_id = h.agents[&side_cid]
        .agent_id
        .clone()
        .expect("public side agent id");

    let new_sink = connect_ready_configured_extension(
        &mut h,
        "new-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner(
        "new-requester",
        request_event("q-duplicate", "ignored duplicate"),
    )
    .expect("rebind duplicate");

    assert_eq!(query_agent_count(&h, "q-duplicate"), 1);
    assert_eq!(
        h.agents[&side_cid].source_connection.as_deref(),
        Some("new-requester")
    );
    assert_eq!(
        directed_acceptance(&new_sink, "q-duplicate")
            .expect("duplicate acceptance")
            .agent_id
            .as_str(),
        side_agent_id
    );
    assert!(source_committed(&h, "old-requester", |event| {
        matches!(event, Event::StartAgentRequest(request) if request.query_id == "q-duplicate")
    }));
    assert!(source_committed(&h, "new-requester", |event| {
        matches!(event, Event::StartAgentRequest(request) if request.query_id == "q-duplicate")
    }));
    h.deliver_finished_side_conversation_result(
        &side_cid,
        &tau_proto::ExtensionName::from("stable-requester"),
        "q-duplicate",
        tau_proto::StartAgentResult {
            query_id: "q-duplicate".to_owned(),
            text: "done".to_owned(),
            error: None,
        },
        None,
    );
    assert!(directed_result(&old_sink, "q-duplicate").is_none());
    assert_eq!(
        directed_result(&new_sink, "q-duplicate")
            .expect("result routed to rebound connection")
            .text,
        "done"
    );
}

/// A duplicate of an admitted-but-not-dispatched request reuses its minted
/// identity and updates only the pending result route.
#[test]
fn pending_duplicate_rebinds_without_minting_or_dispatching() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let old_sink = connect_ready_configured_extension(
        &mut h,
        "old-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    let pending = h
        .prepare_start_agent_request("old-requester", request("q-pending", "first"))
        .expect("prepare")
        .expect("new pending request");
    let expected_agent_id = pending.agent_id.clone();
    h.pending_start_agent_requests.push_back(pending);

    let new_sink = connect_ready_configured_extension(
        &mut h,
        "new-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner(
        "new-requester",
        request_event("q-pending", "ignored duplicate"),
    )
    .expect("rebind pending duplicate");

    assert_eq!(h.pending_start_agent_requests.len(), 1);
    assert_eq!(h.pending_start_agent_requests[0].source_id, "new-requester");
    assert_eq!(
        h.pending_start_agent_requests[0].agent_id,
        expected_agent_id
    );
    assert_eq!(query_agent_count(&h, "q-pending"), 0);
    assert_eq!(
        directed_acceptance(&new_sink, "q-pending")
            .expect("duplicate accepted")
            .agent_id
            .as_str(),
        expected_agent_id
    );
    h.drain_pending_start_agent_requests()
        .expect("dispatch rebound pending request");
    let side_cid = h
        .agent_routes
        .get(&expected_agent_id)
        .cloned()
        .expect("dispatched side agent");
    h.deliver_finished_side_conversation_result(
        &side_cid,
        &tau_proto::ExtensionName::from("stable-requester"),
        "q-pending",
        tau_proto::StartAgentResult {
            query_id: "q-pending".to_owned(),
            text: "pending done".to_owned(),
            error: None,
        },
        None,
    );
    assert!(directed_result(&old_sink, "q-pending").is_none());
    assert_eq!(
        directed_result(&new_sink, "q-pending")
            .expect("result routed to rebound connection")
            .text,
        "pending done"
    );
}

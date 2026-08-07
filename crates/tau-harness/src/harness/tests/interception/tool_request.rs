use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Internal fixture that immediately backgrounds its routed call.
struct PeerBackgroundTool;

impl crate::InternalToolHandler for PeerBackgroundTool {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![tau_proto::ToolSpec {
            name: tau_proto::ToolName::new("peer_background"),
            model_visible_name: None,
            description: Some("peer background fixture".to_owned()),
            parameters: Some(serde_json::json!({"type":"object"})),
            format: None,
            tool_type: tau_proto::ToolType::Function,
            tags: Vec::new(),
            enabled_by_default: true,
            background_support: Some(tau_proto::BackgroundSupport::Instant),
            examples: Vec::new(),
        }]
    }

    fn handles(&self, internal_tool_name: &tau_proto::ToolName) -> bool {
        internal_tool_name.as_str() == "peer_background"
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some((_cid, call, _visible_name)) = host.internal_started_call(started) else {
            return Ok(());
        };
        if call.name.as_str() == "peer_background" {
            host.background_tool_call(
                &call.id,
                CborValue::Text("running in background".to_owned()),
            );
        }
        Ok(())
    }
}

/// Construct non-default extension-originated correlation metadata.
fn extension_originator() -> tau_proto::PromptOriginator {
    tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("request-origin")
            .expect("test extension name must satisfy the identifier grammar"),
        query_id: "query-7".to_owned(),
    }
}

/// Construct one peer request with observable correlation metadata.
fn request(call_id: &str, tool_name: &str, agent_id: tau_proto::AgentId) -> Event {
    Event::ToolRequest(tau_proto::ToolRequest {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("value".to_owned()),
            CborValue::Integer(7.into()),
        )]),
        agent_id,
        originator: extension_originator(),
    })
}

/// Register one routable extension tool through the canonical declaration flow.
fn register_tool(harness: &mut Harness, source: &str, name: &str) {
    harness
        .handle_extension_event(
            source,
            TestProtocolItem::Event(Event::ToolRegistrationDeclared(
                tau_proto::ToolRegistrationDeclared {
                    tool: tau_proto::ToolSpec {
                        name: tau_proto::ToolName::new(name),
                        model_visible_name: None,
                        description: Some("request test tool".to_owned()),
                        parameters: None,
                        format: None,
                        tool_type: tau_proto::ToolType::Function,
                        tags: Vec::new(),
                        enabled_by_default: true,
                        background_support: None,
                        examples: Vec::new(),
                    },
                    tool_group: None,
                    prompt_fragment: None,
                },
            )),
        )
        .expect("register test tool");
}

/// Collect the request-routing family for one call in runtime sequence order.
fn committed_request_family(
    harness: &Harness,
    call_id: &str,
) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        let matches = match &entry.event {
            Event::ToolRequest(event) => event.call_id.as_str() == call_id,
            Event::ToolStarted(event) => event.call_id.as_str() == call_id,
            Event::ToolRejected(event) => event.call_id.as_str() == call_id,
            Event::ToolResult(event) | Event::ProviderToolResult(event) => {
                event.call_id.as_str() == call_id
            }
            Event::ToolError(event) | Event::ProviderToolError(event) => {
                event.call_id.as_str() == call_id
            }
            Event::ToolCancelled(event) => event.call_id.as_str() == call_id,
            _ => false,
        };
        if matches {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// A peer request must commit before routing, install terminal ownership before
/// started delivery, retain rewritten metadata, accept routed ownerless
/// progress, and derive only harness-authored facts.
#[test]
fn committed_request_routes_after_publication_with_harness_derived_source() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let debug_path = harness
        .enable_debug_log(&tmp.path().join("success-debug"))
        .expect("enable debug log");
    connect_ready_configured_extension(
        &mut harness,
        "requester-connection",
        "configured-requester",
        tau_proto::ClientKind::Provider,
    );
    connect_ready_configured_extension(
        &mut harness,
        "tool-connection",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    register_tool(&mut harness, "tool-connection", "request_test");
    connect_test_tool(&mut harness, "started-interceptor");
    harness
        .handle_extension_event(
            "started-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_STARTED)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept started");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness
        .handle_extension_event(
            "requester-connection",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("peer-success", "request_test", agent_id.clone())),
                persist: false,
            })),
        )
        .expect("route committed request");

    assert_eq!(
        harness
            .pending_tool_providers
            .get("peer-success")
            .map(tau_proto::ConnectionId::as_str),
        Some("tool-connection")
    );
    assert!(matches!(
        committed_request_family(&harness, "peer-success").as_slice(),
        [(Some(source), Event::ToolRequest(_))] if source == "requester-connection"
    ));
    harness
        .handle_extension_event(
            "started-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit started");
    assert!(matches!(
        committed_request_family(&harness, "peer-success").as_slice(),
        [
            (Some(request_source), Event::ToolRequest(committed)),
            (Some(started_source), Event::ToolStarted(started)),
        ] if request_source == "requester-connection"
            && started_source == HARNESS_CONNECTION_ID
            && started.agent_id == agent_id
            && started.arguments == committed.arguments
            && started.originator == extension_originator()
    ));
    harness
        .handle_extension_event(
            "tool-connection",
            TestProtocolItem::Event(Event::ToolProgressReported(tau_proto::ToolProgress {
                call_id: "peer-success".into(),
                tool_name: tau_proto::ToolName::new("forged"),
                message: Some("running".to_owned()),
                progress: None,
                display: None,
            })),
        )
        .expect("accept routed progress");
    assert!(event_log_events(&harness).iter().any(|event| matches!(
        event,
        Event::ToolProgress(progress)
            if progress.call_id.as_str() == "peer-success"
                && progress.message.as_deref() == Some("running")
    )));
    let debug = std::fs::read_to_string(debug_path).expect("read debug log");
    let names = debug
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("debug JSON"))
        .filter_map(|line| {
            matches!(
                line["event_name"].as_str(),
                Some("tool.request" | "tool.started")
            )
            .then(|| line["event_name"].as_str().expect("event name").to_owned())
        })
        .collect::<Vec<_>>();
    assert_eq!(names, ["tool.request", "tool.started"]);
}

/// Supplied persistence metadata controls restore persistence, and a durable
/// peer request records stable configured publisher identity rather than its
/// live connection.
#[test]
fn request_preserves_persistence_and_stable_restore_publisher() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "run-local-requester",
        "stable-requester",
        tau_proto::ClientKind::Core,
    );
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    for (call_id, persist) in [("durable-request", true), ("transient-request", false)] {
        harness
            .handle_extension_event(
                "run-local-requester",
                TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                    event: Box::new(request(call_id, "missing", agent_id.clone())),
                    persist,
                })),
            )
            .expect("commit request");
    }

    let restore = harness
        .store
        .session_restore_events(harness.current_session_id.as_str())
        .expect("load restore events");
    let peer_requests = restore
        .iter()
        .filter(|record| {
            matches!(
                &record.event,
                Event::ToolRequest(request)
                    if matches!(
                        request.call_id.as_str(),
                        "durable-request" | "transient-request"
                    )
            )
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        peer_requests.as_slice(),
        [record] if record.source == Some(tau_core::PersistedEventSource::Extension(
            crate::test_extension_name("stable-requester")
        ))
            && matches!(
                &record.event,
                Event::ToolRequest(request) if request.call_id.as_str() == "durable-request"
            )
    ));
}

/// A parked request from an obsolete configured generation may commit but must
/// not create bookkeeping, notices, routes, or derived events.
#[test]
fn stale_parked_request_commits_without_routing() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Tool,
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("stale-request", "missing", agent_id)),
                persist: false,
            })),
        )
        .expect("park request");
    harness
        .extensions
        .entries
        .get_mut("requester")
        .expect("requester")
        .instance_id = tau_proto::ExtensionInstanceId::new(99);
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit stale request");

    assert!(matches!(
        committed_request_family(&harness, "stale-request").as_slice(),
        [(Some(source), Event::ToolRequest(_))] if source == "requester"
    ));
    assert!(!harness.pending_tools.contains_key("stale-request"));
    assert!(!harness.completed_tool_calls.contains("stale-request"));
}

/// An invalid same-name replacement falls back to the original request, while
/// preserving its publisher and persistence metadata through ordinary
/// interception.
#[test]
fn empty_call_id_replacement_falls_back_to_original_request() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Core,
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("original-request", "missing", agent_id.clone())),
                persist: false,
            })),
        )
        .expect("park original");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(request("", "replacement", agent_id)))),
            })),
        )
        .expect("reject invalid replacement");

    assert!(matches!(
        committed_request_family(&harness, "original-request").first(),
        Some((Some(source), Event::ToolRequest(request)))
            if source == "requester" && request.tool_name.as_str() == "missing"
    ));
}

/// Unavailable routing must preserve extension-originated metadata, publish the
/// rejection before the canonical provider terminal and derived projection, and
/// retain a tombstone.
#[test]
fn unavailable_request_closes_with_ordered_harness_outcomes() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let debug_path = harness
        .enable_debug_log(&tmp.path().join("debug"))
        .expect("enable debug log");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Provider,
    );
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("unavailable-request", "absent", agent_id)),
                persist: false,
            })),
        )
        .expect("close unavailable request");

    assert!(matches!(
        committed_request_family(&harness, "unavailable-request").as_slice(),
        [
            (Some(request_source), Event::ToolRequest(_)),
            (Some(rejected_source), Event::ToolRejected(rejected)),
            (Some(provider_error_source), Event::ProviderToolError(provider_error)),
            (Some(error_source), Event::ToolError(error)),
        ] if request_source == "requester"
            && rejected_source == HARNESS_CONNECTION_ID
            && error_source == HARNESS_CONNECTION_ID
            && provider_error_source == HARNESS_CONNECTION_ID
            && rejected.originator == extension_originator()
            && error.originator == extension_originator()
            && provider_error.originator == extension_originator()
    ));
    assert!(!harness.pending_tools.contains_key("unavailable-request"));
    assert!(harness.completed_tool_calls.contains("unavailable-request"));
    let debug = std::fs::read_to_string(debug_path).expect("read debug log");
    let names = debug
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("debug JSON"))
        .filter_map(|line| {
            let name = line["event_name"].as_str()?.to_owned();
            matches!(
                name.as_str(),
                "tool.request" | "tool.rejected" | "tool.error" | "provider.tool_error"
            )
            .then_some(name)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "tool.request",
            "tool.rejected",
            "provider.tool_error",
            "tool.error"
        ]
    );
}

/// Request authority is exact and default-deny, and structurally empty
/// correlation is rejected before generic publication.
#[test]
fn request_authority_and_empty_call_id_fail_before_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "wrong-kind",
        "configured-action",
        tau_proto::ClientKind::Action,
    );
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Tool,
    );
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    for (source, call_id) in [("wrong-kind", "wrong-kind-call"), ("requester", "")] {
        harness
            .handle_extension_event(
                source,
                TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                    event: Box::new(request(call_id, "missing", agent_id.clone())),
                    persist: false,
                })),
            )
            .expect("reject request");
    }

    assert!(committed_request_family(&harness, "wrong-kind-call").is_empty());
    assert!(!harness.pending_tools.contains_key(""));
}

/// A non-transient request whose target cannot enter the session restore stream
/// fails generic commit and therefore cannot run downstream routing.
#[test]
fn unloaded_agent_restore_failure_aborts_request_commit() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Core,
    );
    let routed = connect_test_client(&mut harness, "observer", tau_proto::ClientKind::Ui);
    harness
        .complete_subscription(
            &crate::test_connection_id("observer"),
            Vec::new(),
            vec![EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST)],
        )
        .expect("subscribe live observer");

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request(
                    "unloaded-request",
                    "missing",
                    tau_proto::AgentId::parse("not-loaded").expect("agent id"),
                )),
                persist: true,
            })),
        )
        .expect("store failure is surfaced as a harness fact");

    assert!(
        !harness
            .store
            .session_restore_events(harness.current_session_id.as_str())
            .expect("load restore events")
            .iter()
            .any(|record| {
                matches!(
                    &record.event,
                    Event::ToolRequest(request)
                        if request.call_id.as_str() == "unloaded-request"
                )
            })
    );
    assert!(
        !committed_request_family(&harness, "unloaded-request")
            .iter()
            .any(|(_, event)| !matches!(event, Event::ToolRequest(_)))
    );
    assert!(!harness.pending_tools.contains_key("unloaded-request"));
    assert!(!harness.completed_tool_calls.contains("unloaded-request"));
    assert!(!routed.lock().expect("routed events").iter().any(|routed| {
        matches!(
            &routed.frame,
            HarnessOutputMessage::Deliver(delivery)
                if matches!(
                    delivery.event.as_ref(),
                    Event::ToolRequest(request)
                        if request.call_id.as_str() == "unloaded-request"
                )
        )
    }));
}

/// Pre-Ready requests remain ordinary retained operational messages, including
/// the exact encoded `persist=false` envelope charged to activation accounting.
#[test]
fn pre_ready_request_preserves_retained_accounting() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Provider,
    );
    harness
        .extensions
        .entries
        .get_mut("requester")
        .expect("requester")
        .state = path_crate_extension::ExtensionState::Handshaking;
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let event = request("retained-request", "missing", agent_id);
    let expected_bytes = Harness::encoded_emit_size(&event, false);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(event),
                persist: false,
            })),
        )
        .expect("retain request");
    let stage = &harness.extensions.activation_staging["requester"];
    assert_eq!(stage.retained_message_count, 1);
    assert_eq!(stage.retained_message_bytes, expected_bytes);
    assert!(committed_request_family(&harness, "retained-request").is_empty());

    harness
        .handle_extension_message(
            &crate::test_connection_id("requester"),
            TestMessage::Ready(Default::default()),
        )
        .expect("activate requester");
    assert!(matches!(
        committed_request_family(&harness, "retained-request").first(),
        Some((Some(source), Event::ToolRequest(_))) if source == "requester"
    ));
}

/// Harness-internal request publication has no authenticated peer context and
/// therefore must not enter the peer routing consumer a second time.
#[test]
fn internal_request_publication_does_not_route_as_peer_input() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        request("internal-observation", "missing", agent_id),
    );

    assert!(matches!(
        committed_request_family(&harness, "internal-observation").as_slice(),
        [(Some(source), Event::ToolRequest(_))] if source == HARNESS_CONNECTION_ID
    ));
    assert!(!harness.pending_tools.contains_key("internal-observation"));
    assert!(
        !harness
            .completed_tool_calls
            .contains("internal-observation")
    );
}

/// Rollover commits a peer request deferred behind an intercepted FIFO head,
/// while the stale admission generation suppresses routing, bookkeeping, and
/// derived terminal facts in the replacement session.
#[test]
fn rollover_commits_deferred_request_without_semantic_effects() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "rollover-requester",
        "configured-rollover-requester",
        tau_proto::ClientKind::Provider,
    );
    let _interceptor = connect_test_tool(&mut harness, "rollover-request-blocker");
    harness
        .handle_extension_event(
            "rollover-request-blocker",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register rollover blocker");
    harness.publish_event(None, draft_event("block request FIFO"));

    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let notices_before = event_log_events(&harness)
        .iter()
        .filter(|event| matches!(event, Event::HarnessNotice(_)))
        .count();
    harness
        .handle_extension_event(
            "rollover-requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request(
                    "rollover-deferred-request",
                    "missing_rollover_tool",
                    agent_id,
                )),
                persist: false,
            })),
        )
        .expect("defer request behind intercepted FIFO head");
    assert!(committed_request_family(&harness, "rollover-deferred-request").is_empty());

    harness
        .switch_session(
            "replacement"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            tau_proto::SessionStartReason::New,
        )
        .expect("switch session");

    assert!(matches!(
        committed_request_family(&harness, "rollover-deferred-request").as_slice(),
        [(Some(source), Event::ToolRequest(_))] if source == "rollover-requester"
    ));
    assert!(
        !harness
            .pending_tools
            .contains_key("rollover-deferred-request")
    );
    assert!(
        !harness
            .pending_tool_providers
            .contains_key("rollover-deferred-request")
    );
    assert!(
        !harness
            .completed_tool_calls
            .contains("rollover-deferred-request")
    );
    assert_eq!(
        event_log_events(&harness)
            .iter()
            .filter(|event| matches!(event, Event::HarnessNotice(_)))
            .count(),
        notices_before
    );
}

/// Ownerless peer requests must accept terminal reports from only their exact
/// routed provider, publish canonical closure, and release all live mappings.
#[test]
fn routed_peer_requests_complete_from_terminal_reports() {
    for outcome in ["result", "error", "cancel"] {
        let tmp = TempDir::new().expect("tempdir");
        let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
        connect_ready_configured_extension(
            &mut harness,
            "requester",
            "configured-requester",
            tau_proto::ClientKind::Provider,
        );
        connect_ready_configured_extension(
            &mut harness,
            "tool-owner",
            "configured-tool",
            tau_proto::ClientKind::Tool,
        );
        connect_ready_configured_extension(
            &mut harness,
            "wrong-tool",
            "configured-wrong-tool",
            tau_proto::ClientKind::Tool,
        );
        register_tool(&mut harness, "tool-owner", "peer_terminal");
        let cid = ensure_test_user_agent(&mut harness);
        let agent_id = durable_agent_id_for_conversation(&harness, &cid);
        let call_id = format!("peer-{outcome}");
        harness
            .handle_extension_event(
                "requester",
                TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                    event: Box::new(request(&call_id, "peer_terminal", agent_id)),
                    persist: false,
                })),
            )
            .expect("route peer request");

        let report = match outcome {
            "result" => Event::ToolResultReported(tau_proto::ToolResult {
                presentation: Default::default(),
                call_id: call_id.as_str().into(),
                tool_name: tau_proto::ToolName::new("forged"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("ok".to_owned()),
                provider_content: Vec::new(),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: extension_originator(),
            }),
            "error" => Event::ToolErrorReported(tau_proto::ToolError {
                presentation: Default::default(),
                call_id: call_id.as_str().into(),
                tool_name: tau_proto::ToolName::new("forged"),
                tool_type: tau_proto::ToolType::Function,
                message: "failed".to_owned(),
                details: None,
                display: None,
                originator: extension_originator(),
            }),
            "cancel" => Event::ToolCancelledReported(tau_proto::ToolCancelled {
                presentation: Default::default(),
                call_id: call_id.as_str().into(),
                tool_name: tau_proto::ToolName::new("forged"),
                tool_type: tau_proto::ToolType::Function,
            }),
            _ => unreachable!(),
        };
        harness
            .handle_extension_event("wrong-tool", TestProtocolItem::Event(report.clone()))
            .expect("commit wrong-owner report without closure");
        assert!(harness.pending_tools.contains_key(call_id.as_str()));
        assert_eq!(
            harness
                .pending_tool_providers
                .get(call_id.as_str())
                .map(tau_proto::ConnectionId::as_str),
            Some("tool-owner")
        );
        assert!(
            !committed_request_family(&harness, &call_id)
                .iter()
                .any(|(_, event)| matches!(
                    event,
                    Event::ToolResult(_)
                        | Event::ToolError(_)
                        | Event::ToolCancelled(_)
                        | Event::ProviderToolResult(_)
                        | Event::ProviderToolError(_)
                ))
        );
        harness
            .handle_extension_event("tool-owner", TestProtocolItem::Event(report))
            .expect("commit terminal report");

        let family = committed_request_family(&harness, &call_id);
        assert!(family.iter().any(|(source, event)| {
            source.as_deref() == Some(HARNESS_CONNECTION_ID)
                && match (outcome, event) {
                    ("result", Event::ToolResult(result)) => {
                        result.tool_name.as_str() == "peer_terminal"
                    }
                    ("error", Event::ToolError(error)) => {
                        error.tool_name.as_str() == "peer_terminal"
                    }
                    ("cancel", Event::ToolCancelled(cancelled)) => {
                        cancelled.tool_name.as_str() == "peer_terminal"
                    }
                    _ => false,
                }
        }));
        assert!(!harness.pending_tools.contains_key(call_id.as_str()));
        assert!(
            !harness
                .pending_tool_providers
                .contains_key(call_id.as_str())
        );
        assert!(harness.completed_tool_calls.contains(call_id.as_str()));
    }
}

/// Disconnecting the exact routed owner terminalizes an ownerless peer request,
/// releases every live correlation map, and retains its completion tombstone.
#[test]
fn routed_peer_request_owner_disconnect_closes_and_cleans_up() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Provider,
    );
    connect_ready_configured_extension(
        &mut harness,
        "tool-owner",
        "configured-tool",
        tau_proto::ClientKind::Tool,
    );
    register_tool(&mut harness, "tool-owner", "disconnect_tool");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("disconnect-request", "disconnect_tool", agent_id)),
                persist: false,
            })),
        )
        .expect("route request");

    harness.handle_disconnect(&crate::test_connection_id("tool-owner"));

    assert!(
        committed_request_family(&harness, "disconnect-request")
            .iter()
            .any(|(source, event)| matches!(
                (source.as_deref(), event),
                (Some(HARNESS_CONNECTION_ID), Event::ToolError(_))
            ))
    );
    assert!(!harness.pending_tools.contains_key("disconnect-request"));
    assert!(
        !harness
            .pending_tool_providers
            .contains_key("disconnect-request")
    );
    assert!(!harness.peer_tool_requests.contains("disconnect-request"));
    assert!(harness.completed_tool_calls.contains("disconnect-request"));
}

/// Trusted peer-internal requests use loaded-agent runtime correlation for
/// start/wait state; terminal completion and agent unload must settle
/// accounting and clear that correlation without assigning transcript
/// ownership.
#[test]
fn peer_request_for_internal_tool_uses_loaded_agent_correlation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Core,
    );
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("peer-internal", "skill", agent_id.clone())),
                persist: false,
            })),
        )
        .expect("route internal request");

    let family = committed_request_family(&harness, "peer-internal");
    assert!(family.iter().any(|(source, event)| matches!(
        (source.as_deref(), event),
        (Some(HARNESS_CONNECTION_ID), Event::ToolStarted(started))
            if started.call_id.as_str() == "peer-internal"
    )));
    assert!(
        !family
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolRejected(_)))
    );
    assert_eq!(
        harness.peer_internal_tool_agents.get("peer-internal"),
        Some(&cid)
    );
    assert_eq!(harness.agents[&cid].tools_in_flight, 1);
    assert_eq!(harness.agents[&cid].tools_total, 1);
    assert!(harness.wait_tracks_call_for_test(&"peer-internal".into()));
    let transcript_nodes = default_agent_tree(&harness).nodes().len();

    harness.finish_prebuilt_internal_tool_result(tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "peer-internal".into(),
        tool_name: tau_proto::ToolName::new("skill"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("loaded".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: extension_originator(),
    });
    assert!(
        committed_request_family(&harness, "peer-internal")
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolResult(_)))
    );
    assert!(!harness.pending_tools.contains_key("peer-internal"));
    assert_eq!(harness.agents[&cid].tools_in_flight, 0);
    assert_eq!(default_agent_tree(&harness).nodes().len(), transcript_nodes);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("peer-internal-error", "skill", agent_id.clone())),
                persist: false,
            })),
        )
        .expect("route internal error fixture");
    harness.finish_harness_owned_tool_with_error(
        &cid,
        "peer-internal-error".into(),
        tau_proto::ToolName::new("skill"),
        tau_proto::ToolType::Function,
        "failed".to_owned(),
        None,
    );
    assert!(
        committed_request_family(&harness, "peer-internal-error")
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolError(_)))
    );
    assert!(!harness.pending_tools.contains_key("peer-internal-error"));
    assert_eq!(harness.agents[&cid].tools_in_flight, 0);
    assert_eq!(default_agent_tree(&harness).nodes().len(), transcript_nodes);

    let total_before_message = harness.agents[&cid].tools_total;
    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request(
                    "peer-internal-message",
                    "message",
                    agent_id.clone(),
                )),
                persist: false,
            })),
        )
        .expect("dispatch invalid message request");
    assert!(
        committed_request_family(&harness, "peer-internal-message")
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolError(_)))
    );
    assert_eq!(harness.agents[&cid].tools_total, total_before_message + 1);
    assert_eq!(harness.agents[&cid].tools_in_flight, 0);
    assert!(
        !harness
            .peer_internal_tool_agents
            .contains_key("peer-internal-message")
    );

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("peer-internal-unload", "skill", agent_id)),
                persist: false,
            })),
        )
        .expect("route pending internal request");
    harness.remove_agent(&cid);
    assert!(
        committed_request_family(&harness, "peer-internal-unload")
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolError(_)))
    );
    assert!(!harness.pending_tools.contains_key("peer-internal-unload"));
    assert!(
        !harness
            .peer_internal_tool_agents
            .contains_key("peer-internal-unload")
    );
    assert!(
        harness
            .completed_tool_calls
            .contains("peer-internal-unload")
    );
}

/// Peer-internal request/start/terminal projections for an ephemeral agent must
/// remain absent from durable debug JSONL despite ownerless terminal
/// publication.
#[test]
fn peer_internal_ephemeral_lifecycle_is_suppressed_from_debug_log() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    harness.install_internal_tool_handlers(vec![std::sync::Arc::new(PeerBackgroundTool)]);
    let debug_path = harness
        .enable_debug_log(&tmp.path().join("ephemeral-debug"))
        .expect("enable debug log");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Core,
    );
    harness
        .handle_ui_create_agent_from(
            &crate::test_connection_id("ui-create-test"),
            tau_proto::UiCreateAgent {
                request_id: "test-create-request".to_owned(),
                literal: false,
                session_id: "s1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                role: "engineer".to_owned(),
                model_override: None,
                metadata: Vec::new(),
                initial_prompt: None,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
                parent_agent: None,
                ephemeral: true,
            },
        )
        .expect("create ephemeral agent");
    let agent_id = event_log_events(&harness)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentStarted(started) if started.ephemeral => Some(started.agent_id),
            _ => None,
        })
        .expect("ephemeral agent id");

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request(
                    "ephemeral-peer-internal",
                    "peer_background",
                    agent_id,
                )),
                persist: false,
            })),
        )
        .expect("route ephemeral internal request");
    harness.finish_prebuilt_internal_tool_result(tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "ephemeral-peer-internal".into(),
        tool_name: tau_proto::ToolName::new("peer_background"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: extension_originator(),
    });

    let debug = std::fs::read_to_string(debug_path).expect("read debug log");
    assert!(!debug.contains("ephemeral-peer-internal"));
}

/// A real internal handler may background a trusted peer request; placeholder
/// and terminal facts remain ownerless while accounting and correlation settle.
#[test]
fn peer_internal_background_handler_completes_without_transcript_fold() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    harness.install_internal_tool_handlers(vec![std::sync::Arc::new(PeerBackgroundTool)]);
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Core,
    );
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let transcript_nodes = default_agent_tree(&harness).nodes().len();

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request(
                    "peer-internal-background",
                    "peer_background",
                    agent_id.clone(),
                )),
                persist: false,
            })),
        )
        .expect("background peer-internal request");
    assert!(
        harness
            .tool_turn
            .is_backgrounded(&"peer-internal-background".into())
    );
    assert!(
        committed_request_family(&harness, "peer-internal-background")
            .iter()
            .any(|(_, event)| matches!(
                event,
                Event::ProviderToolResult(result)
                    if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder
            ))
    );
    assert_eq!(harness.agents[&cid].tools_total, 1);
    assert_eq!(harness.agents[&cid].tools_in_flight, 1);

    harness.finish_prebuilt_internal_tool_result(tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "peer-internal-background".into(),
        tool_name: tau_proto::ToolName::new("peer_background"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: extension_originator(),
    });
    assert!(event_log_events(&harness).iter().any(|event| matches!(
        event,
        Event::ToolBackgroundResult(result)
            if result.call_id.as_str() == "peer-internal-background"
    )));
    assert_eq!(harness.agents[&cid].tools_in_flight, 0);
    assert!(
        !harness
            .pending_tools
            .contains_key("peer-internal-background")
    );
    assert!(
        !harness
            .peer_internal_tool_agents
            .contains_key("peer-internal-background")
    );
    assert!(
        harness
            .completed_tool_calls
            .contains("peer-internal-background")
    );
    assert!(
        !harness
            .tool_turn
            .is_backgrounded(&"peer-internal-background".into())
    );
    assert!(
        !harness
            .background_completion_targets
            .contains_key("peer-internal-background")
    );
    assert!(
        harness.agents[&cid]
            .pending_prompts
            .iter()
            .all(|prompt| !prompt.is_activating_background_completion())
    );
    assert_eq!(default_agent_tree(&harness).nodes().len(), transcript_nodes);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request(
                    "peer-internal-background-error",
                    "peer_background",
                    agent_id.clone(),
                )),
                persist: false,
            })),
        )
        .expect("background error fixture");
    harness.finish_prebuilt_internal_tool_error(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: "peer-internal-background-error".into(),
        tool_name: tau_proto::ToolName::new("peer_background"),
        tool_type: tau_proto::ToolType::Function,
        message: "failed".to_owned(),
        details: None,
        display: None,
        originator: extension_originator(),
    });
    assert!(event_log_events(&harness).iter().any(|event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "peer-internal-background-error"
    )));
    assert_eq!(harness.agents[&cid].tools_in_flight, 0);
    assert!(
        !harness
            .pending_tools
            .contains_key("peer-internal-background-error")
    );
    assert!(
        !harness
            .background_completion_targets
            .contains_key("peer-internal-background-error")
    );
    assert_eq!(default_agent_tree(&harness).nodes().len(), transcript_nodes);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request(
                    "peer-internal-background-unload",
                    "peer_background",
                    agent_id,
                )),
                persist: false,
            })),
        )
        .expect("background unload fixture");
    harness.remove_agent(&cid);
    assert!(event_log_events(&harness).iter().any(|event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "peer-internal-background-unload"
    )));
    assert!(!event_log_events(&harness).iter().any(|event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == "peer-internal-background-unload"
    )));
    assert!(
        !harness
            .pending_tools
            .contains_key("peer-internal-background-unload")
    );
    assert!(
        harness
            .completed_tool_calls
            .contains("peer-internal-background-unload")
    );
}

/// Historical subscription replays the stable durable publisher without
/// re-entering request routing or producing any second outcome.
#[test]
fn durable_request_replay_is_observation_only() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "run-local-requester",
        "stable-requester",
        tau_proto::ClientKind::Provider,
    );
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    harness
        .handle_extension_event(
            "run-local-requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("replayed-request", "missing", agent_id)),
                persist: true,
            })),
        )
        .expect("commit durable request");
    let before = committed_request_family(&harness, "replayed-request").len();

    let routed = connect_test_client(&mut harness, "late-ui", tau_proto::ClientKind::Ui);
    harness
        .complete_subscription(
            &crate::test_connection_id("late-ui"),
            vec![EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST)],
            Vec::new(),
        )
        .expect("subscribe to request history");
    let routed = routed.lock().expect("routed events");
    assert!(routed.iter().any(|routed| {
        routed.source_id.is_none()
            && matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if delivery.replay
                        && matches!(
                            delivery.event.as_ref(),
                            Event::ToolRequest(request)
                                if request.call_id.as_str() == "replayed-request"
                        )
            )
    }));
    assert_eq!(
        committed_request_family(&harness, "replayed-request").len(),
        before
    );
}

/// UI admission remains default-deny for tool requests rather than inheriting
/// configured extension routing authority from a claimed client kind.
#[test]
fn ui_cannot_publish_tool_request() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let _routed = connect_test_client(&mut harness, "ui", tau_proto::ClientKind::Ui);
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness
        .handle_client_message(
            &crate::test_connection_id("ui"),
            tau_proto::HarnessInputMessage::emit_with_persist(
                request("ui-request", "missing", agent_id),
                false,
            ),
        )
        .expect("reject UI request");

    assert!(committed_request_family(&harness, "ui-request").is_empty());
}

/// Ordinary interception may replace or drop peer requests; only the final
/// committed same-name payload reaches downstream routing.
#[test]
fn request_interception_replace_and_drop_control_downstream_work() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut harness,
        "requester",
        "configured-requester",
        tau_proto::ClientKind::Tool,
    );
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(tau_proto::EventName::TOOL_REQUEST)],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept requests");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("replace-original", "original", agent_id.clone())),
                persist: false,
            })),
        )
        .expect("park replace request");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(request(
                    "replace-final",
                    "replacement",
                    agent_id.clone(),
                )))),
            })),
        )
        .expect("commit replacement");
    assert!(committed_request_family(&harness, "replace-original").is_empty());
    assert!(matches!(
        committed_request_family(&harness, "replace-final").first(),
        Some((Some(source), Event::ToolRequest(request)))
            if source == "requester" && request.tool_name.as_str() == "replacement"
    ));
    assert!(harness.completed_tool_calls.contains("replace-final"));

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("dropped-request", "missing", agent_id.clone())),
                persist: false,
            })),
        )
        .expect("park dropped request");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("drop request");
    assert!(committed_request_family(&harness, "dropped-request").is_empty());
    assert!(!harness.pending_tools.contains_key("dropped-request"));

    harness
        .handle_extension_event(
            "requester",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(request("wrong-name-original", "missing", agent_id)),
                persist: false,
            })),
        )
        .expect("park wrong-name replacement");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(Some(Box::new(Event::ToolRejected(
                    tau_proto::ToolRejected {
                        call_id: "forged".into(),
                        tool_name: tau_proto::ToolName::new("forged"),
                        tool_type: tau_proto::ToolType::Function,
                        message: "forged".to_owned(),
                        originator: extension_originator(),
                    },
                )))),
            })),
        )
        .expect("fall back from wrong-name replacement");
    assert!(matches!(
        committed_request_family(&harness, "wrong-name-original").first(),
        Some((Some(source), Event::ToolRequest(_))) if source == "requester"
    ));
}

use super::*;
use crate::harness::TerminalSettlement;
use crate::harness::tests::dispatch::{final_tool_result, setup_routed_test_tool_call, tool_error};

/// Collect terminal reports and projections for one call in commit order.
fn committed_terminal_events(
    harness: &Harness,
    call_id: &str,
) -> Vec<(Option<tau_proto::ConnectionId>, Event)> {
    let mut events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = harness.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        let event_call_id = match &entry.event {
            Event::ToolResultReported(result)
            | Event::ToolResult(result)
            | Event::ProviderToolResult(result) => Some(&result.call_id),
            Event::ToolErrorReported(error)
            | Event::ToolError(error)
            | Event::ProviderToolError(error) => Some(&error.call_id),
            Event::ToolCancelledReported(cancelled) | Event::ToolCancelled(cancelled) => {
                Some(&cancelled.call_id)
            }
            Event::ToolBackgroundResult(result) => Some(&result.call_id),
            Event::ToolBackgroundError(error) => Some(&error.call_id),
            _ => None,
        };
        if event_call_id.is_some_and(|id| id.as_str() == call_id) {
            events.push((entry.source, entry.event));
        }
    }
    events
}

/// Ensures exact and prefix UI subscriptions receive only payload-free result
/// projections while non-UI peers may observe the raw generic facts.
#[test]
fn ui_result_subscriptions_exclude_raw_foreground_and_background_facts() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("routing-result", "owned_tool");
    let exact = connect_test_client(&mut harness, "result-ui-exact", tau_proto::ClientKind::Ui);
    let prefix = connect_test_client(&mut harness, "result-ui-prefix", tau_proto::ClientKind::Ui);
    let core = connect_test_client(&mut harness, "result-core", tau_proto::ClientKind::Core);
    harness
        .bus
        .set_subscriptions(
            &crate::test_connection_id("result-ui-exact"),
            Vec::new(),
            vec![
                EventSelector::Exact(tau_proto::EventName::TOOL_RESULT),
                EventSelector::Exact(tau_proto::EventName::TOOL_RESULT_DISPLAY),
                EventSelector::Exact(tau_proto::EventName::TOOL_BACKGROUND_RESULT),
                EventSelector::Exact(tau_proto::EventName::TOOL_BACKGROUND_RESULT_DISPLAY),
            ],
        )
        .expect("exact UI subscription");
    harness
        .bus
        .set_subscriptions(
            &crate::test_connection_id("result-ui-prefix"),
            Vec::new(),
            vec![EventSelector::Prefix("tool.".to_owned())],
        )
        .expect("prefix UI subscription");
    harness
        .bus
        .set_subscriptions(
            &crate::test_connection_id("result-core"),
            Vec::new(),
            vec![
                EventSelector::Exact(tau_proto::EventName::TOOL_RESULT),
                EventSelector::Exact(tau_proto::EventName::TOOL_BACKGROUND_RESULT),
            ],
        )
        .expect("core subscription");

    let result = final_tool_result("routing-result", "owned_tool", "raw-marker");
    let background = tau_proto::ToolBackgroundResult {
        call_id: "routing-background".into(),
        tool_name: ToolName::new("owned_tool"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("background-marker".to_owned()),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };
    harness.publish_event(None, Event::ToolResult(result.clone()));
    harness.publish_event(
        None,
        Event::ToolResultDisplay(tau_proto::ToolResultDisplay::from(&result)),
    );
    harness.publish_event(None, Event::ToolBackgroundResult(background.clone()));
    harness.publish_event(
        None,
        Event::ToolBackgroundResultDisplay(tau_proto::ToolBackgroundResultDisplay::from(
            &background,
        )),
    );

    for frames in [&exact, &prefix] {
        let frames = frames.lock().expect("UI frames");
        assert!(frames.iter().any(|frame| matches!(
            peel_inner_event(&frame.frame),
            Some(Event::ToolResultDisplay(_))
        )));
        assert!(frames.iter().any(|frame| matches!(
            peel_inner_event(&frame.frame),
            Some(Event::ToolBackgroundResultDisplay(_))
        )));
        assert!(frames.iter().all(|frame| !matches!(
            peel_inner_event(&frame.frame),
            Some(Event::ToolResult(_) | Event::ToolBackgroundResult(_))
        )));
    }
    let core = core.lock().expect("core frames");
    assert!(core.iter().any(|frame| matches!(
        peel_inner_event(&frame.frame),
        Some(Event::ToolResult(result))
            if result.result == CborValue::Text("raw-marker".to_owned())
    )));
    assert!(core.iter().any(|frame| matches!(
        peel_inner_event(&frame.frame),
        Some(Event::ToolBackgroundResult(result))
            if result.result == CborValue::Text("background-marker".to_owned())
    )));
}

/// Register one exact interceptor for the supplied terminal event names.
fn intercept_terminal_names(harness: &mut Harness, names: Vec<tau_proto::EventName>) {
    connect_test_tool(harness, "terminal-interceptor");
    harness
        .handle_extension_event(
            "terminal-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: names.into_iter().map(EventSelector::Exact).collect(),
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register terminal interceptor");
}

/// Resolve the currently parked terminal interception action.
fn reply(harness: &mut Harness, action: InterceptAction) {
    harness
        .handle_extension_event(
            "terminal-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply { action })),
        )
        .expect("resolve terminal interception");
}

/// Ownerless peer-internal results retain their dedicated correlation and
/// accounting until the canonical provider fact clears interception.
#[test]
fn parked_ownerless_result_settles_once_after_canonical_commit() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("seed-owner", "owned_tool");
    let cid = harness
        .tool_agents
        .remove("seed-owner")
        .expect("seed owner");
    let call_id = ToolCallId::from("parked-ownerless");
    harness
        .peer_internal_tool_agents
        .insert(call_id.clone(), cid.clone());
    harness.agents.get_mut(&cid).expect("agent").tools_in_flight = 1;
    let agent_id = harness.agents[&cid]
        .agent_id
        .clone()
        .expect("durable agent id");
    let stats_count = |harness: &Harness| {
        event_log_events(harness)
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    Event::AgentStatsUpdated(stats) if stats.agent_id.as_str() == agent_id
                )
            })
            .count()
    };
    let stats_before = stats_count(&harness);
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::PROVIDER_TOOL_RESULT],
    );

    harness.finish_prebuilt_internal_tool_result(final_tool_result(
        call_id.as_str(),
        "skill",
        "loaded",
    ));
    assert!(harness.pending_intercept.is_some());
    assert_eq!(harness.peer_internal_tool_agents.get(&call_id), Some(&cid));
    assert!(!harness.tool_agents.contains_key(&call_id));
    assert_eq!(harness.agents[&cid].tools_in_flight, 1);
    assert_eq!(stats_count(&harness), stats_before);
    assert!(committed_terminal_events(&harness, call_id.as_str()).is_empty());

    reply(&mut harness, InterceptAction::Pass(None));

    assert!(harness.pending_intercept.is_none());
    assert!(!harness.peer_internal_tool_agents.contains_key(&call_id));
    assert!(!harness.tool_agents.contains_key(&call_id));
    assert_eq!(harness.agents[&cid].tools_in_flight, 0);
    assert_eq!(stats_count(&harness), stats_before + 1);
    let events = committed_terminal_events(&harness, call_id.as_str());
    assert!(matches!(
        events.as_slice(),
        [(_, Event::ProviderToolResult(_)), (_, Event::ToolResult(_)),]
    ));
    assert_eq!(
        event_log_events(&harness)
            .into_iter()
            .filter(|event| matches!(
                event,
                Event::ToolResultDisplay(display) if display.call_id == call_id
            ))
            .count(),
        1
    );
}

/// A truly uncorrelated peer request remains live while its canonical result is
/// parked, then clears only after commit.
#[test]
fn parked_uncorrelated_peer_result_clears_tracking_after_commit() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("seed-peer", "owned_tool");
    let call_id = ToolCallId::from("parked-peer");
    harness.peer_tool_requests.insert(call_id.clone());
    harness.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: ToolName::new("peer_tool"),
            internal_name: ToolName::new("peer_tool"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::PROVIDER_TOOL_RESULT],
    );

    harness.handle_extension_tool_result(
        crate::harness::harness_connection_id(),
        final_tool_result(call_id.as_str(), "peer_tool", "done"),
    );
    assert!(harness.pending_intercept.is_some());
    assert!(harness.peer_tool_requests.contains(&call_id));
    assert!(harness.pending_tools.contains_key(&call_id));
    assert!(committed_terminal_events(&harness, call_id.as_str()).is_empty());

    reply(&mut harness, InterceptAction::Pass(None));

    assert!(!harness.peer_tool_requests.contains(&call_id));
    assert!(!harness.pending_tools.contains_key(&call_id));
    assert!(matches!(
        committed_terminal_events(&harness, call_id.as_str()).as_slice(),
        [(_, Event::ProviderToolResult(_)), (_, Event::ToolResult(_)),]
    ));
}

/// A provider-terminal append error rejects that attempt without faulting the
/// live epoch; the same report may retry after the path recovers.
#[test]
fn provider_terminal_append_failure_remains_retryable() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("store-fault", "owned_tool");
    let cid = harness.tool_agents["store-fault"].clone();
    let tools_in_flight = harness.agents[&cid].tools_in_flight;
    let agent_id = harness.agents[&cid]
        .agent_id
        .as_deref()
        .expect("durable agent id")
        .to_owned();
    let journal = harness
        .state_dir
        .join("agents")
        .join(&agent_id)
        .join("events.cbor");
    let backup = journal.with_extension("cbor.test-backup");
    std::fs::rename(&journal, &backup).expect("park journal");
    std::fs::create_dir(&journal).expect("block journal path");

    let report =
        Event::ToolResultReported(final_tool_result("store-fault", "owned_tool", "result"));
    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report.clone()))
        .expect("raw report remains a bounded observation");

    assert!(harness.tool_agents.contains_key("store-fault"));
    assert_eq!(harness.agents[&cid].tools_in_flight, tools_in_flight);
    assert!(
        !committed_terminal_events(&harness, "store-fault")
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolResult(_)))
    );
    assert!(
        harness
            .agent_store
            .agent(&agent_id)
            .expect("live agent tree")
            .unresolved_foreground_tool_calls()
            .iter()
            .any(|call| call.call_id.as_str() == "store-fault")
    );

    std::fs::remove_dir(&journal).expect("remove journal blocker");
    std::fs::rename(&backup, &journal).expect("restore journal");
    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report))
        .expect("later observation remains bounded");
    assert!(harness.pending_intercept.is_none());
    assert!(harness.deferred_publishes.is_empty());
    assert!(harness.agents[&cid].pending_prompts.is_empty());
    let notices_before = event_log_events(&harness)
        .iter()
        .filter(|event| matches!(event, Event::HarnessNotice(_)))
        .count();
    harness.emit_info("nonsemantic publication remains live after append failure");
    assert_eq!(
        event_log_events(&harness)
            .iter()
            .filter(|event| matches!(event, Event::HarnessNotice(_)))
            .count(),
        notices_before + 1
    );
    assert!(!harness.tool_agents.contains_key("store-fault"));
    assert_eq!(harness.agents[&cid].tools_in_flight, tools_in_flight - 1);
    assert!(
        committed_terminal_events(&harness, "store-fault")
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolResult(_)))
    );
    assert!(
        harness
            .agent_store
            .agent_events(&agent_id)
            .expect("read restored agent journal")
            .iter()
            .any(|record| matches!(record.event, Event::ProviderToolResult(_)))
    );
}

/// Transcript validation rejection is not a physical or integrity storage
/// failure while later clean appends remain retryable.
#[test]
fn provider_terminal_validation_rejection_does_not_fail_stop() {
    let tmp = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(tmp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    harness.publish_for_agent(
        &cid,
        Event::ProviderToolResult(final_tool_result("missing-open-call", "missing", "invalid")),
    );

    let agent_id = harness.agents[&cid]
        .agent_id
        .clone()
        .expect("durable agent id");
    let records_before = harness
        .agent_store
        .agent_events(&agent_id)
        .expect("agent records")
        .len();
    harness.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: crate::parse_agent_id(&agent_id),
            head: tau_proto::AgentHead::Root,
        }),
    );
    assert_eq!(
        harness
            .agent_store
            .agent_events(&agent_id)
            .expect("agent records")
            .len(),
        records_before + 1
    );
}

/// A direct semantic append fault drops an already-deferred terminal instead of
/// letting it park or retaining it as an online continuation.
#[test]
fn direct_append_fault_discards_deferred_terminal() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("deferred-fault", "owned_tool");
    let cid = harness.tool_agents["deferred-fault"].clone();
    let agent_id = harness.agents[&cid]
        .agent_id
        .clone()
        .expect("durable agent id");
    intercept_terminal_names(&mut harness, vec![tau_proto::EventName::HARNESS_NOTICE]);
    harness.emit_info("park nonsemantic publication");
    assert!(harness.pending_intercept.is_some());

    assert_eq!(
        harness.publish_terminal_tool_result(
            Some(&cid),
            None,
            final_tool_result("deferred-fault", "owned_tool", "deferred"),
        ),
        TerminalSettlement::PostCommit
    );
    assert_eq!(harness.deferred_publishes.len(), 1);

    let journal = harness
        .state_dir
        .join("agents")
        .join(&agent_id)
        .join("events.cbor");
    let backup = journal.with_extension("cbor.direct-fault-backup");
    std::fs::rename(&journal, &backup).expect("park journal");
    std::fs::create_dir(&journal).expect("block journal");
    assert!(
        harness
            .record_accepted_visible_user_interaction(&agent_id)
            .is_err()
    );
    std::fs::remove_dir(&journal).expect("remove journal blocker");
    std::fs::rename(&backup, &journal).expect("restore journal");

    reply(&mut harness, InterceptAction::Pass(None));
    assert!(harness.pending_intercept.is_none());
    assert!(harness.deferred_publishes.is_empty());
    assert!(!harness.tool_agents.contains_key("deferred-fault"));
    harness.publish_terminal_tool_result(
        None,
        None,
        final_tool_result("ownerless-after-fault", "ownerless", "live"),
    );
    let ordered = event_log_events(&harness)
        .into_iter()
        .filter(|event| match event {
            Event::ProviderToolResult(result) | Event::ToolResult(result) => {
                result.call_id.as_str() == "ownerless-after-fault"
            }
            Event::ToolResultDisplay(result) => result.call_id.as_str() == "ownerless-after-fault",
            _ => false,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        ordered.as_slice(),
        [
            Event::ProviderToolResult(provider),
            Event::ToolResult(generic),
            Event::ToolResultDisplay(display),
        ] if provider.result == CborValue::Text("live".to_owned())
            && generic.result == provider.result
            && display.call_id == provider.call_id
    ));
    harness.publish_event(
        None,
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "ownerless-background-after-fault".into(),
            tool_name: ToolName::new("ownerless"),
            tool_type: tau_proto::ToolType::Function,
            message: "live".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    assert!(event_log_events(&harness).iter().any(|event| matches!(
        event,
        Event::ProviderToolResult(result)
            if result.call_id.as_str() == "ownerless-after-fault"
    )));
    let display = event_log_events(&harness)
        .into_iter()
        .find(|event| {
            matches!(
                event,
                Event::ToolResultDisplay(result)
                    if result.call_id.as_str() == "ownerless-after-fault"
            )
        })
        .expect("ownerless result publishes a UI display projection");
    let encoded = tau_proto::encode_message_to_vec(&display).expect("encode display event");
    assert!(
        !encoded
            .windows(b"live".len())
            .any(|window| window == b"live"),
        "UI display projection must not contain the raw result marker"
    );
    assert!(event_log_events(&harness).iter().any(|event| matches!(
        event,
        Event::ToolBackgroundError(error)
            if error.call_id.as_str() == "ownerless-background-after-fault"
    )));
    let session_id = harness.current_session_id.clone();
    let role = harness.selected_role.clone();
    assert!(
        harness
            .try_create_durable_user_agent(session_id, &role)
            .is_ok(),
        "later semantic work remains available"
    );
}

/// A rewritten result report commits before validation. Canonical UI/provider
/// projections use the harness source and reject both rewrite and Drop actions.
#[test]
fn result_report_replacement_drives_protected_canonical_projections() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("result-replaced", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![
            tau_proto::EventName::TOOL_RESULT_REPORTED,
            tau_proto::EventName::TOOL_RESULT,
            tau_proto::EventName::PROVIDER_TOOL_RESULT,
        ],
    );

    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "result-replaced",
                "owned_tool",
                "original",
            ))),
        )
        .expect("park result report");
    assert!(harness.tool_agents.contains_key("result-replaced"));
    assert!(committed_terminal_events(&harness, "result-replaced").is_empty());

    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ToolResultReported(
            final_tool_result("result-replaced", "owned_tool", "replacement"),
        )))),
    );
    assert!(
        harness.tool_agents.contains_key("result-replaced"),
        "provider-terminal interception must retain live call ownership"
    );
    assert!(matches!(
        harness
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::ProviderToolResult(_))
    ));

    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ProviderToolResult(
            final_tool_result("result-replaced", "owned_tool", "forged canonical"),
        )))),
    );
    assert!(
        !harness.tool_agents.contains_key("result-replaced"),
        "cleanup runs only after the provider terminal commits"
    );
    assert!(matches!(
        harness
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::ToolResult(_))
    ));
    reply(&mut harness, InterceptAction::Drop);

    assert!(matches!(
        committed_terminal_events(&harness, "result-replaced").as_slice(),
        [
            (Some(report_source), Event::ToolResultReported(report)),
            (Some(provider_source), Event::ProviderToolResult(provider)),
            (Some(result_source), Event::ToolResult(result)),
        ] if report_source == "conn-owner"
            && result_source == HARNESS_CONNECTION_ID
            && provider_source == HARNESS_CONNECTION_ID
            && matches!(&report.result, CborValue::Text(text) if text == "replacement")
            && result == report
            && provider == report
    ));
}

/// Dropping a mutable result report prevents cleanup and every canonical
/// projection.
#[test]
fn dropped_result_report_has_no_downstream_effect() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("result-dropped", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_RESULT_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "result-dropped",
                "owned_tool",
                "drop",
            ))),
        )
        .expect("park result report");
    reply(&mut harness, InterceptAction::Drop);

    assert!(committed_terminal_events(&harness, "result-dropped").is_empty());
    assert!(harness.tool_agents.contains_key("result-dropped"));
    assert_eq!(
        harness
            .pending_tool_providers
            .get("result-dropped")
            .map(tau_proto::ConnectionId::as_str),
        Some("conn-owner")
    );
}

/// A rewritten error report drives canonical failure projections, whose Drop
/// and rewrite actions cannot change the accepted failure.
#[test]
fn error_report_replacement_drives_protected_canonical_projections() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("error-replaced", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![
            tau_proto::EventName::TOOL_ERROR_REPORTED,
            tau_proto::EventName::TOOL_ERROR,
            tau_proto::EventName::PROVIDER_TOOL_ERROR,
        ],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolErrorReported(tool_error(
                "error-replaced",
                "owned_tool",
                "original",
            ))),
        )
        .expect("park error report");
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ToolErrorReported(tool_error(
            "error-replaced",
            "owned_tool",
            "replacement",
        ))))),
    );
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ProviderToolError(tool_error(
            "error-replaced",
            "owned_tool",
            "forged provider error",
        ))))),
    );
    reply(&mut harness, InterceptAction::Drop);

    assert!(matches!(
        committed_terminal_events(&harness, "error-replaced").as_slice(),
        [
            (Some(report_source), Event::ToolErrorReported(report)),
            (Some(provider_source), Event::ProviderToolError(provider)),
            (Some(error_source), Event::ToolError(error)),
        ] if report_source == "conn-owner"
            && error_source == HARNESS_CONNECTION_ID
            && provider_source == HARNESS_CONNECTION_ID
            && report.message == "replacement"
            && error == report
            && provider == report
    ));
}

/// Dropping a mutable error report leaves the routed call live.
#[test]
fn dropped_error_report_has_no_downstream_effect() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("error-dropped", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_ERROR_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolErrorReported(tool_error(
                "error-dropped",
                "owned_tool",
                "drop",
            ))),
        )
        .expect("park error report");
    reply(&mut harness, InterceptAction::Drop);

    assert!(committed_terminal_events(&harness, "error-dropped").is_empty());
    assert!(harness.tool_agents.contains_key("error-dropped"));
}

/// A rewritten cancellation report drives one protected harness-sourced
/// foreground cancellation.
#[test]
fn cancellation_report_replacement_drives_protected_canonical_fact() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("cancel-replaced", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![
            tau_proto::EventName::TOOL_CANCELLED_REPORTED,
            tau_proto::EventName::TOOL_CANCELLED,
        ],
    );
    let cancellation = |tool_name: &str| {
        Event::ToolCancelledReported(tau_proto::ToolCancelled {
            call_id: "cancel-replaced".into(),
            tool_name: tau_proto::ToolName::new(tool_name),
            tool_type: tau_proto::ToolType::Function,
        })
    };
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(cancellation("forged_original")),
        )
        .expect("park cancellation report");
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(cancellation("forged_replacement")))),
    );
    assert!(
        harness.tool_agents.contains_key("cancel-replaced"),
        "canonical cancellation parking must retain live call ownership"
    );
    assert!(
        !committed_terminal_events(&harness, "cancel-replaced")
            .iter()
            .any(|(_, event)| matches!(event, Event::ToolCancelled(_)))
    );
    reply(
        &mut harness,
        InterceptAction::Pass(Some(Box::new(Event::ToolCancelled(
            tau_proto::ToolCancelled {
                call_id: "cancel-replaced".into(),
                tool_name: tau_proto::ToolName::new("forged_canonical"),
                tool_type: tau_proto::ToolType::Function,
            },
        )))),
    );
    assert!(!harness.tool_agents.contains_key("cancel-replaced"));

    assert!(matches!(
        committed_terminal_events(&harness, "cancel-replaced").as_slice(),
        [
            (Some(report_source), Event::ToolCancelledReported(report)),
            (Some(canonical_source), Event::ToolCancelled(canonical)),
        ] if report_source == "conn-owner"
            && canonical_source == HARNESS_CONNECTION_ID
            && report.tool_name.as_str() == "forged_replacement"
            && canonical.tool_name.as_str() == "owned_tool"
    ));
}

/// Dropping a mutable cancellation report leaves the routed call live.
#[test]
fn dropped_cancellation_report_has_no_downstream_effect() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("cancel-dropped", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_CANCELLED_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolCancelledReported(tau_proto::ToolCancelled {
                call_id: "cancel-dropped".into(),
                tool_name: tau_proto::ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
            })),
        )
        .expect("park cancellation report");
    reply(&mut harness, InterceptAction::Drop);

    assert!(committed_terminal_events(&harness, "cancel-dropped").is_empty());
    assert!(harness.tool_agents.contains_key("cancel-dropped"));
}

/// Direct canonical spoofing, a non-Tool/Core report, and an unknown call all
/// fail at their respective authority/validation boundaries.
#[test]
fn terminal_report_authority_and_route_validation_fail_closed() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("authority-call", "owned_tool");
    connect_ready_configured_extension(
        &mut harness,
        "provider-peer",
        "configured-provider",
        tau_proto::ClientKind::Provider,
    );
    harness
        .handle_extension_event(
            "provider-peer",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "authority-call",
                "owned_tool",
                "wrong kind",
            ))),
        )
        .expect("reject wrong-kind report");
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResult(final_tool_result(
                "authority-call",
                "owned_tool",
                "forged canonical",
            ))),
        )
        .expect("reject direct canonical result");
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "unknown-call",
                "owned_tool",
                "unknown",
            ))),
        )
        .expect("commit unknown report");

    assert!(committed_terminal_events(&harness, "authority-call").is_empty());
    assert!(matches!(
        committed_terminal_events(&harness, "unknown-call").as_slice(),
        [(Some(source), Event::ToolResultReported(_))] if source == "conn-owner"
    ));
    assert!(harness.tool_agents.contains_key("authority-call"));
}

/// A parked stale configured generation may commit its report but cannot close
/// the current generation's route.
#[test]
fn stale_parked_generation_cannot_publish_terminal_canonical_fact() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("stale-result", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_RESULT_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "stale-result",
                "owned_tool",
                "stale",
            ))),
        )
        .expect("park stale result report");
    harness
        .extensions
        .entries
        .get_mut("conn-owner")
        .expect("owner generation")
        .instance_id = tau_proto::ExtensionInstanceId::new(43);
    reply(&mut harness, InterceptAction::Pass(None));

    assert!(matches!(
        committed_terminal_events(&harness, "stale-result").as_slice(),
        [(Some(source), Event::ToolResultReported(_))] if source == "conn-owner"
    ));
    assert!(harness.tool_agents.contains_key("stale-result"));
}

/// Disconnecting the captured source while its report is parked prevents
/// terminal state mutation even if interception later passes the report.
#[test]
fn disconnected_parked_source_cannot_publish_terminal_canonical_fact() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("disconnected-result", "owned_tool");
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_RESULT_REPORTED],
    );
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                "disconnected-result",
                "owned_tool",
                "stale",
            ))),
        )
        .expect("park disconnected result report");
    harness
        .extensions
        .entries
        .get_mut("conn-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Disconnected;
    reply(&mut harness, InterceptAction::Pass(None));

    assert!(matches!(
        committed_terminal_events(&harness, "disconnected-result").as_slice(),
        [(Some(source), Event::ToolResultReported(_))] if source == "conn-owner"
    ));
    assert!(harness.tool_agents.contains_key("disconnected-result"));
}

/// Pre-Ready terminal reports remain ordinary retained operational messages
/// with exact encoded-byte charging before activation drains them.
#[test]
fn pre_ready_terminal_report_preserves_retained_byte_accounting() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("retained-result", "owned_tool");
    harness
        .extensions
        .entries
        .get_mut("conn-owner")
        .expect("owner")
        .state = crate::extension::ExtensionState::Handshaking;
    let report = Event::ToolResultReported(final_tool_result(
        "retained-result",
        "owned_tool",
        "retained",
    ));
    let expected_bytes = Harness::encoded_emit_size(&report, false);
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Message(TestMessage::Emit(tau_proto::Emit {
                event: Box::new(report),
                persist: false,
            })),
        )
        .expect("retain pre-Ready report");
    let stage = &harness.extensions.activation_staging["conn-owner"];
    assert_eq!(stage.retained_message_count, 1);
    assert_eq!(stage.retained_message_bytes, expected_bytes);
    assert!(committed_terminal_events(&harness, "retained-result").is_empty());

    harness
        .handle_extension_message(
            &crate::test_connection_id("conn-owner"),
            TestMessage::Ready(Default::default()),
        )
        .expect("activate and drain report");
    assert!(matches!(
        committed_terminal_events(&harness, "retained-result").as_slice(),
        [
            (_, Event::ToolResultReported(_)),
            (Some(provider_source), Event::ProviderToolResult(_)),
            (Some(result_source), Event::ToolResult(_)),
        ] if result_source == HARNESS_CONNECTION_ID
            && provider_source == HARNESS_CONNECTION_ID
    ));
}

/// Result, error, and cancellation reports for backgrounded calls preserve the
/// existing real-background completion projections and never close provider
/// transcript state a second time.
#[test]
fn backgrounded_terminal_reports_preserve_background_completion_behavior() {
    for (suffix, report, expected_error) in [
        (
            "result",
            Event::ToolResultReported(final_tool_result("background-result", "owned_tool", "done")),
            None,
        ),
        (
            "error",
            Event::ToolErrorReported(tool_error("background-error", "owned_tool", "failed")),
            Some("failed"),
        ),
        (
            "cancel",
            Event::ToolCancelledReported(tau_proto::ToolCancelled {
                call_id: "background-cancel".into(),
                tool_name: tau_proto::ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
            }),
            Some("Tool cancelled"),
        ),
    ] {
        let call_id = format!("background-{suffix}");
        let (_tmp, mut harness) = setup_routed_test_tool_call(&call_id, "owned_tool");
        assert!(
            harness
                .tool_turn
                .begin_backgrounding(&call_id.clone().into())
        );
        assert!(harness.tool_turn.mark_backgrounded(&call_id.clone().into()));
        harness
            .handle_extension_event("conn-owner", TestProtocolItem::Event(report))
            .expect("commit background terminal report");
        let events = committed_terminal_events(&harness, &call_id);
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| matches!(
                    event,
                    Event::ToolResult(_)
                        | Event::ToolError(_)
                        | Event::ToolCancelled(_)
                        | Event::ProviderToolResult(_)
                        | Event::ProviderToolError(_)
                ))
                .count(),
            0,
            "background completion must not emit a second foreground terminal"
        );
        match expected_error {
            None => assert!(events.iter().any(|(source, event)| {
                source.as_deref() == Some(HARNESS_CONNECTION_ID)
                    && matches!(
                        event,
                        Event::ToolBackgroundResult(result)
                            if matches!(&result.result, CborValue::Text(text) if text == "done")
                    )
            })),
            Some(message) => assert!(events.iter().any(|(source, event)| {
                source.as_deref() == Some(HARNESS_CONNECTION_ID)
                    && matches!(
                        event,
                        Event::ToolBackgroundError(error) if error.message == message
                    )
            })),
        }
        assert!(!harness.tool_agents.contains_key(call_id.as_str()));
    }
}

/// A parked canonical background terminal must retain its runtime ownership
/// and preallocated identity until commit releases dependent completion work.
#[test]
fn parked_background_terminal_defers_cleanup_and_completion_prompt() {
    let call_id = ToolCallId::from("parked-background");
    let (_tmp, mut harness) = setup_routed_test_tool_call(call_id.as_str(), "owned_tool");
    let cid = harness.tool_agents[&call_id].clone();
    assert!(harness.tool_turn.begin_backgrounding(&call_id));
    assert!(harness.tool_turn.mark_backgrounded(&call_id));
    intercept_terminal_names(
        &mut harness,
        vec![tau_proto::EventName::TOOL_BACKGROUND_RESULT],
    );

    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                call_id.as_str(),
                "owned_tool",
                "done",
            ))),
        )
        .expect("park background terminal");

    assert!(harness.pending_intercept.is_some());
    assert!(harness.tool_agents.contains_key(&call_id));
    assert!(harness.pending_terminal_observations.contains_key(&call_id));
    assert!(harness.agents[&cid].pending_prompts.is_empty());

    reply(&mut harness, InterceptAction::Pass(None));
    assert!(harness.pending_intercept.is_none());
    assert!(!harness.tool_agents.contains_key(&call_id));
    assert!(!harness.pending_terminal_observations.contains_key(&call_id));
}

/// A failed canonical background append must leave runtime completion pending
/// so a later identical provider report can commit and settle it once.
#[test]
fn background_terminal_append_failure_remains_retryable() {
    let call_id = ToolCallId::from("background-store-fault");
    let (_tmp, mut harness) = setup_routed_test_tool_call(call_id.as_str(), "owned_tool");
    let cid = harness.tool_agents[&call_id].clone();
    let tools_in_flight = harness.agents[&cid].tools_in_flight;
    assert!(harness.tool_turn.begin_backgrounding(&call_id));
    assert!(harness.tool_turn.mark_backgrounded(&call_id));
    let agent_id = harness.agents[&cid]
        .agent_id
        .clone()
        .expect("durable agent");
    let journal = harness
        .state_dir
        .join("agents")
        .join(&agent_id)
        .join("events.cbor");
    let backup = journal.with_extension("cbor.background-test-backup");
    std::fs::rename(&journal, &backup).expect("park journal");
    std::fs::create_dir(&journal).expect("block journal path");
    let report =
        Event::ToolResultReported(final_tool_result(call_id.as_str(), "owned_tool", "done"));

    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report.clone()))
        .expect("raw report remains observable");
    assert!(harness.tool_agents.contains_key(&call_id));
    assert_eq!(harness.agents[&cid].tools_in_flight, tools_in_flight);
    assert!(harness.pending_terminal_observations.contains_key(&call_id));

    std::fs::remove_dir(&journal).expect("remove blocker");
    std::fs::rename(&backup, &journal).expect("restore journal");
    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report))
        .expect("retry report");
    assert!(!harness.tool_agents.contains_key(&call_id));
    assert_eq!(harness.agents[&cid].tools_in_flight, tools_in_flight - 1);
}

/// A competing disconnect terminal after an append failure must supersede the
/// losing completed classification instead of reusing its identity or cause.
#[test]
fn failed_result_then_disconnect_commits_fresh_disconnected_classification() {
    let call_id = ToolCallId::from("failed-result-disconnect");
    let (_tmp, mut harness) = setup_routed_test_tool_call(call_id.as_str(), "owned_tool");
    let cid = harness.tool_agents[&call_id].clone();
    let agent_id = harness.agents[&cid]
        .agent_id
        .clone()
        .expect("durable agent");
    let journal = harness
        .state_dir
        .join("agents")
        .join(&agent_id)
        .join("events.cbor");
    let backup = journal.with_extension("cbor.competing-test-backup");
    std::fs::rename(&journal, &backup).expect("park journal");
    std::fs::create_dir(&journal).expect("block journal path");
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                call_id.as_str(),
                "owned_tool",
                "done",
            ))),
        )
        .expect("failed result report remains bounded");
    let losing = harness.pending_terminal_observations[&call_id].observation_id;
    std::fs::remove_dir(&journal).expect("remove blocker");
    std::fs::rename(&backup, &journal).expect("restore journal");

    harness.fail_pending_tool_calls_for_connection(&crate::test_connection_id("conn-owner"));

    let records = harness
        .agent_store
        .agent_events(&agent_id)
        .expect("agent records");
    assert!(records.iter().any(|record| matches!(
        &record.event,
        Event::AgentToolTerminalClassified(classified)
            if classified.terminal != losing
                && classified.cause == tau_proto::ToolTerminalCause::ProviderDisconnected
    )));
}

/// A competing cancellation after an append failure must receive a fresh
/// terminal identity and retain the exact cancellation-request cause.
#[test]
fn failed_result_then_cancellation_commits_fresh_cancellation_classification() {
    let call_id = ToolCallId::from("failed-result-cancel");
    let (_tmp, mut harness) = setup_routed_test_tool_call(call_id.as_str(), "owned_tool");
    let cid = harness.tool_agents[&call_id].clone();
    let agent_id = harness.agents[&cid]
        .agent_id
        .clone()
        .expect("durable agent");
    let journal = harness
        .state_dir
        .join("agents")
        .join(&agent_id)
        .join("events.cbor");
    let backup = journal.with_extension("cbor.cancel-test-backup");
    std::fs::rename(&journal, &backup).expect("park journal");
    std::fs::create_dir(&journal).expect("block journal path");
    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolResultReported(final_tool_result(
                call_id.as_str(),
                "owned_tool",
                "done",
            ))),
        )
        .expect("failed result report remains bounded");
    let losing = harness.pending_terminal_observations[&call_id].observation_id;
    std::fs::remove_dir(&journal).expect("remove blocker");
    std::fs::rename(&backup, &journal).expect("restore journal");
    let request = tau_proto::ObservationId::from_bytes([44; 16]);
    harness
        .pending_cancellation_observations
        .insert(call_id.clone(), request);

    harness
        .handle_extension_event(
            "conn-owner",
            TestProtocolItem::Event(Event::ToolCancelledReported(tau_proto::ToolCancelled {
                call_id: call_id.clone(),
                tool_name: tau_proto::ToolName::new("owned_tool"),
                tool_type: tau_proto::ToolType::Function,
            })),
        )
        .expect("cancellation report");

    let records = harness
        .agent_store
        .agent_events(&agent_id)
        .expect("agent records");
    assert!(records.iter().any(|record| matches!(
        &record.event,
        Event::AgentToolTerminalClassified(classified)
            if classified.terminal != losing
                && classified.cause == tau_proto::ToolTerminalCause::Cancellation { request }
    )));
}

/// A duplicate report remains an observable peer report, but completed-call
/// tracking prevents a second canonical result/provider projection.
#[test]
fn duplicate_result_report_cannot_repeat_terminal_cleanup_or_projection() {
    let (_tmp, mut harness) = setup_routed_test_tool_call("duplicate-result", "owned_tool");
    let report =
        Event::ToolResultReported(final_tool_result("duplicate-result", "owned_tool", "done"));
    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report.clone()))
        .expect("first report");
    harness
        .handle_extension_event("conn-owner", TestProtocolItem::Event(report))
        .expect("duplicate report");

    let events = committed_terminal_events(&harness, "duplicate-result");
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(event, Event::ToolResultReported(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|(_, event)| matches!(
                event,
                Event::ToolResult(_) | Event::ProviderToolResult(_)
            ))
            .count(),
        2,
        "one UI result plus one provider result"
    );
}

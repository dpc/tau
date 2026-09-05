use std::collections::VecDeque;

use super::*;
use crate::harness::operator_agent_unload::PendingOperatorUnload;
use crate::harness::standalone_execution_accounting_state::StandaloneExecutionAccountingOwner;

fn request(agent_id: tau_proto::AgentId) -> tau_proto::UnloadSessionAgent {
    tau_proto::UnloadSessionAgent {
        request_id: "unload-1".to_owned(),
        session_id: "s1".parse().expect("valid session id"),
        agent_id,
    }
}

fn received_event(recipient_id: tau_proto::AgentId) -> Event {
    Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("unload-race-message").expect("message id"),
        sender_id: crate::parse_agent_id("sender"),
        sender_session_id: None,
        recipient_id,
        kind: tau_proto::AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: "accepted before unload".to_owned(),
    })
}

fn assert_busy_with(setup: impl FnOnce(&mut Harness, &AgentId, &tau_proto::AgentId)) {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    setup(&mut harness, &cid, &agent_id);
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let request = request(agent_id.clone());
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(request.clone()),
        )
        .expect("request");
    assert_eq!(
        directed_outcome(&frames, &request),
        Some(tau_proto::UnloadSessionAgentOutcome::AgentBusy)
    );
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
    assert!(event_log_events(&harness).iter().all(|event| !matches!(
        event,
        Event::SessionAgentUnloaded(_)
            | Event::AgentPromptTerminated(_)
            | Event::ToolCancelled(_)
            | Event::AgentStartFailed(_)
            | Event::AgentManualCompactionRequestFailed(_)
    )));
}

fn directed_outcome(
    frames: &Arc<Mutex<Vec<RoutedFrame>>>,
    expected: &tau_proto::UnloadSessionAgent,
) -> Option<tau_proto::UnloadSessionAgentOutcome> {
    frames
        .lock()
        .expect("frames")
        .iter()
        .rev()
        .find_map(|frame| match &frame.frame {
            HarnessOutputMessage::UnloadSessionAgentResult(result)
                if result.request_id == expected.request_id
                    && result.session_id == expected.session_id
                    && result.agent_id == expected.agent_id =>
            {
                Some(result.outcome)
            }
            _ => None,
        })
}

/// An idle durable target commits one ordinary unload and retires only its live
/// route.
#[test]
fn idle_saved_agent_unload_commits_and_preserves_history() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let observer = connect_test_client_with_origin(
        &mut harness,
        "observer",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    harness
        .handle_client_message(
            &crate::test_connection_id("observer"),
            HarnessInputMessage::Subscribe(Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SESSION_AGENT_UNLOADED,
                )],
            }),
        )
        .expect("observer subscription");
    let extension = connect_ready_configured_extension(
        &mut harness,
        "configured-observer",
        "configured-observer",
        tau_proto::ClientKind::Tool,
    );
    harness
        .handle_extension_event(
            "configured-observer",
            TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SESSION_AGENT_UNLOADED,
                )],
            })),
        )
        .expect("extension subscription");
    harness
        .agent_runtime
        .agent_runtime_indicators
        .entry(crate::test_connection_id("timer"))
        .or_default()
        .entry(agent_id.clone())
        .or_default()
        .insert(tau_proto::AgentRuntimeIndicator::TimerScheduled);
    harness
        .peer_messaging
        .peer_input_rate
        .insert(agent_id.clone(), VecDeque::from([Instant::now()]));
    let cache_clear_baseline = harness.provider_runtime.cache_refresh_clear_count;

    let request = request(agent_id.clone());
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(request.clone()),
        )
        .expect("unload request");

    assert_eq!(
        directed_outcome(&frames, &request),
        Some(tau_proto::UnloadSessionAgentOutcome::Unloaded)
    );
    assert_eq!(
        harness.provider_runtime.cache_refresh_clear_count,
        cache_clear_baseline + 1
    );
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .unload_result_after_retirement
    );
    assert!(
        !harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .roster_durable_ever_loaded
            .contains(&agent_id)
    );
    assert!(
        !harness
            .agent_runtime
            .agent_registry
            .agents
            .contains_key(&cid)
    );
    assert!(
        harness
            .agent_runtime
            .agent_runtime_indicators
            .values()
            .all(|by_agent| !by_agent.contains_key(&agent_id))
    );
    assert!(
        !harness
            .peer_messaging
            .peer_input_rate
            .contains_key(&agent_id)
    );
    let observer = observer.lock().expect("observer frames");
    assert!(observer.iter().any(|frame| matches!(
        &frame.frame,
        HarnessOutputMessage::Deliver(delivery)
            if matches!(delivery.event(), Event::SessionAgentUnloaded(_))
    )));
    assert!(observer.iter().all(|frame| !matches!(
        frame.frame,
        HarnessOutputMessage::UnloadSessionAgentResult(_)
    )));
    let extension = extension.lock().expect("extension frames");
    assert!(extension.iter().any(|frame| matches!(
        &frame.frame,
        HarnessOutputMessage::Deliver(delivery)
            if matches!(delivery.event(), Event::SessionAgentUnloaded(_))
    )));
    assert!(extension.iter().all(|frame| !matches!(
        frame.frame,
        HarnessOutputMessage::UnloadSessionAgentResult(_)
    )));
}

/// Accepted queued work rejects unload without changing membership or runtime
/// admission.
#[test]
fn queued_prompt_makes_saved_agent_busy_without_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    harness
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("busy".to_owned()));
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );

    let request = request(agent_id.clone());
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(request.clone()),
        )
        .expect("unload request");

    assert_eq!(
        directed_outcome(&frames, &request),
        Some(tau_proto::UnloadSessionAgentOutcome::AgentBusy)
    );
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
    assert!(
        !harness
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .terminating
    );
}

/// Semantic admission rejection restores the exact route and returns a typed
/// rollback outcome.
#[test]
fn rejected_unload_restores_live_runtime() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let cache_clear_baseline = harness.provider_runtime.cache_refresh_clear_count;
    reject_next_semantic_admission(&harness);

    let request = request(agent_id.clone());
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(request.clone()),
        )
        .expect("unload request");

    assert_eq!(
        directed_outcome(&frames, &request),
        Some(tau_proto::UnloadSessionAgentOutcome::TransitionRejected)
    );
    assert_eq!(
        harness.provider_runtime.cache_refresh_clear_count,
        cache_clear_baseline
    );
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .pending_operator_unloads
            .is_empty()
    );
    assert!(
        !harness
            .agent_runtime
            .agent_watch
            .expected_unloads
            .contains(agent_id.as_str())
    );
    assert!(
        event_log_events(&harness)
            .iter()
            .all(|event| !matches!(event, Event::SessionAgentUnloaded(_)))
    );
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
    assert_eq!(
        harness
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(&agent_id),
        Some(&cid)
    );
    assert!(
        !harness
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("agent")
            .dispatch
            .terminating
    );
}

/// Classification follows the stable precedence before any live-route mutation.
#[test]
fn unload_outcome_precedence_covers_roster_and_route_states() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let durable = durable_agent_id_for_conversation(&harness, &cid);
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let submit = |harness: &mut Harness, request: tau_proto::UnloadSessionAgent, expected| {
        harness
            .handle_client_message(
                &crate::test_connection_id("operator"),
                HarnessInputMessage::UnloadSessionAgent(request.clone()),
            )
            .expect("request");
        assert_eq!(directed_outcome(&frames, &request), Some(expected));
    };

    let unknown = crate::parse_agent_id("unknown");
    let mut stale = request(unknown.clone());
    stale.session_id = "other".parse().expect("session");
    harness.agent_runtime.agent_registry.roster_valid = false;
    submit(
        &mut harness,
        stale,
        tau_proto::UnloadSessionAgentOutcome::StaleSession,
    );
    submit(
        &mut harness,
        request(unknown.clone()),
        tau_proto::UnloadSessionAgentOutcome::MembershipUnavailable,
    );
    harness.agent_runtime.agent_registry.roster_valid = true;
    submit(
        &mut harness,
        request(unknown),
        tau_proto::UnloadSessionAgentOutcome::AgentNotFound,
    );

    let ephemeral = crate::parse_agent_id("ephemeral");
    harness
        .agent_runtime
        .agent_registry
        .roster_ever_loaded
        .insert(ephemeral.clone());
    harness
        .agent_runtime
        .agent_registry
        .roster_loaded
        .insert(ephemeral.clone());
    submit(
        &mut harness,
        request(ephemeral),
        tau_proto::UnloadSessionAgentOutcome::UnsupportedEphemeral,
    );

    harness
        .agent_runtime
        .agent_registry
        .roster_loaded
        .remove(&durable);
    submit(
        &mut harness,
        request(durable.clone()),
        tau_proto::UnloadSessionAgentOutcome::AlreadyUnloaded,
    );
    harness
        .agent_runtime
        .agent_registry
        .roster_loaded
        .insert(durable.clone());
    harness
        .agent_runtime
        .agent_registry
        .pending_operator_unloads
        .insert(
            durable.clone(),
            PendingOperatorUnload {
                requester: crate::test_connection_id("operator"),
                request_id: "prior".to_owned(),
                cid: cid.clone(),
                watch_was_expected: false,
            },
        );
    submit(
        &mut harness,
        request(durable.clone()),
        tau_proto::UnloadSessionAgentOutcome::AlreadyUnloading,
    );
    harness
        .agent_runtime
        .agent_registry
        .pending_operator_unloads
        .remove(&durable);
    harness
        .agent_runtime
        .agent_registry
        .agent_routes
        .remove(&durable);
    submit(
        &mut harness,
        request(durable),
        tau_proto::UnloadSessionAgentOutcome::AgentUnavailable,
    );
}

/// Only an authenticated attached socket UI can invoke the lifecycle control
/// RPC.
#[test]
fn unload_rpc_rejects_non_socket_and_non_ui_connections_silently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let memory_ui = connect_test_client(&mut harness, "memory-ui", tau_proto::ClientKind::Ui);
    let socket_tool = connect_test_client_with_origin(
        &mut harness,
        "socket-tool",
        tau_proto::ClientKind::Tool,
        ConnectionOrigin::Socket,
    );
    let request = request(agent_id.clone());
    for peer in ["memory-ui", "socket-tool"] {
        harness
            .handle_client_message(
                &crate::test_connection_id(peer),
                HarnessInputMessage::UnloadSessionAgent(request.clone()),
            )
            .expect("ignored request");
    }
    assert!(memory_ui.lock().expect("frames").is_empty());
    assert!(socket_tool.lock().expect("frames").is_empty());
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
}

/// Shutdown state takes precedence over every otherwise-valid membership
/// classification.
#[test]
fn shutdown_makes_membership_unavailable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    harness.ui_runtime.shutdown_requested = true;
    let request = request(agent_id);
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(request.clone()),
        )
        .expect("request");
    assert_eq!(
        directed_outcome(&frames, &request),
        Some(tau_proto::UnloadSessionAgentOutcome::MembershipUnavailable)
    );
}

/// A target-addressed receive parked in interception is accepted work and
/// blocks unload.
#[test]
fn parked_agent_receive_makes_target_busy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_MESSAGE_RECEIVED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");
    harness.publish_event(None, received_event(agent_id.clone()));
    assert!(harness.runtime_io.publication.pending_intercept.is_some());
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let request = request(agent_id.clone());
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(request.clone()),
        )
        .expect("request");
    assert_eq!(
        directed_outcome(&frames, &request),
        Some(tau_proto::UnloadSessionAgentOutcome::AgentBusy)
    );
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
}

/// A parked unload owns one reservation, rejects duplicates, and completes only
/// after commit.
#[test]
fn parked_unload_is_idempotent_until_interceptor_releases_commit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SESSION_AGENT_UNLOADED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");
    let frames = connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    let first = request(agent_id.clone());
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(first.clone()),
        )
        .expect("first");
    assert_eq!(directed_outcome(&frames, &first), None);
    harness
        .handle_authenticated_ui_prompt_submitted(
            crate::harness::harness_connection_id(),
            tau_proto::UiPromptSubmitted {
                literal: false,
                session_id: harness.session_runtime.current_session_id.clone(),
                text: "must reject after reservation".to_owned(),
                agent_id: agent_id.clone(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: Some("post-reservation".to_owned()),
            },
        )
        .expect("terminating target rejection");
    assert!(
        harness
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .expect("reserved runtime")
            .dispatch
            .pending_prompts
            .is_empty()
    );
    let mut second = request(agent_id.clone());
    second.request_id = "unload-2".to_owned();
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(second.clone()),
        )
        .expect("second");
    assert_eq!(
        directed_outcome(&frames, &second),
        Some(tau_proto::UnloadSessionAgentOutcome::AlreadyUnloading)
    );
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release unload");
    assert_eq!(
        directed_outcome(&frames, &first),
        Some(tau_proto::UnloadSessionAgentOutcome::Unloaded)
    );
    assert!(
        !harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
    assert_eq!(
        event_log_events(&harness)
            .iter()
            .filter(|event| matches!(event, Event::SessionAgentUnloaded(_)))
            .count(),
        1
    );
}

/// Disconnecting the requester cannot retract an unload already parked in
/// publication.
#[test]
fn requester_disconnect_does_not_retract_parked_unload() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut harness = quiet_provider_harness(temp.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = durable_agent_id_for_conversation(&harness, &cid);
    connect_test_tool(&mut harness, "interceptor");
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::SESSION_AGENT_UNLOADED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register interceptor");
    connect_test_client_with_origin(
        &mut harness,
        "operator",
        tau_proto::ClientKind::Ui,
        ConnectionOrigin::Socket,
    );
    harness
        .handle_client_message(
            &crate::test_connection_id("operator"),
            HarnessInputMessage::UnloadSessionAgent(request(agent_id.clone())),
        )
        .expect("request");
    harness.handle_disconnect(&crate::test_connection_id("operator"));
    harness
        .handle_extension_event(
            "interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release");
    assert!(
        !harness
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&agent_id)
    );
}

/// Distinct teardown owner families all flow through the same no-discard
/// classifier.
#[test]
fn classifier_rejects_tool_compaction_startup_and_accounting_owners() {
    assert_busy_with(|harness, cid, _| {
        harness
            .tool_routing
            .tool_runtime
            .tool_agents
            .insert(ToolCallId::from("busy-tool"), cid.clone());
    });
    assert_busy_with(|harness, _, agent_id| {
        harness
            .prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .insert((
                agent_id.clone(),
                tau_proto::CompactionTransactionId::parse("busy-compaction").expect("transaction"),
            ));
    });
    assert_busy_with(|harness, cid, _| {
        harness
            .agent_runtime
            .agent_registry
            .start_coordinator
            .agents
            .insert(cid.clone(), tau_proto::StartOperationId(1));
    });
    assert_busy_with(|harness, cid, agent_id| {
        harness
            .prompt_coordination
            .standalone_accounting
            .owners
            .insert(
                "busy-accounting".parse().expect("prompt id"),
                StandaloneExecutionAccountingOwner {
                    session_id: harness.session_runtime.current_session_id.clone(),
                    agent_id: agent_id.clone(),
                    cid: cid.clone(),
                    transaction_id: tau_proto::CompactionTransactionId::parse(
                        "accounting-transaction",
                    )
                    .expect("transaction"),
                    model: "test/model".into(),
                    estimated_cost_rates: tau_proto::ESTIMATED_API_COST_FALLBACK,
                },
            );
    });
}

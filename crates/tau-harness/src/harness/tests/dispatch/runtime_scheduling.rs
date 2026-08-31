//! Tests for runtime scheduling behavior.

use super::*;
use crate::harness::publication_completion::StartPersistenceFailureScope;
use crate::harness::start_coordinator::{
    MAX_START_QUERY_ID_BYTES, MAX_START_RETAINED_BYTES, StartPhase, StartPhaseOwner,
};

/// Park acceptance so the coordinator's count bound is exercised without
/// advancing or freeing any operation.
#[test]
fn start_coordinator_enforces_operation_count_bound() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "acceptance-bound-interceptor");
    h.handle_extension_event(
        "acceptance-bound-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_START_ACCEPTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");

    for index in 0..65 {
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query(&format!("bounded-{index}")),
        )
        .expect("admit bounded request");
    }

    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert_eq!(
        coordinator.operations.len(),
        crate::harness::start_coordinator::MAX_START_OPERATIONS
    );
    assert_eq!(coordinator.requests.len(), coordinator.operations.len());
    assert_eq!(coordinator.agents.len(), coordinator.operations.len());
    assert!(
        coordinator.retained_bytes <= crate::harness::start_coordinator::MAX_START_RETAINED_BYTES
    );
    assert!(
        !coordinator
            .requests
            .keys()
            .any(|(_, query_id)| query_id == "bounded-64")
    );
}

/// Parked acceptances reserve their minted ids, so a collision-prone template
/// cannot overwrite the agent-to-operation index.
#[test]
fn start_coordinator_reserves_agent_ids_before_acceptance_commits() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    h.config.agent_id_template = "fixed-agent".to_owned();
    let _interceptor = connect_test_tool(&mut h, "acceptance-id-interceptor");
    h.handle_extension_event(
        "acceptance-id-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_START_ACCEPTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");

    for query_id in ["collision-a", "collision-b"] {
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query(query_id),
        )
        .expect("admit colliding request");
    }

    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert_eq!(coordinator.operations.len(), 2);
    assert_eq!(coordinator.requests.len(), 2);
    assert_eq!(coordinator.agents.len(), 2);
    let mut ids = coordinator.agents.keys().collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 2);
}

/// Parked starts cannot retain more than the aggregate startup payload budget.
#[test]
fn start_coordinator_enforces_aggregate_payload_bound() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "acceptance-byte-interceptor");
    h.handle_extension_event(
        "acceptance-byte-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_START_ACCEPTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");

    for index in 0..5 {
        let mut query = ext_query(&format!("payload-{index}"));
        query.instruction = "x".repeat(1024 * 1024);
        h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), query)
            .expect("admit bounded request");
    }

    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.len() < 5);
    assert_eq!(coordinator.requests.len(), coordinator.operations.len());
    assert_eq!(coordinator.agents.len(), coordinator.operations.len());
    assert!(
        coordinator.retained_bytes <= crate::harness::start_coordinator::MAX_START_RETAINED_BYTES
    );
}

/// Prompt commit drops the charged backing allocations before making the 4 MiB
/// budget reusable by another parked startup.
#[test]
fn startup_prompt_commit_releases_physical_payload_before_budget_reuse() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "physical-bound-interceptor");
    h.handle_extension_event(
        "physical-bound-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register checkpoint interceptor");
    let payload_bytes = MAX_START_RETAINED_BYTES - 64 * 1024;
    let mut first = ext_query("physical-first");
    first.instruction = "x".repeat(payload_bytes);
    first.task_name = Some("charged task".repeat(128));
    h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), first)
        .expect("park first checkpoint");

    let first_start_id = *h
        .agent_runtime
        .agent_registry
        .start_coordinator
        .operations
        .keys()
        .next()
        .expect("first operation");
    let first_operation =
        &h.agent_runtime.agent_registry.start_coordinator.operations[&first_start_id];
    assert_eq!(first_operation.phase, StartPhase::AwaitDispatchCommit);
    assert_eq!(first_operation.retained_bytes, 0);
    assert_eq!(first_operation.pending.query.instruction.capacity(), 0);
    assert_eq!(
        first_operation
            .pending
            .query
            .trusted_internal_spans
            .capacity(),
        0
    );
    assert!(first_operation.pending.query.task_name.is_none());
    assert!(first_operation.pending.query.tool_call_id.is_none());
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .start_coordinator
            .retained_bytes,
        0
    );

    let mut second = ext_query("physical-second");
    second.instruction = "y".repeat(payload_bytes);
    let second = h
        .prepare_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), second)
        .expect("prepare second")
        .expect("new second request");
    h.begin_start_operation(second);
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert_eq!(coordinator.operations.len(), 2);
    assert!(coordinator.retained_bytes <= MAX_START_RETAINED_BYTES);
    assert!(coordinator.retained_bytes > payload_bytes);
    assert_eq!(
        coordinator.operations[&first_start_id]
            .pending
            .query
            .instruction
            .capacity(),
        0
    );
}

/// Oversized requester correlation is rejected before reserving an id or
/// touching startup storage.
#[test]
fn start_coordinator_rejects_oversized_query_before_reservation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let query_id = "q".repeat(MAX_START_QUERY_ID_BYTES + 1);
    let mut query = ext_query(&query_id);
    query.instruction = "never retained".to_owned();

    h.handle_start_agent_request(&crate::test_connection_id(HARNESS_CONNECTION_ID), query)
        .expect("reject oversized query");

    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// Once `AgentStarted` installs the live runtime, agent-list projection must
/// replace—not append to—the accepted placeholder row.
#[test]
fn agent_list_deduplicates_placeholder_after_agent_started() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "loaded-list-interceptor");
    h.handle_extension_event(
        "loaded-list-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::SESSION_AGENT_LOADED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register membership interceptor");
    h.handle_start_agent_request(
        &crate::test_connection_id(HARNESS_CONNECTION_ID),
        ext_query("agent-list-dedup"),
    )
    .expect("start");
    let agent_id = event_log_events(&h)
        .iter()
        .find_map(|event| match event {
            Event::StartAgentAccepted(accepted) if accepted.query_id == "agent-list-dedup" => {
                Some(accepted.agent_id.clone())
            }
            _ => None,
        })
        .expect("accepted id");

    assert_eq!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .filter(|agent| agent.identity.agent_id.as_ref() == Some(&agent_id))
            .count(),
        1
    );
    assert!(
        h.pending_agent_summary_data()
            .iter()
            .all(|(pending_id, _)| pending_id != agent_id.as_str())
    );
}

/// Synthetic owner and stream failures target only the exact durable owner,
/// session, and prepared generation across every startup phase.
#[test]
fn persistence_failures_target_exact_owner_generation_and_mixed_phases() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let owner_epoch = h
        .session_runtime
        .persistence_owner
        .as_ref()
        .expect("durable owner")
        .owner_epoch();
    let mut inserted = Vec::new();
    for (index, phase) in [
        StartPhase::AwaitAcceptedCommit,
        StartPhase::AwaitStartedCommit,
        StartPhase::AwaitLoadedCommit,
        StartPhase::AwaitPromptCommit,
        StartPhase::AwaitDispatchCommit,
    ]
    .into_iter()
    .enumerate()
    {
        let pending = h
            .prepare_start_agent_request(
                &crate::test_connection_id(HARNESS_CONNECTION_ID),
                ext_query(&format!("owner-phase-{index}")),
            )
            .expect("prepare")
            .expect("new request");
        let agent_id = pending.cid.clone();
        let start_id = h.insert_start_operation_for_test(
            pending,
            phase,
            true,
            Some(owner_epoch),
            (2 <= index).then_some(10 + index as u64),
        );
        inserted.push((start_id, agent_id, phase));
    }
    let ephemeral_pending = h
        .prepare_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("owner-ephemeral"),
        )
        .expect("prepare")
        .expect("new request");
    let ephemeral_id = h.insert_start_operation_for_test(
        ephemeral_pending,
        StartPhase::AwaitDispatchCommit,
        false,
        None,
        None,
    );
    let stale_pending = h
        .prepare_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("owner-stale"),
        )
        .expect("prepare")
        .expect("new request");
    let stale_agent_id = stale_pending.cid.clone();
    let stale_id = h.insert_start_operation_for_test(
        stale_pending,
        StartPhase::AwaitDispatchCommit,
        true,
        Some(owner_epoch + 1),
        Some(77),
    );

    let unprepared_agent_id = inserted
        .iter()
        .find_map(|(_, agent_id, phase)| {
            (*phase == StartPhase::AwaitStartedCommit).then_some(agent_id.clone())
        })
        .expect("accepted but unprepared start");
    h.fail_start_operations_for_persistence([StartPersistenceFailureScope::Agent {
        agent_id: unprepared_agent_id,
        owner_epoch,
        generation: 999,
    }]);
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .len(),
        7,
        "an unprepared start cannot match a stream-local diagnostic"
    );
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStartFailed(_)))
    );

    h.fail_start_operations_for_persistence([StartPersistenceFailureScope::OwnerExit {
        owner_epoch,
    }]);

    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert_eq!(coordinator.operations.len(), 2);
    assert!(coordinator.operations.contains_key(&ephemeral_id));
    assert!(coordinator.operations.contains_key(&stale_id));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentStartFailed(_)))
            .count(),
        4,
        "preaccept rejects without a failure; every accepted durable phase terminalizes"
    );
    for (start_id, _, _) in &inserted {
        assert!(!coordinator.operations.contains_key(start_id));
    }

    h.fail_start_operations_for_persistence([StartPersistenceFailureScope::Agent {
        agent_id: stale_agent_id.clone(),
        owner_epoch,
        generation: 76,
    }]);
    assert!(
        h.agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .contains_key(&stale_id)
    );
    h.fail_start_operations_for_persistence([StartPersistenceFailureScope::Agent {
        agent_id: stale_agent_id.clone(),
        owner_epoch,
        generation: 77,
    }]);
    assert!(
        h.agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .contains_key(&stale_id),
        "a stream failure from a different persistence owner must not terminalize the start"
    );
    h.fail_start_operations_for_persistence([StartPersistenceFailureScope::Agent {
        agent_id: stale_agent_id,
        owner_epoch: owner_epoch + 1,
        generation: 77,
    }]);
    assert!(
        !h.agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .contains_key(&stale_id)
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .contains_key(&ephemeral_id)
    );
    h.begin_start_failure(ephemeral_id, tau_proto::AgentStartFailure::Canceled);
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// Cancellation owns every startup cut, including the preaccept rejection cut,
/// without leaving a second terminal owner or retained payload.
#[test]
fn cancellation_terminalizes_every_startup_phase_exactly_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let mut starts = Vec::new();
    for (index, phase) in [
        StartPhase::AwaitAcceptedCommit,
        StartPhase::AwaitStartedCommit,
        StartPhase::AwaitLoadedCommit,
        StartPhase::AwaitPromptCommit,
        StartPhase::AwaitDispatchCommit,
    ]
    .into_iter()
    .enumerate()
    {
        let pending = h
            .prepare_start_agent_request(
                &crate::test_connection_id(HARNESS_CONNECTION_ID),
                ext_query(&format!("cancel-phase-{index}")),
            )
            .expect("prepare")
            .expect("new request");
        let start_id = h.insert_start_operation_for_test(pending, phase, false, None, None);
        starts.push((start_id, phase));
    }

    for (start_id, phase) in starts {
        if phase == StartPhase::AwaitAcceptedCommit {
            h.abort_preaccept_start(start_id, tau_proto::AgentStartFailure::Canceled);
        } else {
            h.begin_start_failure(start_id, tau_proto::AgentStartFailure::Canceled);
        }
    }

    let failures = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStartFailed(failed) => Some(failed),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 4);
    assert!(
        failures
            .iter()
            .all(|failed| failed.reason == tau_proto::AgentStartFailure::Canceled)
    );
    for phase in [
        tau_proto::AgentStartPhase::AgentStarted,
        tau_proto::AgentStartPhase::SessionAgentLoaded,
        tau_proto::AgentStartPhase::AgentPromptSubmitted,
        tau_proto::AgentStartPhase::AgentInferenceDispatchStarted,
    ] {
        assert_eq!(
            failures
                .iter()
                .filter(|failed| failed.phase == phase)
                .count(),
            1
        );
    }
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// Clean shutdown rejects a private acceptance and emits one live
/// `SessionStopped` terminal for every already-accepted phase.
#[test]
fn session_shutdown_terminalizes_every_startup_phase_exactly_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    for (index, phase) in [
        StartPhase::AwaitAcceptedCommit,
        StartPhase::AwaitStartedCommit,
        StartPhase::AwaitLoadedCommit,
        StartPhase::AwaitPromptCommit,
        StartPhase::AwaitDispatchCommit,
    ]
    .into_iter()
    .enumerate()
    {
        let pending = h
            .prepare_start_agent_request(
                &crate::test_connection_id(HARNESS_CONNECTION_ID),
                ext_query(&format!("shutdown-phase-{index}")),
            )
            .expect("prepare")
            .expect("new request");
        h.insert_start_operation_for_test(pending, phase, false, None, None);
    }

    h.fail_start_operations_for_session_shutdown();

    let failures = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStartFailed(failed) => Some(failed),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 4);
    assert!(
        failures
            .iter()
            .all(|failed| failed.reason == tau_proto::AgentStartFailure::SessionStopped)
    );
    for phase in [
        tau_proto::AgentStartPhase::AgentStarted,
        tau_proto::AgentStartPhase::SessionAgentLoaded,
        tau_proto::AgentStartPhase::AgentPromptSubmitted,
        tau_proto::AgentStartPhase::AgentInferenceDispatchStarted,
    ] {
        assert_eq!(
            failures
                .iter()
                .filter(|failed| failed.phase == phase)
                .count(),
            1
        );
    }
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// A committed creation fact whose runtime installation loses its authenticated
/// parent closes with one creation-worker terminal and exposes no route.
#[test]
fn committed_agent_started_runtime_install_failure_terminalizes_without_route() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let mut pending = h
        .prepare_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("forced-install-failure"),
        )
        .expect("prepare")
        .expect("new request");
    pending.query.tool_call_id = Some(tau_proto::ToolCallId::new("missing-parent-tool"));
    pending.parent_cid = Some(crate::parse_agent_id("missing-parent"));
    let agent_id = pending.cid.clone();
    let start_id = h.insert_start_operation_for_test(
        pending,
        StartPhase::AwaitStartedCommit,
        false,
        None,
        None,
    );
    let event = Event::AgentStarted(tau_proto::AgentStarted {
        creator: None,
        agent_id: agent_id.clone(),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    });

    h.commit_start_phase(
        StartPhaseOwner {
            start_id,
            expected_phase: StartPhase::AwaitStartedCommit,
            expected_event: tau_proto::EventName::AGENT_STARTED,
        },
        &event,
        None,
    );

    assert!(
        !h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&agent_id)
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentStartFailed(failed)
                if failed.start_id == start_id
                    && failed.reason == tau_proto::AgentStartFailure::CreationWorker))
            .count(),
        1
    );
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// A stream-local failure and canonical startup-prompt commit are serialized by
/// the central loop: failure-first prevents prompt/dispatch, while prompt and
/// checkpoint success make the later diagnostic stale.
#[test]
fn stream_failure_races_startup_prompt_commit_without_double_terminal() {
    for failure_first in [true, false] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        let interceptor_name = if failure_first {
            "prompt-race-failure-first"
        } else {
            "prompt-race-commit-first"
        };
        let _interceptor = connect_test_tool(&mut h, interceptor_name);
        h.handle_extension_event(
            interceptor_name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register prompt interceptor");
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query(if failure_first {
                "prompt-race-failure-first"
            } else {
                "prompt-race-commit-first"
            }),
        )
        .expect("start");
        let (start_id, agent_id, owner_epoch, generation) = {
            let (start_id, operation) = h
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .iter()
                .next()
                .expect("prompt-phase operation");
            assert_eq!(operation.phase, StartPhase::AwaitPromptCommit);
            (
                *start_id,
                operation.pending.cid.clone(),
                operation.persistence_owner_epoch.expect("owner epoch"),
                operation
                    .persistence_generation
                    .expect("prepared generation"),
            )
        };
        let failure = StartPersistenceFailureScope::Agent {
            agent_id: agent_id.clone(),
            owner_epoch,
            generation,
        };

        if failure_first {
            h.fail_start_operations_for_persistence([failure]);
        } else {
            h.handle_extension_event(
                interceptor_name,
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("commit prompt and checkpoint");
            assert!(
                !h.agent_runtime
                    .agent_registry
                    .start_coordinator
                    .operations
                    .contains_key(&start_id)
            );
            h.fail_start_operations_for_persistence([failure]);
        }

        let events = event_log_events(&h);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::AgentStartFailed(failed)
                    if failed.start_id == start_id))
                .count(),
            usize::from(failure_first)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptSubmitted(prompt)
                    if prompt.agent_id == agent_id))
                .count(),
            usize::from(!failure_first)
        );
        assert_eq!(
            events
                .iter()
                .filter(
                    |event| matches!(event, Event::AgentInferenceDispatchStarted(started)
                    if started.agent_id == agent_id)
                )
                .count(),
            usize::from(!failure_first)
        );
        let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
        assert!(coordinator.operations.is_empty());
        assert!(coordinator.requests.is_empty());
        assert!(coordinator.agents.is_empty());
        assert_eq!(coordinator.retained_bytes, 0);
    }
}

/// A selected model whose provider route disappeared before checkpoint claim is
/// rejected before the checkpoint or any provider prompt can commit.
#[test]
fn startup_provider_route_loss_rejects_before_checkpoint_and_delivery() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "route-loss-prompt-interceptor");
    h.handle_extension_event(
        "route-loss-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register prompt interceptor");
    h.handle_start_agent_request(
        &crate::test_connection_id(HARNESS_CONNECTION_ID),
        ext_query("startup-route-loss"),
    )
    .expect("start");
    let (start_id, agent_id) = {
        let (start_id, operation) = h
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .iter()
            .next()
            .expect("prompt phase");
        (*start_id, operation.pending.cid.clone())
    };
    for route in h.provider_runtime.model_routes.values_mut() {
        *route = crate::test_connection_id("missing-provider-route");
    }
    h.handle_extension_event(
        "route-loss-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit prompt");

    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentStartFailed(failed)
                if failed.start_id == start_id
                    && failed.reason == tau_proto::AgentStartFailure::DispatchRejected))
            .count(),
        1
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
    )));
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id
    )));
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// Semantic admission rejection of the inference checkpoint closes startup as
/// `DispatchRejected` and never reaches provider prompt materialization.
#[test]
fn startup_checkpoint_semantic_admission_rejects_without_provider_delivery() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let _interceptor = connect_test_tool(&mut h, "checkpoint-interceptor");
    h.handle_extension_event(
        "checkpoint-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register checkpoint interceptor");
    h.handle_start_agent_request(
        &crate::test_connection_id(HARNESS_CONNECTION_ID),
        ext_query("startup-checkpoint-admission"),
    )
    .expect("park checkpoint");
    let (start_id, agent_id) = {
        let (start_id, operation) = h
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .iter()
            .next()
            .expect("checkpoint phase");
        assert_eq!(operation.phase, StartPhase::AwaitDispatchCommit);
        (*start_id, operation.pending.cid.clone())
    };
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "checkpoint-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("attempt checkpoint admission");

    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentStartFailed(failed)
                if failed.start_id == start_id
                    && failed.reason == tau_proto::AgentStartFailure::DispatchRejected))
            .count(),
        1
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
    )));
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentPromptCreated(prompt) if prompt.agent_id == agent_id
    )));
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// Cancellation and checkpoint commit have one central-loop winner: cancel
/// first closes the accepted obligation, while checkpoint first completes
/// startup and makes later cancellation ordinary agent teardown.
#[test]
fn cancellation_races_startup_checkpoint_commit_with_one_winner() {
    for cancel_first in [true, false] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        let interceptor_name = if cancel_first {
            "checkpoint-cancel-first"
        } else {
            "checkpoint-commit-first"
        };
        let query_id = format!("{interceptor_name}-query");
        let _interceptor = connect_test_tool(&mut h, interceptor_name);
        h.handle_extension_event(
            interceptor_name,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register checkpoint interceptor");
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query(&query_id),
        )
        .expect("park checkpoint");
        let (start_id, agent_id) = {
            let (start_id, operation) = h
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .iter()
                .next()
                .expect("checkpoint operation");
            assert_eq!(operation.phase, StartPhase::AwaitDispatchCommit);
            (*start_id, operation.pending.cid.clone())
        };

        if cancel_first {
            h.cancel_start_agent_request(
                &query_id,
                &tau_proto::ToolCallId::new("checkpoint-race-cancel"),
                false,
            )
            .expect("cancel parked checkpoint");
        } else {
            h.handle_extension_event(
                interceptor_name,
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("commit checkpoint");
            assert!(
                !h.agent_runtime
                    .agent_registry
                    .start_coordinator
                    .operations
                    .contains_key(&start_id)
            );
            let late_cancel = h.cancel_start_agent_request(
                &query_id,
                &tau_proto::ToolCallId::new("checkpoint-race-cancel"),
                false,
            );
            assert!(
                late_cancel.is_err(),
                "completed startup no longer exposes a startup cancellation owner"
            );
        }

        let events = event_log_events(&h);
        assert_eq!(
            events
                .iter()
                .filter(
                    |event| matches!(event, Event::AgentInferenceDispatchStarted(started)
                    if started.agent_id == agent_id)
                )
                .count(),
            usize::from(!cancel_first)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::AgentStartFailed(failed)
                    if failed.start_id == start_id
                        && failed.reason == tau_proto::AgentStartFailure::Canceled))
                .count(),
            usize::from(cancel_first)
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::AgentStartFailed(failed)
                    if failed.start_id == start_id))
                .count(),
            usize::from(cancel_first)
        );
        let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
        assert!(coordinator.operations.is_empty());
        assert!(coordinator.requests.is_empty());
        assert!(coordinator.agents.is_empty());
        assert_eq!(coordinator.retained_bytes, 0);
    }
}

/// Cold restart never reconstructs coordinator state or dispatches an initial
/// prompt whose startup checkpoint did not commit.
#[test]
fn cold_restart_classifies_membership_and_prompt_prefixes_without_dispatch() {
    for (cut_name, intercepted_event, expect_creation, expect_prompt, expect_unavailable) in [
        (
            "acceptance-only",
            tau_proto::EventName::AGENT_STARTED,
            false,
            false,
            false,
        ),
        (
            "started-only",
            tau_proto::EventName::SESSION_AGENT_LOADED,
            true,
            false,
            false,
        ),
        (
            "membership-only",
            tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            true,
            false,
            true,
        ),
        (
            "prompt-only",
            tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            true,
            true,
            true,
        ),
    ] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join(cut_name);
        let agent_id = {
            let mut h = quiet_provider_harness(&state).expect("harness");
            let interceptor_name = format!("{cut_name}-interceptor");
            let _interceptor = connect_test_tool(&mut h, &interceptor_name);
            h.handle_extension_event(
                &interceptor_name,
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(intercepted_event)],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("register cut interceptor");
            h.handle_start_agent_request(
                &crate::test_connection_id(HARNESS_CONNECTION_ID),
                ext_query(&format!("cut-{cut_name}")),
            )
            .expect("start");
            let agent_id = event_log_events(&h)
                .iter()
                .find_map(|event| match event {
                    Event::StartAgentAccepted(accepted)
                        if accepted.query_id == format!("cut-{cut_name}") =>
                    {
                        Some(accepted.agent_id.clone())
                    }
                    _ => None,
                })
                .expect("accepted id");
            assert!(h.runtime_io.publication.pending_intercept.is_some());
            drop(h);
            agent_id
        };

        let h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        assert!(
            !h.agent_runtime
                .agent_registry
                .agent_routes
                .contains_key(&agent_id)
        );
        assert_eq!(
            h.agent_runtime
                .agent_registry
                .restored_unavailable
                .contains_key(&agent_id),
            expect_unavailable
        );
        let agent_events = h
            .session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .unwrap_or_default();
        assert_eq!(
            agent_events
                .iter()
                .any(|record| matches!(record.event, Event::AgentStarted(_))),
            expect_creation
        );
        let submitted = agent_events
            .iter()
            .filter_map(|record| match &record.event {
                Event::AgentPromptSubmitted(prompt) => Some(prompt.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if expect_prompt {
            let expected = format!("instruction cut-{cut_name}");
            assert_eq!(submitted, [expected.as_str()]);
        } else {
            assert!(submitted.is_empty());
        }
        assert!(
            agent_events
                .iter()
                .all(|record| !matches!(record.event, Event::AgentInferenceDispatchStarted(_)))
        );
        let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
        assert!(coordinator.operations.is_empty());
        assert!(coordinator.requests.is_empty());
        assert!(coordinator.agents.is_empty());
        assert_eq!(coordinator.retained_bytes, 0);
    }
}

/// An unrelated checkpoint on another branch cannot complete an interrupted
/// startup whose canonical initial prompt it does not cover.
#[test]
fn cold_restart_rejects_off_branch_checkpoint_as_startup_completion() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("off-branch-checkpoint");
    let agent_id = {
        let mut h = quiet_provider_harness(&state).expect("harness");
        let _interceptor = connect_test_tool(&mut h, "off-branch-checkpoint-interceptor");
        h.handle_extension_event(
            "off-branch-checkpoint-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register checkpoint interceptor");
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("off-branch-checkpoint"),
        )
        .expect("park canonical checkpoint");
        let (agent_id, startup_through) = {
            let operation = h
                .agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .values()
                .next()
                .expect("startup operation");
            (
                operation.pending.cid.clone(),
                operation
                    .startup_through
                    .expect("canonical startup prompt head"),
            )
        };
        assert_ne!(startup_through, tau_proto::AgentHead::Root);
        let mut off_branch = match h
            .runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| pending.event.clone())
        {
            Some(Event::AgentInferenceDispatchStarted(checkpoint)) => checkpoint,
            other => panic!("expected parked checkpoint, got {other:?}"),
        };
        off_branch.through = tau_proto::AgentHead::Root;
        off_branch.activation_cut = Some(tau_proto::AgentHead::Root);
        h.append_direct_agent_semantic_event(
            agent_id.as_str(),
            tau_core::AgentEventParent::Root,
            Event::AgentInferenceDispatchStarted(off_branch),
        )
        .expect("append unrelated off-branch checkpoint");
        drop(h);
        agent_id
    };

    let reopened =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("cold reopen");
    assert!(
        !reopened
            .agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&agent_id)
    );
    assert!(
        reopened
            .agent_runtime
            .agent_registry
            .restored_unavailable
            .contains_key(&agent_id)
    );
    assert!(
        reopened
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .is_empty()
    );
    assert!(reopened.runtime_io.publication.idle_dispatches.is_empty());
}

/// A committed startup checkpoint is the exact cold-restart success boundary;
/// later provider uncertainty follows ordinary inference recovery.
#[test]
fn cold_restart_restores_checkpointed_start_without_coordinator() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("checkpointed");
    let agent_id = {
        let mut h = quiet_provider_harness(&state).expect("harness");
        h.handle_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("cut-checkpointed"),
        )
        .expect("start");
        let accepted = event_log_events(&h)
            .iter()
            .find_map(|event| match event {
                Event::StartAgentAccepted(accepted) if accepted.query_id == "cut-checkpointed" => {
                    Some(accepted.clone())
                }
                _ => None,
            })
            .expect("accepted");
        assert!(
            h.session_runtime
                .agent_store
                .agent_events(accepted.agent_id.as_str())
                .expect("agent journal")
                .iter()
                .any(|record| matches!(record.event, Event::AgentInferenceDispatchStarted(_)))
        );
        assert!(
            h.agent_runtime
                .agent_registry
                .start_coordinator
                .operations
                .is_empty()
        );
        drop(h);
        accepted.agent_id
    };

    let h = quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
        .expect("resume");
    assert!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&agent_id)
    );
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// A physical post-accept creation failure terminalizes accepted requests
/// without exposing the failed identity while later FIFO work continues.
#[test]
fn accepted_start_storage_failure_terminalizes_and_continues_fifo() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("harness");
    let first = h
        .prepare_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("q-fail"),
        )
        .expect("prepare first")
        .expect("first pending");
    let second = h
        .prepare_start_agent_request(
            &crate::test_connection_id(HARNESS_CONNECTION_ID),
            ext_query("q-next"),
        )
        .expect("prepare second")
        .expect("second pending");
    let first_agent_id = first.agent_id.clone();
    let second_agent_id = second.agent_id.clone();
    h.agent_runtime
        .agent_registry
        .pending_start_requests
        .push_back(first);
    h.agent_runtime
        .agent_registry
        .pending_start_requests
        .push_back(second);
    let blocked_journal = state
        .join("agents")
        .join(&first_agent_id)
        .join("events.cbor");
    std::fs::create_dir_all(&blocked_journal).expect("block first journal");

    h.drain_pending_start_agent_requests().expect("drain");

    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::StartAgentAccepted(accepted) if accepted.query_id == "q-fail"
            ))
            .count(),
        1
    );
    let failed_results: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::StartAgentResult(result) if result.query_id == "q-fail" => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(failed_results.len(), 1);
    assert!(
        !h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(first_agent_id.as_str())
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentPromptSubmitted(prompt) if prompt.agent_id.as_str() == first_agent_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentStarted(started) if started.agent_id.as_str() == first_agent_id
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStartFailed(failed)
                    if failed.agent_id.as_str() == first_agent_id
                        && failed.reason == tau_proto::AgentStartFailure::CreationWorker
            ))
            .count(),
        1,
        "events: {events:#?}"
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(second_agent_id.as_str())
    );
    let second_records = h
        .session_runtime
        .agent_store
        .agent_events(&second_agent_id)
        .expect("second agent records");
    assert!(!second_records.is_empty());
    assert!(
        h.session_runtime
            .store
            .session("s1")
            .is_some_and(|membership| {
                membership.contains_agent(&crate::parse_agent_id(&second_agent_id))
            })
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::StartAgentResult(result)
                    if result.query_id == "q-next" && result.error.is_none()
            ))
            .count(),
        0
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentStarted(started) if started.agent_id.as_str() == second_agent_id
    )));
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );
}

#[test]
fn shared_start_agent_requests_start_concurrently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");

    h.handle_start_agent_request(&crate::test_connection_id("conn-a"), ext_query("q-a"))
        .expect("query a");
    h.handle_start_agent_request(&crate::test_connection_id("conn-b"), ext_query("q-b"))
        .expect("query b");

    assert!(ext_query_cid(&h, "q-a").is_some());
    assert!(ext_query_cid(&h, "q-b").is_some());
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// Start-agent requests do not use harness-level scheduling; filesystem
/// coordination is handled by ext-shell directory locks.
#[test]
fn start_agent_requests_do_not_block_independent_queries() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-a");
    let _ = connect_test_tool(&mut h, "conn-b");
    let _ = connect_test_tool(&mut h, "conn-c");
    let _ = connect_test_tool(&mut h, "conn-d");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-a"),
        ext_query("q-update-a"),
    )
    .expect("update query a");
    h.handle_start_agent_request(&crate::test_connection_id("conn-b"), ext_query("q-shared"))
        .expect("shared query");
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-c"),
        ext_query("q-update-b"),
    )
    .expect("update query b");
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-d"),
        ext_query("q-exclusive"),
    )
    .expect("exclusive query");

    for query_id in ["q-update-a", "q-shared", "q-update-b", "q-exclusive"] {
        assert!(
            ext_query_cid(&h, query_id).is_some(),
            "{query_id} should start immediately"
        );
    }
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// Tool-backed nested start-agent requests are independent agents and do not
/// wait on their parent at harness level.
#[test]
fn nested_start_agent_request_starts_independently() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());
    let _ = connect_test_tool(&mut h, "conn-delegate");

    h.handle_start_agent_request(
        &crate::test_connection_id("conn-delegate"),
        ext_query("q-outer"),
    )
    .expect("outer query");
    let outer_cid = ext_query_cid(&h, "q-outer").expect("outer started");

    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("nested-call".into(), outer_cid.clone());
    let mut nested = ext_query("q-nested");
    nested.tool_call_id = Some("nested-call".into());
    nested.task_name = Some("nested".to_owned());
    h.handle_start_agent_request(&crate::test_connection_id("conn-delegate"), nested)
        .expect("nested query");

    let nested_cid = ext_query_cid(&h, "q-nested").expect("nested started");
    assert_ne!(outer_cid, nested_cid);
    assert!(
        h.agent_runtime
            .agent_registry
            .pending_start_requests
            .is_empty()
    );

    h.shutdown().expect("shutdown");
}

/// Releasing a later agent's context barrier must bypass an earlier retained
/// context-not-ready obligation without consuming, duplicating, or rebinding
/// it. This protects per-agent readiness independence and bounded no-progress
/// drains.
#[test]
fn reverse_agent_context_readiness_dispatches_each_obligation_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let retained_cid = ensure_test_user_agent(&mut h);
    let ready_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let retained_agent_id = durable_agent_id_for_conversation(&h, &retained_cid);
    let ready_agent_id = durable_agent_id_for_conversation(&h, &ready_cid);
    let context_provider = tau_proto::ConnectionId::parse("reverse-readiness-context")
        .expect("test connection id must satisfy the identifier grammar");
    for agent_id in [&retained_agent_id, &ready_agent_id] {
        set_test_agent_context_wait(
            &mut h,
            agent_id.clone(),
            path_std_collections::HashSet::from([context_provider.clone()]),
        );
    }

    h.dispatch_prompt_for_agent(
        &retained_cid,
        PendingPrompt::user("retained activation".to_owned()),
    )
    .expect("defer retained activation");
    h.dispatch_prompt_for_agent(
        &ready_cid,
        PendingPrompt::user("ready activation".to_owned()),
    )
    .expect("defer ready activation");
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);
    assert_eq!(
        h.runtime_io.publication.idle_dispatches[0].cid,
        retained_cid
    );
    assert_eq!(h.runtime_io.publication.idle_dispatches[1].cid, ready_cid);
    let retained_obligation = h.runtime_io.publication.idle_dispatches[0].clone();
    let ready_obligation = h.runtime_io.publication.idle_dispatches[1].clone();
    assert!(retained_obligation.obligation.is_committed());
    assert!(ready_obligation.obligation.is_committed());
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&retained_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&ready_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));

    finish_test_agent_context_wait(&mut h, &ready_agent_id);
    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == retained_agent_id
    )));
    let ready_checkpoint = assert_inference_dispatch_lifecycle(
        &events,
        &ready_agent_id,
        ready_obligation
            .activation_through
            .expect("ready activation watermark"),
        ready_obligation.activation_cut,
        ExpectedProviderSubmission::Pending,
    );
    assert_inference_dispatch_owner(
        &h.agent_runtime.agent_registry.agents[&ready_cid],
        &ready_checkpoint,
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&retained_cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    let retained = &h.runtime_io.publication.idle_dispatches[0];
    assert_eq!(retained.cid, retained_cid);
    assert_eq!(
        retained.activation_through,
        retained_obligation.activation_through
    );
    assert_eq!(retained.activation_cut, retained_obligation.activation_cut);
    assert_eq!(
        retained.obligation.is_committed(),
        retained_obligation.obligation.is_committed()
    );

    let events_before_stable_drain = event_log_events(&h);
    h.drain_publish_idle_dispatches();
    assert_eq!(event_log_events(&h), events_before_stable_drain);
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    let retained = &h.runtime_io.publication.idle_dispatches[0];
    assert_eq!(retained.cid, retained_cid);
    assert_eq!(
        retained.activation_through,
        retained_obligation.activation_through
    );
    assert_eq!(retained.activation_cut, retained_obligation.activation_cut);
    assert_eq!(
        retained.obligation.is_committed(),
        retained_obligation.obligation.is_committed()
    );

    let ready_provider =
        h.provider_runtime.pending_prompts[&ready_checkpoint.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &ready_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: ready_checkpoint.agent_prompt_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("record ready-agent provider submission");
    let events = event_log_events(&h);
    let submitted_ready_checkpoint = assert_inference_dispatch_lifecycle(
        &events,
        &ready_agent_id,
        ready_checkpoint.through,
        ready_checkpoint.activation_cut,
        ExpectedProviderSubmission::Submitted,
    );
    assert_eq!(
        submitted_ready_checkpoint.agent_prompt_id,
        ready_checkpoint.agent_prompt_id
    );
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);
    assert_eq!(
        h.runtime_io.publication.idle_dispatches[0].cid,
        retained_cid
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == retained_agent_id
    )));

    finish_test_agent_context_wait(&mut h, &retained_agent_id);
    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    let retained_checkpoint = assert_inference_dispatch_lifecycle(
        &events,
        &retained_agent_id,
        retained_obligation
            .activation_through
            .expect("retained activation watermark"),
        retained_obligation.activation_cut,
        ExpectedProviderSubmission::Pending,
    );
    assert_inference_dispatch_owner(
        &h.agent_runtime.agent_registry.agents[&retained_cid],
        &retained_checkpoint,
    );
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());

    let retained_provider =
        h.provider_runtime.pending_prompts[&retained_checkpoint.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &retained_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: retained_checkpoint.agent_prompt_id.clone(),
            originator: tau_proto::PromptOriginator::User,
        }),
    )
    .expect("record retained-agent provider submission");
    h.drain_publish_idle_dispatches();
    let events = event_log_events(&h);
    for (agent_id, checkpoint) in [
        (&retained_agent_id, &retained_checkpoint),
        (&ready_agent_id, &ready_checkpoint),
    ] {
        let submitted_checkpoint = assert_inference_dispatch_lifecycle(
            &events,
            agent_id,
            checkpoint.through,
            checkpoint.activation_cut,
            ExpectedProviderSubmission::Submitted,
        );
        assert_eq!(
            submitted_checkpoint.agent_prompt_id,
            checkpoint.agent_prompt_id
        );
    }
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// One selected deferred obligation with uncertain ownership must not become a
/// global agent slot that blocks a later runnable agent.
#[test]
fn blocked_deferred_dispatch_does_not_head_of_line_block_other_agent() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let blocked_cid = ensure_test_user_agent(&mut h);
    let runnable_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let blocked_agent_id = durable_agent_id_for_conversation(&h, &blocked_cid);
    let runnable_agent_id = durable_agent_id_for_conversation(&h, &runnable_cid);
    let context_provider = tau_proto::ConnectionId::parse("deferred-fairness-context")
        .expect("test connection id must satisfy the identifier grammar");
    for agent_id in [&blocked_agent_id, &runnable_agent_id] {
        set_test_agent_context_wait(
            &mut h,
            agent_id.clone(),
            path_std_collections::HashSet::from([context_provider.clone()]),
        );
    }

    h.dispatch_prompt_for_agent(
        &blocked_cid,
        PendingPrompt::user("blocked activation".to_owned()),
    )
    .expect("defer blocked activation");
    h.dispatch_prompt_for_agent(
        &runnable_cid,
        PendingPrompt::user("runnable activation".to_owned()),
    )
    .expect("defer runnable activation");
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);
    assert_eq!(h.runtime_io.publication.idle_dispatches[0].cid, blocked_cid);
    assert_eq!(
        h.runtime_io.publication.idle_dispatches[1].cid,
        runnable_cid
    );
    let blocked_obligation = h.runtime_io.publication.idle_dispatches[0].clone();
    let runnable_obligation = h.runtime_io.publication.idle_dispatches[1].clone();
    finish_test_agent_context_wait(&mut h, &blocked_agent_id);
    finish_test_agent_context_wait(&mut h, &runnable_agent_id);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&blocked_cid)
        .expect("blocked agent")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::DispatchUncertain {
        owner: path_crate_agent::InferenceCheckpointOwner::Inference,
        agent_prompt_id: test_agent_prompt_id("ap-blocked-uncertain"),
        through: blocked_obligation
            .activation_through
            .expect("blocked activation watermark"),
        model: Some("test/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: blocked_obligation.activation_cut,
    };

    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    assert!(
        events.iter().all(
            |event| !matches!(event, Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == blocked_agent_id)
        ),
        "blocked agent must not receive another checkpoint"
    );
    let runnable_checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == runnable_agent_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(runnable_checkpoints.len(), 1);
    let runnable_checkpoint = runnable_checkpoints[0];
    assert_eq!(
        runnable_checkpoint.through,
        runnable_obligation
            .activation_through
            .expect("runnable activation watermark")
    );
    assert_eq!(
        runnable_checkpoint.activation_cut,
        runnable_obligation.activation_cut
    );
    assert_eq!(runnable_checkpoint.model, Some("test/model".into()));
    assert_eq!(
        runnable_checkpoint.operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptStarted(started)
            if started.agent_prompt_id == runnable_checkpoint.agent_prompt_id
                && started.agent_id == runnable_agent_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_prompt_id == runnable_checkpoint.agent_prompt_id
                && created.agent_id == runnable_agent_id
    )));
    assert_eq!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .filter(|deferred| deferred.cid == blocked_cid)
            .count(),
        1
    );
    assert!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .all(|deferred| deferred.cid != runnable_cid)
    );

    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&blocked_cid)
        .expect("release blocked agent")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::None;
    h.drain_publish_idle_dispatches();

    let events = event_log_events(&h);
    let blocked_checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == blocked_agent_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(blocked_checkpoints.len(), 1);
    assert_eq!(
        blocked_checkpoints[0].through,
        blocked_obligation
            .activation_through
            .expect("blocked activation watermark")
    );
    assert_eq!(
        blocked_checkpoints[0].activation_cut,
        blocked_obligation.activation_cut
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentPromptCreated(created)
            if created.agent_prompt_id == blocked_checkpoints[0].agent_prompt_id
                && created.agent_id == blocked_agent_id
    )));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// Readiness coalescing keeps incomparable branch activations as distinct
/// obligations; dispatching the selected sibling does not consume the dormant
/// branch, which becomes runnable after the sibling turn finishes and
/// reselects.
#[test]
fn readiness_deferred_incomparable_activations_remain_distinct() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    set_test_agent_context_wait(
        &mut h,
        agent_id.clone(),
        path_std_collections::HashSet::from([tau_proto::ConnectionId::parse("context-provider")
            .expect("test connection id must satisfy the identifier grammar")]),
    );

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("branch A activation".to_owned()))
        .expect("park branch A");
    let branch_a = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("branch A");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("branch B activation".to_owned()))
        .expect("park branch B");
    let branch_b = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .head
        .expect("branch B");
    assert_ne!(branch_a, branch_b);
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 2);

    finish_test_agent_context_wait(&mut h, &agent_id);
    h.drain_publish_idle_dispatches();
    let branch_b_prompt = read_nth_prompt_created(&h, 0);
    let checkpoints = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].through, tau_proto::AgentHead::Node(branch_b));
    assert_eq!(h.runtime_io.publication.idle_dispatches.len(), 1);

    h.handle_provider_response_finished(provider_text_response(
        &branch_b_prompt.agent_prompt_id,
        agent_id.clone(),
        "branch B complete",
    ))
    .expect("finish selected sibling");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(branch_a),
        }),
    );
    let checkpoints = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(
        checkpoints[1].through,
        tau_proto::AgentHead::Node(branch_a),
        "branch_a={branch_a:?}, branch_b={branch_b:?}, checkpoints={checkpoints:?}"
    );
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
}

/// Fixed-seed randomized queue and lifecycle states keep the streaming
/// selector exactly equal to the previous collecting reference, including
/// current encounter order and output-length priority.
#[test]
fn randomized_streaming_runnable_selection_matches_collecting_reference() {
    fn owner_ready(index: usize) -> path_crate_agent::OutputLengthContinuationState {
        let source_agent_prompt_id = test_agent_prompt_id(format!("scheduler-source-{index}"));
        path_crate_agent::OutputLengthContinuationState::OwnerReady(
            path_crate_agent::OutputLengthContinuationDispatch {
                plan: path_crate_agent::OutputLengthContinuationPlan {
                    agent_prompt_id: test_agent_prompt_id(format!("scheduler-successor-{index}")),
                    owner: tau_proto::OutputLengthContinuationOwner {
                        outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(
                            &source_agent_prompt_id,
                        ),
                        source_agent_prompt_id,
                        ordinal: 1,
                    },
                    dispatch: path_crate_agent::InferenceDispatchOwnership {
                        model: tau_proto::ModelId::from("test/model"),
                        operation: tau_proto::PromptOperation::Inference,
                        activation_cut: tau_proto::AgentHead::Root,
                    },
                },
                through: tau_proto::AgentHead::Root,
            },
        )
    }

    fn input(agent_id: tau_proto::AgentId, text: &str) -> Event {
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id,
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        })
    }

    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let agents = (0..24)
        .map(|_| {
            h.create_durable_user_agent(
                h.session_runtime.current_session_id.clone(),
                &h.config.selected_role.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut branch_nodes = path_std_collections::HashMap::new();
    for cid in &agents {
        let agent_id = durable_agent_id_for_conversation(&h, cid);
        h.publish_for_agent(cid, input(agent_id.clone(), "selected wake branch"));
        let selected = h.agent_runtime.agent_registry.agents[cid]
            .identity
            .head
            .expect("selected node");
        h.publish_for_agent(
            cid,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: agent_id.clone(),
                head: tau_proto::AgentHead::Root,
            }),
        );
        h.publish_for_agent(cid, input(agent_id.clone(), "dormant wake branch"));
        let dormant = h.agent_runtime.agent_registry.agents[cid]
            .identity
            .head
            .expect("dormant node");
        h.publish_for_agent(
            cid,
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id,
                head: tau_proto::AgentHead::Node(selected),
            }),
        );
        branch_nodes.insert(cid.clone(), (selected, dormant));
    }
    let mut random = 0xbb67_ae85_84ca_a73b_u64;

    for case in 0_usize..256 {
        let mut allowed = path_std_collections::HashSet::new();
        for (index, cid) in agents.iter().enumerate() {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let durable_agent_id = durable_agent_id_for_conversation(&h, cid);
            let agent = h
                .agent_runtime
                .agent_registry
                .agents
                .get_mut(cid)
                .expect("agent");
            agent.dispatch.pending_prompts.clear();
            agent.dispatch.pending_message_wakes.clear();
            let (selected, dormant) = branch_nodes[cid];
            for (wake_index, node_id) in [
                random.is_multiple_of(7).then_some(selected),
                random.is_multiple_of(11).then_some(dormant),
                random.is_multiple_of(13).then_some(selected),
                None,
            ]
            .into_iter()
            .enumerate()
            {
                if node_id.is_some() || (wake_index == 3 && random.is_multiple_of(17)) {
                    agent.dispatch.pending_message_wakes.push_back(
                        path_crate_agent::PendingMessageWake {
                            source: path_crate_agent::PendingMessageWakeSource::MessageFact {
                                durable_event_seq: tau_core::PersistedAgentEventSeq::new(
                                    (case * agents.len() + index + wake_index) as u64,
                                ),
                            },
                            node_id,
                            activation_observation: None,
                            source_observation: None,
                        },
                    );
                }
            }
            agent.dispatch.pending_replay_activation = random.is_multiple_of(13);
            agent.dispatch.terminating = random.is_multiple_of(11);
            agent.dispatch.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
            agent.turn.turn_state = if random.is_multiple_of(7) {
                AgentTurnState::AgentThinking {
                    agent_prompt_id: test_agent_prompt_id(format!("scheduler-busy-{case}-{index}")),
                }
            } else {
                AgentTurnState::Idle
            };
            agent.turn.output_length_continuation = if random.is_multiple_of(17) {
                owner_ready(index)
            } else {
                Default::default()
            };
            for prompt_index in 0..(random as usize % 5) {
                let mut prompt = if (random >> prompt_index).is_multiple_of(3) {
                    PendingPrompt::passive_background_completion(format!(
                        "passive {case} {index} {prompt_index}"
                    ))
                } else {
                    PendingPrompt::user(format!("active {case} {index} {prompt_index}"))
                };
                if !prompt.is_passive_background_completion()
                    && (random >> (prompt_index + 5)).is_multiple_of(19)
                {
                    prompt.initial_prompt_correlation =
                        Some(path_crate_agent::InitialPromptCorrelation {
                            request_id: format!("request-{case}-{index}"),
                            agent_id: durable_agent_id.clone(),
                            ctx_id: format!("ctx-{case}-{index}"),
                            bootstrap_prompt: false,
                            activation_through: None,
                        });
                }
                agent.dispatch.pending_prompts.push_back(prompt);
            }
            if !random.is_multiple_of(5) {
                allowed.insert(cid.clone());
            }
        }
        let allowed = case.is_multiple_of(2).then_some(&allowed);

        let reference = {
            let runnable =
                h.agent_runtime
                    .agent_registry
                    .agents
                    .iter()
                    .filter_map(|(agent_id, conv)| {
                        let non_passive = conv
                            .dispatch
                            .pending_prompts
                            .iter()
                            .enumerate()
                            .find(|(_, prompt)| !prompt.is_passive_background_completion());
                        let ready_message_wake = conv
                            .identity
                            .agent_id
                            .as_deref()
                            .and_then(|durable_id| h.session_runtime.agent_store.agent(durable_id))
                            .is_some_and(|tree| {
                                let branch = tree
                                    .branch_node_ids_from(conv.identity.head)
                                    .into_iter()
                                    .collect::<path_std_collections::HashSet<_>>();
                                conv.dispatch.pending_message_wakes.iter().any(|wake| {
                                    wake.node_id.is_some_and(|node| branch.contains(&node))
                                })
                            });
                        (allowed.is_none_or(|allowed| allowed.contains(agent_id))
                            && (allowed.is_some()
                                || !h
                                    .runtime_io
                                    .publication
                                    .capacity_rejected_activations
                                    .contains_key(agent_id))
                            && (non_passive.is_some()
                                || ready_message_wake
                                || conv.dispatch.pending_replay_activation)
                            && matches!(conv.turn.turn_state, AgentTurnState::Idle)
                            && !conv.dispatch.terminating
                            && matches!(
                                conv.dispatch.activation_dispatch,
                                path_crate_agent::ActivationDispatchState::None
                            )
                            && (non_passive.as_ref().is_none_or(|(_, prompt)| {
                                prompt.initial_prompt_correlation.is_none()
                            }) || h.agent_initialization_ready_for(agent_id))
                            && !h.has_deferred_prompt_dispatch_for(agent_id)
                            && !h.agent_has_open_foreground_tool_round(agent_id))
                        .then(|| {
                            let dispatch_ready_message_wake =
                                non_passive.as_ref().is_none_or(|(_, prompt)| {
                                    prompt.initial_prompt_correlation.is_none()
                                }) && ready_message_wake;
                            (
                                agent_id.clone(),
                                non_passive.map(|(index, prompt)| {
                                    (index, prompt.initial_prompt_correlation.clone())
                                }),
                                dispatch_ready_message_wake,
                                matches!(
                                    conv.turn.output_length_continuation,
                                    path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                                ),
                            )
                        })
                    })
                    .collect::<Vec<_>>();
            runnable
                .iter()
                .find(|(_, _, _, owner)| *owner)
                .or_else(|| runnable.first())
                .cloned()
        };

        let (selected, work) = h.next_runnable_agent_measured(allowed);
        assert_eq!(
            selected.as_ref().map(|selection| (
                selection.agent_id.clone(),
                selection.prompt_index,
                selection.initial_prompt_correlation.clone(),
                selection.had_ready_message_wake,
            )),
            reference.map(|(agent_id, prompt, ready_message_wake, _)| (
                agent_id,
                prompt.as_ref().map(|(index, _)| *index),
                prompt.and_then(|(_, correlation)| correlation),
                ready_message_wake,
            )),
            "case {case}"
        );
        assert_eq!(work[6], usize::from(selected.is_some()));
        assert_eq!(work[7], 0, "streaming selection allocates no candidate Vec");
        assert!(work[0] <= agents.len());
    }
}

/// Initial and post-tool normal/canceled terminals each own one durable outer
/// finish and protected automatic-compaction start, with no retained
/// completion.
#[test]
fn automatic_policy_terminal_matrix_commits_owned_suffix_once() {
    for post_tool in [false, true] {
        for canceled in [false, true] {
            let td = TempDir::new().expect("tempdir");
            let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
            enable_remote_compaction_for_test_model(&mut h);
            let info = h
                .provider_runtime
                .model_info
                .get_mut(&"test/model".into())
                .expect("test model");
            info.supports_compaction = false;
            info.supports_standalone_compaction = true;
            let cid = ensure_test_user_agent(&mut h);
            h.config
                .available_roles
                .get_mut(&h.config.selected_role)
                .expect("selected role")
                .compactions
                .insert(
                    "owned-terminal-matrix".to_owned(),
                    tau_config::settings::CompactionPolicy {
                        threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(1),
                        enable: true,
                        when: tau_config::settings::ContextPolicyWhen {
                            at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
                            statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
                        },
                    },
                );
            {
                let agent = h
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get_mut(&cid)
                    .expect("agent");
                agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(100));
                agent.execution.context_usage_model = Some("test/model".into());
                agent.execution.context_usage_prompt_id =
                    Some(test_agent_prompt_id("ap-test-provider-usage"));
                agent.execution.context_usage_head = agent.identity.head;
            }
            h.dispatch_prompt_for_agent(
                &cid,
                PendingPrompt::user(format!("matrix post_tool={post_tool} canceled={canceled}")),
            )
            .expect("dispatch initial inference");
            let initial = read_nth_prompt_created(&h, 0);
            let terminal_prompt = if post_tool {
                h.handle_provider_response_finished(provider_tool_response(
                    &initial,
                    "matrix-tool",
                    "self_info",
                    CborValue::Map(Vec::new()),
                ))
                .expect("finish matrix tool round");
                read_nth_prompt_created(&h, 1)
            } else {
                initial
            };
            if canceled {
                h.finalize_canceled_in_flight_prompt(&cid);
            } else {
                let mut response = provider_text_response(
                    &terminal_prompt.agent_prompt_id,
                    terminal_prompt.agent_id.clone(),
                    "done",
                );
                response.usage = Some(tau_proto::ProviderTokenUsage {
                    prompt_sent_tokens: 250,
                    response_received_tokens: 1,
                    ..Default::default()
                });
                h.handle_provider_response_finished(response)
                    .expect("finish matrix inference");
            }

            let records = h
                .session_runtime
                .agent_store
                .agent_events(terminal_prompt.agent_id.as_str())
                .expect("durable records");
            let terminals = records
                .iter()
                .filter_map(|record| match &record.event {
                    Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == terminal_prompt.agent_prompt_id =>
                    {
                        Some(&record.event)
                    }
                    Event::AgentPromptTerminated(terminated)
                        if terminated.agent_prompt_id == terminal_prompt.agent_prompt_id =>
                    {
                        Some(&record.event)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                terminals.len(),
                1,
                "post_tool={post_tool} canceled={canceled}"
            );
            if canceled {
                let Event::AgentPromptTerminated(terminated) = terminals[0] else {
                    panic!("canceled matrix terminal");
                };
                assert!(
                    terminated.automatic_compaction_decision.is_none(),
                    "cancellation without a prior exact provider observation cannot mint authority"
                );
                assert!(records.iter().all(|record| !matches!(
                    record.event,
                    Event::AgentStandaloneCompactionStarted(_)
                )));
                h.shutdown().expect("shutdown");
                continue;
            }
            let decision = match terminals[0] {
                Event::ProviderResponseFinished(response) if !canceled => response
                    .automatic_compaction_decision
                    .as_ref()
                    .expect("ordinary terminal owns decision"),
                Event::AgentPromptTerminated(terminated) if canceled => terminated
                    .automatic_compaction_decision
                    .as_ref()
                    .expect("canceled terminal owns decision"),
                Event::ProviderResponseFinished(_) | Event::AgentPromptTerminated(_) => {
                    panic!(
                        "terminal variant disagrees with post_tool={post_tool} canceled={canceled}"
                    )
                }
                _ => unreachable!("filtered canonical terminal"),
            };
            let transaction_id = &decision.transaction_id;
            let finishes = records
                .iter()
                .filter_map(|record| match &record.event {
                    Event::AgentOuterTurnFinished(finished)
                        if finished.outer_turn_id == decision.outer_turn_id =>
                    {
                        Some(finished)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                finishes.len(),
                1,
                "post_tool={post_tool} canceled={canceled}"
            );
            assert_eq!(
                finishes[0].automatic_compaction_decision.as_ref(),
                Some(transaction_id),
                "post_tool={post_tool} canceled={canceled}"
            );
            assert_eq!(
                records
                    .iter()
                    .filter(|record| matches!(
                        &record.event,
                        Event::AgentStandaloneCompactionStarted(started)
                            if &started.transaction_id == transaction_id
                    ))
                    .count(),
                1,
                "post_tool={post_tool} canceled={canceled}"
            );
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .is_empty(),
                "post_tool={post_tool} canceled={canceled}"
            );
            h.shutdown().expect("shutdown");
        }
    }
}

/// Outer-finish notices consume the exact terminal candidate, ignore live-role
/// rewrites, and preserve one-shot hysteresis while usage remains high.
#[test]
fn outer_finish_alert_uses_terminal_snapshot_and_retains_hysteresis() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let prompt_id = tau_proto::AgentPromptId::parse("ap-alert-finish").expect("prompt");
    let outer_turn_id = tau_proto::AgentOuterTurnId::for_prompt(&prompt_id);
    let alert = tau_config::settings::ContextSizeAlert {
        threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(100)
            .expect("positive test threshold"),
        enable: true,
        message: "captured finish alert".to_owned(),
        when: tau_config::settings::ContextPolicyWhen {
            at: path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished,
            statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Done]),
        },
    };
    let arm = |agent: &mut Agent| {
        agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(200));
        agent.turn.terminal_status_was_available = false;
        agent.turn.terminal_notice_eligible = true;
        agent.turn.terminal_notice_outer_turn_id = Some(outer_turn_id.clone());
        agent
            .turn
            .terminal_context_size_alerts
            .insert("captured".to_owned(), alert.clone());
    };
    arm(h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent"));
    h.config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("role")
        .context_size_alerts
        .clear();
    h.queue_outer_turn_finished_context_size_alerts(&cid, &outer_turn_id);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .filter(|prompt| prompt.is_context_size_alert())
            .count(),
        1
    );
    arm(h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent"));
    h.queue_outer_turn_finished_context_size_alerts(&cid, &outer_turn_id);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .filter(|prompt| prompt.is_context_size_alert())
            .count(),
        1,
        "high usage must not refire the same alert"
    );
    h.shutdown().expect("shutdown");
}

/// Runtime deadline processing completes a registered input wait normally,
/// clears its foreground tracking, and does not suspend the outer turn.
#[test]
fn input_wait_timeout_completes_once_inside_running_turn() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let call = wait_input_call("wait-input-timeout");
    seed_tools_running(
        &mut h,
        &cid,
        vec![call.id.clone(), ToolCallId::from("still-running-sibling")],
    );
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("register input wait");
    let deadline = h.next_input_wait_deadline().expect("input deadline");
    assert_eq!(h.next_runtime_deadline(), Some(deadline));

    h.process_runtime_deadlines_at(deadline);
    assert_eq!(tool_result_count(&h, call.id.as_str()), 1);
    assert!(!h.input_wait_pending_for(&cid));
    assert!(
        !h.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(&call.id)
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id == call.id
                && result.result == CborValue::Map(vec![(
                    CborValue::Text("timed_out".to_owned()),
                    CborValue::Bool(true),
                )])
                && result.display.as_ref().is_some_and(|display|
                    display.status == tau_proto::ToolUseStatus::Warning
                        && display.status_text == "timeout")
    )));
    h.process_runtime_deadlines_at(deadline);
    h.activate_waits_for(&cid, tau_proto::ObservationId::random());
    assert_eq!(tool_result_count(&h, call.id.as_str()), 1);
    h.shutdown().expect("shutdown");
}

/// Production timeout publication adds one advisory on the third consecutive
/// activating-input timeout and leaves later timeouts in that run unadorned.
#[test]
fn repeated_input_wait_timeouts_add_one_advisory() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let has_advice = |h: &Harness, call_id: &str| {
        event_log_contains_any_source(h, |event| {
            matches!(
                event,
                Event::ToolResult(result)
                    if result.call_id.as_str() == call_id
                        && matches!(
                            &result.result,
                            CborValue::Map(entries)
                                if entries.iter().any(|(key, _)|
                                    key == &CborValue::Text("advice".to_owned()))
                        )
            )
        })
    };

    for index in 1..=4 {
        let call_id = format!("repeated-input-wait-{index}");
        let mut call = wait_input_call(&call_id);
        call.call_ref = Some(tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([index; 16]),
            item_index: 0,
        });
        seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
        let now = path_std_time::Instant::now();
        h.handle_wait_tool_call_at(&cid, &call, ToolName::new("wait"), now)
            .expect("register input wait");
        h.process_input_wait_deadlines(
            h.next_input_wait_deadline()
                .expect("registered input deadline"),
        );
        assert_eq!(has_advice(&h, &call_id), index == 3);
    }
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "resumed work".to_owned(),
        )
        .expect("valid working status"),
    )
    .expect("reset guard with status report");
    for index in 5..=7 {
        let call_id = format!("repeated-input-wait-{index}");
        let mut call = wait_input_call(&call_id);
        call.call_ref = Some(tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([index; 16]),
            item_index: 0,
        });
        seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
        h.handle_wait_tool_call_at(
            &cid,
            &call,
            ToolName::new("wait"),
            path_std_time::Instant::now(),
        )
        .expect("register reset input wait");
        h.process_input_wait_deadlines(
            h.next_input_wait_deadline()
                .expect("registered input deadline"),
        );
        assert_eq!(has_advice(&h, &call_id), index == 7);
    }
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Waiting,
            "await automation".to_owned(),
        )
        .expect("valid waiting status"),
    )
    .expect("report waiting");
    for index in 8..=10 {
        let call_id = format!("repeated-input-wait-{index}");
        let mut call = wait_input_call(&call_id);
        call.call_ref = Some(tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([index; 16]),
            item_index: 0,
        });
        seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
        h.handle_wait_tool_call_at(
            &cid,
            &call,
            ToolName::new("wait"),
            path_std_time::Instant::now(),
        )
        .expect("register waiting-status input wait");
        h.process_input_wait_deadlines(
            h.next_input_wait_deadline()
                .expect("registered input deadline"),
        );
        assert!(!has_advice(&h, &call_id));
    }
    h.shutdown().expect("shutdown");
}

/// A stale generic publish-idle obligation must not checkpoint a continuation
/// while the owning provider turn is still blocked in a foreground input wait.
///
/// The historical failure committed such a checkpoint, then refused to send its
/// prompt because the foreground round was open. That left an unrecoverable
/// `DispatchUncertain` owner with no corresponding provider request.
#[test]
fn deferred_dispatch_waits_for_open_foreground_round_to_finish() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("enter input wait".to_owned()))
        .expect("dispatch initial inference");
    let initial_prompt = read_nth_prompt_created(&h, 0);
    let initial_provider =
        h.provider_runtime.pending_prompts[&initial_prompt.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &initial_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: initial_prompt.agent_prompt_id.clone(),
            originator: initial_prompt.originator.clone(),
        }),
    )
    .expect("record initial provider submission");

    let wait_call_id = ToolCallId::from("wait-deferred-open-round");
    h.handle_provider_response_finished(provider_input_wait_response(
        &initial_prompt,
        wait_call_id.as_str(),
        10,
    ))
    .expect("provider opens foreground input wait");
    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolStarted(started) if started.call_id == wait_call_id
    )));
    assert!(h.input_wait_pending_for(&cid));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(h.agent_has_open_foreground_tool_round(&cid));
    let open_round_activation_cut = h
        .activation_cut_before_current_head(&cid)
        .expect("closed prefix before open-round response");

    let events_before_drain = event_log_events(&h);
    let checkpoints_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
        .count();
    let starts_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptStarted(_)))
        .count();
    let prompts_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
        .count();
    let submissions_before = events_before_drain
        .iter()
        .filter(|event| matches!(event, Event::ProviderPromptSubmitted(_)))
        .count();
    h.runtime_io.publication.idle_dispatches.push_back(
        path_crate_harness::interception::DeferredPromptDispatch {
            activation_source_seq: None,
            cid: cid.clone(),
            activation_cut: None,
            activation_through: None,
            obligation: DeferredActivationObligation::OrdinaryPublishIdle,
        },
    );

    h.drain_publish_idle_dispatches();

    let events_while_waiting = event_log_events(&h);
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        checkpoints_before,
        "an open foreground round must block checkpoint creation"
    );
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptStarted(_)))
            .count(),
        starts_before
    );
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        prompts_before
    );
    assert_eq!(
        events_while_waiting
            .iter()
            .filter(|event| matches!(event, Event::ProviderPromptSubmitted(_)))
            .count(),
        submissions_before
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::None
    ));
    assert_eq!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .filter(|deferred| deferred.cid == cid && !deferred.obligation.is_committed())
            .count(),
        1,
        "the stale generic obligation remains queued exactly once"
    );

    let deadline = h.next_input_wait_deadline().expect("input-wait deadline");
    h.process_runtime_deadlines_at(deadline);
    assert!(!h.input_wait_pending_for(&cid));
    assert!(!h.agent_has_open_foreground_tool_round(&cid));
    let through = tau_proto::AgentHead::Node(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .head
            .expect("wait terminal"),
    );
    let continuation = read_nth_prompt_created(&h, 1);
    let continuation_provider =
        h.provider_runtime.pending_prompts[&continuation.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &continuation_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: continuation.agent_prompt_id.clone(),
            originator: continuation.originator.clone(),
        }),
    )
    .expect("record continuation provider submission");

    let events = event_log_events(&h);
    let checkpoints = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some(started)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].through, through);
    assert_eq!(checkpoints[0].model, Some("test/model".into()));
    assert_eq!(
        checkpoints[0].operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    assert_eq!(
        checkpoints[0].activation_cut,
        Some(open_round_activation_cut)
    );
    let sequence = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("checkpoint")
            }
            Event::AgentPromptStarted(started)
                if started.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("started")
            }
            Event::AgentPromptCreated(created)
                if created.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("created")
            }
            Event::ProviderPromptSubmitted(submitted)
                if submitted.agent_prompt_id == continuation.agent_prompt_id =>
            {
                Some("submitted")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sequence, ["checkpoint", "started", "created", "submitted"]);
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// Visible input that settles an activating wait must release the complete
/// foreground tool round and dispatch one continuation. A second visible input
/// remains ordered behind that continuation instead of leaving the agent
/// permanently idle with queued activation.
#[test]
fn activating_wait_settlement_dispatches_once_and_preserves_next_input() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("enter input wait".to_owned()))
        .expect("dispatch initial inference");
    let initial_prompt = read_nth_prompt_created(&h, 0);
    let initial_provider =
        h.provider_runtime.pending_prompts[&initial_prompt.agent_prompt_id].clone();
    h.handle_extension_event_inner(
        &initial_provider,
        Event::ProviderPromptSubmittedReported(tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: initial_prompt.agent_prompt_id.clone(),
            originator: initial_prompt.originator.clone(),
        }),
    )
    .expect("record initial provider submission");
    h.handle_provider_response_finished(provider_input_wait_response(
        &initial_prompt,
        "activating-wait-settlement",
        60,
    ))
    .expect("open activating wait");
    assert!(h.input_wait_pending_for(&cid));

    let checkpoints_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentInferenceDispatchStarted(_))
    });
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::user("first visible activation".to_owned()),
        )
        .expect("submit first activation"),
        PromptSubmission::Queued
    );
    assert!(!h.input_wait_pending_for(&cid));
    assert_eq!(tool_result_count(&h, "activating-wait-settlement"), 1);
    assert_eq!(
        event_log_count(&h, |event| {
            matches!(event, Event::AgentInferenceDispatchStarted(_))
        }),
        checkpoints_before + 1,
        "settlement releases exactly one post-tool continuation"
    );
    let continuation_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("continuation prompt");
    let continuation = read_prompt_created(&h, &continuation_id);
    assert!(continuation.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("first visible activation"))
    }));
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::user("second visible activation".to_owned()),
        )
        .expect("submit second activation"),
        PromptSubmission::Queued
    );
    h.handle_provider_response_finished(provider_text_response(
        &continuation.agent_prompt_id,
        agent_id.clone(),
        "continuation complete",
    ))
    .expect("finish continuation");
    let next_prompt_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("second activation prompt");
    let next_prompt = read_prompt_created(&h, &next_prompt_id);
    assert!(next_prompt.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("second visible activation"))
    }));
    assert!(h.runtime_io.publication.deferred.is_empty());
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// A peer-created endpoint adopts user authority before opening an activating
/// wait. Later visible input must settle the wait and release exactly one
/// continuation under that adopted authority.
#[test]
fn peer_entrypoint_activating_wait_settlement_dispatches_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    configure_inter_session_receivers(&mut h, &[("engineer", true)]);
    let received = h.handle_external_agent_message_request_without_auth_for_test(
        tau_proto::ExternalAgentMessageRequest {
            request_id: "peer-activating-wait".to_owned(),
            message_id: tau_proto::AgentMessageId::parse("peer-activating-wait-message")
                .expect("message id"),
            capability: "test-only".to_owned(),
            sender_session_id: test_session_id("sender-session"),
            sender_id: crate::parse_agent_id("sender-agent"),
            recipient_session_id: h.session_runtime.current_session_id.clone(),
            recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
            kind: tau_proto::AgentMessageKind::Message,
            message: "wait for visible input".to_owned(),
        },
    );
    assert_eq!(received.failure, None);
    assert!(received.started);
    let agent_id = received.recipient_id.expect("peer endpoint");
    let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .peer_entrypoint_endpoint
    );
    let initial_prompt = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(prompt_id, prompt_cid)| {
            (prompt_cid == &cid).then(|| read_prompt_created(&h, prompt_id))
        })
        .expect("side prompt");
    assert!(matches!(
        initial_prompt.originator,
        tau_proto::PromptOriginator::Extension { .. }
    ));
    let mut initial_response = provider_text_response(
        &initial_prompt.agent_prompt_id,
        agent_id.clone(),
        "peer query complete",
    );
    initial_response.originator = initial_prompt.originator.clone();
    h.handle_provider_response_finished(initial_response)
        .expect("finish restricted peer query");
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::human_ui_watch_notified("wait for visible input".to_owned()),
        )
        .expect("adopt peer endpoint"),
        PromptSubmission::Dispatched
    );
    let adopted_prompt_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("adopted prompt");
    let initial_prompt = read_prompt_created(&h, &adopted_prompt_id);
    assert_eq!(initial_prompt.originator, tau_proto::PromptOriginator::User);
    let mut wait_response =
        provider_input_wait_response(&initial_prompt, "side-activating-wait", 60);
    wait_response.originator = initial_prompt.originator.clone();
    wait_response.output_items.insert(
        0,
        ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Full,
            text: "wait for visible input".to_owned(),
        }),
    );
    h.handle_provider_response_finished(wait_response)
        .expect("open activating wait");
    assert!(h.input_wait_pending_for(&cid));

    let checkpoints_before = event_log_count(&h, |event| {
        matches!(event, Event::AgentInferenceDispatchStarted(_))
    });
    let interactions_before = event_log_count(&h, |event| {
        matches!(
            event,
            Event::AgentUserInteractionRecorded(interaction) if interaction.agent_id == agent_id
        )
    });
    h.record_accepted_visible_user_interaction(agent_id.as_str())
        .expect("record first visible interaction");
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::human_ui_watch_notified("first visible activation".to_owned()),
        )
        .expect("submit first activation"),
        PromptSubmission::Queued
    );
    assert_eq!(tool_result_count(&h, "side-activating-wait"), 1);
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentUserInteractionRecorded(interaction) if interaction.agent_id == agent_id
        )),
        interactions_before + 1
    );
    assert_eq!(
        event_log_count(&h, |event| {
            matches!(event, Event::AgentInferenceDispatchStarted(_))
        }),
        checkpoints_before + 1,
        "settlement releases exactly one adopted-endpoint continuation"
    );
    let continuation_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("continuation prompt");
    let continuation = read_prompt_created(&h, &continuation_id);
    assert_eq!(continuation.originator, initial_prompt.originator);
    assert!(continuation.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("first visible activation"))
    }));
    let settlement_order = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("peer journal")
        .iter()
        .filter_map(|record| match &record.event {
            Event::AgentToolTerminalClassified(_) => Some("classified"),
            Event::ProviderToolResult(result)
                if result.call_id.as_str() == "side-activating-wait" =>
            {
                Some("terminal")
            }
            Event::AgentToolWaitSettled(_) => Some("settled"),
            Event::AgentPromptSteered(steered)
                if steered.agent_id == agent_id
                    && steered.text == "first visible activation"
                    && steered.submission_source == tau_proto::PromptSubmissionSource::HumanUi =>
            {
                Some("steered")
            }
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_prompt_id == continuation_id =>
            {
                Some("checkpoint")
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        settlement_order,
        ["classified", "terminal", "settled", "steered", "checkpoint"]
    );

    h.record_accepted_visible_user_interaction(agent_id.as_str())
        .expect("record second visible interaction");
    assert_eq!(
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::human_ui_watch_notified("second visible activation".to_owned()),
        )
        .expect("submit second activation"),
        PromptSubmission::Queued
    );
    assert_eq!(
        event_log_count(&h, |event| matches!(
            event,
            Event::AgentUserInteractionRecorded(interaction) if interaction.agent_id == agent_id
        )),
        interactions_before + 2
    );
    let mut continuation_response = provider_text_response(
        &continuation.agent_prompt_id,
        agent_id.clone(),
        "continuation complete",
    );
    continuation_response.originator = continuation.originator.clone();
    h.handle_provider_response_finished(continuation_response)
        .expect("finish continuation");
    let next_prompt_id = h.agent_runtime.agent_registry.agents[&cid]
        .dispatch
        .in_flight_prompt
        .clone()
        .expect("second activation prompt");
    let next_prompt = read_prompt_created(&h, &next_prompt_id);
    assert!(next_prompt.context.flatten().iter().any(|item| {
        text_part(item).is_some_and(|text| text.contains("second visible activation"))
    }));
    assert!(h.runtime_io.publication.idle_dispatches.is_empty());
    h.shutdown().expect("shutdown");
}

/// A crash after the wait terminal and visible-input steer commit, but before
/// the inference checkpoint commits, leaves the durable steer as the recovery
/// owner. Resume dispatches it once without repairing the already-complete
/// wait.
#[test]
fn peer_entrypoint_activating_wait_restart_recovers_committed_steer_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let agent_id = {
        let mut h = quiet_provider_harness(&state).expect("start");
        configure_inter_session_receivers(&mut h, &[("engineer", true)]);
        let received = h.handle_external_agent_message_request_without_auth_for_test(
            tau_proto::ExternalAgentMessageRequest {
                request_id: "peer-activating-wait-restart".to_owned(),
                message_id: tau_proto::AgentMessageId::parse(
                    "peer-activating-wait-restart-message",
                )
                .expect("message id"),
                capability: "test-only".to_owned(),
                sender_session_id: test_session_id("sender-session"),
                sender_id: crate::parse_agent_id("sender-agent"),
                recipient_session_id: h.session_runtime.current_session_id.clone(),
                recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
                kind: tau_proto::AgentMessageKind::Message,
                message: "wait for visible input".to_owned(),
            },
        );
        assert_eq!(received.failure, None);
        assert!(received.started);
        let agent_id = received.recipient_id.expect("peer endpoint");
        let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
        let initial_prompt = h
            .prompt_coordination
            .prompt_runtime
            .agents
            .iter()
            .find_map(|(prompt_id, prompt_cid)| {
                (prompt_cid == &cid).then(|| read_prompt_created(&h, prompt_id))
            })
            .expect("side prompt");
        let mut initial_response = provider_text_response(
            &initial_prompt.agent_prompt_id,
            agent_id.clone(),
            "peer query complete",
        );
        initial_response.originator = initial_prompt.originator.clone();
        h.handle_provider_response_finished(initial_response)
            .expect("finish restricted peer query");
        assert_eq!(
            h.submit_prompt_to_agent(
                h.session_runtime.current_session_id.clone(),
                agent_id.as_str(),
                PendingPrompt::human_ui_watch_notified("wait for visible input".to_owned()),
            )
            .expect("adopt peer endpoint"),
            PromptSubmission::Dispatched
        );
        let adopted_prompt_id = h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .clone()
            .expect("adopted prompt");
        let initial_prompt = read_prompt_created(&h, &adopted_prompt_id);
        assert_eq!(initial_prompt.originator, tau_proto::PromptOriginator::User);
        let mut wait_response =
            provider_input_wait_response(&initial_prompt, "restart-side-activating-wait", 60);
        wait_response.originator = initial_prompt.originator.clone();
        wait_response.output_items.insert(
            0,
            ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "wait for visible input".to_owned(),
            }),
        );
        h.handle_provider_response_finished(wait_response)
            .expect("open activating wait");

        let _checkpoint_interceptor = connect_test_tool(&mut h, "wait-checkpoint-interceptor");
        h.handle_extension_event(
            "wait-checkpoint-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register checkpoint interceptor");
        let checkpoints_before = event_log_count(&h, |event| {
            matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
            )
        });
        h.record_accepted_visible_user_interaction(agent_id.as_str())
            .expect("record first visible interaction");
        assert_eq!(
            h.submit_prompt_to_agent(
                h.session_runtime.current_session_id.clone(),
                agent_id.as_str(),
                PendingPrompt::human_ui_watch_notified("first visible activation".to_owned()),
            )
            .expect("submit first activation"),
            PromptSubmission::Queued
        );
        assert_eq!(tool_result_count(&h, "restart-side-activating-wait"), 1);
        assert!(event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptSteered(steered)
                if steered.agent_id == agent_id
                    && steered.text == "first visible activation"
                    && steered.submission_source
                        == tau_proto::PromptSubmissionSource::HumanUi
        )));
        assert_eq!(
            event_log_count(&h, |event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
            )),
            checkpoints_before
        );
        assert!(h.runtime_io.publication.pending_intercept.is_some());
        assert!(
            h.session_runtime.persistence_owner.as_ref().is_some_and(
                |owner| owner.wait_for_latest_durability_for_test(Duration::from_secs(5))
            )
        );
        drop(h);
        agent_id
    };

    {
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume after checkpoint cut");
        let cid = h.agent_runtime.agent_registry.agent_routes[agent_id.as_str()].clone();
        assert_eq!(
            event_log_count(&h, |event| matches!(
                event,
                Event::AgentInferenceDispatchStarted(started) if started.agent_id == agent_id
            )),
            1,
            "resume dispatches the uncovered durable steer once"
        );
        assert!(!event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ProviderToolError(error)
                if error.call_id.as_str() == "restart-side-activating-wait"
        )));
        let continuation_id = h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .in_flight_prompt
            .clone()
            .expect("recovered continuation");
        let continuation = read_prompt_created(&h, &continuation_id);
        assert_eq!(continuation.originator, tau_proto::PromptOriginator::User);
        assert!(continuation.context.flatten().iter().any(|item| {
            text_part(item).is_some_and(|text| text.contains("first visible activation"))
        }));
        let mut continuation_response =
            provider_text_response(&continuation_id, agent_id.clone(), "continuation complete");
        continuation_response.originator = continuation.originator.clone();
        h.handle_provider_response_finished(continuation_response)
            .expect("finish recovered continuation");
        h.shutdown().expect("shutdown recovered harness");
    }

    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("second resume");
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentInferenceDispatchStarted(started)
            if started.agent_id == agent_id
    )));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ProviderToolError(error)
            if error.call_id.as_str() == "restart-side-activating-wait"
    )));
    h.shutdown().expect("shutdown second resume");
}

/// Committed endpoint unload crosses the production lifecycle boundary and
/// drops runtime-only input waits, retained completion/checkpoint owners,
/// attempt markers, and deferred activation obligations before removal.
#[test]
fn agent_unload_discards_registered_input_wait() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let durable_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("agent id");
    let call = wait_input_call("wait-input-unload");
    seed_tools_running(&mut h, &cid, vec![call.id.clone()]);
    h.handle_wait_tool_call(&cid, &call, ToolName::new("wait"))
        .expect("register input wait");
    assert!(h.input_wait_pending_for(&cid));
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-unload-retry").expect("transaction");
    h.prompt_coordination
        .prompt_runtime
        .pending_publish_completions
        .insert(
            cid.clone(),
            AgentPublishCompletion::StandaloneContinuation {
                transaction_id: transaction_id.clone(),
                model: "test/model".into(),
                activation_cut: tau_proto::AgentHead::Root,
                batch_parent: tau_proto::AgentHead::Root,
                source: None,
                retry_prompts: vec![PendingPrompt::user("stale unload retry".to_owned())],
                complete_on_commit: true,
                owned_publication: None,
            },
        );
    h.prompt_coordination
        .compaction_runtime
        .enqueued_inference_checkpoints
        .insert((crate::parse_agent_id(&durable_id), transaction_id));
    h.enqueue_committed_activation_dispatch(
        cid.clone(),
        Some(tau_proto::AgentHead::Root),
        Some(tau_proto::AgentHead::Root),
    );

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
            session_id: h.session_runtime.current_session_id.clone(),
            agent_id: crate::parse_agent_id(&durable_id),
        }),
    );
    assert!(!h.input_wait_pending_for(&cid));
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
    );
    assert!(
        h.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .is_empty()
    );
    assert!(
        h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .all(|dispatch| dispatch.cid != cid)
    );
    h.activate_waits_for(&cid, tau_proto::ObservationId::random());
    assert_eq!(tool_result_count(&h, call.id.as_str()), 0);
    h.shutdown().expect("shutdown");
}

/// Exact `wait` is scoped to the background call owner before any waiter is
/// registered. A cross-owner wait should fail immediately rather than creating
/// active wait state that later messages could interrupt.
#[test]
fn cross_owner_exact_wait_is_rejected_without_active_wait_state() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config.selected_model = Some("test/model".into());

    let _tool_events = connect_test_tool(&mut h, "conn-cross-msg-wait");
    h.tool_routing.registry.register(
        &crate::test_connection_id("conn-cross-msg-wait"),
        instant_background_test_tool_spec("slow_cross_msg_wait"),
    );

    let target_cid = ensure_test_user_agent(&mut h);
    let waiter_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let target_agent_id = h.agent_runtime.agent_registry.agents[&target_cid]
        .identity
        .agent_id
        .clone()
        .expect("target agent id");
    let waiter_agent_id = h.agent_runtime.agent_registry.agents[&waiter_cid]
        .identity
        .agent_id
        .clone()
        .expect("waiter agent id");
    finish_test_agent_context_wait(
        &mut h,
        &tau_proto::AgentId::parse(&waiter_agent_id).expect("agent id"),
    );

    let background_call_id: ToolCallId = "bg-cross-msg-wait".into();
    start_background_tool_and_finish_placeholder_turn(
        &mut h,
        &target_cid,
        background_call_id.as_str(),
        "slow_cross_msg_wait",
    );

    let wait_call_id: ToolCallId = "wait-cross-msg-interrupt".into();
    let wait_call = AgentToolCall {
        call_ref: None,
        id: wait_call_id.clone(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(background_call_id.to_string()),
        )]),
    };
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&waiter_cid)
        .expect("waiter agent")
        .dispatch
        .pending_prompts
        .push_back(PendingPrompt::user("queued waiter input".to_owned()));
    h.handle_wait_tool_call(&waiter_cid, &wait_call, ToolName::new("wait"))
        .expect("reject cross-owner wait before queued-input preemption");

    assert!(event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolError(error)
            if error.call_id.as_str() == wait_call_id.as_str()
                && error.message == "unknown tool call: `bg-cross-msg-wait`"
    )));

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("test-message-to-target-owner")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&target_agent_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "target owner only".to_owned(),
        }),
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("wait_outcome: interrupted"))
    )));

    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("test-message-to-wait-owner")
                .expect("test identifier must satisfy its grammar"),
            sender_id: crate::parse_agent_id("manager"),
            sender_session_id: None,
            recipient_id: crate::parse_agent_id(&waiter_agent_id),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "waiter should resume".to_owned(),
        }),
    );

    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolResult(result)
            if result.call_id.as_str() == wait_call_id.as_str()
                && matches!(&result.result, CborValue::Text(text) if text.contains("wait_outcome: interrupted"))
    )));
    h.shutdown().expect("shutdown");
}

/// A crash after an activating prompt occurrence but before its marked owner's
/// response closes that owner as Stale, materializes the exact occurrence, and
/// dispatches one successor. Prompt, typed-message, and raw-fact ingress share
/// this rule.
#[test]
fn resume_supersedes_uncertain_v1_owner_for_each_activation_variant() {
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let cases = [
        (
            "injected deferred Q",
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                inference_activation: true,
                agent_id: agent_id.clone(),
                text: "injected deferred Q".to_owned(),
                message_class: tau_proto::PromptMessageClass::Internal,
            }),
        ),
        (
            "steered deferred Q",
            Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                self_compaction_terminal: None,
                inference_activation: true,
                submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
                agent_id: agent_id.clone(),
                text: "steered deferred Q".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                ctx_id: Some("deferred-q".to_owned()),
            }),
        ),
        (
            "typed message deferred Q",
            Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
                message_id: tau_proto::AgentMessageId::parse("typed-deferred-q")
                    .expect("message id"),
                sender_id: tau_proto::AgentId::parse("sender").expect("sender"),
                sender_session_id: None,
                recipient_id: agent_id.clone(),
                kind: tau_proto::AgentMessageKind::Message,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: None,
                watch_lifecycle: None,
                message: "typed message deferred Q".to_owned(),
            }),
        ),
        (
            "raw fact deferred Q",
            Event::MessageDelivered(tau_proto::MessageDelivered::new(
                tau_proto::MessagePublisherId::parse("external").expect("publisher"),
                tau_proto::MessageAgentTarget::new(agent_id.as_str()),
                tau_proto::MessageFactId::new("raw-deferred-q"),
                tau_proto::MessageParty {
                    stable_id: "external".to_owned(),
                    display_name: None,
                    sender_auth: None,
                },
                None,
                "raw fact deferred Q",
            )),
        ),
    ];

    for (text, activation) in cases {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_main_agent_loaded(&state);
        let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
        append_seed_agent_event(
            &mut store,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: "H".to_owned(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::Internal,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        );
        let through = store
            .agent("main")
            .and_then(tau_core::AgentTree::head)
            .expect("H node");
        let owner = test_agent_prompt_id(format!("ap-{}", text.replace(' ', "-")));
        append_seed_agent_event(
            &mut store,
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                output_length_continuation: None,
                agent_id: agent_id.clone(),
                transaction_id: None,
                agent_prompt_id: owner.clone(),
                through: tau_proto::AgentHead::Node(through),
                model: Some("echo/model".into()),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(tau_proto::AgentHead::Root),
            }),
        );
        if activation.message_agent_target().is_some() {
            store
                .append_agent_message_fact_at(
                    "main",
                    None,
                    activation,
                    tau_proto::UnixMicros::now(),
                )
                .expect("append raw activating fact");
        } else {
            append_seed_agent_event(&mut store, activation);
        }
        assert!(
            store
                .agent("main")
                .and_then(|tree| tree
                    .node_for_durable_event_seq(tau_core::PersistedAgentEventSeq::new(3)))
                .is_none(),
            "the deferred occurrence has no node before closure"
        );
        drop(store);

        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let prompt = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentPromptCreated(prompt) => Some(prompt),
                _ => None,
            })
            .unwrap_or_else(|| {
                let cid = test_user_agent(&h);
                panic!(
                    "{text}: no successor; state={:?}, dispatch={:?}, wakes={:?}, events={:?}",
                    h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
                    h.agent_runtime.agent_registry.agents[&cid]
                        .dispatch
                        .activation_dispatch,
                    h.agent_runtime.agent_registry.agents[&cid]
                        .dispatch
                        .pending_message_wakes,
                    event_log_events(&h)
                        .iter()
                        .map(Event::name)
                        .collect::<Vec<_>>()
                )
            });
        let rendered = serde_json::to_string(&prompt.context).expect("context");
        assert_eq!(rendered.matches(text).count(), 1);
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentPromptTerminated(terminated)
                        if terminated.agent_prompt_id == owner
                            && terminated.reason
                                == tau_proto::AgentPromptTerminationReason::Stale
                ))
                .count(),
            1,
            "restore closes the uncertain owner exactly once"
        );
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            1,
            "one deferred occurrence creates one successor"
        );
        h.shutdown().expect("shutdown");
    }
}

/// Explicit-false canonical facts are passive transcript context and must not
/// independently wake inference during cold replay.
#[test]
fn resume_does_not_dispatch_false_canonical_facts() {
    let agent_id = tau_proto::AgentId::parse("main").expect("agent id");
    let cases = [
        Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: "passive submitted".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
        Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            inference_activation: false,
            agent_id: agent_id.clone(),
            text: "passive injected".to_owned(),
            message_class: tau_proto::PromptMessageClass::Internal,
        }),
        Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
            self_compaction_terminal: None,
            inference_activation: false,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            agent_id,
            text: "passive steered".to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::Internal,
            internal_kind: None,
            ctx_id: None,
        }),
    ];

    for event in cases {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_inference_activation_event(&state, event);

        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        assert!(!event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptCreated(_) | Event::AgentInferenceDispatchStarted(_)
        )));
        h.shutdown().expect("shutdown");
    }
}

/// Every provider-qualified wait declaration records a typed pre-resolution
/// observation, including malformed arguments and unresolved exact targets.
#[test]
fn wait_observation_classifies_invalid_and_unresolved_exact_arguments() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut harness);
    let agent_id = harness.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent id");

    for (byte, call_id, arguments, expected_mode) in [
        (
            31,
            "invalid-wait",
            CborValue::Text("not a map".into()),
            tau_proto::ToolWaitMode::InvalidArguments,
        ),
        (
            32,
            "unresolved-wait",
            CborValue::Map(vec![(
                CborValue::Text("tool_call_id".into()),
                CborValue::Text("missing-target".into()),
            )]),
            tau_proto::ToolWaitMode::ExactUnresolved,
        ),
    ] {
        let call_ref = tau_proto::ToolCallRef {
            declaration: tau_proto::ObservationId::from_bytes([byte; 16]),
            item_index: 0,
        };
        harness
            .handle_wait_tool_call(
                &cid,
                &AgentToolCall {
                    call_ref: Some(call_ref),
                    id: call_id.into(),
                    name: ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments,
                },
                ToolName::new("wait"),
            )
            .expect("handle wait");
        assert!(
            harness
                .session_runtime
                .agent_store
                .agent_events(&agent_id)
                .expect("agent records")
                .iter()
                .any(|record| matches!(
                    &record.event,
                    Event::AgentToolWaitObserved(observed)
                        if observed.wait_call == call_ref && observed.mode == expected_mode
                ))
        );
    }
}

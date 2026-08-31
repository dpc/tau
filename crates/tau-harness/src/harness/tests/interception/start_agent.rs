//! Contract tests for `SPEC-start-agent-requests`.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::{event_log as path_crate_event_log, extension as path_crate_extension};

/// Build one start-agent request with observable correlation and instruction.
fn request(query_id: &str, instruction: &str) -> StartAgentRequest {
    StartAgentRequest {
        trusted_internal_spans: Vec::new(),
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
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
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
    let mut seq = path_crate_event_log::EventLogSeq::new(0);
    while let Some(entry) = h.runtime_io.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        if predicate(&entry.event) {
            return entry;
        }
    }
    panic!("matching committed event");
}

/// Count side agents carrying one extension query correlation.
fn query_agent_count(h: &Harness, query_id: &str) -> usize {
    h.agent_runtime
        .agent_registry
        .agents
        .values()
        .filter(|agent| {
            matches!(
                &agent.identity.originator,
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

/// Assert every bounded startup owner and index was released.
fn assert_start_coordinator_empty(h: &Harness) {
    let coordinator = &h.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
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

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-drop", &crate::test_connection_id("drop-me")),
    )
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

/// Dropping the first post-accept phase closes the accepted obligation exactly
/// once without exposing creation, membership, prompt, or dispatch.
#[test]
fn dropped_agent_started_phase_commits_one_correlated_failure() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let watcher_cid = ensure_test_user_agent(&mut h);
    let watcher_id = h
        .ensure_agent_id_for_agent(&watcher_cid)
        .expect("watcher id");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "agent-started-interceptor");
    h.handle_extension_event(
        "agent-started-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
                EventSelector::Exact(tau_proto::EventName::AGENT_START_FAILED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register creation interceptor");

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-drop-started", "initial"),
    )
    .expect("park creation");
    let accepted = directed_acceptance(&sink, "q-drop-started").expect("committed acceptance");
    h.set_agent_watch(
        watcher_id.as_str(),
        accepted.agent_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentStart,
    );
    h.handle_extension_event(
        "agent-started-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop creation");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentStartFailed(_))
    ));
    h.handle_extension_event(
        "agent-started-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("must-pass failure terminal");

    let events = event_log_events(&h);
    let accepted_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::StartAgentAccepted(candidate)
                    if candidate.start_id == accepted.start_id
            )
        })
        .expect("accepted occurrence");
    let failures = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            Event::AgentStartFailed(failed)
                if failed.start_id == accepted.start_id
                    && failed.agent_id == accepted.agent_id
                    && failed.reason == tau_proto::AgentStartFailure::InterceptionDropped =>
            {
                Some(index)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 1);
    assert!(accepted_index < failures[0]);
    assert!(events.iter().all(|event| {
        !matches!(
            event,
            Event::AgentStarted(started) if started.agent_id == accepted.agent_id
        )
    }));
    assert!(directed_result(&sink, "q-drop-started").is_some_and(|result| result.error.is_some()));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentMessageReceived(message)
                    if message.sender_id == accepted.agent_id
                        && message.recipient_id == watcher_id
                        && message.kind == tau_proto::AgentMessageKind::WatchLifecycle
            ))
            .count(),
        1
    );
    assert!(
        !h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&accepted.agent_id)
    );
    assert_start_coordinator_empty(&h);
}

/// A wrong-family replacement rejects a nonterminal startup phase instead of
/// falling back to the original immutable creation fact.
#[test]
fn wrong_family_agent_started_replacement_terminalizes_start() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "wrong-family-start-interceptor");
    h.handle_extension_event(
        "wrong-family-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register creation interceptor");
    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-wrong-family", "initial"),
    )
    .expect("park creation");
    let accepted = directed_acceptance(&sink, "q-wrong-family").expect("accepted");

    h.handle_extension_event(
        "wrong-family-start-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::HarnessNotice(
                tau_proto::HarnessNotice {
                    kind: tau_proto::notice_kind::EXTENSION_NOTICE.to_owned(),
                    message: "wrong family".to_owned(),
                    level: tau_proto::NoticeLevel::Info,
                    purpose: tau_proto::NoticePurpose::Diagnostic,
                },
            )))),
        })),
    )
    .expect("reject replacement");

    let events = event_log_events(&h);
    assert!(events.iter().all(|event| {
        !matches!(
            event,
            Event::AgentStarted(started) if started.agent_id == accepted.agent_id
        )
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStartFailed(failed) if failed.start_id == accepted.start_id
            ))
            .count(),
        1
    );
    assert_start_coordinator_empty(&h);
}

/// Interceptor disconnect preserves the global Pass(None) policy: the original
/// startup phase commits and no contradictory failure terminal is emitted.
#[test]
fn startup_interceptor_disconnect_passes_original_without_failure() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "disconnect-start-interceptor");
    h.handle_extension_event(
        "disconnect-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register creation interceptor");
    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-disconnect-start", "initial"),
    )
    .expect("park creation");

    h.fail_pending_intercept_for_disconnect(&crate::test_connection_id(
        "disconnect-start-interceptor",
    ));

    let events = event_log_events(&h);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AgentStarted(_)))
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Event::AgentStartFailed(_)))
    );
    assert_start_coordinator_empty(&h);
}

/// Failure after membership removes runtime authority at the failure terminal;
/// the separately published unload may still park or reject without reviving
/// it.
#[test]
fn post_membership_failure_removes_route_before_unload_resolves() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "post-membership-interceptor");
    h.handle_extension_event(
        "post-membership-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register prompt and unload interceptor");
    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-post-membership", "initial"),
    )
    .expect("park initial prompt");
    let accepted = directed_acceptance(&sink, "q-post-membership").expect("accepted");

    h.handle_extension_event(
        "post-membership-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("drop initial prompt");

    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::SessionAgentUnloaded(unloaded))
            if unloaded.agent_id == accepted.agent_id
    ));
    assert!(
        !h.agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&accepted.agent_id)
    );
    assert!(
        !h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&accepted.agent_id)
    );
    let parked_current = h
        .build_session_agent_list(
            &h.session_runtime.current_session_id,
            tau_proto::SessionAgentListScope::Current,
        )
        .expect("parked current roster");
    let parked_entry = parked_current
        .iter()
        .find(|entry| entry.agent_id == accepted.agent_id)
        .expect("committed membership remains visible until unload commits");
    assert_eq!(
        parked_entry.lifecycle,
        tau_proto::SessionAgentLifecycle::Unavailable
    );
    assert_start_coordinator_empty(&h);

    h.handle_extension_event(
        "post-membership-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit unload");
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStartFailed(failed) if failed.start_id == accepted.start_id
            ))
            .count(),
        1
    );
    let failure_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentStartFailed(failed) if failed.start_id == accepted.start_id
            )
        })
        .expect("failure terminal");
    let unload_indexes = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                Event::SessionAgentUnloaded(unloaded)
                    if unloaded.agent_id == accepted.agent_id
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(unload_indexes.len(), 1);
    assert!(
        unload_indexes
            .first()
            .is_none_or(|unload_index| failure_index < *unload_index)
    );
    let warm_current = h
        .build_session_agent_list(
            &h.session_runtime.current_session_id,
            tau_proto::SessionAgentListScope::Current,
        )
        .expect("warm current roster");
    assert!(
        warm_current
            .iter()
            .all(|entry| entry.agent_id != accepted.agent_id)
    );
    assert!(
        h.pending_agent_summary_data()
            .iter()
            .all(|(agent_id, _)| agent_id != accepted.agent_id.as_str())
    );
    drop(h);

    let reopened =
        quiet_provider_harness_with_start_reason(tmp.path(), tau_proto::SessionStartReason::Resume)
            .expect("cold reopen");
    assert!(
        !reopened
            .agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&accepted.agent_id)
    );
    assert!(
        !reopened
            .agent_runtime
            .agent_registry
            .session_loaded
            .contains(&accepted.agent_id)
    );
    assert!(
        !reopened
            .agent_runtime
            .agent_registry
            .restored_unavailable
            .contains_key(&accepted.agent_id)
    );
    let reopened_current = reopened
        .build_session_agent_list(
            &reopened.session_runtime.current_session_id,
            tau_proto::SessionAgentListScope::Current,
        )
        .expect("reopened current roster");
    assert!(
        reopened_current
            .iter()
            .all(|entry| entry.agent_id != accepted.agent_id)
    );
    assert!(
        reopened
            .pending_agent_summary_data()
            .iter()
            .all(|(agent_id, _)| agent_id != accepted.agent_id.as_str())
    );
}

/// Semantic admission rejection of the ordinary unload preserves the durable
/// membership prefix as unavailable without restoring startup work.
#[test]
fn post_membership_unload_admission_rejection_restores_unavailable() {
    let tmp = TempDir::new().expect("tempdir");
    let accepted = {
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        let sink = connect_ready_configured_extension(
            &mut h,
            "requester",
            "requester",
            tau_proto::ClientKind::Action,
        );
        let _interceptor = connect_test_tool(&mut h, "preunload-interceptor");
        h.handle_extension_event(
            "preunload-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![
                    EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_SUBMITTED),
                    EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_UNLOADED),
                ],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register prompt and unload interceptor");
        h.handle_extension_event_inner(
            &crate::test_connection_id("requester"),
            request_event("q-cold-preunload", "initial"),
        )
        .expect("park prompt");
        let accepted = directed_acceptance(&sink, "q-cold-preunload").expect("accepted");
        h.handle_extension_event(
            "preunload-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Drop,
            })),
        )
        .expect("fail prompt and park unload");
        assert!(matches!(
            h.runtime_io
                .publication
                .pending_intercept
                .as_ref()
                .map(|pending| &pending.event),
            Some(Event::SessionAgentUnloaded(unloaded))
                if unloaded.agent_id == accepted.agent_id
        ));
        reject_next_semantic_admission(&h);
        h.handle_extension_event(
            "preunload-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("reject unload semantic admission");
        assert!(event_log_events(&h).iter().all(|event| !matches!(
            event,
            Event::SessionAgentUnloaded(unloaded) if unloaded.agent_id == accepted.agent_id
        )));
        let current = h
            .build_session_agent_list(
                &h.session_runtime.current_session_id,
                tau_proto::SessionAgentListScope::Current,
            )
            .expect("preunload current roster");
        assert!(current.iter().any(|entry| {
            entry.agent_id == accepted.agent_id
                && entry.lifecycle == tau_proto::SessionAgentLifecycle::Unavailable
        }));
        drop(h);
        accepted
    };

    let reopened =
        quiet_provider_harness_with_start_reason(tmp.path(), tau_proto::SessionStartReason::Resume)
            .expect("cold reopen");
    assert!(
        !reopened
            .agent_runtime
            .agent_registry
            .agent_routes
            .contains_key(&accepted.agent_id)
    );
    assert!(
        reopened
            .agent_runtime
            .agent_registry
            .restored_unavailable
            .contains_key(&accepted.agent_id)
    );
    let current = reopened
        .build_session_agent_list(
            &reopened.session_runtime.current_session_id,
            tau_proto::SessionAgentListScope::Current,
        )
        .expect("reopened preunload current roster");
    assert!(current.iter().any(|entry| {
        entry.agent_id == accepted.agent_id
            && entry.lifecycle == tau_proto::SessionAgentLifecycle::Unavailable
    }));
    let coordinator = &reopened.agent_runtime.agent_registry.start_coordinator;
    assert!(coordinator.operations.is_empty());
    assert!(coordinator.requests.is_empty());
    assert!(coordinator.agents.is_empty());
    assert_eq!(coordinator.retained_bytes, 0);
}

/// Final process shutdown closes a post-accept startup before generation
/// rollover instead of silently discarding its parked prompt owner.
#[test]
fn process_shutdown_terminalizes_parked_startup_prompt() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "shutdown-start-interceptor");
    h.handle_extension_event(
        "shutdown-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register prompt interceptor");
    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-shutdown-start", "initial"),
    )
    .expect("park initial prompt");

    h.shutdown().expect("shutdown");

    let failures = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStartFailed(failed)
                if failed.reason == tau_proto::AgentStartFailure::SessionStopped =>
            {
                Some(failed)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 1);
    assert!(event_log_events(&h).iter().all(|event| {
        !matches!(
            event,
            Event::AgentInferenceDispatchStarted(started)
                if started.agent_id == failures[0].agent_id
        )
    }));
    assert_start_coordinator_empty(&h);
}

/// The startup prompt's committed replacement is the only text folded and
/// delivered in the provider work request.
#[test]
fn startup_prompt_replacement_reaches_provider_with_canonical_text_only() {
    fn provider_context_text_count(prompt: &tau_proto::AgentPromptCreated, text: &str) -> usize {
        prompt
            .context
            .flatten()
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    tau_proto::ContextItem::Message(message)
                        if message.content.iter().any(|part| matches!(
                            part,
                            tau_proto::ContentPart::Text { text: candidate }
                                | tau_proto::ContentPart::SyntheticCompactionSummary {
                                    text: candidate
                                }
                                | tau_proto::ContentPart::HarnessInternalText {
                                    text: candidate
                                } if candidate == text
                        ))
                )
            })
            .count()
    }

    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let provider_sink = connect_test_client(
        &mut h,
        "canonical-provider",
        tau_proto::ClientKind::Provider,
    );
    h.provider_runtime.model_routes.insert(
        tau_proto::ModelId::from("test/model"),
        crate::test_connection_id("canonical-provider"),
    );
    connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "startup-prompt-interceptor");
    h.handle_extension_event(
        "startup-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register prompt interceptor");
    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-prompt-replace", "original startup text"),
    )
    .expect("park initial prompt");
    let mut replacement = match h
        .runtime_io
        .publication
        .pending_intercept
        .as_ref()
        .map(|pending| pending.event.clone())
    {
        Some(Event::AgentPromptSubmitted(prompt)) => prompt,
        other => panic!("expected parked startup prompt, got {other:?}"),
    };
    let child_id = replacement.agent_id.clone();
    replacement.text = "canonical replacement text".to_owned();

    h.handle_extension_event(
        "startup-prompt-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(Some(Box::new(Event::AgentPromptSubmitted(replacement)))),
        })),
    )
    .expect("commit replacement");

    let events = event_log_events(&h);
    let submitted = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentPromptSubmitted(prompt) if prompt.agent_id == child_id => {
                Some(prompt.text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(submitted, ["canonical replacement text"]);
    let provider_prompts = provider_sink
        .lock()
        .expect("provider sink")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::AgentPromptCreated(prompt)) if prompt.agent_id == child_id => {
                Some(prompt.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(provider_prompts.len(), 1);
    assert_eq!(
        provider_context_text_count(&provider_prompts[0], "canonical replacement text"),
        1
    );
    assert_eq!(
        provider_context_text_count(&provider_prompts[0], "original startup text"),
        0
    );
    assert_start_coordinator_empty(&h);
}

/// Cancellation removes the exact parked acceptance owner before releasing its
/// private reservation, so it cannot commit later as an orphan obligation.
#[test]
fn preaccept_cancel_removes_parked_owner_without_failure_terminal() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "acceptance-interceptor");
    h.handle_extension_event(
        "acceptance-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_START_ACCEPTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-cancel-before-accept", "initial"),
    )
    .expect("park acceptance");
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    h.cancel_start_agent_request(
        "q-cancel-before-accept",
        &tau_proto::ToolCallId::new("unused-cancel-call"),
        false,
    )
    .expect("cancel parked acceptance");

    let events = event_log_events(&h);
    assert!(events.iter().all(|event| {
        !matches!(
            event,
            Event::StartAgentAccepted(accepted)
                if accepted.query_id == "q-cancel-before-accept"
        ) && !matches!(event, Event::AgentStartFailed(_))
    }));
    assert!(directed_acceptance(&sink, "q-cancel-before-accept").is_none());
    assert!(
        directed_result(&sink, "q-cancel-before-accept")
            .is_some_and(|result| result.error.is_some())
    );
    assert!(h.runtime_io.publication.pending_intercept.is_none());
    assert_start_coordinator_empty(&h);
}

/// Ephemeral classification begins only after acceptance commits, so a dropped
/// or canceled private reservation leaves no AgentStore id residue.
#[test]
fn preaccept_ephemeral_rejection_leaves_no_agent_store_reservation() {
    for cancel in [false, true] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        let sink = connect_ready_configured_extension(
            &mut h,
            "requester",
            "requester",
            tau_proto::ClientKind::Action,
        );
        let parent_cid = ensure_test_user_agent(&mut h);
        let parent_id = h
            .ensure_agent_id_for_agent(&parent_cid)
            .expect("parent public id");
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&parent_cid)
            .expect("parent runtime")
            .identity
            .persistence = tau_core::AgentPersistenceMode::Ephemeral;
        let _interceptor = connect_test_tool(&mut h, "ephemeral-acceptance-interceptor");
        h.handle_extension_event(
            "ephemeral-acceptance-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_START_ACCEPTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register acceptance interceptor");
        let query_id = if cancel {
            "q-ephemeral-cancel"
        } else {
            "q-ephemeral-drop"
        };
        let mut query = request(query_id, "ephemeral child");
        query.parent_agent = Some(parent_id);
        h.handle_start_agent_request(&crate::test_connection_id("requester"), query)
            .expect("park ephemeral acceptance");
        let operation = h
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .values()
            .next()
            .expect("parked operation");
        assert!(operation.pending.persistence.is_ephemeral());
        let child_id = operation.pending.agent_id.clone();
        assert!(
            !h.session_runtime
                .agent_store
                .agent_id_is_reserved(&child_id)
        );

        if cancel {
            h.cancel_start_agent_request(
                query_id,
                &tau_proto::ToolCallId::new("unused-ephemeral-cancel"),
                false,
            )
            .expect("cancel private reservation");
        } else {
            h.handle_extension_event(
                "ephemeral-acceptance-interceptor",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Drop,
                })),
            )
            .expect("drop private acceptance");
        }

        assert!(
            !h.session_runtime
                .agent_store
                .agent_id_is_reserved(&child_id)
        );
        assert!(directed_acceptance(&sink, query_id).is_none());
        assert_start_coordinator_empty(&h);
    }
}

/// The acceptance occurrence itself is the visibility boundary: replacement
/// commits before projection, while drop rejects the private reservation.
#[test]
fn parked_acceptance_replace_and_drop_have_post_commit_visibility() {
    for (case, action, expect_accepted) in [
        ("replace", None, true),
        ("drop", Some(InterceptAction::Drop), false),
    ] {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(tmp.path()).expect("harness");
        let sink = connect_ready_configured_extension(
            &mut h,
            "requester",
            "requester",
            tau_proto::ClientKind::Action,
        );
        let _interceptor = connect_test_tool(&mut h, "acceptance-interceptor");
        h.handle_extension_event(
            "acceptance-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_START_ACCEPTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register acceptance interceptor");
        let query_id = format!("q-acceptance-{case}");
        h.handle_start_agent_request(
            &crate::test_connection_id("requester"),
            request(&query_id, "initial"),
        )
        .expect("park acceptance");
        assert!(directed_acceptance(&sink, &query_id).is_none());
        let (start_id, operation) = h
            .agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .iter()
            .next()
            .expect("parked acceptance");
        let replacement = Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
            start_id: *start_id,
            query_id: operation.pending.query.query_id.clone(),
            agent_id: operation.pending.cid.clone(),
        });
        h.handle_extension_event(
            "acceptance-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: action
                    .unwrap_or_else(|| InterceptAction::Pass(Some(Box::new(replacement)))),
            })),
        )
        .expect("resolve acceptance");

        assert_eq!(
            directed_acceptance(&sink, &query_id).is_some(),
            expect_accepted
        );
        let events = event_log_events(&h);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::StartAgentAccepted(accepted) if accepted.query_id == query_id
                ))
                .count(),
            usize::from(expect_accepted)
        );
        if !expect_accepted {
            assert!(
                events
                    .iter()
                    .all(|event| !matches!(event, Event::AgentStartFailed(_)))
            );
        }
        assert_start_coordinator_empty(&h);
    }
}

/// Once acceptance wins, cancellation owns exactly one post-accept terminal
/// rather than erasing the already-visible obligation.
#[test]
fn acceptance_commit_racing_cancellation_emits_one_failure_obligation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "startup-interceptor");
    h.handle_extension_event(
        "startup-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_START_ACCEPTED),
                EventSelector::Exact(tau_proto::EventName::AGENT_STARTED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register startup interceptor");
    h.handle_start_agent_request(
        &crate::test_connection_id("requester"),
        request("q-commit-cancel", "initial"),
    )
    .expect("park acceptance");
    h.handle_extension_event(
        "startup-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit acceptance");
    assert!(directed_acceptance(&sink, "q-commit-cancel").is_some());
    assert_eq!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| pending.event.name()),
        Some(tau_proto::EventName::AGENT_STARTED)
    );

    h.cancel_start_agent_request(
        "q-commit-cancel",
        &tau_proto::ToolCallId::new("unused-cancel-call"),
        false,
    )
    .expect("cancel accepted start");

    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentStartFailed(failed)
                if failed.reason == tau_proto::AgentStartFailure::Canceled))
            .count(),
        1
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        Event::AgentStarted(started) if started.agent_id.as_str() != "main"
    )));
    assert_start_coordinator_empty(&h);
}

/// Temporary live publication pressure retains the compact failure owner and a
/// later capacity wake commits exactly one terminal and directed result.
#[test]
fn startup_failure_terminal_retries_once_after_capacity_wake() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Action,
    );
    let _interceptor = connect_test_tool(&mut h, "started-interceptor");
    h.handle_extension_event(
        "started-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register creation interceptor");
    h.handle_start_agent_request(
        &crate::test_connection_id("requester"),
        request("q-terminal-retry", "initial"),
    )
    .expect("park creation");
    h.runtime_io
        .publication
        .reject_next_start_terminal_live_admission_for_test = true;
    h.handle_extension_event(
        "started-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("reject creation and retain terminal");

    assert_eq!(h.runtime_io.publication.retained_start_terminals.len(), 1);
    assert!(directed_result(&sink, "q-terminal-retry").is_none());
    assert!(
        event_log_events(&h)
            .iter()
            .all(|event| !matches!(event, Event::AgentStartFailed(_)))
    );

    let main_cid = ensure_test_user_agent(&mut h);
    let _capacity_interceptor = connect_test_tool(&mut h, "capacity-wake-interceptor");
    h.handle_extension_event(
        "capacity-wake-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_SUBMITTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register capacity probe interceptor");
    h.publish_pending_prompt_for_agent(&main_cid, PendingPrompt::user("capacity probe".to_owned()))
        .expect("park durable capacity probe");
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "capacity-wake-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject durable capacity probe");
    h.session_runtime
        .persistence_owner
        .as_ref()
        .expect("durable owner")
        .signal_capacity_ready_for_test();
    h.observe_semantic_persistence_progress();

    assert!(h.runtime_io.publication.retained_start_terminals.is_empty());
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentStartFailed(_)))
            .count(),
        1
    );
    assert!(
        directed_result(&sink, "q-terminal-retry").is_some_and(|result| result.error.is_some())
    );
    assert_eq!(
        sink.lock()
            .expect("sink")
            .iter()
            .filter(|routed| matches!(
                peel_inner_event(&routed.frame),
                Some(Event::StartAgentResult(result))
                    if result.query_id == "q-terminal-retry"
            ))
            .count(),
        1
    );
    assert_start_coordinator_empty(&h);
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

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        request_event("q-original", &crate::test_connection_id("original")),
    )
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

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        Event::StartAgentRequest(invalid),
    )
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

/// Configured extensions may request an agent, but only the harness may stamp
/// instruction spans for internal provider presentation.
#[test]
fn configured_extension_cannot_assert_trusted_instruction_spans() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let sink = connect_ready_configured_extension(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Tool,
    );
    let mut invalid = request("q-forged-span", "untrusted instruction");
    invalid.trusted_internal_spans = vec![tau_proto::TrustedInternalSpan { start: 0, end: 9 }];

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        Event::StartAgentRequest(invalid),
    )
    .expect("process rejected request");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::StartAgentRequest(request) if request.query_id == "q-forged-span"
        )
    }));
    let result = directed_result(&sink, "q-forged-span").expect("directed rejection");
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("cannot assert trusted internal"))
    );
    assert_eq!(query_agent_count(&h, "q-forged-span"), 0);
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

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        Event::StartAgentRequest(invalid),
    )
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

/// A loaded explicit parent that disagrees with the loaded tool-call owner is
/// rejected only after the raw request observation commits.
#[test]
fn loaded_parent_tool_owner_mismatch_commits_before_directed_rejection() {
    struct CommitObserver {
        raw_committed: Arc<AtomicBool>,
    }
    impl ConnectionSink for CommitObserver {
        fn send(&mut self, frame: RoutedFrame) -> Result<(), ConnectionSendError> {
            if matches!(
                peel_inner_event(&frame.frame),
                Some(Event::StartAgentRequest(request))
                    if request.query_id == "q-parent-owner-mismatch"
            ) {
                self.raw_committed.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }
    struct OrderedProjectionSink {
        raw_committed: Arc<AtomicBool>,
        projection_after_commit: Arc<AtomicBool>,
        frames: Arc<Mutex<Vec<RoutedFrame>>>,
    }
    impl ConnectionSink for OrderedProjectionSink {
        fn send(&mut self, frame: RoutedFrame) -> Result<(), ConnectionSendError> {
            if matches!(
                peel_inner_event(&frame.frame),
                Some(Event::StartAgentResult(result))
                    if result.query_id == "q-parent-owner-mismatch"
            ) {
                self.projection_after_commit
                    .store(self.raw_committed.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            self.frames.lock().expect("projection frames").push(frame);
            Ok(())
        }
    }

    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let raw_committed = Arc::new(AtomicBool::new(false));
    h.runtime_io.bus.connect(Connection::new(
        PendingConnectionMetadata {
            id: Some(crate::test_connection_id("owner-mismatch-observer")),
            name: crate::test_extension_name("owner-mismatch-observer"),
            kind: tau_proto::ClientKind::External,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(CommitObserver {
            raw_committed: Arc::clone(&raw_committed),
        }),
    ));
    h.runtime_io
        .bus
        .set_subscriptions(
            &crate::test_connection_id("owner-mismatch-observer"),
            Vec::new(),
            vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_START_REQUEST,
            )],
        )
        .expect("subscribe commit observer");
    let projection_after_commit = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(Mutex::new(Vec::new()));
    h.runtime_io.bus.connect(Connection::new(
        PendingConnectionMetadata {
            id: Some(crate::test_connection_id("requester")),
            name: crate::test_extension_name("requester"),
            kind: tau_proto::ClientKind::Core,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(OrderedProjectionSink {
            raw_committed: Arc::clone(&raw_committed),
            projection_after_commit: Arc::clone(&projection_after_commit),
            frames: Arc::clone(&sink),
        }),
    ));
    mark_connected_test_extension_configured(
        &mut h,
        "requester",
        "requester",
        tau_proto::ClientKind::Core,
    );
    let main_cid = ensure_test_user_agent(&mut h);
    h.handle_start_agent_request(
        &crate::test_connection_id("requester"),
        request("q-loaded-parent", "seed loaded parent"),
    )
    .expect("create explicit loaded parent");
    let explicit_parent = directed_acceptance(&sink, "q-loaded-parent")
        .expect("seed acceptance")
        .agent_id;
    let call_id = tau_proto::ToolCallId::new("mismatched-parent-owner");
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(call_id.clone(), main_cid);
    let mut invalid = request("q-parent-owner-mismatch", "invalid");
    invalid.parent_agent = Some(explicit_parent);
    invalid.tool_call_id = Some(call_id);

    h.handle_extension_event_inner(
        &crate::test_connection_id("requester"),
        Event::StartAgentRequest(invalid),
    )
    .expect("process mismatched parent request");

    assert!(raw_committed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(projection_after_commit.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::StartAgentRequest(request)
                    if request.query_id == "q-parent-owner-mismatch"
            ))
            .count(),
        1
    );
    assert!(directed_acceptance(&sink, "q-parent-owner-mismatch").is_none());
    let results = sink
        .lock()
        .expect("projection frames")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result))
                if result.query_id == "q-parent-owner-mismatch" =>
            {
                Some(result.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("does not match tool_call_id owner"))
    );
    assert!(event_log_events(&h).iter().all(|event| {
        !matches!(
            event,
            Event::StartAgentAccepted(accepted)
                if accepted.query_id == "q-parent-owner-mismatch"
        ) && !matches!(event, Event::AgentStartFailed(_))
    }));
    assert_eq!(query_agent_count(&h, "q-parent-owner-mismatch"), 0);
    assert_start_coordinator_empty(&h);
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
        h.handle_extension_event_inner(
            &crate::test_connection_id(&source),
            Event::StartAgentRequest(invalid),
        )
        .expect("publish configured request");
        assert!(source_committed(&h, &source, |event| {
            matches!(
                event,
                Event::StartAgentRequest(request) if request.query_id == query_id
            )
        }));
    }

    let unconfigured = connect_test_tool(&mut h, "unconfigured");
    h.handle_extension_event_inner(
        &crate::test_connection_id("unconfigured"),
        request_event("q-unconfigured", &crate::test_connection_id("spoofed")),
    )
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
            events: Arc::clone(&socket_sink),
        }),
    ));
    h.handle_extension_event_inner(
        &crate::test_connection_id("socket-origin"),
        request_event("q-socket", &crate::test_connection_id("socket")),
    )
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
    h.handle_extension_event_inner(
        &crate::test_connection_id("old-requester"),
        request_event("q-stale", &crate::test_connection_id("stale")),
    )
    .expect("park request");

    h.handle_disconnect(&crate::test_connection_id("old-requester"));
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
        &crate::test_connection_id("requester"),
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
    assert_eq!(h.session_runtime.current_session_id.as_str(), "s1");
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
        .state = path_crate_extension::ExtensionState::Handshaking;

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
    h.handle_extension_message(
        &crate::test_connection_id("requester"),
        TestMessage::Ready(Default::default()),
    )
    .expect("activate requester");

    assert!(source_committed(&h, "requester", |event| {
        matches!(
            event,
            Event::StartAgentRequest(request) if request.query_id == "q-pre-ready-session"
        )
    }));
    assert_eq!(h.session_runtime.current_session_id.as_str(), "s2");
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
    h.handle_extension_event_inner(
        &crate::test_connection_id("old-requester"),
        request_event("q-duplicate", &crate::test_connection_id("first")),
    )
    .expect("start first request");
    let side_cid = h
        .agent_runtime
        .agent_registry
        .agents
        .iter()
        .find_map(|(cid, agent)| {
            matches!(
                &agent.identity.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. }
                    if query_id == "q-duplicate"
            )
            .then(|| cid.clone())
        })
        .expect("side agent");
    let side_agent_id = h.agent_runtime.agent_registry.agents[&side_cid]
        .identity
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
        &crate::test_connection_id("new-requester"),
        request_event("q-duplicate", "ignored duplicate"),
    )
    .expect("rebind duplicate");

    assert_eq!(query_agent_count(&h, "q-duplicate"), 1);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&side_cid]
            .identity
            .source_connection
            .as_deref(),
        Some("new-requester")
    );
    assert_eq!(
        directed_acceptance(&new_sink, "q-duplicate")
            .expect("duplicate acceptance")
            .agent_id
            .as_str(),
        side_agent_id.as_str()
    );
    assert!(source_committed(&h, "old-requester", |event| {
        matches!(event, Event::StartAgentRequest(request) if request.query_id == "q-duplicate")
    }));
    assert!(source_committed(&h, "new-requester", |event| {
        matches!(event, Event::StartAgentRequest(request) if request.query_id == "q-duplicate")
    }));
    h.deliver_finished_side_conversation_result(
        &side_cid,
        &tau_proto::ExtensionName::parse("stable-requester")
            .expect("test extension name must satisfy the identifier grammar"),
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
        .prepare_start_agent_request(
            &crate::test_connection_id("old-requester"),
            request("q-pending", &crate::test_connection_id("first")),
        )
        .expect("prepare")
        .expect("new pending request");
    let expected_agent_id = pending.agent_id.clone();
    h.agent_runtime
        .agent_registry
        .pending_start_requests
        .push_back(pending);

    let new_sink = connect_ready_configured_extension(
        &mut h,
        "new-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_extension_event_inner(
        &crate::test_connection_id("new-requester"),
        request_event("q-pending", "ignored duplicate"),
    )
    .expect("rebind pending duplicate");

    assert_eq!(
        h.agent_runtime.agent_registry.pending_start_requests.len(),
        1
    );
    assert_eq!(
        h.agent_runtime.agent_registry.pending_start_requests[0].source_id,
        "new-requester"
    );
    assert_eq!(
        h.agent_runtime.agent_registry.pending_start_requests[0].agent_id,
        expected_agent_id
    );
    assert_eq!(query_agent_count(&h, "q-pending"), 0);
    assert!(directed_acceptance(&new_sink, "q-pending").is_none());
    h.drain_pending_start_agent_requests()
        .expect("dispatch rebound pending request");
    assert_eq!(
        directed_acceptance(&new_sink, "q-pending")
            .expect("acceptance follows canonical commit")
            .agent_id
            .as_str(),
        expected_agent_id
    );
    let side_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(expected_agent_id.as_str())
        .cloned()
        .expect("dispatched side agent");
    h.deliver_finished_side_conversation_result(
        &side_cid,
        &tau_proto::ExtensionName::parse("stable-requester")
            .expect("test extension name must satisfy the identifier grammar"),
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

/// A duplicate can rebind the sole parked acceptance owner without minting a
/// second identity or observing acceptance before the occurrence commits.
#[test]
fn await_acceptance_duplicate_rebinds_without_early_projection() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let old_sink = connect_ready_configured_extension(
        &mut h,
        "old-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    let _interceptor = connect_test_tool(&mut h, "acceptance-interceptor");
    h.handle_extension_event(
        "acceptance-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_START_ACCEPTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register acceptance interceptor");
    h.handle_start_agent_request(
        &crate::test_connection_id("old-requester"),
        request("q-await-duplicate", "initial"),
    )
    .expect("park acceptance");
    let reserved = h
        .agent_runtime
        .agent_registry
        .start_coordinator
        .operations
        .values()
        .next()
        .expect("operation")
        .pending
        .cid
        .clone();

    let new_sink = connect_ready_configured_extension(
        &mut h,
        "new-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_start_agent_request(
        &crate::test_connection_id("new-requester"),
        request("q-await-duplicate", "ignored duplicate"),
    )
    .expect("rebind parked acceptance");
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .start_coordinator
            .operations
            .len(),
        1
    );
    assert!(directed_acceptance(&old_sink, "q-await-duplicate").is_none());
    assert!(directed_acceptance(&new_sink, "q-await-duplicate").is_none());

    h.handle_extension_event(
        "acceptance-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit sole acceptance");
    assert!(directed_acceptance(&old_sink, "q-await-duplicate").is_none());
    assert_eq!(
        directed_acceptance(&new_sink, "q-await-duplicate")
            .expect("latest route receives acceptance")
            .agent_id,
        reserved
    );
    assert_start_coordinator_empty(&h);
}

/// A duplicate after acceptance but before creation commits receives the cached
/// acceptance and becomes the sole route for the eventual startup terminal.
#[test]
fn postaccept_preterminal_duplicate_rebinds_acceptance_and_failure() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(tmp.path()).expect("harness");
    let old_sink = connect_ready_configured_extension(
        &mut h,
        "old-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    let _interceptor = connect_test_tool(&mut h, "started-interceptor");
    h.handle_extension_event(
        "started-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_STARTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register creation interceptor");
    h.handle_start_agent_request(
        &crate::test_connection_id("old-requester"),
        request("q-postaccept-duplicate", "initial"),
    )
    .expect("commit acceptance and park creation");
    assert!(directed_acceptance(&old_sink, "q-postaccept-duplicate").is_some());

    let new_sink = connect_ready_configured_extension(
        &mut h,
        "new-requester",
        "stable-requester",
        tau_proto::ClientKind::Tool,
    );
    h.handle_start_agent_request(
        &crate::test_connection_id("new-requester"),
        request("q-postaccept-duplicate", "ignored duplicate"),
    )
    .expect("rebind accepted start");
    assert!(directed_acceptance(&new_sink, "q-postaccept-duplicate").is_some());
    h.handle_extension_event(
        "started-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Drop,
        })),
    )
    .expect("reject creation");

    assert!(directed_result(&old_sink, "q-postaccept-duplicate").is_none());
    assert!(
        directed_result(&new_sink, "q-postaccept-duplicate")
            .is_some_and(|result| result.error.is_some())
    );
    assert_start_coordinator_empty(&h);
}

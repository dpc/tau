//! Integrated explicit-response continuation and durable ownership regressions.

use super::super::dispatch::{
    enable_remote_compaction_for_test_model, event_log_count, provider_text_response,
    provider_tool_response, standalone_compaction_success_response,
};
use super::*;

/// Construct a paid partial response without inventing input or tool work.
fn partial_response(prompt: &AgentPromptCreated) -> ProviderResponseFinished {
    let mut response = provider_text_response(
        &prompt.agent_prompt_id,
        prompt.agent_id.clone(),
        "Stable partial answer",
    );
    response.stop_reason = tau_proto::ProviderStopReason::Continue;
    if let ContextItem::Message(message) = &mut response.output_items[0] {
        message.phase = Some(tau_proto::MessagePhase::PartialAnswer);
    }
    response.usage = Some(tau_proto::ProviderTokenUsage {
        model: Some(prompt.model.clone()),
        prompt_sent_tokens: 11,
        prompt_cached_tokens: 0,
        prompt_cache_read_ceiling_tokens: None,
        cache: None,
        response_received_tokens: 7,
        stats: Default::default(),
    });
    response
}

/// Initial and post-tool nonterminal responses commit and account exactly once,
/// preserve the outer turn, and replay partial text without synthetic input.
#[test]
fn explicit_response_continuation_preserves_outer_turn_and_accounting() {
    for post_tool in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
            .expect("dispatch");
        let initial = read_nth_prompt_created(&h, 0);
        let source = if post_tool {
            h.handle_provider_response_finished(provider_tool_response(
                &initial,
                "continuation-tool",
                "self_info",
                CborValue::Map(Vec::new()),
            ))
            .expect("tool response");
            read_nth_prompt_created(&h, 1)
        } else {
            initial
        };
        let turn = h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .outer_turn
            .owned_id()
            .cloned();
        let generation = h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .turn_generation;
        let before = event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_)));
        h.handle_provider_response_finished(partial_response(&source))
            .expect("partial");
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_))),
            before + 1
        );
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnFinished(_))),
            0
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .outer_turn
                .owned_id(),
            turn.as_ref()
        );
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .turn_generation,
            generation
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty()
        );
        let successor = read_nth_prompt_created(&h, before);
        let final_response = provider_text_response(
            &successor.agent_prompt_id,
            successor.agent_id.clone(),
            "Final answer",
        );
        h.handle_provider_response_finished(final_response)
            .expect("final");
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnFinished(_))),
            1
        );
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentPromptSteered(_))),
            0
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(source.agent_id.as_str())
            .expect("records");
        let responses: Vec<_> = records
            .iter()
            .filter_map(|r| match &r.event {
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == source.agent_prompt_id =>
                {
                    Some(response)
                }
                _ => None,
            })
            .collect();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0]
                .usage
                .as_ref()
                .expect("paid usage")
                .response_received_tokens,
            7
        );
        let live = h
            .session_runtime
            .agent_store
            .agent(source.agent_id.as_str())
            .expect("tree");
        assert!(live.pending_response_continuation().is_none());
        let cold = tau_core::AgentTree::try_from_events(source.agent_id.clone(), &records);
        assert!(cold.is_ok(), "cold replay: {cold:?}");
        let finish = records
            .iter()
            .position(|r| matches!(&r.event, Event::AgentOuterTurnFinished(_)))
            .expect("finish");
        let crash_cut =
            tau_core::AgentTree::from_events(source.agent_id.clone(), &records[..finish]);
        assert!(crash_cut.pending_response_continuation().is_none());
        assert_eq!(crash_cut.response_continuation_finish_repair(), turn);
    }
}

/// A backend cannot create an unbounded paid no-tool loop; the seventeenth
/// nonterminal result keeps its text/usage but settles the existing outer turn.
#[test]
fn explicit_response_continuation_has_durable_finite_bound() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
        .expect("dispatch");
    for index in 0..17 {
        let prompt = read_nth_prompt_created(&h, index);
        h.handle_provider_response_finished(partial_response(&prompt))
            .expect("partial");
        if index == 15 {
            let records = h
                .session_runtime
                .agent_store
                .snapshot_agent_events_for_test(prompt.agent_id.as_str())
                .expect("records");
            let cold = tau_core::AgentTree::from_events(prompt.agent_id.clone(), &records);
            assert_eq!(cold.response_continuation_count(), 16);
        }
    }
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_))),
        17
    );
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnFinished(_))),
        1
    );
    let last = read_nth_prompt_created(&h, 16);
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(last.agent_id.as_str())
        .expect("records");
    let response = records
        .iter()
        .find_map(|r| match &r.event {
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == last.agent_prompt_id =>
            {
                Some(response)
            }
            _ => None,
        })
        .expect("last terminal");
    assert_eq!(response.stop_reason, tau_proto::ProviderStopReason::Error);
    assert_eq!(
        response
            .usage
            .as_ref()
            .expect("paid")
            .response_received_tokens,
        7
    );
    assert!(!response.output_items.is_empty());
}

/// A parked/rejected source cannot dispatch early, and cancellation wins before
/// its terminal commits. Initial and post-tool paths retain only one terminal.
#[test]
fn explicit_response_continuation_waits_for_commit_and_cancellation() {
    for (post_tool, cancel) in [(false, false), (true, false), (false, true), (true, true)] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
            .expect("dispatch");
        let initial = read_nth_prompt_created(&h, 0);
        let source = if post_tool {
            h.handle_provider_response_finished(provider_tool_response(
                &initial,
                "retained-continuation-tool",
                "self_info",
                CborValue::Map(Vec::new()),
            ))
            .expect("tools");
            read_nth_prompt_created(&h, 1)
        } else {
            initial
        };
        connect_test_tool(&mut h, "partial-interceptor");
        h.handle_extension_event(
            "partial-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept");
        let before = event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_)));
        h.handle_provider_response_finished(partial_response(&source))
            .expect("park");
        reject_next_semantic_admission(&h);
        h.handle_extension_event(
            "partial-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("reject");
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_))),
            before
        );
        if cancel {
            h.handle_cancel_prompt(
                crate::harness::harness_connection_id(),
                &tau_proto::UiCancelPrompt {
                    session_id: h.session_runtime.current_session_id.clone(),
                    target_agent_id: Some(source.agent_id.clone()),
                    agent_prompt_id: None,
                },
            );
        }
        h.retry_pending_agent_publish_completion(&cid);
        h.retry_pending_agent_publish_completion(&cid);
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty()
        );
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_))),
            before + usize::from(!cancel)
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(source.agent_id.as_str())
            .expect("records");
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(&r.event, Event::ProviderResponseFinished(r)
            if r.agent_prompt_id == source.agent_prompt_id))
                .count(),
            1
        );
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(&r.event, Event::AgentOuterTurnFinished(_)))
                .count(),
            usize::from(cancel)
        );
        let cold = tau_core::AgentTree::from_events(source.agent_id.clone(), &records);
        assert!(cold.pending_response_continuation().is_none());
    }
}

/// Restart may dispatch a committed but unclaimed response once. A second
/// restart pauses at its durable successor checkpoint instead of paying again;
/// a committed cancellation/finish suppresses the source permanently.
#[test]
fn explicit_response_continuation_restart_claims_once_and_respects_finish() {
    for cancelled in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let (source, turn, parent);
        {
            let mut h = quiet_provider_harness(&state).expect("harness");
            let cid = ensure_test_user_agent(&mut h);
            h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
                .expect("dispatch");
            source = read_nth_prompt_created(&h, 0);
            turn = h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .outer_turn
                .owned_id()
                .expect("turn")
                .clone();
            parent = h
                .session_runtime
                .agent_store
                .agent(source.agent_id.as_str())
                .expect("tree")
                .head();
            h.shutdown().expect("shutdown");
        }
        wait_for_session_unlock(&state, "s1");
        let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
        store
            .append_agent_event_at(
                source.agent_id.as_str(),
                None,
                tau_core::AgentEventParent::from_head(
                    parent.map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                ),
                Event::ProviderResponseFinished(partial_response(&source)),
                tau_proto::UnixMicros::now(),
            )
            .expect("source crash cut");
        if cancelled {
            store
                .append_agent_event_at(
                    source.agent_id.as_str(),
                    None,
                    tau_core::AgentEventParent::InheritHead,
                    Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
                        agent_id: source.agent_id.clone(),
                        session_id: test_session_id("s1"),
                        outer_turn_id: turn.clone(),
                        disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                        automatic_compaction_decision: None,
                    }),
                    tau_proto::UnixMicros::now(),
                )
                .expect("cancel finish");
        }
        drop(store);
        for restart in 0..2 {
            let mut resumed = quiet_provider_harness_with_start_reason(
                &state,
                tau_proto::SessionStartReason::Resume,
            )
            .expect("resume");
            assert_eq!(
                event_log_count(&resumed, |e| matches!(e, Event::AgentPromptCreated(_))),
                usize::from(!cancelled && restart == 0)
            );
            let tree = resumed
                .session_runtime
                .agent_store
                .agent(source.agent_id.as_str())
                .expect("tree");
            assert!(tree.pending_response_continuation().is_none());
            if !cancelled {
                assert_eq!(tree.response_continuation_count(), 1);
                assert_eq!(tree.response_continuation_outer_turn(), Some(turn.clone()));
            }
            resumed.shutdown().expect("shutdown");
            drop(resumed);
            wait_for_session_unlock(&state, "s1");
        }
    }
}

/// Cancellation retires owed work both before checkpoint admission and while
/// that checkpoint is intercepted, without dispatching a new paid response.
#[test]
fn explicit_response_continuation_cancel_between_responses() {
    for parked_checkpoint in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
            .expect("dispatch");
        let source = read_nth_prompt_created(&h, 0);
        if parked_checkpoint {
            connect_test_tool(&mut h, "checkpoint-interceptor");
            h.handle_extension_event(
                "checkpoint-interceptor",
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(
                        tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                    )],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("intercept");
        } else {
            h.agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("agent")
                .dispatch
                .discovery_fold_pending = true;
        }
        h.handle_provider_response_finished(partial_response(&source))
            .expect("partial");
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_))),
            1
        );
        assert!(h.has_pending_response_continuation(&cid));
        h.handle_cancel_prompt(
            crate::harness::harness_connection_id(),
            &tau_proto::UiCancelPrompt {
                session_id: h.session_runtime.current_session_id.clone(),
                target_agent_id: Some(source.agent_id.clone()),
                agent_prompt_id: None,
            },
        );
        if parked_checkpoint {
            h.handle_extension_event(
                "checkpoint-interceptor",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("checkpoint");
        } else {
            h.agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("agent")
                .dispatch
                .discovery_fold_pending = false;
        }
        h.drain_publish_idle_dispatches();
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_))),
            1
        );
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnFinished(_))),
            1,
            "parked={parked_checkpoint}, turn={:?}, dispatch={:?}, cancel={:?}",
            h.agent_runtime.agent_registry.agents[&cid].turn.outer_turn,
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .activation_dispatch,
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .pending_cancel
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(source.agent_id.as_str())
            .expect("records");
        let cold = tau_core::AgentTree::from_events(source.agent_id.clone(), &records);
        assert!(cold.pending_response_continuation().is_none());
        assert!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .pending_cancel
                .is_none()
        );
        if parked_checkpoint {
            let finish = records
                .iter()
                .position(|r| matches!(&r.event, Event::AgentOuterTurnFinished(_)))
                .expect("finish");
            let crash_cut =
                tau_core::AgentTree::from_events(source.agent_id.clone(), &records[..finish]);
            assert!(crash_cut.pending_response_continuation().is_none());
            assert!(crash_cut.response_continuation_finish_repair().is_some());
        }
    }
}

/// Rejecting successor admission must retain its committed source, preserve the
/// outer turn, and produce only one successor on a later capacity wake.
#[test]
fn explicit_response_continuation_retries_rejected_checkpoint_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
        .expect("dispatch");
    let source = read_nth_prompt_created(&h, 0);
    connect_test_tool(&mut h, "checkpoint-capacity");
    h.handle_extension_event(
        "checkpoint-capacity",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    h.handle_provider_response_finished(partial_response(&source))
        .expect("partial");
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "checkpoint-capacity",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject checkpoint");
    assert!(h.has_pending_response_continuation(&cid));
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnFinished(_))),
        0
    );
    h.drain_capacity_rejected_idle_dispatches(&HashSet::from([cid.clone()]));
    h.handle_extension_event(
        "checkpoint-capacity",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("admit checkpoint");
    h.drain_capacity_rejected_idle_dispatches(&HashSet::from([cid]));
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentPromptCreated(_))),
        2
    );
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnStarted(_))),
        1
    );
}

/// Fresh partial-response usage may compact before the next sampling without
/// finishing the outer turn, resetting its budget, or duplicating its
/// checkpoint.
#[test]
fn explicit_response_continuation_preserves_compaction_interleaving() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    let role = h
        .config
        .available_roles
        .get_mut(&h.config.selected_role)
        .expect("role");
    role.compactions.clear();
    role.compactions.insert(
        "partial-preflight".to_owned(),
        tau_config::settings::CompactionPolicy {
            threshold: path_tau_config_settings::CompactionPolicyThreshold::Tokens(10),
            enable: true,
            when: path_tau_config_settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::BeforeInference,
                statuses: None,
            },
        },
    );
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
        .expect("dispatch");
    let source = read_nth_prompt_created(&h, 0);
    let turn = h.agent_runtime.agent_registry.agents[&cid]
        .turn
        .outer_turn
        .owned_id()
        .cloned();
    h.handle_provider_response_finished(partial_response(&source))
        .expect("partial");
    let compact = read_nth_prompt_created(&h, 1);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnFinished(_))),
        0
    );
    h.handle_provider_response_finished(standalone_compaction_success_response(
        &compact, "Summary",
    ))
    .expect("compact");
    let next = read_nth_prompt_created(&h, 2);
    assert_eq!(next.operation, tau_proto::PromptOperation::Inference);
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .outer_turn
            .owned_id(),
        turn.as_ref()
    );
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(source.agent_id.as_str())
        .expect("records");
    let cold = tau_core::AgentTree::from_events(source.agent_id.clone(), &records);
    assert_eq!(cold.response_continuation_count(), 1);
    assert!(cold.pending_response_continuation().is_none());
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(&r.event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(&r.event, Event::AgentCompacted(_)))
            .count(),
        1
    );
    h.handle_provider_response_finished(provider_text_response(
        &next.agent_prompt_id,
        next.agent_id,
        "Final",
    ))
    .expect("final");
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentOuterTurnFinished(_))),
        1
    );
}

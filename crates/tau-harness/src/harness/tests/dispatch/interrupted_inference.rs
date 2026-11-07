//! Cold ordinary-inference recovery without replaying completed side effects.

use std::fs::File;

use super::*;

/// Changes current selection before startup so recovery must use its
/// checkpoint.
fn resume_with_changed_model(state: &Path) -> Harness {
    quiet_provider_harness_for_with_start_reason_storage_mode_and_hook(
        "s1",
        state,
        tau_proto::SessionStartReason::Resume,
        crate::HarnessStorageMode::Durable,
        Some(Box::new(|h| {
            super::provider_responses::add_routed_test_model(h, "other/model");
            h.config.selected_model = Some("other/model".into());
            for agent in h.agent_runtime.agent_registry.agents.values_mut() {
                agent.identity.model_override = Some("other/model".into());
            }
        })),
    )
    .expect("restart with changed model")
}

/// Replaces a stopped fixture's journal with an exact pre-shutdown crash cut.
fn install_crash_cut(state: &Path, records: &[tau_core::PersistedAgentEvent]) {
    install_agent_crash_cut(state, "main", records);
}

/// Installs an exact cut for a normally created durable test agent.
fn install_agent_crash_cut(
    state: &Path,
    agent_id: &str,
    records: &[tau_core::PersistedAgentEvent],
) {
    wait_for_session_unlock(state, "s1");
    let mut journal =
        File::create(state.join("agents").join(agent_id).join("events.cbor")).expect("journal");
    for record in records {
        let mut encoded = Vec::new();
        ciborium::into_writer(record, &mut encoded).expect("encode");
        journal
            .write_all(&(encoded.len() as u64).to_le_bytes())
            .expect("length");
        journal.write_all(&encoded).expect("record");
    }
    journal.sync_all().expect("sync");
}

/// Captures a real completed round before its successor checkpoint, without
/// injecting activation or manually dispatching after restart.
fn completed_tool_round_cut(
    state: &Path,
) -> (AgentPromptCreated, Vec<tau_core::PersistedAgentEvent>) {
    let mut h = quiet_provider_harness(state).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
        .expect("dispatch");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_tool_response(
        &source,
        "completed-before-crash",
        "self_info",
        CborValue::Map(Vec::new()),
    ))
    .expect("tool response");
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(source.agent_id.as_str())
        .expect("records")
        .to_vec();
    let end = records
        .iter()
        .position(|record| match &record.event {
            Event::ProviderToolResult(result) => {
                result.call_id.as_str() == "completed-before-crash"
            }
            Event::ProviderToolError(error) => error.call_id.as_str() == "completed-before-crash",
            _ => false,
        })
        .expect("last tool terminal")
        + 1;
    let cut = records[..end].to_vec();
    assert_eq!(
        cut.iter()
            .filter(|record| matches!(record.event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        1,
        "the cut precedes the successor checkpoint"
    );
    h.shutdown().expect("shutdown");
    install_agent_crash_cut(state, source.agent_id.as_str(), &cut);
    (source, cut)
}

/// A committed last tool result owes fresh inference even when no successor
/// checkpoint or new input exists. Cold restore must not replay the tool.
#[test]
fn completed_tool_round_before_successor_recovers_without_new_input() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (source, cut) = completed_tool_round_cut(&state);
    let tree = tau_core::AgentTree::try_from_events(source.agent_id.clone(), &cut).expect("replay");
    let turn = tree
        .prompt_started(&source.agent_prompt_id)
        .expect("start")
        .outer_turn_id
        .clone();
    assert_eq!(
        tree.pending_tool_round_continuation()
            .expect("debt")
            .agent_prompt_id,
        source.agent_prompt_id
    );
    let mut h = resume_with_changed_model(&state);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1,
        "closed tool round must resume without external activation"
    );
    let successor = read_nth_prompt_created(&h, 0);
    assert_eq!(successor.model, source.model);
    assert_ne!(successor.agent_prompt_id, source.agent_prompt_id);
    assert_eq!(
        successor
            .context
            .flatten_iter()
            .filter(|item| matches!(
                item, ContextItem::ToolResult(result)
                    if result.call_id.as_str() == "completed-before-crash"
            ))
            .count(),
        1
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(_) | Event::ToolStarted(_)
    )));
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent(source.agent_id.as_str())
            .expect("tree")
            .prompt_started(&successor.agent_prompt_id)
            .expect("start")
            .outer_turn_id,
        turn
    );
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(source.agent_id.as_str())
        .expect("records")
        .to_vec();
    assert!(
        tau_core::AgentTree::try_from_events(source.agent_id.clone(), &records)
            .expect("replay")
            .pending_tool_round_continuation()
            .is_none()
    );
    h.shutdown().expect("shutdown");
    // The new owner, not the tool round, owns another crash. Exercise the
    // independently durable checkpoint before its prompt-start fact as well.
    let checkpoint = records
        .iter()
        .rposition(|r| matches!(r.event, Event::AgentInferenceDispatchStarted(_)))
        .expect("successor checkpoint");
    install_agent_crash_cut(&state, source.agent_id.as_str(), &records[..=checkpoint]);
    let mut h = resume_with_changed_model(&state);
    let next = read_nth_prompt_created(&h, 0);
    let tree = h
        .session_runtime
        .agent_store
        .agent(source.agent_id.as_str())
        .expect("tree");
    assert_eq!(
        tree.prompt_started(&next.agent_prompt_id)
            .expect("start")
            .outer_turn_id,
        turn
    );
    assert_eq!(next.model, source.model);
    assert!(tree.pending_tool_round_continuation().is_none());
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(_) | Event::ToolStarted(_)
    )));
    h.handle_provider_response_finished(provider_text_response(
        &next.agent_prompt_id,
        next.agent_id.clone(),
        "done",
    ))
    .expect("final");
    h.shutdown().expect("shutdown");
    let mut h = resume_with_changed_model(&state);
    assert!(
        !event_log_contains_any_source(&h, |event| matches!(event, Event::AgentPromptCreated(_))),
        "a settled successor cannot reopen its historical tool round"
    );
    h.shutdown().expect("shutdown");
}

/// Canonical restart repair supplies an uncertain-effect error, not permission
/// to rerun the interrupted tool, and owes the same original outer turn.
#[test]
fn repaired_tool_round_recovers_without_new_input_or_tool_replay() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (source, mut cut) = completed_tool_round_cut(&state);
    cut.pop().expect("remove last terminal");
    let tree = tau_core::AgentTree::try_from_events(source.agent_id.clone(), &cut).expect("replay");
    assert!(tree.has_open_foreground_tool_round());
    assert!(tree.pending_tool_round_continuation().is_none());
    let turn = tree
        .prompt_started(&source.agent_prompt_id)
        .expect("start")
        .outer_turn_id
        .clone();
    install_agent_crash_cut(&state, source.agent_id.as_str(), &cut);
    let mut h = resume_with_changed_model(&state);
    let successor = read_nth_prompt_created(&h, 0);
    let tree = h
        .session_runtime
        .agent_store
        .agent(source.agent_id.as_str())
        .expect("tree");
    assert_eq!(
        tree.prompt_started(&successor.agent_prompt_id)
            .expect("start")
            .outer_turn_id,
        turn
    );
    assert_eq!(successor.model, source.model);
    let results = successor
        .context
        .flatten_iter()
        .filter_map(|item| match item {
            ContextItem::ToolResult(result)
                if result.call_id.as_str() == "completed-before-crash" =>
            {
                Some(result)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert!(
        results[0]
            .render_provider_text()
            .contains("Side effects may have occurred")
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(_) | Event::ToolStarted(_)
    )));
    h.shutdown().expect("shutdown");
}

/// A parallel round with one committed result and one restart repair must join
/// once, including already accepted input, without replaying either tool.
#[test]
fn partially_completed_tool_round_coalesces_queued_input() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("answer".to_owned()))
        .expect("dispatch");
    let source = read_nth_prompt_created(&h, 0);
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("test-bridge").expect("publisher"),
            tau_proto::MessageAgentTarget::new(source.agent_id.as_str()),
            tau_proto::MessageFactId::new("queued-during-tools"),
            tau_proto::MessageParty {
                stable_id: "external".to_owned(),
                display_name: None,
                sender_auth: None,
                sender_trust: None,
            },
            None,
            "queued before tools finished",
        )),
    );
    let mut response = provider_tool_response(
        &source,
        "parallel-first",
        "self_info",
        CborValue::Map(Vec::new()),
    );
    response.output_items.extend(
        provider_tool_response(
            &source,
            "parallel-second",
            "self_info",
            CborValue::Map(Vec::new()),
        )
        .output_items,
    );
    h.handle_provider_response_finished(response)
        .expect("tools");
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(source.agent_id.as_str())
        .expect("records")
        .to_vec();
    let first = records
        .iter()
        .position(|record| match &record.event {
            Event::ProviderToolResult(result) => result.call_id.as_str() == "parallel-first",
            Event::ProviderToolError(error) => error.call_id.as_str() == "parallel-first",
            _ => false,
        })
        .expect("first result");
    let cut = &records[..=first];
    let tree = tau_core::AgentTree::try_from_events(source.agent_id.clone(), cut).expect("replay");
    assert!(tree.has_open_foreground_tool_round());
    assert!(
        tree.pending_terminal_tool_result(&"parallel-first".into())
            .is_some()
    );
    let turn = tree
        .prompt_started(&source.agent_prompt_id)
        .expect("start")
        .outer_turn_id
        .clone();
    h.shutdown().expect("shutdown");
    install_agent_crash_cut(&state, source.agent_id.as_str(), cut);
    let mut h = resume_with_changed_model(&state);
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1
    );
    let successor = read_nth_prompt_created(&h, 0);
    assert_eq!(successor.model, source.model);
    let tree = h
        .session_runtime
        .agent_store
        .agent(source.agent_id.as_str())
        .expect("tree");
    assert_eq!(
        tree.prompt_started(&successor.agent_prompt_id)
            .expect("start")
            .outer_turn_id,
        turn
    );
    let context = serde_json::to_string(&successor.context).expect("context");
    assert_eq!(context.matches("queued before tools finished").count(), 1);
    for id in ["parallel-first", "parallel-second"] {
        assert_eq!(
            successor
                .context
                .flatten_iter()
                .filter(|item| matches!(
                    item, ContextItem::ToolResult(result) if result.call_id.as_str() == id
                ))
                .count(),
            1
        );
    }
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(_) | Event::ToolStarted(_)
    )));
    h.shutdown().expect("shutdown");
}

/// Finished outer turns and unselected tool aggregates cannot supply execution
/// authority, even though their results remain durably available.
#[test]
fn completed_tool_round_respects_finish_and_selected_branch() {
    for finished in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let (source, cut) = completed_tool_round_cut(&state);
        let tree =
            tau_core::AgentTree::try_from_events(source.agent_id.clone(), &cut).expect("replay");
        let turn = tree
            .prompt_started(&source.agent_prompt_id)
            .expect("start")
            .outer_turn_id
            .clone()
            .expect("turn");
        let event = if finished {
            Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
                agent_id: source.agent_id.clone(),
                session_id: test_session_id("s1"),
                outer_turn_id: turn,
                disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                automatic_compaction_decision: None,
            })
        } else {
            Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                agent_id: source.agent_id.clone(),
                head: tau_proto::AgentHead::Root,
            })
        };
        let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
        store
            .append_agent_event_at(
                source.agent_id.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                event,
                tau_proto::UnixMicros::now(),
            )
            .expect("finish/move");
        drop(store);
        let mut h = resume_with_changed_model(&state);
        assert!(!event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::AgentPromptCreated(_)
        )));
        h.shutdown().expect("shutdown");
    }
}

/// Cancellation terminals precede the outer-turn finish. A crash in that gap
/// must not reopen the canceled turn, including when another parallel call
/// still needs a conservative restart repair.
#[test]
fn canceled_tool_terminal_before_turn_finish_never_resumes() {
    for partial in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let mut h = quiet_provider_harness(&state).expect("harness");
        connect_test_tool(&mut h, "cancel-owner");
        h.tool_routing.registry.register(
            &crate::test_connection_id("cancel-owner"),
            shared_test_tool_spec("held_tool"),
        );
        let cid = ensure_test_user_agent(&mut h);
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("do work".to_owned()))
            .expect("dispatch");
        let source = read_nth_prompt_created(&h, 0);
        let mut response = provider_tool_response(
            &source,
            "cancel-first",
            "held_tool",
            CborValue::Map(Vec::new()),
        );
        response.output_items.extend(
            provider_tool_response(
                &source,
                "cancel-second",
                "held_tool",
                CborValue::Map(Vec::new()),
            )
            .output_items,
        );
        h.handle_provider_response_finished(response)
            .expect("tools");
        assert!(
            h.session_runtime
                .agent_store
                .agent(source.agent_id.as_str())
                .expect("tree")
                .has_open_foreground_tool_round()
        );
        h.handle_cancel_prompt(
            crate::harness::harness_connection_id(),
            &tau_proto::UiCancelPrompt {
                session_id: test_session_id("s1"),
                target_agent_id: Some(source.agent_id.clone()),
                agent_prompt_id: None,
            },
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(source.agent_id.as_str())
            .expect("records")
            .to_vec();
        let terminals = records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                matches!(record.event, Event::ToolCancelled(_)).then_some(index)
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 2);
        let cut = &records[..=terminals[usize::from(!partial)]];
        assert!(
            !cut.iter()
                .any(|r| matches!(r.event, Event::AgentOuterTurnFinished(_)))
        );
        h.shutdown().expect("shutdown");
        install_agent_crash_cut(&state, source.agent_id.as_str(), cut);
        for _ in 0..2 {
            let mut h = resume_with_changed_model(&state);
            assert!(!event_log_contains_any_source(&h, |event| matches!(
                event,
                Event::AgentPromptCreated(_) | Event::ToolRequest(_) | Event::ToolStarted(_)
            )));
            let tree = h
                .session_runtime
                .agent_store
                .agent(source.agent_id.as_str())
                .expect("tree");
            assert!(!tree.has_open_foreground_tool_round());
            assert!(tree.pending_tool_round_continuation().is_none());
            assert!(tree.interrupted_tool_round_outer_turn().is_none());
            h.shutdown().expect("shutdown");
        }
    }
}

/// An input wait loses its runtime timer on restart, but its canonical repair
/// must lead to a fresh decision rather than leave the bot silently idle.
#[test]
fn interrupted_input_wait_resumes_after_canonical_repair() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("wait for work".to_owned()))
        .expect("dispatch");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_input_wait_response(
        &source,
        "interrupted-input-wait",
        60,
    ))
    .expect("wait");
    let cut = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(source.agent_id.as_str())
        .expect("cut")
        .to_vec();
    let tree = tau_core::AgentTree::try_from_events(source.agent_id.clone(), &cut).expect("replay");
    assert!(tree.has_open_foreground_tool_round());
    let turn = tree
        .prompt_started(&source.agent_prompt_id)
        .expect("start")
        .outer_turn_id
        .clone();
    h.shutdown().expect("shutdown");
    install_agent_crash_cut(&state, source.agent_id.as_str(), &cut);
    let mut h = resume_with_changed_model(&state);
    let successor = read_nth_prompt_created(&h, 0);
    assert_eq!(successor.model, source.model);
    let tree = h
        .session_runtime
        .agent_store
        .agent(source.agent_id.as_str())
        .expect("tree");
    assert_eq!(
        tree.prompt_started(&successor.agent_prompt_id)
            .expect("start")
            .outer_turn_id,
        turn
    );
    assert_eq!(successor.context.flatten_iter().filter(|item| matches!(
        item, ContextItem::ToolResult(result) if result.call_id.as_str() == "interrupted-input-wait"
    )).count(), 1);
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(_) | Event::ToolStarted(_)
    )));
    h.shutdown().expect("shutdown");
}

/// Cold tool ownership survives a terminal parked past startup and rejected
/// admission, so the normal completion seam resumes exactly once on commit.
#[test]
fn restored_tool_projection_waits_for_retained_terminal_commit() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (source, mut cut) = completed_tool_round_cut(&state);
    cut.pop().expect("last terminal");
    install_agent_crash_cut(&state, source.agent_id.as_str(), &cut);
    let mut h = quiet_provider_harness_for_with_start_reason_storage_mode_and_hook(
        "s1",
        &state,
        tau_proto::SessionStartReason::Resume,
        crate::HarnessStorageMode::Durable,
        Some(Box::new(|h| {
            connect_test_tool(h, "repair-interceptor");
            h.handle_extension_event(
                "repair-interceptor",
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(
                        tau_proto::EventName::PROVIDER_TOOL_ERROR,
                    )],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("intercept");
        })),
    )
    .expect("resume");
    let cid = h
        .runtime_agent_id_for_target_agent(Some(source.agent_id.as_str()))
        .expect("agent");
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::AgentPromptCreated(_)
    )));
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "repair-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject");
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::ToolsRunning { .. }
    ));
    for _ in 0..2 {
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("owner")
            .signal_capacity_ready_for_test();
        let mut served = 0;
        h.handle_runtime_event(
            HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress),
            &mut served,
        )
        .expect("retry");
    }
    h.drain_publish_idle_dispatches();
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1
    );
    assert!(!event_log_contains_any_source(&h, |event| matches!(
        event,
        Event::ToolRequest(_) | Event::ToolStarted(_)
    )));
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(source.agent_id.as_str())
        .expect("records");
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r.event, Event::ProviderToolError(_)))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// A rejected successor does not consume canonical tool-round debt. Retrying
/// admission preserves one turn, while cancellation settles it without work.
#[test]
fn closed_tool_successor_retries_admission_and_respects_cancellation() {
    for cancel in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let (source, _) = completed_tool_round_cut(&state);
        let mut h = quiet_provider_harness_for_with_start_reason_storage_mode_and_hook(
            "s1",
            &state,
            tau_proto::SessionStartReason::Resume,
            crate::HarnessStorageMode::Durable,
            Some(Box::new(|h| {
                connect_test_tool(h, "closed-round-interceptor");
                h.handle_extension_event(
                    "closed-round-interceptor",
                    TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                        selectors: vec![EventSelector::Exact(
                            tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                        )],
                        priority: InterceptionPriority::new(0),
                    })),
                )
                .expect("intercept");
            })),
        )
        .expect("resume");
        let cid = h
            .runtime_agent_id_for_target_agent(Some(source.agent_id.as_str()))
            .expect("agent");
        assert!(h.runtime_io.publication.pending_intercept.is_some());
        reject_next_semantic_admission(&h);
        h.handle_extension_event(
            "closed-round-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("reject admission");
        assert!(
            h.session_runtime
                .agent_store
                .agent(source.agent_id.as_str())
                .expect("tree")
                .pending_tool_round_continuation()
                .is_some()
        );
        if cancel {
            h.handle_cancel_prompt(
                crate::harness::harness_connection_id(),
                &tau_proto::UiCancelPrompt {
                    session_id: test_session_id("s1"),
                    target_agent_id: Some(source.agent_id.clone()),
                    agent_prompt_id: None,
                },
            );
        } else {
            h.session_runtime
                .persistence_owner
                .as_ref()
                .expect("owner")
                .signal_capacity_ready_for_test();
            let mut served = 0;
            h.handle_runtime_event(
                HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress),
                &mut served,
            )
            .expect("retry");
        }
        while h.runtime_io.publication.pending_intercept.is_some() {
            h.handle_extension_event(
                "closed-round-interceptor",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("release");
        }
        h.drain_publish_idle_dispatches();
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|e| matches!(e, Event::AgentPromptCreated(_)))
                .count(),
            usize::from(!cancel)
        );
        assert!(!h.has_pending_response_continuation(&cid));
        assert!(!event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ToolRequest(_) | Event::ToolStarted(_)
        )));
        h.shutdown().expect("shutdown");
    }
}

/// Seeds a closed send-tool round, then interrupts the next ordinary sampling.
/// The queued peer input is durable but cannot activate the pre-fix cold owner.
fn interrupted_post_tool_fixture(
    state: &Path,
) -> (tau_proto::AgentPromptId, Vec<tau_core::PersistedAgentEvent>) {
    interrupted_post_tool_fixture_with_compaction(state, false)
}

/// Optionally commits a replacement before the closed tool round so the first
/// interrupted sampling belongs to that successful standalone transaction.
fn interrupted_post_tool_fixture_with_compaction(
    state: &Path,
    compacted: bool,
) -> (tau_proto::AgentPromptId, Vec<tau_core::PersistedAgentEvent>) {
    seed_main_agent_loaded(state);
    let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
    if compacted {
        let transaction_id = tau_proto::CompactionTransactionId::parse("ct-interrupted").unwrap();
        let compact_prompt_id = test_agent_prompt_id("ap-interrupted-compact");
        append_seed_agent_event(
            &mut store,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: crate::parse_agent_id("main"),
                transaction_id: transaction_id.clone(),
                compact_prompt_id: compact_prompt_id.clone(),
                cut: tau_proto::AgentHead::Root,
                resume_through: Some(tau_proto::AgentHead::Root),
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::Manual,
            }),
        );
        append_seed_agent_event(
            &mut store,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                effectiveness: None,
                compaction_input_bytes: None,
                agent_id: crate::parse_agent_id("main"),
                transaction_id,
                compact_prompt_id: compact_prompt_id.clone(),
                cut: tau_proto::AgentHead::Root,
                suffix_end: tau_proto::AgentHead::Root,
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                replacement_window: provider_text_response(
                    &compact_prompt_id,
                    crate::parse_agent_id("main"),
                    "committed replacement",
                )
                .output_items,
                original_input_tokens: None,
                compaction_output_tokens: None,
            }),
        );
    }
    let mut response = provider_text_response(
        &test_agent_prompt_id("ap-prior-send"),
        crate::parse_agent_id("main"),
        "",
    );
    response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    response.output_items = vec![ContextItem::ToolCall(ToolCallItem {
        call_id: "prior-send".into(),
        name: ToolName::new("message"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    append_seed_agent_event(&mut store, Event::ProviderResponseFinished(response));
    append_seed_agent_event(
        &mut store,
        Event::ProviderToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "prior-send".into(),
            tool_name: ToolName::new("message"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("prior send accepted exactly once".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::default(),
            originator: tau_proto::PromptOriginator::User,
            display: None,
        }),
    );
    drop(store);
    let mut h =
        quiet_provider_harness_with_start_reason(state, tau_proto::SessionStartReason::Resume)
            .expect("seed runtime");
    let cid = h
        .runtime_agent_id_for_target_agent(Some("main"))
        .expect("main");
    if !compacted {
        h.dispatch_prompt_after_publish_idle(&cid);
    }
    let old = read_nth_prompt_created(&h, 0).agent_prompt_id;
    let provider = h.provider_runtime.pending_prompts[&old].clone();
    h.handle_extension_event(
        provider.as_str(),
        TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
            tau_proto::ProviderResponseUpdated {
                agent_prompt_id: old.clone(),
                agent_id: crate::parse_agent_id("main"),
                deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
                    output_index: 0,
                    text: "provisional text already displayed".to_owned(),
                    phase: Some(tau_proto::MessagePhase::PartialAnswer),
                }],
                compaction: None,
                status: None,
                response_stats: None,
                originator: tau_proto::PromptOriginator::User,
            },
        )),
    )
    .expect("partial output");
    assert!(
        event_log_contains_any_source(&h, |event| matches!(
            event,
            Event::ProviderResponseUpdated(_)
        )),
        "partial output was actually published before the crash"
    );
    h.publish_event(
        Some(&crate::test_connection_id(HARNESS_CONNECTION_ID)),
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("test-bridge").expect("publisher"),
            tau_proto::MessageAgentTarget::new("main"),
            tau_proto::MessageFactId::new("queued-after-interruption"),
            tau_proto::MessageParty {
                stable_id: "external".to_owned(),
                display_name: None,
                sender_auth: None,
                sender_trust: None,
            },
            None,
            "queued peer work",
        )),
    );
    let cut = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test("main")
        .expect("cut")
        .to_vec();
    h.shutdown().expect("shutdown");
    install_crash_cut(state, &cut);
    (old, cut)
}

/// Reproduces the bot's completed-send / interrupted-next-inference cut.
/// Repeated restarts must close only the current request, retain queued input
/// and the completed tool result, and never redispatch the send.
#[test]
fn interrupted_post_tool_restart_recovers_without_human_input_or_tool_replay() {
    assert_interrupted_post_tool_restart(false);
}

/// Reproduces a succeeded compaction followed by a completed tool and an
/// interrupted transaction-owned inference, including later idle restore facts.
#[test]
fn interrupted_post_compaction_restart_recovers_without_tool_or_summary_replay() {
    assert_interrupted_post_tool_restart(true);
}

/// Both eligible checkpoint owners must preserve closed effects and turn
/// identity through repeated fresh-identity recovery.
fn assert_interrupted_post_tool_restart(compacted: bool) {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (mut old, _) = interrupted_post_tool_fixture_with_compaction(&state, compacted);
    for _ in 0..3 {
        let mut h = resume_with_changed_model(&state);
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test("main")
            .expect("records")
            .to_vec();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    &record.event, Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == old && response.interrupted_retry
                            && response.stop_reason == tau_proto::ProviderStopReason::Error
                            && response.output_items.is_empty() && response.usage.is_none()
                ))
                .count(),
            1,
            "one canonical interrupted error for the exact old request"
        );
        let successor = read_nth_prompt_created(&h, 0);
        assert_ne!(successor.agent_prompt_id, old);
        assert_eq!(successor.model, tau_proto::ModelId::from("test/model"));
        let context = serde_json::to_string(&successor.context).expect("context");
        assert_eq!(
            context.matches("committed replacement").count(),
            usize::from(compacted)
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, Event::AgentCompacted(_)))
                .count(),
            usize::from(compacted)
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionStarted(_)))
                .count(),
            usize::from(compacted)
        );
        assert_eq!(successor.context.flatten_iter().filter(|item| matches!(
            item, ContextItem::ToolResult(result)
                if result.call_id.as_str() == "prior-send"
                    && result.render_provider_text().contains("prior send accepted exactly once")
        )).count(), 1);
        assert_eq!(context.matches("queued peer work").count(), 1);
        assert!(!context.contains("provisional text already displayed"));
        assert!(
            !context.contains("Inference was interrupted"),
            "errors are not assistant context"
        );
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            1,
            "recovery must not create a same-process retry loop"
        );
        assert!(
            !event_log_contains_any_source(&h, |event| matches!(
                event,
                Event::ToolRequest(_) | Event::ToolStarted(_)
            )),
            "completed side effects must not dispatch again"
        );
        let tree = h.session_runtime.agent_store.agent("main").expect("tree");
        assert!(
            tree.pending_interrupted_inference().is_none(),
            "successor claims obligation"
        );
        let starts = records
            .iter()
            .filter_map(|record| match &record.event {
                Event::AgentOuterTurnStarted(started) => Some(started),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1, "restart retains the original outer turn");
        assert_eq!(
            tree.prompt_started(&successor.agent_prompt_id)
                .expect("start")
                .outer_turn_id
                .as_ref(),
            Some(&starts[0].outer_turn_id)
        );
        assert!(tree.outer_turn_is_open(&starts[0].outer_turn_id));
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
        );
        let replay = tau_core::AgentTree::try_from_events(crate::parse_agent_id("main"), &records)
            .expect("cold replay");
        assert!(replay.pending_interrupted_inference().is_none());
        old = successor.agent_prompt_id;
        h.shutdown().expect("shutdown");
        install_crash_cut(&state, &records);
    }
}

/// A crash after the canonical interrupted error but before its new checkpoint
/// must preserve the retry obligation, without writing a second old terminal.
#[test]
fn interrupted_terminal_before_successor_survives_restart() {
    assert_interrupted_terminal_before_successor(false, false);
}

/// A second crash after the post-compaction interrupted terminal preserves the
/// fresh-inference obligation and original outer turn, not the compaction
/// owner.
#[test]
fn interrupted_post_compaction_terminal_before_successor_survives_restart() {
    assert_interrupted_terminal_before_successor(true, false);
}

/// The outer-turn start is independently durable before prompt admission.
/// Neither that crash nor another after repair may strand its open turn.
#[test]
fn interrupted_outer_turn_start_before_prompt_start_preserves_one_turn() {
    for compacted in [false, true] {
        assert_interrupted_terminal_before_successor(compacted, true);
    }
}

/// A crash after the transaction-owned checkpoint but before prompt admission
/// must recover under a fresh identity, never reconstruct the old request.
#[test]
fn interrupted_post_compaction_checkpoint_before_prompt_start_recovers() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (old, mut cut) = interrupted_post_tool_fixture_with_compaction(&state, true);
    let owner = cut
        .iter()
        .position(|record| {
            matches!(
                &record.event, Event::AgentInferenceDispatchStarted(checkpoint)
                    if checkpoint.agent_prompt_id == old
            )
        })
        .expect("checkpoint");
    cut.truncate(owner + 1);
    install_crash_cut(&state, &cut);
    let mut h = resume_with_changed_model(&state);
    let fresh = read_nth_prompt_created(&h, 0);
    assert_ne!(fresh.agent_prompt_id, old);
    assert_eq!(fresh.model, tau_proto::ModelId::from("test/model"));
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test("main")
        .expect("records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event, Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == old && response.interrupted_retry
            ))
            .count(),
        1
    );
    assert!(!records.iter().any(|record| matches!(
        &record.event, Event::AgentPromptStarted(started) if started.agent_prompt_id == old
    )));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentCompacted(_)))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// Compaction between a real completed tool round and its next inference must
/// preserve the original turn even if the new checkpoint has no prompt start.
#[test]
fn interrupted_post_tool_compaction_checkpoint_retains_inherited_turn() {
    for reactive in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        seed_main_agent_loaded(&state);
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(if reactive {
            u64::MAX
        } else {
            900
        }));
        let cid = h
            .runtime_agent_id_for_target_agent(Some("main"))
            .expect("main");
        h.dispatch_prompt_for_agent(
            &cid,
            PendingPrompt::user("run a tool then continue".to_owned()),
        )
        .expect("dispatch");
        let initial = read_nth_prompt_created(&h, 0);
        let original_turn = tau_proto::AgentOuterTurnId::for_prompt(&initial.agent_prompt_id);
        let mut response =
            provider_text_response(&initial.agent_prompt_id, crate::parse_agent_id("main"), "");
        response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
        response.output_items = vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "closed-missing-tool".into(),
            name: ToolName::new("missing_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })];
        response.usage = Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 900,
            ..Default::default()
        });
        h.handle_provider_response_finished(response)
            .expect("tool response");
        h.drain_publish_idle_dispatches();
        let next = read_nth_prompt_created(&h, 1);
        let compact = if reactive {
            h.handle_provider_response_finished(context_overflow_response(&next))
                .expect("overflow");
            read_nth_prompt_created(&h, 2)
        } else {
            next
        };
        assert_eq!(
            compact.operation,
            tau_proto::PromptOperation::StandaloneCompaction
        );
        h.handle_provider_response_finished(
            strict_fake_compact_response(&compact).expect("closed tool round"),
        )
        .expect("compact success");
        let post = read_nth_prompt_created(&h, if reactive { 3 } else { 2 });
        let mut cut = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test("main")
            .expect("records")
            .to_vec();
        let checkpoint = cut.iter().position(|record| matches!(
            &record.event, Event::AgentInferenceDispatchStarted(checkpoint) if checkpoint.agent_prompt_id == post.agent_prompt_id
        )).expect("post-compaction checkpoint");
        cut.truncate(checkpoint + 1);
        assert_eq!(
            cut.iter()
                .filter(|record| matches!(record.event, Event::AgentOuterTurnStarted(_)))
                .count(),
            1
        );
        h.shutdown().expect("shutdown");
        install_crash_cut(&state, &cut);
        let mut restored = resume_with_changed_model(&state);
        let fresh = read_nth_prompt_created(&restored, 0);
        let tree = restored
            .session_runtime
            .agent_store
            .agent("main")
            .expect("tree");
        assert_eq!(
            tree.prompt_started(&fresh.agent_prompt_id)
                .expect("fresh start")
                .outer_turn_id
                .as_ref(),
            Some(&original_turn)
        );
        restored
            .handle_provider_response_finished(provider_text_response(
                &fresh.agent_prompt_id,
                crate::parse_agent_id("main"),
                "finished tool continuation",
            ))
            .expect("finish");
        let records = restored
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test("main")
            .expect("records");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, Event::AgentOuterTurnStarted(_)))
                .count(),
            1
        );
        assert_eq!(records.iter().filter(|record| matches!(
            &record.event, Event::AgentOuterTurnFinished(finished) if finished.outer_turn_id == original_turn
        )).count(), 1);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, Event::AgentCompacted(_)))
                .count(),
            1
        );
        assert!(
            !event_log_events(&restored)
                .iter()
                .any(|event| matches!(event, Event::ToolRequest(_) | Event::ToolStarted(_)))
        );
        restored.shutdown().expect("shutdown");
    }
}

/// The canonical terminal is independently sufficient to recover both eligible
/// owner types without publishing the old terminal twice.
fn assert_interrupted_terminal_before_successor(compacted: bool, before_prompt_start: bool) {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (old, mut original) = interrupted_post_tool_fixture_with_compaction(&state, compacted);
    if before_prompt_start {
        let start = original.iter().position(|record| matches!(
            &record.event, Event::AgentOuterTurnStarted(started) if started.agent_prompt_id == old
        )).expect("outer turn start");
        original.truncate(start + 1);
        install_crash_cut(&state, &original);
    }
    let mut first =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("first recovery");
    let mut terminal_prompt = old.clone();
    if before_prompt_start {
        for _ in 0..2 {
            let successor = read_nth_prompt_created(&first, 0).agent_prompt_id;
            let mut cut = first
                .session_runtime
                .agent_store
                .snapshot_agent_events_for_test("main")
                .expect("successor records")
                .to_vec();
            assert_eq!(
                cut.iter()
                    .filter(|record| matches!(record.event, Event::AgentOuterTurnStarted(_)))
                    .count(),
                1
            );
            let owner = cut
                .iter()
                .position(|record| {
                    matches!(
                        &record.event, Event::AgentInferenceDispatchStarted(checkpoint)
                            if checkpoint.agent_prompt_id == successor
                    )
                })
                .expect("successor checkpoint");
            cut.truncate(owner + 1);
            first.shutdown().expect("close successor cut");
            install_crash_cut(&state, &cut);
            first = resume_with_changed_model(&state);
            terminal_prompt = successor;
        }
    }
    let mut cut = first
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test("main")
        .expect("records")
        .to_vec();
    let terminal_index = cut
        .iter()
        .position(|record| {
            matches!(
                &record.event, Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == terminal_prompt && response.interrupted_retry
            )
        })
        .expect("interrupted terminal");
    assert_eq!(
        cut.iter()
            .filter(|record| matches!(record.event, Event::AgentOuterTurnStarted(_)))
            .count(),
        1,
        "first repair retains the existing open turn even without prompt admission"
    );
    cut.truncate(terminal_index + 1);
    let replay = tau_core::AgentTree::try_from_events(crate::parse_agent_id("main"), &cut)
        .expect("terminal-only replay");
    assert_eq!(
        replay
            .pending_interrupted_inference()
            .expect("owed")
            .agent_prompt_id,
        terminal_prompt
    );
    first.shutdown().expect("shutdown");
    install_crash_cut(&state, &cut);
    let mut restored = resume_with_changed_model(&state);
    let successor = read_nth_prompt_created(&restored, 0);
    assert_ne!(successor.agent_prompt_id, old);
    assert_eq!(successor.model, tau_proto::ModelId::from("test/model"));
    let tree = restored
        .session_runtime
        .agent_store
        .agent("main")
        .expect("tree");
    let original_turn = tau_proto::AgentOuterTurnId::for_prompt(&old);
    assert_eq!(
        tree.prompt_started(&successor.agent_prompt_id)
            .expect("new start")
            .outer_turn_id
            .as_ref(),
        Some(&original_turn)
    );
    assert!(tree.outer_turn_is_open(&original_turn));
    assert_eq!(
        restored
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test("main")
            .expect("records")
            .iter()
            .filter(|record| matches!(
                &record.event, Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == old
            ))
            .count(),
        1
    );
    restored
        .handle_provider_response_finished(provider_text_response(
            &successor.agent_prompt_id,
            crate::parse_agent_id("main"),
            "finished recovered work",
        ))
        .expect("real completion");
    assert!(
        !restored
            .session_runtime
            .agent_store
            .agent("main")
            .expect("tree")
            .outer_turn_is_open(&original_turn)
    );
    let records = restored
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test("main")
        .expect("completed records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentOuterTurnStarted(_)))
            .count(),
        1
    );
    assert_eq!(records.iter().filter(|record| matches!(
        &record.event, Event::AgentOuterTurnFinished(finished) if finished.outer_turn_id == original_turn
    )).count(), 1);
    restored.shutdown().expect("shutdown");
}

/// Only a harness-authored output-free failure of an unresolved ordinary owner
/// can retain retry authority. Provider reports and invalid accounting cannot
/// turn arbitrary failures into fresh paid work.
#[test]
fn interrupted_retry_requires_exact_owner_and_cannot_be_authored_by_provider_report() {
    assert_interrupted_retry_requires_exact_owner(false);
}

/// Transaction ownership cannot grant provider-authored retry authority or
/// permit synthetic output/accounting on the repair terminal.
#[test]
fn interrupted_post_compaction_retry_validates_exact_owner_and_empty_terminal() {
    assert_interrupted_retry_requires_exact_owner(true);
}

/// Exercise core validation against each eligible cold checkpoint owner.
fn assert_interrupted_retry_requires_exact_owner(compacted: bool) {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (old, cut) = interrupted_post_tool_fixture_with_compaction(&state, compacted);
    let tree = tau_core::AgentTree::try_from_events(crate::parse_agent_id("main"), &cut)
        .expect("old owner");
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("recovery");
    let response = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test("main")
        .expect("records")
        .iter()
        .find_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) if response.agent_prompt_id == old => {
                Some(response.clone())
            }
            _ => None,
        })
        .expect("recovery terminal");
    tree.validate_event(&Event::ProviderResponseFinished(response.clone()))
        .expect("valid");
    let mut invalid = response.clone();
    invalid.agent_prompt_id = test_agent_prompt_id("unknown-owner");
    assert!(
        tree.validate_event(&Event::ProviderResponseFinished(invalid))
            .is_err()
    );
    let mut invalid = response.clone();
    invalid.output_items =
        provider_text_response(&old, crate::parse_agent_id("main"), "invented output").output_items;
    assert!(
        tree.validate_event(&Event::ProviderResponseFinished(invalid))
            .is_err()
    );
    let mut invalid = response.clone();
    invalid.usage = Some(tau_proto::ProviderTokenUsage::default());
    assert!(
        tree.validate_event(&Event::ProviderResponseFinished(invalid))
            .is_err()
    );
    let encoded = serde_json::to_value(&response).expect("encode");
    assert_eq!(encoded["interrupted_retry"], true);
    let report: tau_proto::ProviderResponseFinishedReport =
        serde_json::from_value(encoded).expect("reports ignore unknown harness-only fields");
    assert!(
        !report
            .into_canonical(tau_proto::FinalStatusDisposition::Accepted)
            .interrupted_retry
    );
    let mut ordinary = response;
    ordinary.interrupted_retry = false;
    let encoded = serde_json::to_value(&ordinary).expect("old-compatible encode");
    assert!(encoded.get("interrupted_retry").is_none());
    assert!(
        !serde_json::from_value::<ProviderResponseFinished>(encoded)
            .expect("missing field defaults false")
            .interrupted_retry
    );
    h.shutdown().expect("shutdown");
}

/// A delegated worker interrupted after its canonical error but before retry
/// remains owned by its parent rather than being mistaken for completed work.
#[test]
fn interrupted_terminal_does_not_detach_delegated_worker_on_replay() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (old, _) = interrupted_post_tool_fixture(&state);
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("recovery");
    let mut records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test("main")
        .expect("records");
    let terminal = records.iter().position(|record| matches!(&record.event,
        Event::ProviderResponseFinished(response) if response.agent_prompt_id == old && response.interrupted_retry
    )).expect("interrupted terminal");
    records.truncate(terminal + 1);
    let originator = tau_proto::PromptOriginator::Extension {
        name: crate::harness::harness_extension_name().clone(),
        query_id: "delegated-restart-work".to_owned(),
    };
    for record in &mut records {
        match &mut record.event {
            Event::AgentStarted(started) => {
                started.parent_agent = Some(crate::parse_agent_id("parent"));
                started.creator = Some(tau_proto::AgentCreator::Agent {
                    session_id: test_session_id("s1"),
                    agent_id: crate::parse_agent_id("parent"),
                });
            }
            Event::ProviderResponseFinished(response) => response.originator = originator.clone(),
            Event::AgentPromptStarted(started) => started.originator = originator.clone(),
            _ => {}
        }
    }
    let tree = tau_core::AgentTree::try_from_events(crate::parse_agent_id("main"), &records)
        .expect("cold delegated cut");
    assert!(tree.pending_interrupted_inference().is_some());
    let creation = records.iter().find_map(|record| match &record.event {
        Event::AgentStarted(started) => Some(started),
        _ => None,
    });
    assert!(
        !Harness::journal_proves_completed_start_agent_worker(&records, creation, &originator),
        "retry debt is not a completed parent-owned request"
    );
    h.shutdown().expect("shutdown");
}

/// A post-compaction cold terminal remains owned through interception and
/// rejected admission; capacity wakes must publish it once before dispatch.
#[test]
fn interrupted_post_compaction_terminal_retries_rejected_admission_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (old, _) = interrupted_post_tool_fixture_with_compaction(&state, true);
    let mut h = quiet_provider_harness_for_with_start_reason_storage_mode_and_hook(
        "s1",
        &state,
        tau_proto::SessionStartReason::Resume,
        crate::HarnessStorageMode::Durable,
        Some(Box::new(|h| {
            connect_test_tool(h, "cold-interceptor");
            h.handle_extension_event(
                "cold-interceptor",
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(
                        tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                    )],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("intercept cold error");
        })),
    )
    .expect("restore");
    let cid = h
        .runtime_agent_id_for_target_agent(Some("main"))
        .expect("main");
    let held = h
        .runtime_io
        .publication
        .pending_intercept
        .as_ref()
        .expect("held")
        .event
        .clone();
    assert!(matches!(&held, Event::ProviderResponseFinished(response)
        if response.agent_prompt_id == old && response.interrupted_retry));
    assert!(
        !event_log_events(&h)
            .iter()
            .any(|event| matches!(event, Event::AgentPromptCreated(_)))
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "cold-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release");
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
    );
    for _ in 0..2 {
        h.session_runtime
            .persistence_owner
            .as_ref()
            .expect("persistence")
            .signal_capacity_ready_for_test();
        let mut served_clients = 0;
        h.handle_runtime_event(
            HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress),
            &mut served_clients,
        )
        .expect("capacity wake");
    }
    h.drain_publish_idle_dispatches();
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test("main")
        .expect("records");
    assert_eq!(
        records.iter().filter(|record| record.event == held).count(),
        1
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1
    );
    assert_ne!(read_nth_prompt_created(&h, 0).agent_prompt_id, old);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_uncertain_supersessions
            .is_empty()
    );
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// Interception and rejected admission retain the exact restart error, while
/// cancellation wins before commit and cannot resurrect the interrupted work.
#[test]
fn interrupted_error_publication_retries_exactly_once_and_respects_cancellation() {
    for cancel in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        interrupted_post_tool_fixture(&state);
        let mut h =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("restore");
        let cid = h
            .runtime_agent_id_for_target_agent(Some("main"))
            .expect("main");
        let old = read_nth_prompt_created(&h, 0).agent_prompt_id;
        let provider = h.provider_runtime.pending_prompts[&old].clone();
        let model = h.provider_runtime.model_info[&"test/model".into()].clone();
        h.handle_disconnect(&provider);
        connect_ready_configured_extension(
            &mut h,
            "restart-provider",
            "restart-provider",
            tau_proto::ClientKind::Provider,
        );
        h.publish_provider_models_update(
            &crate::test_connection_id("restart-provider"),
            crate::test_extension_name("restart-provider"),
            tau_proto::ProviderModelsDeclared {
                models: vec![model],
            },
        );
        connect_test_tool(&mut h, "restart-interceptor");
        h.handle_extension_event(
            "restart-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept restart error");
        let Some(tau_core::InferenceDispatchRecovery::DispatchUncertain(checkpoint)) = h
            .session_runtime
            .agent_store
            .agent("main")
            .expect("tree")
            .inference_dispatch_recovery()
        else {
            panic!("uncertain checkpoint")
        };
        assert!(h.stage_interrupted_inference(&cid, &checkpoint));
        h.try_publish_ready_uncertain_supersession(&cid);
        let held = h
            .runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .expect("held terminal")
            .event
            .clone();
        assert!(matches!(&held, Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == old && response.interrupted_retry));
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            1
        );
        if cancel {
            h.handle_cancel_prompt(
                crate::harness::harness_connection_id(),
                &tau_proto::UiCancelPrompt {
                    session_id: h.session_runtime.current_session_id.clone(),
                    target_agent_id: Some(crate::parse_agent_id("main")),
                    agent_prompt_id: None,
                },
            );
        } else {
            reject_next_semantic_admission(&h);
        }
        h.handle_extension_event(
            "restart-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release intercepted terminal");
        if !cancel {
            assert_eq!(
                event_log_events(&h)
                    .iter()
                    .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                    .count(),
                1
            );
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .contains_key(&cid)
            );
            h.session_runtime
                .persistence_owner
                .as_ref()
                .expect("persistence")
                .signal_capacity_ready_for_test();
            let mut served_clients = 0;
            h.handle_runtime_event(
                HarnessEvent::Command(HarnessCommand::SemanticPersistenceProgress),
                &mut served_clients,
            )
            .expect("retry rejected terminal");
        }
        h.drain_publish_idle_dispatches();
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test("main")
            .expect("records");
        assert_eq!(
            records.iter().filter(|record| record.event == held).count(),
            usize::from(!cancel)
        );
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            1 + usize::from(!cancel)
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_uncertain_supersessions
                .is_empty()
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty()
        );
        assert_eq!(records.iter().filter(|record| matches!(
            &record.event, Event::AgentPromptTerminated(terminal)
                if terminal.agent_prompt_id == old && terminal.reason == tau_proto::AgentPromptTerminationReason::Canceled
        )).count(), usize::from(cancel));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
                .count(),
            usize::from(cancel)
        );
        h.shutdown().expect("shutdown");
    }
}

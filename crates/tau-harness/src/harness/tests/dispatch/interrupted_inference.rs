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
    wait_for_session_unlock(state, "s1");
    let mut journal = File::create(state.join("agents/main/events.cbor")).expect("journal");
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

/// Seeds a closed send-tool round, then interrupts the next ordinary sampling.
/// The queued peer input is durable but cannot activate the pre-fix cold owner.
fn interrupted_post_tool_fixture(
    state: &Path,
) -> (tau_proto::AgentPromptId, Vec<tau_core::PersistedAgentEvent>) {
    seed_main_agent_loaded(state);
    let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
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
    h.dispatch_prompt_after_publish_idle(&cid);
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
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (mut old, _) = interrupted_post_tool_fixture(&state);
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
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (old, _) = interrupted_post_tool_fixture(&state);
    let mut first =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("first recovery");
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
                    if response.agent_prompt_id == old && response.interrupted_retry
            )
        })
        .expect("interrupted terminal");
    cut.truncate(terminal_index + 1);
    let replay = tau_core::AgentTree::try_from_events(crate::parse_agent_id("main"), &cut)
        .expect("terminal-only replay");
    assert_eq!(
        replay
            .pending_interrupted_inference()
            .expect("owed")
            .agent_prompt_id,
        old
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
    let original_turn = tree
        .prompt_started(&old)
        .expect("old start")
        .outer_turn_id
        .clone()
        .expect("turn");
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
    restored.shutdown().expect("shutdown");
}

/// Only a harness-authored output-free failure of an unresolved ordinary owner
/// can retain retry authority. Provider reports and invalid accounting cannot
/// turn arbitrary failures into fresh paid work.
#[test]
fn interrupted_retry_requires_exact_owner_and_cannot_be_authored_by_provider_report() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (old, cut) = interrupted_post_tool_fixture(&state);
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

//! Tests for compaction lifecycle behavior.

use super::super::dispatch::{
    context_overflow_response, enable_remote_compaction_for_test_model, provider_text_response,
    standalone_compaction_success_response,
};
use super::*;

/// A successful standalone response retains its exact compacted boundary and
/// prompt ownership through append rejection, then clears both exactly once
/// after durable admission and cold replay.
#[test]
fn standalone_success_is_owed_until_compacted_admission() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let parent = h
        .selected_head_for_agent(&cid)
        .unwrap_or(tau_proto::AgentHead::Root);
    let transaction_id =
        tau_proto::CompactionTransactionId::parse("ct-s1-success").expect("transaction id");
    h.publish_for_agent(
        &cid,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            compact_prompt_id: test_agent_prompt_id("ap-s1-success"),
            cut: parent,
            resume_through: None,
            model: "test/model".into(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        }),
    );
    let prompt = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::AgentPromptCreated(prompt)
                if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction =>
            {
                Some(prompt)
            }
            _ => None,
        })
        .expect("standalone prompt");
    let boundary_parent = h
        .selected_head_for_agent(&cid)
        .unwrap_or(tau_proto::AgentHead::Root);
    connect_test_tool(&mut h, "s1-compacted-reject");
    h.handle_extension_event(
        "s1-compacted-reject",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::AGENT_COMPACTED)],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register compacted interceptor");
    h.handle_provider_response_finished(standalone_compaction_success_response(
        &prompt,
        "replacement",
    ))
    .expect("park compacted boundary");
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&prompt.agent_prompt_id)
            && h.prompt_coordination
                .prompt_runtime
                .tool_specs
                .contains_key(&prompt.agent_prompt_id)
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "s1-compacted-reject",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("reject compacted append");
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
            && h.prompt_coordination
                .prompt_runtime
                .agents
                .contains_key(&prompt.agent_prompt_id)
            && h.prompt_coordination
                .prompt_runtime
                .tool_specs
                .contains_key(&prompt.agent_prompt_id)
    );
    h.retry_pending_agent_publish_completion(&cid);
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&prompt.agent_prompt_id)
            && !h
                .prompt_coordination
                .prompt_runtime
                .tool_specs
                .contains_key(&prompt.agent_prompt_id)
            && !h
                .prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(&cid)
    );
    let compacted = |h: &Harness| {
        h.session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("agent events")
            .into_iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentCompacted(compacted)
                        if compacted.transaction_id.as_ref() == Some(&transaction_id)
                )
            })
            .collect::<Vec<_>>()
    };
    let live = compacted(&h);
    assert_eq!(live.len(), 1);
    assert_eq!(
        live[0].parent,
        tau_core::AgentEventParent::from_head(boundary_parent)
    );
    drop(h);
    wait_for_session_unlock(&state, "s1");
    let resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert_eq!(compacted(&resumed).len(), 1);
}

/// Automatic success must satisfy an already queued UI intent exactly once
/// after either an attempted interceptor Drop or a rejected semantic append.
#[test]
fn ui_compaction_satisfaction_is_owed_across_publication_failures() {
    for reject_append in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td
            .path()
            .join(if reject_append { "reject" } else { "drop" });
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        let cut = h
            .selected_head_for_agent(&cid)
            .unwrap_or(tau_proto::AgentHead::Root);
        let transaction_id =
            tau_proto::CompactionTransactionId::parse("ct-s5-ui-auto").expect("transaction id");
        h.publish_for_agent(
            &cid,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: agent_id.clone(),
                transaction_id: transaction_id.clone(),
                compact_prompt_id: test_agent_prompt_id("ap-s5-ui-auto"),
                cut,
                resume_through: None,
                model: "test/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::AutomaticThreshold,
            }),
        );
        let prompt = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentPromptCreated(prompt)
                    if prompt.operation == tau_proto::PromptOperation::StandaloneCompaction =>
                {
                    Some(prompt)
                }
                _ => None,
            })
            .expect("standalone prompt");
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(agent_id.as_str()),
        );
        let request_id = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentManualCompactionRequested(request) => Some(request.request_id),
                _ => None,
            })
            .expect("queued UI request");
        let interceptor = if reject_append {
            "s5-ui-reject"
        } else {
            "s5-ui-drop"
        };
        connect_test_tool(&mut h, interceptor);
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_MANUAL_COMPACTION_REQUEST_SATISFIED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register satisfaction interceptor");
        h.handle_provider_response_finished(standalone_compaction_success_response(
            &prompt,
            "replacement",
        ))
        .expect("complete automatic compaction");
        let satisfaction_parent = h
            .selected_head_for_agent(&cid)
            .unwrap_or(tau_proto::AgentHead::Root);
        assert!(matches!(
            h.runtime_io
                .publication
                .pending_intercept
                .as_ref()
                .map(|pending| &pending.event),
            Some(Event::AgentManualCompactionRequestSatisfied(satisfied))
                if satisfied.request_id == request_id
                    && satisfied.transaction_id == transaction_id
        ));
        if reject_append {
            reject_next_semantic_admission(&h);
        }
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: if reject_append {
                    InterceptAction::Pass(None)
                } else {
                    InterceptAction::Drop
                },
            })),
        )
        .expect("resolve satisfaction publication");
        if reject_append {
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .contains_key(&cid)
            );
            h.retry_pending_agent_publish_completion(&cid);
        }
        let satisfied = |h: &Harness| {
            h.session_runtime
                .agent_store
                .agent_events(agent_id.as_str())
                .expect("agent events")
                .into_iter()
                .filter(|record| {
                    matches!(
                        &record.event,
                        Event::AgentManualCompactionRequestSatisfied(satisfied)
                            if satisfied.request_id == request_id
                                && satisfied.transaction_id == transaction_id
                    )
                })
                .collect::<Vec<_>>()
        };
        let live = satisfied(&h);
        assert_eq!(live.len(), 1, "reject_append={reject_append}");
        assert_eq!(
            live[0].parent,
            tau_core::AgentEventParent::from_head(satisfaction_parent)
        );
        drop(h);
        wait_for_session_unlock(&state, "s1");
        let resumed =
            echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let replayed = satisfied(&resumed);
        assert_eq!(replayed.len(), 1, "reject_append={reject_append}");
        assert_eq!(
            replayed[0].parent,
            tau_core::AgentEventParent::from_head(satisfaction_parent)
        );
    }
}

/// Cold replay's Interrupted repair remains owed across attempted Drop and
/// semantic append rejection, preserving its exact parent exactly once.
#[test]
fn interrupted_compaction_replay_repair_is_owed_across_publication_failures() {
    for reject_append in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td
            .path()
            .join(if reject_append { "reject" } else { "drop" });
        let mut h = echo_harness(&state).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        let parent = h
            .selected_head_for_agent(&cid)
            .unwrap_or(tau_proto::AgentHead::Root);
        let transaction_id =
            tau_proto::CompactionTransactionId::parse("ct-s5-replay").expect("transaction id");
        h.append_direct_agent_semantic_event(
            agent_id.as_str(),
            tau_core::AgentEventParent::from_head(parent),
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: agent_id.clone(),
                transaction_id: transaction_id.clone(),
                compact_prompt_id: test_agent_prompt_id("ap-s5-replay"),
                cut: parent,
                resume_through: None,
                model: "echo/model".into(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: tau_proto::PromptOriginator::User,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::Manual,
            }),
        )
        .expect("seed interrupted transaction");

        let interceptor = if reject_append {
            "s5-replay-reject"
        } else {
            "s5-replay-drop"
        };
        connect_test_tool(&mut h, interceptor);
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_STANDALONE_COMPACTION_FAILED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register replay-repair interceptor");

        h.agent_runtime.agent_registry.agents.clear();
        h.agent_runtime.agent_registry.agent_routes.clear();
        h.agent_runtime.agent_registry.session_loaded.clear();
        h.rehydrate_agents_from_session();
        let restored_cid = h
            .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .expect("restored agent");
        assert!(matches!(
            h.runtime_io
                .publication
                .pending_intercept
                .as_ref()
                .map(|pending| &pending.event),
            Some(Event::AgentStandaloneCompactionFailed(failed))
                if failed.transaction_id == transaction_id
                    && failed.reason
                        == tau_proto::StandaloneCompactionFailureReason::Interrupted
        ));
        if reject_append {
            reject_next_semantic_admission(&h);
        }
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: if reject_append {
                    InterceptAction::Pass(None)
                } else {
                    InterceptAction::Drop
                },
            })),
        )
        .expect("resolve replay repair");
        if reject_append {
            assert!(
                h.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .contains_key(&restored_cid)
            );
            h.retry_pending_agent_publish_completion(&restored_cid);
        }
        let repaired = h
            .session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("agent events")
            .into_iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentStandaloneCompactionFailed(failed)
                        if failed.transaction_id == transaction_id
                            && failed.reason
                                == tau_proto::StandaloneCompactionFailureReason::Interrupted
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(repaired.len(), 1, "reject_append={reject_append}");
        assert_eq!(
            repaired[0].parent,
            tau_core::AgentEventParent::from_head(parent)
        );
        drop(h);
        wait_for_session_unlock(&state, "s1");
        let resumed =
            echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
                .expect("cold replay");
        let replayed = resumed
            .session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("replayed events")
            .into_iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentStandaloneCompactionFailed(failed)
                        if failed.transaction_id == transaction_id
                            && failed.reason
                                == tau_proto::StandaloneCompactionFailureReason::Interrupted
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(replayed.len(), 1, "reject_append={reject_append}");
        assert_eq!(
            replayed[0].parent,
            tau_core::AgentEventParent::from_head(parent)
        );
    }
}

/// The normal eligible path must issue exactly one successor with the source
/// identity and account for both provider responses independently.
#[test]
fn output_length_continuation_delivers_exactly_one_captured_successor() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish normally".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 17))
        .expect("source length response");
    let successor = read_nth_prompt_created(&h, 1);
    assert_ne!(successor.agent_prompt_id, source.agent_prompt_id);
    assert_eq!(successor.model, source.model);
    assert_eq!(successor.operation, tau_proto::PromptOperation::Inference);
    assert!(successor.context.blocks.iter().any(|block| matches!(
        block,
        tau_proto::ContextBlock::AssistantResponse(response)
            if response.output_items.iter().any(|item| matches!(
                item,
                ContextItem::ReasoningText(reasoning)
                    if reasoning.kind == tau_proto::ReasoningTextKind::Full
                        && reasoning.text == "retained reasoning"
            ))
    )));
    assert!(successor.context.flatten_iter().any(|item| matches!(
        item,
        ContextItem::Message(message)
            if message.role == tau_proto::ContextRole::User
                && message.content.iter().any(|part| matches!(
                    part,
                    tau_proto::ContentPart::Text { text }
                        if text.contains(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION)
                ))
    )));

    let mut terminal = provider_text_response(
        &successor.agent_prompt_id,
        successor.agent_id.clone(),
        "finished answer",
    );
    terminal.usage = Some(tau_proto::ProviderTokenUsage {
        response_received_tokens: 23,
        ..Default::default()
    });
    terminal.backend = Some(tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::ChatCompletions,
        base_url: "https://example.invalid/v1".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    });
    h.handle_provider_response_finished(terminal)
        .expect("successor terminal");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let responses = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => Some(response),
            _ => None,
        })
        .collect::<Vec<_>>();
    let event_position = |predicate: &dyn Fn(&Event) -> bool| {
        records
            .iter()
            .position(|record| predicate(&record.event))
            .expect("required durable continuation fact")
    };
    let plan_position = event_position(&|event| {
        matches!(
            event,
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == source.agent_prompt_id
                    && matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                    )
        )
    });
    let steer_position = event_position(&|event| {
        matches!(
            event,
            Event::AgentPromptSteered(steer)
                if steer.internal_kind
                    == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
                    && steer.text == tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION
        )
    });
    let owner_position = event_position(&|event| {
        matches!(
            event,
            Event::AgentInferenceDispatchStarted(owner)
                if owner.agent_prompt_id == successor.agent_prompt_id
                    && owner.output_length_continuation.is_some()
                    && owner.model.as_ref() == Some(&source.model)
        )
    });
    let successor_start_position = event_position(&|event| {
        matches!(
            event,
            Event::AgentPromptStarted(started)
                if started.agent_prompt_id == successor.agent_prompt_id
                    && started.outer_turn_id
                        == Some(tau_proto::AgentOuterTurnId::for_prompt(
                            &source.agent_prompt_id
                        ))
        )
    });
    assert!(
        plan_position < steer_position
            && steer_position < owner_position
            && owner_position < successor_start_position
    );
    // The steer is the exact harness-internal user-role instruction: full
    // trusted span, internal class, harness source, and no compaction or
    // context authority.
    let steer_record = records
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentPromptSteered(steer)
                if steer.internal_kind
                    == Some(tau_proto::InternalPromptKind::OutputLengthContinuation) =>
            {
                Some(steer)
            }
            _ => None,
        })
        .expect("durable continuation steer");
    assert!(steer_record.inference_activation);
    assert_eq!(
        steer_record.message_class,
        tau_proto::PromptMessageClass::Internal
    );
    assert_eq!(
        steer_record.submission_source,
        tau_proto::PromptSubmissionSource::HarnessInternal
    );
    assert_eq!(steer_record.self_compaction_terminal, None);
    assert_eq!(steer_record.ctx_id, None);
    assert_eq!(
        steer_record.trusted_internal_spans,
        vec![tau_proto::TrustedInternalSpan {
            start: 0,
            end: u32::try_from(steer_record.text.len()).expect("bounded instruction"),
        }]
    );

    // The successor owner keeps the source checkpoint's activation cut and
    // folds on the same selected-branch node as the successor prompt-start.
    let source_dispatch = records
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(dispatch)
                if dispatch.agent_prompt_id == source.agent_prompt_id =>
            {
                Some(dispatch)
            }
            _ => None,
        })
        .expect("source dispatch");
    let owner_record = records
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(dispatch)
                if dispatch.agent_prompt_id == successor.agent_prompt_id
                    && dispatch.output_length_continuation.is_some() =>
            {
                Some(dispatch)
            }
            _ => None,
        })
        .expect("successor owner");
    assert_eq!(owner_record.activation_cut, source_dispatch.activation_cut);
    assert_eq!(
        owner_record.operation,
        Some(tau_proto::PromptOperation::Inference)
    );
    assert_eq!(owner_record.agent_id, source.agent_id);
    assert!(
        matches!(owner_record.through, tau_proto::AgentHead::Node(_)),
        "the owner folds on the steer-created selected-branch head"
    );
    let owner_parent = records
        .iter()
        .find(|record| {
            matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(dispatch)
                    if dispatch.agent_prompt_id == successor.agent_prompt_id
                        && dispatch.output_length_continuation.is_some()
            )
        })
        .expect("owner record")
        .parent;
    let start_parent = records
        .iter()
        .find(|record| {
            matches!(
                &record.event,
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == successor.agent_prompt_id
            )
        })
        .expect("successor start record")
        .parent;
    assert_eq!(
        owner_parent, start_parent,
        "successor dispatch and prompt-start stay on the selected branch"
    );

    // The transient successor prompt is delivered only after the successor's
    // durable prompt-start commits.
    let log = event_log_events(&h);
    let start_position = log
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == successor.agent_prompt_id
            )
        })
        .expect("successor prompt-start in event log");
    let created_position = log
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentPromptCreated(prompt)
                    if prompt.agent_prompt_id == successor.agent_prompt_id
            )
        })
        .expect("successor prompt-created in event log");
    assert!(
        start_position < created_position,
        "prompt-created delivery must follow the durable prompt-start commit"
    );
    assert_eq!(responses.len(), 2);
    assert!(matches!(
        responses[0].output_length_disposition,
        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
    ));
    assert!(matches!(
        responses[1].output_length_disposition,
        tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            outer_turn_finish_owed: true,
            ..
        }
    ));
    assert_eq!(
        responses
            .iter()
            .map(|response| {
                response
                    .usage
                    .as_ref()
                    .map_or(0, |usage| usage.response_received_tokens)
            })
            .collect::<Vec<_>>(),
        vec![17, 23]
    );
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .received_tokens,
        40
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
    h.shutdown().expect("shutdown");
}

/// A reserved successor's append-rejected context response retries exactly once
/// before compaction starts; after cold restore, only its exact post-compaction
/// descendant retains the lineage and a second Length closes it without a third
/// ordinary inference.
#[test]
fn output_length_reactive_compaction_terminalizes_exact_descendant() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.submit_user_prompt(test_session_id("s1"), "recover once".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 5))
        .expect("source plan");
    let successor = read_nth_prompt_created(&h, 1);
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    let public_id = durable_agent_id_for_conversation(&h, &cid);
    h.agent_runtime.agent_watch.provider_status.insert(
        public_id.to_string(),
        tau_proto::AgentWatchProviderStatusNotification {
            session_id: test_session_id("s1"),
            subscription_id: String::new(),
            turn_generation: h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .turn_generation,
            agent_prompt_id: successor.agent_prompt_id.clone(),
            state: tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Transport,
                attempt: 3,
                next_retry_delay_secs: 0,
            },
            initial: false,
        },
    );
    reject_semantic_admissions(&h, 2);
    h.handle_provider_response_finished(context_overflow_response(&successor))
        .expect("reserved successor context rejection");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2,
        "compaction cannot start before the rejection is write-complete"
    );
    let _start_interceptor = connect_test_tool(&mut h, "reactive-start-interceptor");
    h.handle_extension_event(
        "reactive-start-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register reactive start interceptor");
    h.retry_pending_agent_publications();
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentStandaloneCompactionStarted(_))
    ));
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "reactive-start-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release reactive start into append failure");
    h.retry_pending_agent_publications();
    h.handle_disconnect(&crate::test_connection_id("reactive-start-interceptor"));
    let compact = read_nth_prompt_created(&h, 2);
    assert_eq!(
        compact.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    assert!(matches!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .get(public_id.as_str())
            .map(|status| &status.state),
        Some(tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 4 })
    ));
    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id.clone(),
        "compact replacement",
    ))
    .expect("compaction succeeds");
    let descendant = read_nth_prompt_created(&h, 3);
    h.shutdown().expect("shutdown at reactive descendant cut");
    drop(h);
    let mut h =
        quiet_provider_harness_with_start_reason(td.path(), tau_proto::SessionStartReason::Resume)
            .expect("cold reactive restore");
    enable_remote_compaction_for_test_model(&mut h);
    let unrelated_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    h.dispatch_prompt_for_agent(
        &unrelated_cid,
        PendingPrompt::user("valid unrelated inference".to_owned()),
    )
    .expect("dispatch unrelated inference");
    let unrelated_agent_id = durable_agent_id_for_conversation(&h, &unrelated_cid);
    let unrelated = event_log_events(&h)
        .iter()
        .find_map(|event| match event {
            Event::AgentPromptCreated(created) if created.agent_id == unrelated_agent_id => {
                Some(created.clone())
            }
            _ => None,
        })
        .expect("unrelated prompt");
    h.handle_provider_response_finished(reasoning_only_length_response(&unrelated, 2))
        .expect("valid unrelated inference plans its own lineage");
    h.handle_provider_response_finished(reasoning_only_length_response(&descendant, 7))
        .expect("descendant length terminal");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let responses = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => Some(response),
            _ => None,
        })
        .collect::<Vec<_>>();
    let rejected = responses
        .iter()
        .copied()
        .find(|response| response.agent_prompt_id == successor.agent_prompt_id)
        .expect("reserved rejection");
    assert_eq!(
        rejected.recovery_disposition,
        tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
    );
    assert_eq!(
        rejected.output_length_disposition,
        tau_proto::OutputLengthDisposition::None
    );
    assert_eq!(rejected.provider_attempt.get(), 4);
    assert_eq!(
        responses
            .iter()
            .filter(|response| response.agent_prompt_id == successor.agent_prompt_id)
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentStandaloneCompactionStarted(_)))
            .count(),
        1
    );
    assert!(responses.iter().any(|response| {
        response.agent_prompt_id == descendant.agent_prompt_id
            && matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationTerminal {
                    outcome: tau_proto::OutputLengthContinuationOutcome::Incomplete,
                    outer_turn_finish_owed: true,
                    ..
                }
            )
    }));
    assert!(!responses.iter().any(|response| {
        response.agent_prompt_id == unrelated.agent_prompt_id
            && matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
            )
    }));
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentPromptStarted(_)))
            .count(),
        4
    );
    h.shutdown().expect("shutdown");
}

/// A cold cut after reactive compaction commits but before its exact descendant
/// checkpoint restores the reserved output-length owner. Cancellation commits
/// that checkpoint and the sole Cancelled terminal without provider delivery.
#[test]
fn output_length_reactive_post_compaction_checkpoint_cut_is_cancellable() {
    let td = TempDir::new().expect("tempdir");
    let source_agent_id;
    let descendant_prompt_id;
    let cut_records;
    {
        let mut h = quiet_provider_harness(td.path()).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        h.submit_user_prompt(test_session_id("s1"), "cold reactive cut".to_owned())
            .expect("submit");
        let source = read_nth_prompt_created(&h, 0);
        source_agent_id = source.agent_id.clone();
        h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
            .expect("source plan");
        let successor = read_nth_prompt_created(&h, 1);
        h.handle_provider_response_finished(context_overflow_response(&successor))
            .expect("plan recovery");
        let compact = read_nth_prompt_created(&h, 2);
        let _interceptor = connect_test_tool(&mut h, "reactive-checkpoint-cut");
        h.handle_extension_event(
            "reactive-checkpoint-cut",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register checkpoint cut");
        h.handle_provider_response_finished(provider_text_response(
            &compact.agent_prompt_id,
            compact.agent_id,
            "compact replacement",
        ))
        .expect("commit compaction");
        descendant_prompt_id = match h
            .runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event)
        {
            Some(Event::AgentInferenceDispatchStarted(started)) => started.agent_prompt_id.clone(),
            other => panic!("expected parked descendant checkpoint, got {other:?}"),
        };
        cut_records = h
            .session_runtime
            .agent_store
            .agent_events(source_agent_id.as_str())
            .expect("cut records")
            .to_vec();
        h.shutdown().expect("shutdown before crash cut");
    }
    let journal_path = td
        .path()
        .join("agents")
        .join(source_agent_id.as_str())
        .join("events.cbor");
    let mut journal = File::create(&journal_path).expect("rewrite crash cut");
    for record in &cut_records {
        let mut encoded = Vec::new();
        ciborium::into_writer(record, &mut encoded).expect("encode cut record");
        journal
            .write_all(&(encoded.len() as u64).to_le_bytes())
            .expect("write cut length");
        journal.write_all(&encoded).expect("write cut record");
    }
    journal.sync_all().expect("sync crash cut");

    let mut restored =
        quiet_provider_harness_with_start_reason(td.path(), tau_proto::SessionStartReason::Resume)
            .expect("cold restore");
    let cid = restored
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source_agent_id.as_str())
        .cloned()
        .expect("restored route");
    assert!(matches!(
        restored.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .activation_dispatch,
        crate::agent::ActivationDispatchState::AwaitingCheckpoint { .. }
            | crate::agent::ActivationDispatchState::DispatchUncertain { .. }
    ));
    assert!(matches!(
        restored.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        crate::agent::OutputLengthContinuationState::Active(_)
    ));
    restored.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: restored.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source_agent_id.clone()),
            agent_prompt_id: Some(descendant_prompt_id.clone()),
        },
    );
    let records = restored
        .session_runtime
        .agent_store
        .agent_events(source_agent_id.as_str())
        .expect("restored records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(started)
                    if started.agent_prompt_id == descendant_prompt_id
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == descendant_prompt_id
                        && matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                outcome:
                                    tau_proto::OutputLengthContinuationOutcome::Cancelled,
                                ..
                            }
                        )
            ))
            .count(),
        1
    );
    restored.shutdown().expect("shutdown");
}

/// Deferred wait-claim cleanup is runtime-only: the durable compaction intent
/// remains authoritative and no disconnected-client response is required.
#[test]
fn deferred_compaction_wait_claim_cleanup_is_silent_and_not_logged() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("harness");
    let (_requesting_ui_id, mut requesting_ui) = connect_socket_ui(&mut h);
    let (_observer_id, mut observer) = connect_socket_ui(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let wait_call_id = ToolCallId::from("wait-call");
    h.prompt_coordination
        .compaction_runtime
        .pending_ui_after_wait
        .insert(
            cid.clone(),
            crate::harness::PendingUiCompactionAfterWait {
                session_generation: h.session_runtime.current_session_generation,
                agent_id: agent_id.clone(),
                wait_call_id: wait_call_id.clone(),
            },
        );
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert(wait_call_id.clone(), cid.clone());
    let baseline_seq = h.runtime_io.event_log.next_seq();

    h.rollback_failed_wait_compaction_terminal(&Event::ToolCancelled(tau_proto::ToolCancelled {
        presentation: Default::default(),
        call_id: wait_call_id,
        tool_name: tau_proto::ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));

    assert_no_message(&mut requesting_ui);
    assert_no_message(&mut observer);
    assert_eq!(h.runtime_io.event_log.next_seq(), baseline_seq);

    h.prompt_coordination
        .compaction_runtime
        .pending_ui_after_wait
        .insert(
            cid.clone(),
            crate::harness::PendingUiCompactionAfterWait {
                session_generation: h.session_runtime.current_session_generation,
                agent_id,
                wait_call_id: ToolCallId::from("wait-unload"),
            },
        );
    let logged_before_unload = event_log_events(&h).len();
    h.remove_agent_after_prompt_closure(&cid);
    assert_no_message(&mut requesting_ui);
    assert_no_message(&mut observer);
    assert!(
        event_log_events(&h)[logged_before_unload..]
            .iter()
            .all(|event| !matches!(event, Event::HarnessNotice(_)))
    );
}

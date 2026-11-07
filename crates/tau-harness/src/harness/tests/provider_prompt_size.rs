//! Exact provider-frame admission and nonfatal local rejection.

use tau_config::settings::ProviderCacheMaxIdle;

use super::*;

/// Parks the real dispatch after its durable start, before provider delivery.
fn park_prompt(h: &mut Harness, cid: &AgentId) -> AgentPromptCreated {
    let interceptor = connect_test_tool(h, "size-interceptor");
    h.handle_extension_event(
        "size-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    let agent_id = durable_agent_id_for_conversation(h, cid);
    h.submit_prompt_to_agent(
        h.session_runtime.current_session_id.clone(),
        agent_id.as_str(),
        PendingPrompt::user("retained user checkpoint".to_owned()),
    )
    .expect("submit");
    let (Event::AgentPromptCreated(prompt), _) = intercepted_payload(&interceptor) else {
        panic!("prompt parked");
    };
    prompt
}

/// Installs the final materialized payload at the delivery cut. Prompt-created
/// facts are immutable to real interceptors, so fixture construction happens
/// directly in the retained owner, not through a forbidden peer replacement.
fn release_prompt(h: &mut Harness, prompt: AgentPromptCreated) {
    h.runtime_io
        .publication
        .pending_intercept
        .as_mut()
        .expect("parked")
        .event = Event::AgentPromptCreated(prompt);
    h.handle_extension_event(
        "size-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release");
}

/// Drives actual transport ingress so successful later work proves that no
/// partial oversized frame poisoned the shared provider stream.
fn await_success(h: &mut Harness, agent_id: &AgentId) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut served_clients = 0;
    loop {
        if event_log_events(h).iter().any(|event| {
            matches!(event,
                Event::ProviderResponseFinished(response)
                    if &response.agent_id == agent_id && response.error.is_none()
                        && response.stop_reason == tau_proto::ProviderStopReason::EndTurn
            )
        }) {
            return;
        }
        let event = h
            .runtime_io
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("provider must survive and answer later prompt");
        let event = h.expand_component_ingress_wake(event);
        h.handle_runtime_event(event, &mut served_clients)
            .expect("live provider");
    }
}

/// Starts independent work on the same provider before rejecting a sibling.
fn start_other_agent(h: &mut Harness) -> AgentPromptCreated {
    let cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let agent_id = durable_agent_id_for_conversation(h, &cid);
    finish_test_agent_context_wait(h, &agent_id);
    h.submit_prompt_to_agent(
        h.session_runtime.current_session_id.clone(),
        agent_id.as_str(),
        PendingPrompt::user("independent work".to_owned()),
    )
    .expect("other agent");
    read_nth_prompt_created(h, 0)
}

/// Envelope bytes, CBOR string headers and exact inclusive limit all count,
/// rather than just the prompt's visible text or image payload.
#[test]
fn provider_prompt_encoded_boundary_includes_complete_envelope() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let mut prompt = park_prompt(&mut h, &cid);
    prompt.system_prompt = "x".repeat(tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize);
    let frame = |prompt| {
        HarnessOutputMessage::Deliver(EventDelivery::live(
            tau_proto::UnixMicros::now(),
            Event::AgentPromptCreated(prompt),
        ))
    };
    let oversized = frame(prompt.clone());
    let size = tau_proto::encode_message_to_vec(&oversized)
        .expect("encode")
        .len();
    prompt.system_prompt.truncate(
        prompt.system_prompt.len() - (size - tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize),
    );
    let exact = frame(prompt.clone());
    assert_eq!(
        tau_proto::encode_message_to_vec(&exact)
            .expect("encode")
            .len() as u64,
        tau_proto::MAX_SEMANTIC_MESSAGE_BYTES
    );
    assert!(super::super::protocol_message_fits(&exact));
    let encoded = tau_proto::encode_message_to_vec(&exact).expect("encode");
    assert!(
        HarnessOutputReader::new(std::io::Cursor::new(encoded))
            .read_message()
            .expect("reader accepts exact bound")
            .is_some()
    );
    prompt.system_prompt.push('x');
    assert!(
        tau_proto::encode_message_to_vec(&prompt)
            .expect("prompt payload")
            .len()
            < tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize,
        "payload-only admission would incorrectly accept this frame"
    );
    assert!(!super::super::protocol_message_fits(&frame(prompt.clone())));
    prompt.system_prompt.pop();
    prompt.system_prompt.pop();
    assert!(super::super::protocol_message_fits(&frame(prompt)));
}

/// Both accumulated typed images and non-image prompt data must fail only their
/// exact dispatch, without retrying, charging usage, or losing the transcript.
#[test]
fn oversized_provider_prompts_settle_without_killing_shared_transport() {
    for images in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path()).expect("harness");
        // Establish the default user agent before creating the independent one.
        let cid = ensure_test_user_agent(&mut h);
        let other = start_other_agent(&mut h);
        let mut prompt = park_prompt(&mut h, &cid);
        assert_ne!(other.agent_id, prompt.agent_id);
        let prompt_id = prompt.agent_prompt_id.clone();
        let agent_id = prompt.agent_id.clone();
        let cid = h.agent_id_for_prompt(&prompt_id).expect("owner");
        if images {
            prompt
                .context
                .blocks
                .push(tau_proto::ContextBlock::ToolResults(
                    tau_proto::ToolResultsBlock {
                        items: (0..53)
                            .map(|index| ToolResultItem {
                                call_id: format!("image-{index}").into(),
                                tool_type: tau_proto::ToolType::Function,
                                status: ToolResultStatus::Success,
                                output: tau_proto::ToolResponse::from_cbor(&CborValue::Text(
                                    "image".to_owned(),
                                )),
                                presentation: Default::default(),
                                provider_content: vec![tau_proto::ToolResultContentPart::Image(
                                    tau_proto::ImageContent {
                                        media_type: tau_proto::ImageMediaType::Png,
                                        data: Arc::from(vec![0_u8; 338_013]),
                                        width: 1920,
                                        height: 1120,
                                        detail: tau_proto::ImageDetail::High,
                                    },
                                )],
                            })
                            .collect(),
                    },
                ));
        } else {
            prompt.system_prompt = "x".repeat(tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize);
        }
        release_prompt(&mut h, prompt);
        let events = event_log_events(&h);
        let terminals: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == prompt_id =>
                {
                    Some(response)
                }
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 1);
        let failure = terminals[0];
        assert!(
            failure
                .error
                .as_ref()
                .expect("diagnostic")
                .contains("no model request was made")
        );
        assert_eq!(
            failure.failure_kind,
            Some(tau_proto::ProviderFailureKind::Unknown)
        );
        assert!(failure.usage.is_none());
        assert!(failure.output_items.is_empty());
        assert!(failure.provider_response_id.is_none());
        assert!(events.iter().all(|event| !matches!(event,
            Event::ProviderPromptSubmitted(submitted) if submitted.agent_prompt_id == prompt_id)));
        let runtime = &h.agent_runtime.agent_registry.agents[&cid];
        assert!(matches!(runtime.turn.turn_state, AgentTurnState::Idle));
        assert!(runtime.dispatch.in_flight_prompt.is_none());
        assert!(runtime.turn.outer_turn.owned_id().is_none());
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_dispatches
                .is_empty()
        );
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty()
        );
        assert!(!h.provider_runtime.pending_prompts.contains_key(&prompt_id));
        assert!(
            !h.prompt_coordination
                .prompt_runtime
                .agents
                .contains_key(&prompt_id)
        );
        assert!(h.ui_runtime.pending_retry_prompts.is_empty());
        assert!(
            h.provider_runtime
                .pending_prompts
                .contains_key(&other.agent_prompt_id)
        );
        let stored = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("retained journal");
        assert!(stored.iter().any(|record| matches!(&record.event,
            Event::AgentPromptSubmitted(submitted) if submitted.text == "retained user checkpoint")));
        assert_eq!(
            stored
                .iter()
                .filter(|record| matches!(&record.event,
            Event::ProviderResponseFinished(response) if response.agent_prompt_id == prompt_id))
                .count(),
            1
        );
        // Stop intercepting; the next ordinary prompt must cross the same live
        // writer and reader successfully. The oversized projection never alters
        // canonical history.
        h.runtime_io
            .publication
            .interceptors
            .replace_for_connection(
                &crate::test_connection_id("size-interceptor"),
                crate::test_extension_name("size-interceptor"),
                vec![],
                InterceptionPriority::new(0),
            );
        h.submit_prompt_to_agent(
            h.session_runtime.current_session_id.clone(),
            agent_id.as_str(),
            PendingPrompt::user("normal follow-up".to_owned()),
        )
        .expect("next prompt");
        let next = read_nth_prompt_created(&h, 2);
        await_success(&mut h, &other.agent_id);
        await_success(&mut h, &next.agent_id);
        assert!(h.runtime_io.publication.pending_error.is_none());
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event,
            Event::AgentPromptStarted(started) if started.agent_prompt_id == prompt_id))
                .count(),
            1
        );
        h.shutdown().expect("shutdown");
        drop(h);
        wait_for_session_unlock(td.path(), "s1");
        let mut resumed =
            echo_harness_with_start_reason("s1", td.path(), tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let observer = connect_test_client(
            &mut resumed,
            "late-size-observer",
            tau_proto::ClientKind::Ui,
        );
        resumed
            .handle_client_event(
                "late-size-observer",
                TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
                    historical_selectors: vec![EventSelector::Exact(
                        tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                    )],
                    live_selectors: vec![],
                })),
            )
            .expect("subscribe");
        drive_harness_until_history_complete(&mut resumed);
        assert_eq!(observer.lock().expect("observer").iter().filter(|routed|
            matches!(&routed.frame, HarnessOutputMessage::Deliver(delivery)
                if delivery.replay && matches!(delivery.event.as_ref(),
                    Event::ProviderResponseFinished(response) if response.agent_prompt_id == prompt_id
                        && response.error.as_ref().is_some_and(|error| error.contains("no model request was made"))))
        ).count(), 1, "cold replay preserves the exact local terminal");
        assert!(
            event_log_events(&resumed)
                .iter()
                .all(|event| !matches!(event, Event::AgentPromptCreated(_))),
            "resume cannot resend rejected work"
        );
    }
}

/// Cache refresh has its own envelope; even a previously admitted prefix must
/// be checked again, and skipped refresh ownership must not leave a retry loop.
#[test]
fn oversized_provider_cache_refresh_is_skipped_without_transport_delivery() {
    use crate::provider_cache_residency::{ProviderCacheResidency, tests as fixtures};
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    h.provider_runtime.cache_residency =
        ProviderCacheResidency::runtime(tau_config::settings::ProviderCacheRefresh {
            enabled: true,
            max_idle_seconds: ProviderCacheMaxIdle::new(300).expect("idle"),
        });
    let provider = connect_ready_configured_extension(
        &mut h,
        "provider",
        "provider",
        tau_proto::ClientKind::Provider,
    );
    let model = fixtures::model("provider");
    for (id, usage) in [
        ("size-write", fixtures::usage(0, 10)),
        ("size-read", fixtures::usage(10, 0)),
    ] {
        let mut prompt = fixtures::prompt("provider", id);
        prompt.system_prompt = "x".repeat(tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize);
        h.provider_runtime.cache_residency.track_prompt(
            crate::test_connection_id("provider"),
            &prompt,
            Some(&model),
        );
        h.provider_runtime.cache_residency.finish_prompt(
            &prompt.agent_prompt_id,
            true,
            Some(&usage),
        );
    }
    h.extend_cache_refresh_tool_window([ToolCallId::from("size-cohort")]);
    h.provider_runtime
        .cache_residency
        .force_scheduled_due_for_test();
    provider.lock().expect("provider").clear();
    h.process_cache_refresh_deadline();
    assert!(provider.lock().expect("provider").is_empty());
    assert!(h.provider_runtime.cache_residency.next_deadline().is_none());
    h.process_cache_refresh_deadline();
    assert!(
        provider.lock().expect("provider").is_empty(),
        "no automatic retry"
    );
}

/// A late cancel after local dispatch disposal is rejected as already idle;
/// it must not replace the retained error or create a cancel waiter or resend.
#[test]
fn oversized_provider_prompt_late_cancel_leaves_local_terminal_intact() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let mut prompt = park_prompt(&mut h, &cid);
    let prompt_id = prompt.agent_prompt_id.clone();
    let agent_id = prompt.agent_id.clone();
    prompt.system_prompt = "x".repeat(tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize);
    h.runtime_io
        .publication
        .interceptors
        .replace_for_connection(
            &crate::test_connection_id("size-interceptor"),
            crate::test_extension_name("size-interceptor"),
            vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            InterceptionPriority::new(0),
        );
    release_prompt(&mut h, prompt);
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::Idle
    ));
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(agent_id),
            agent_prompt_id: Some(prompt_id.clone()),
        },
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_none(),
        "the already-disposed request does not accept late cancellation"
    );
    h.handle_extension_event(
        "size-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release terminal");
    let runtime = &h.agent_runtime.agent_registry.agents[&cid];
    assert!(matches!(runtime.turn.turn_state, AgentTurnState::Idle));
    assert!(runtime.dispatch.in_flight_prompt.is_none());
    assert!(runtime.dispatch.pending_cancel.is_none());
    assert!(runtime.turn.outer_turn.owned_id().is_none());
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event,
        Event::ProviderResponseFinished(response) if response.agent_prompt_id == prompt_id))
            .count(),
        1
    );
}

/// Full persistence admission may delay the local terminal, but must retain
/// that exact error rather than resend the oversized request or strand its
/// owner.
#[test]
fn oversized_provider_prompt_terminal_survives_admission_backpressure() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    let cid = ensure_test_user_agent(&mut h);
    let mut prompt = park_prompt(&mut h, &cid);
    let prompt_id = prompt.agent_prompt_id.clone();
    prompt.system_prompt = "x".repeat(tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize);
    reject_next_semantic_admission(&h);
    release_prompt(&mut h, prompt);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid),
        "rejected local terminal must retain exact publication ownership"
    );
    h.retry_pending_agent_publish_completion(&cid);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event,
        Event::ProviderResponseFinished(response) if response.agent_prompt_id == prompt_id))
            .count(),
        1
    );
}

/// Tool-result continuations use the same admission seam as initial requests;
/// their local error closes the outer turn once, including after a harmless
/// late cancel of the already-disposed request.
#[test]
fn oversized_provider_prompt_after_tool_result_settles_once() {
    for cancel in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path()).expect("harness");
        let cid = ensure_test_user_agent(&mut h);
        h.submit_user_prompt(
            h.session_runtime.current_session_id.clone(),
            "echo checkpoint".to_owned(),
        )
        .expect("submit");
        let source = read_nth_prompt_created(&h, 0);
        let interceptor = connect_test_tool(&mut h, "size-interceptor");
        h.handle_extension_event(
            "size-interceptor",
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::AGENT_PROMPT_CREATED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("intercept continuation");
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut served_clients = 0;
        while h.runtime_io.publication.pending_intercept.is_none() {
            let event = h
                .runtime_io
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("tool result");
            let event = h.expand_component_ingress_wake(event);
            h.handle_runtime_event(event, &mut served_clients)
                .expect("live provider");
        }
        let (Event::AgentPromptCreated(mut prompt), _) = intercepted_payload(&interceptor) else {
            panic!("tool continuation");
        };
        let prompt_id = prompt.agent_prompt_id.clone();
        assert_ne!(prompt_id, source.agent_prompt_id);
        assert!(
            prompt
                .context
                .flatten_iter()
                .any(|item| matches!(item, ContextItem::ToolResult(_)))
        );
        prompt.system_prompt = "x".repeat(tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize);
        h.runtime_io
            .publication
            .interceptors
            .replace_for_connection(
                &crate::test_connection_id("size-interceptor"),
                crate::test_extension_name("size-interceptor"),
                if cancel {
                    vec![EventSelector::Exact(
                        tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                    )]
                } else {
                    vec![]
                },
                InterceptionPriority::new(0),
            );
        release_prompt(&mut h, prompt);
        if cancel {
            assert!(matches!(
                h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
                AgentTurnState::Idle
            ));
            h.handle_cancel_prompt(
                crate::harness::harness_connection_id(),
                &tau_proto::UiCancelPrompt {
                    session_id: h.session_runtime.current_session_id.clone(),
                    target_agent_id: Some(source.agent_id.clone()),
                    agent_prompt_id: Some(prompt_id.clone()),
                },
            );
            assert!(
                h.agent_runtime.agent_registry.agents[&cid]
                    .dispatch
                    .pending_cancel
                    .is_none(),
                "late cancellation cannot replace the local terminal"
            );
            h.handle_extension_event(
                "size-interceptor",
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("release terminal");
        }
        let runtime = &h.agent_runtime.agent_registry.agents[&cid];
        assert!(matches!(runtime.turn.turn_state, AgentTurnState::Idle));
        assert!(runtime.dispatch.pending_cancel.is_none());
        assert!(runtime.dispatch.in_flight_prompt.is_none());
        assert!(runtime.turn.outer_turn.owned_id().is_none());
        assert!(
            h.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .is_empty()
        );
        let events = event_log_events(&h);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event,
            Event::ProviderResponseFinished(response) if response.agent_prompt_id == prompt_id))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event,
            Event::AgentOuterTurnFinished(finished) if finished.agent_id == source.agent_id))
                .count(),
            1
        );
        assert!(events.iter().all(|event| !matches!(event,
            Event::ProviderPromptSubmitted(submitted) if submitted.agent_prompt_id == prompt_id)));
    }
}

/// Standalone compaction must reject locally too, retain its route-failure fact
/// through backpressure, and never charge or send a compact request.
#[test]
fn oversized_provider_compaction_retains_one_local_failure() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("harness");
    super::dispatch::enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model")
        .supports_standalone_compaction = true;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let cut = h
        .selected_head_for_agent(&cid)
        .unwrap_or(tau_proto::AgentHead::Root);
    let interceptor = connect_test_tool(&mut h, "size-interceptor");
    h.handle_extension_event(
        "size-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept");
    let transaction_id = tau_proto::CompactionTransactionId::parse("ct-size").expect("transaction");
    let prompt_id = tau_proto::AgentPromptId::parse("ap-size").expect("prompt");
    let usage_before = h.session_runtime.current_session_state.token_usage.clone();
    h.publish_for_agent(
        &cid,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id,
            transaction_id: transaction_id.clone(),
            compact_prompt_id: prompt_id.clone(),
            cut,
            resume_through: None,
            model: "test/model".into(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        }),
    );
    let (Event::AgentPromptCreated(mut prompt), _) = intercepted_payload(&interceptor) else {
        panic!("compact prompt");
    };
    prompt.system_prompt = "x".repeat(tau_proto::MAX_SEMANTIC_MESSAGE_BYTES as usize);
    reject_next_semantic_admission(&h);
    release_prompt(&mut h, prompt);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
    );
    h.retry_pending_agent_publish_completion(&cid);
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .is_empty()
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .agents
            .contains_key(&prompt_id)
    );
    assert!(!h.provider_runtime.pending_prompts.contains_key(&prompt_id));
    assert_eq!(
        h.session_runtime.current_session_state.token_usage,
        usage_before
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event,
        Event::AgentStandaloneCompactionFailed(failed) if failed.transaction_id == transaction_id
            && failed.reason == tau_proto::StandaloneCompactionFailureReason::RouteFailed))
            .count(),
        1
    );
    assert!(event_log_events(&h).iter().all(|event| !matches!(event,
        Event::ProviderPromptSubmitted(submitted) if submitted.agent_prompt_id == prompt_id)));
}

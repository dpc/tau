//! Tests for agent lifecycle behavior.

use super::super::dispatch::{
    context_overflow_response, enable_remote_compaction_for_test_model, provider_text_response,
};
use super::*;

/// Ensures targetless user shell output is routed to the default user agent
/// instead of panicking when the shell extension omits a target agent id.
#[test]
fn targetless_shell_output_injects_into_default_agent() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.inject_user_shell_output(&tau_proto::ShellCommandFinished {
        command_id: tau_proto::ShellCommandId::parse("shell-1")
            .expect("test identifier must satisfy its grammar"),
        session_id: test_session_id("s1"),
        command: "printf hello".to_owned(),
        include_in_context: true,
        target_agent_id: None,
        output: "hello".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    });

    let injected = loaded_agent_events(&h, "s1")
        .into_iter()
        .find_map(|event| match event {
            Event::AgentUserMessageInjected(injected) if injected.text.contains("printf hello") => {
                Some(injected)
            }
            _ => None,
        })
        .expect("shell output injected into agent transcript");
    assert_eq!(injected.agent_id, agent_id);
    assert!(injected.text.contains("<user_shell"));
    assert!(injected.text.contains("hello"));
}

/// Late shell completion must not append new durable work after terminal
/// teardown has begun, whether explicitly or implicitly targeted.
#[test]
fn terminating_agent_rejects_late_shell_output() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .terminating = true;

    for target_agent_id in [Some(agent_id.clone()), None] {
        h.inject_user_shell_output(&tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::parse("late-shell")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "printf late".to_owned(),
            include_in_context: true,
            target_agent_id,
            output: "late".to_owned(),
            exit_code: Some(0),
            cancelled: false,
        });
    }
    assert!(
        !loaded_agent_events(&h, "s1")
            .into_iter()
            .any(|event| matches!(
                event,
                Event::AgentUserMessageInjected(injected) if injected.text.contains("printf late")
            ))
    );
}

#[test]
fn agents_context_is_injected_when_agent_is_created() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_connection_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Eager init at construction may have already appended a real
    // AGENTS.md (ext-shell walks the test cwd). Clear so we assert
    // only on the test-injected pair below.
    h.prompt_coordination.context_discovery.agents_files.clear();
    h.prompt_coordination
        .context_discovery
        .agents_files
        .push(DiscoveredAgentsFile {
            source_id: crate::test_connection_id(tools_connection_id.clone()),
            file_path: PathBuf::from("/repo/AGENTS.md"),
            content: "# Root\n- root rule\n".to_owned(),
        });
    h.prompt_coordination
        .context_discovery
        .agents_files
        .push(DiscoveredAgentsFile {
            source_id: crate::test_connection_id(tools_connection_id.clone()),
            file_path: PathBuf::from("/repo/pkg/AGENTS.md"),
            content: "# Package\n- package rule\n".to_owned(),
        });
    let _cid = ensure_test_user_agent(&mut h);

    let events = loaded_agent_events(&h, "s1");
    let bootstrap = events
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::AgentInitializationContextSet(context)
                if context
                    .agents_message
                    .as_deref()
                    .is_some_and(|message| message.contains("/repo/pkg")) =>
            {
                context.agents_message.as_deref()
            }
            _ => None,
        })
        .expect("expected AGENTS.md initialization context");
    assert!(bootstrap.contains("# AGENTS.md instructions"));
    assert!(bootstrap.contains("<AGENTS_FILE path=\"/repo/pkg/AGENTS.md\">"));
    assert!(bootstrap.contains("<AGENTS_FILE path=\"/repo/AGENTS.md\">"));
    assert!(bootstrap.contains("</AGENTS_FILE>"));
    let root_pos = bootstrap.find("root rule").expect("root rule");
    let pkg_pos = bootstrap.find("package rule").expect("package rule");
    assert!(
        root_pos < pkg_pos,
        "broader file should appear before nested one"
    );

    h.shutdown().expect("shutdown");
}

/// A rejected planned-steer append must retain cancellation until the repaired
/// steer commits an owner and one Cancelled terminal.
#[test]
fn output_length_steer_append_failure_retains_pending_cancellation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "finish the task".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let durable_agent_id = source.agent_id.to_string();
    let _interceptor = connect_test_tool(&mut h, "length-steer-interceptor");
    h.handle_extension_event(
        "length-steer-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_PROMPT_STEERED),
                EventSelector::Exact(tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register steer interceptor");

    h.handle_provider_response_finished(reasoning_only_length_response(&source, 17))
        .expect("length terminal accepted");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "planned steer is intercepted"
    );
    assert!(h.agent_runtime.agent_registry.agents.values().any(|agent| {
        matches!(
            agent.turn.output_length_continuation,
            crate::agent::OutputLengthContinuationState::Planned(_)
        )
    }));
    h.config.selected_model = Some("changed/model".into());
    let (requesting_ui_id, mut requesting_ui) = connect_socket_ui(&mut h);
    let (_observer_id, mut observer) = connect_socket_ui(&mut h);
    let cancel_baseline = h.runtime_io.event_log.next_seq();
    h.handle_cancel_prompt(
        &requesting_ui_id,
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    let notice = read_notice(&mut requesting_ui);
    assert_eq!(notice.purpose, tau_proto::NoticePurpose::Response);
    assert_eq!(notice.message, "cancelling current prompt");
    assert_no_message(&mut observer);
    assert_eq!(h.runtime_io.event_log.next_seq(), cancel_baseline);
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .any(|agent| agent.dispatch.pending_cancel.is_some()),
        "states: {:?}",
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .map(|agent| (
                &agent.turn.output_length_continuation,
                &agent.turn.turn_state
            ))
            .collect::<Vec<_>>()
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "length-steer-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release planned steer into append failure");
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .values()
            .any(|completion| {
                matches!(
                    completion,
                    crate::harness::interception::AgentPublishCompletion::OutputLengthSteer { .. }
                )
            })
    );
    h.handle_extension_event(
        "length-steer-interceptor",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            historical_selectors: Vec::new(),
            live_selectors: Vec::new(),
        })),
    )
    .expect("retry retained planned steer");
    assert!(matches!(
        h.runtime_io.publication.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::AgentInferenceDispatchStarted(owner))
            if owner.output_length_continuation.is_some()
    ));
    h.handle_extension_event(
        "length-steer-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit repaired successor owner");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(&durable_agent_id)
        .expect("durable agent events");
    let (outer_turn_id, successor_agent_prompt_id) = records
        .iter()
        .find_map(|record| {
            let Event::ProviderResponseFinished(response) = &record.event else {
                return None;
            };
            let tau_proto::OutputLengthDisposition::ContinuationPlanned {
                outer_turn_id,
                successor_agent_prompt_id,
                ordinal: 1,
                limit: 1,
            } = &response.output_length_disposition
            else {
                return None;
            };
            assert_eq!(
                response
                    .usage
                    .as_ref()
                    .map(|usage| usage.response_received_tokens),
                Some(17)
            );
            Some((outer_turn_id.clone(), successor_agent_prompt_id.clone()))
        })
        .expect("durable continuation plan");
    let plan_index = records
        .iter()
        .position(|record| {
            matches!(
                record.event,
                Event::ProviderResponseFinished(ref response)
                    if matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                    )
            )
        })
        .expect("plan position");
    let steer_index = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::AgentPromptSteered(steered)
                    if steered.internal_kind
                        == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
                        && steered.text == tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION
                        && steered.message_class == tau_proto::PromptMessageClass::Internal
                        && steered.submission_source
                            == tau_proto::PromptSubmissionSource::HarnessInternal
                        && steered.trusted_internal_spans
                            == vec![tau_proto::TrustedInternalSpan {
                                start: 0,
                                end: u32::try_from(
                                    tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION.len()
                                )
                                .expect("bounded instruction"),
                            }]
            )
        })
        .expect("exact trusted steer");
    let owner_index = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(started)
                     if started.agent_prompt_id == successor_agent_prompt_id
                        && started.model.as_ref() == Some(&source.model)
                        && started.output_length_continuation.as_ref().is_some_and(|owner| {
                            owner.outer_turn_id == outer_turn_id
                                && owner.source_agent_prompt_id == source.agent_prompt_id
                                && owner.ordinal == 1
                         })
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "successor owner; events: {:?}",
                records
                    .iter()
                    .map(|record| record.event.name())
                    .collect::<Vec<_>>()
            )
        });
    assert!(plan_index < steer_index);
    assert!(steer_index < owner_index);
    assert!(
        !records.iter().any(|record| matches!(
            &record.event,
            Event::AgentPromptStarted(started)
                if started.agent_prompt_id == successor_agent_prompt_id
        )),
        "pending cancellation terminalizes after owner commit without dispatching"
    );
    let records = h
        .session_runtime
        .agent_store
        .agent_events(&durable_agent_id)
        .expect("durable agent events after cancellation");
    assert!(
        records.iter().any(|record| matches!(
        &record.event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == successor_agent_prompt_id
                && matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationTerminal {
                        outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                        outer_turn_finish_owed: true,
                        ..
                    }
                )
        )),
        "events after cancellation: {:?}",
        records
            .iter()
            .map(|record| record.event.name())
            .collect::<Vec<_>>()
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .all(|agent| agent.dispatch.pending_cancel.is_none()),
        "committed cancellation terminal must release runtime cancellation"
    );
    h.shutdown().expect("shutdown");
}

/// Branch movement after a committed plan retries an append-rejected reserved
/// owner, arbitrates cancellation on the dormant lineage, isolates the selected
/// sibling, retires both activation forms, and leaves later work live without
/// materializing or dispatching the reserved successor.
#[test]
fn output_length_branch_move_finishes_dormant_lineage_without_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(
        test_session_id("s1"),
        "branch before continuation".to_owned(),
    )
    .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    let _interceptor = connect_test_tool(&mut h, "dormant-length-interceptor");
    let _owner_interceptor = connect_test_tool(&mut h, "dormant-owner-interceptor");
    h.handle_extension_event(
        "dormant-length-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STEERED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register steer interceptor");
    h.handle_extension_event(
        "dormant-owner-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register owner interceptor");
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 7))
        .expect("planned response");
    assert!(h.runtime_io.publication.pending_intercept.is_some());

    let sibling = h
        .append_direct_agent_semantic_event(
            source.agent_id.as_str(),
            tau_core::AgentEventParent::Root,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: source.agent_id.clone(),
                text: "selected sibling".to_owned(),
                inference_activation: false,
                message_class: Default::default(),
            }),
        )
        .expect("append sibling")
        .selected_head_id
        .expect("sibling node");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("source agent")
        .identity
        .head = Some(sibling);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: tau_proto::AgentHead::Node(sibling),
        }),
    );
    h.handle_extension_event(
        "dormant-length-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release old selected-branch steer");
    while matches!(
        h.runtime_io.publication.pending_intercept.as_ref().map(|pending| &pending.event),
        Some(Event::AgentPromptSteered(steered))
            if steered.internal_kind
                == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
    ) {
        h.handle_extension_event(
            "dormant-length-interceptor",
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("release exact dormant steer");
    }
    assert!(
        matches!(
            h.runtime_io.publication.pending_intercept.as_ref().map(|pending| &pending.event),
            Some(Event::AgentInferenceDispatchStarted(owner))
                if owner.output_length_continuation.is_some()
        ),
        "pending: {:?}",
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| pending.event.name())
    );
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "dormant-owner-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release dormant owner into append rejection");
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: tau_proto::AgentHead::Node(sibling),
        }),
    );
    h.retry_pending_agent_publications();
    h.handle_disconnect(&crate::test_connection_id("dormant-length-interceptor"));
    h.handle_disconnect(&crate::test_connection_id("dormant-owner-interceptor"));

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable records");
    let successor = records
        .iter()
        .find_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => {
                let tau_proto::OutputLengthDisposition::ContinuationPlanned {
                    successor_agent_prompt_id,
                    ..
                } = &response.output_length_disposition
                else {
                    return None;
                };
                Some(successor_agent_prompt_id.clone())
            }
            _ => None,
        })
        .expect("reserved successor");
    let steer_index = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::AgentPromptSteered(steered)
                    if steered.internal_kind
                        == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
            )
        })
        .expect("dormant steer");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptSteered(steered)
                    if steered.internal_kind
                        == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
            ))
            .count(),
        1
    );
    let owner_index = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(owner)
                    if owner.agent_prompt_id == successor
                        && owner.output_length_continuation.is_some()
            )
        })
        .expect("dormant owner");
    let dormant_steer = records
        .iter()
        .find_map(|record| match &record.event {
            Event::AgentInferenceDispatchStarted(owner)
                if owner.agent_prompt_id == successor
                    && owner.output_length_continuation.is_some() =>
            {
                Some(owner.through)
            }
            _ => None,
        })
        .expect("dormant steer watermark");
    let failed_index = records
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == successor
                        && response.output_items.is_empty()
                        && matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                                outer_turn_finish_owed: true,
                                ..
                            }
                        )
            )
        })
        .expect("dormant failure");
    let finish_index = records
        .iter()
        .position(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
        .expect("owed finish");
    assert!(steer_index < owner_index);
    assert!(owner_index < failed_index);
    assert!(failed_index < finish_index);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentInferenceDispatchStarted(owner)
                    if owner.agent_prompt_id == successor
                        && owner.output_length_continuation.is_some()
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
                    if response.agent_prompt_id == successor
            ))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
            .count(),
        1
    );
    assert!(!records.iter().any(|record| matches!(
        &record.event,
        Event::AgentPromptStarted(started) if started.agent_prompt_id == successor
    )));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == successor))
            .count(),
        0
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        Some(sibling)
    );
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .output_length_continuation,
        crate::agent::OutputLengthContinuationState::None
    ));
    assert!(
        matches!(
            h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
            AgentTurnState::Idle
        ),
        "state: {:?}",
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_none()
    );
    assert!(
        !h.runtime_io
            .publication
            .idle_dispatches
            .iter()
            .any(|dispatch| dispatch.cid == cid)
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(&cid)
    );
    let selected_context = crate::prompt::assemble_prompt_context_from(
        h.session_runtime
            .agent_store
            .agent(source.agent_id.as_str())
            .expect("durable tree"),
        Some(sibling),
    );
    let selected_context = format!("{:?}", selected_context.context);
    assert!(!selected_context.contains("retained reasoning"));
    assert!(!selected_context.contains(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION));
    assert!(!selected_context.contains("branch was deselected"));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::HarnessNotice(notice)
                    if notice.message.contains("dormant original branch")
            ))
            .count(),
        1
    );
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: dormant_steer,
        }),
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        1,
        "reselecting the repaired branch cannot revive its retired activation"
    );
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: tau_proto::AgentHead::Node(sibling),
        }),
    );
    h.submit_user_prompt(test_session_id("s1"), "later work".to_owned())
        .expect("submit later work");
    let later = read_nth_prompt_created(&h, 1);
    assert_ne!(later.agent_prompt_id, successor);
    h.shutdown().expect("shutdown");
}

/// Cold startup completes the next exact dormant repair after steer, owner, and
/// terminal crash cuts without materializing provider work.
#[test]
fn output_length_dormant_repair_resumes_each_cold_cut() {
    for (label, parked_event) in [
        (
            "steer",
            tau_proto::EventName::AGENT_INFERENCE_DISPATCH_STARTED,
        ),
        ("owner", tau_proto::EventName::PROVIDER_RESPONSE_FINISHED),
        ("terminal", tau_proto::EventName::AGENT_OUTER_TURN_FINISHED),
    ] {
        let td = TempDir::new().expect("tempdir");
        let source_agent_id;
        let successor;
        let selected_sibling;
        let cut_records;
        {
            let mut h = echo_harness(td.path()).expect("start");
            h.submit_user_prompt(test_session_id("s1"), format!("cold cut {label}"))
                .expect("submit");
            let source = read_nth_prompt_created(&h, 0);
            source_agent_id = source.agent_id.clone();
            let cid = h
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(source.agent_id.as_str())
                .cloned()
                .expect("source route");
            let steer_connection = format!("cold-{label}-steer");
            let target_connection = format!("cold-{label}-target");
            let _steer = connect_test_tool(&mut h, &steer_connection);
            let _target = connect_test_tool(&mut h, &target_connection);
            h.handle_extension_event(
                &steer_connection,
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(
                        tau_proto::EventName::AGENT_PROMPT_STEERED,
                    )],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("register steer cut");
            h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
                .expect("source plan");
            successor = h
                .session_runtime
                .agent_store
                .agent_events(source.agent_id.as_str())
                .expect("source records")
                .iter()
                .find_map(|record| match &record.event {
                    Event::ProviderResponseFinished(response) => {
                        match &response.output_length_disposition {
                            tau_proto::OutputLengthDisposition::ContinuationPlanned {
                                successor_agent_prompt_id,
                                ..
                            } => Some(successor_agent_prompt_id.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .expect("reserved successor");
            h.handle_extension_event(
                &target_connection,
                TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                    selectors: vec![EventSelector::Exact(parked_event.clone())],
                    priority: InterceptionPriority::new(0),
                })),
            )
            .expect("register target cut");
            let sibling = h
                .append_direct_agent_semantic_event(
                    source.agent_id.as_str(),
                    tau_core::AgentEventParent::Root,
                    Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                        agent_id: source.agent_id.clone(),
                        text: "cold sibling".to_owned(),
                        inference_activation: false,
                        message_class: Default::default(),
                    }),
                )
                .expect("append sibling")
                .selected_head_id
                .expect("sibling");
            selected_sibling = sibling;
            h.agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("source agent")
                .identity
                .head = Some(sibling);
            h.publish_for_agent(
                &cid,
                Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
                    agent_id: source.agent_id.clone(),
                    head: tau_proto::AgentHead::Node(sibling),
                }),
            );
            h.handle_extension_event(
                &steer_connection,
                TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                    action: InterceptAction::Pass(None),
                })),
            )
            .expect("commit dormant steer");
            while matches!(
                h.runtime_io
                    .publication
                    .pending_intercept
                    .as_ref()
                    .map(|pending| &pending.event),
                Some(Event::AgentPromptSteered(_))
            ) {
                h.handle_extension_event(
                    &steer_connection,
                    TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                        action: InterceptAction::Pass(None),
                    })),
                )
                .expect("commit exact dormant steer");
            }
            assert_eq!(
                h.runtime_io
                    .publication
                    .pending_intercept
                    .as_ref()
                    .map(|pending| pending.event.name()),
                Some(parked_event),
                "{label} cut must park the next fact"
            );
            cut_records = h
                .session_runtime
                .agent_store
                .agent_events(source.agent_id.as_str())
                .expect("cold cut records")
                .to_vec();
            h.shutdown().expect("shutdown before journal cut");
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
            echo_harness_with_start_reason("s1", td.path(), tau_proto::SessionStartReason::Resume)
                .expect("cold restore");
        let records = restored
            .session_runtime
            .agent_store
            .agent_events(source_agent_id.as_str())
            .expect("restored records");
        let steer_count = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentPromptSteered(steered)
                        if steered.internal_kind
                            == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
                )
            })
            .count();
        let owner_count = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentInferenceDispatchStarted(started)
                        if started.agent_prompt_id == successor
                )
            })
            .count();
        let terminal_count = records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == successor
                            && matches!(
                                response.output_length_disposition,
                                tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                    outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                                    ..
                                }
                            )
                )
            })
            .count();
        let finish_count = records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
            .count();
        assert_eq!(
            (steer_count, owner_count, terminal_count, finish_count),
            (1, 1, 1, 1)
        );
        let cut_owner_count = cut_records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentInferenceDispatchStarted(started)
                        if started.agent_prompt_id == successor
                )
            })
            .count();
        let cut_terminal_count = cut_records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == successor
                )
            })
            .count();
        let cut_finish_count = cut_records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
            .count();
        assert_eq!(
            (cut_owner_count, cut_terminal_count, cut_finish_count),
            match label {
                "steer" => (0, 0, 0),
                "owner" => (1, 0, 0),
                "terminal" => (1, 1, 0),
                _ => unreachable!("known cold cut"),
            }
        );
        let restored_cid = restored
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(source_agent_id.as_str())
            .expect("restored route");
        assert_eq!(
            restored.agent_runtime.agent_registry.agents[restored_cid]
                .identity
                .head,
            Some(selected_sibling)
        );
        assert!(!records.iter().any(|record| matches!(
            &record.event,
            Event::AgentPromptStarted(started) if started.agent_prompt_id == successor
        )));
        assert_eq!(
            event_log_events(&restored)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::HarnessNotice(notice)
                        if notice.message.contains("dormant original branch")
                ))
                .count(),
            1,
            "{label} cut notices: {:?}",
            event_log_events(&restored)
                .iter()
                .filter_map(|event| match event {
                    Event::HarnessNotice(notice) => Some(notice.message.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        );
        assert!(!event_log_events(&restored).iter().any(|event| matches!(
            event,
            Event::AgentPromptCreated(prompt) if prompt.agent_prompt_id == successor
        )));
        restored.shutdown().expect("restored shutdown");
    }
}

/// Branch movement after successor prompt-start leaves the already-dispatched
/// owner as the sole terminal authority and never mints a synthetic failure.
#[test]
fn output_length_post_start_branch_move_waits_for_real_terminal() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "already dispatched".to_owned())
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
    let sibling = h
        .append_direct_agent_semantic_event(
            source.agent_id.as_str(),
            tau_core::AgentEventParent::Root,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: source.agent_id.clone(),
                text: "post-start sibling".to_owned(),
                inference_activation: false,
                message_class: Default::default(),
            }),
        )
        .expect("append sibling")
        .selected_head_id
        .expect("sibling node");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("source agent")
        .identity
        .head = Some(sibling);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: tau_proto::AgentHead::Node(sibling),
        }),
    );
    let sibling_usage_before = h.agent_runtime.agent_registry.agents[&cid]
        .execution
        .context_input_tokens;
    let _finish_interceptor = connect_test_tool(&mut h, "post-start-finish-interceptor");
    h.handle_extension_event(
        "post-start-finish-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_OUTER_TURN_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register finish cut");
    let mut terminal = provider_text_response(
        &successor.agent_prompt_id,
        successor.agent_id.clone(),
        "real terminal",
    );
    terminal.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 99,
        response_received_tokens: 7,
        ..Default::default()
    });
    h.handle_provider_response_finished(terminal)
        .expect("real successor terminal");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentOuterTurnFinished(_))
    ));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .execution
            .context_input_tokens,
        sibling_usage_before
    );

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == successor.agent_prompt_id
            ))
            .count(),
        1
    );
    assert!(!records.iter().any(|record| matches!(
        &record.event,
        Event::ProviderResponseFinished(response)
            if response.agent_prompt_id == successor.agent_prompt_id
                && matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationTerminal {
                        outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                        ..
                    }
                )
    )));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        Some(sibling)
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
    );
    h.shutdown().expect("shutdown at missing finish");
    drop(h);
    let mut restored =
        echo_harness_with_start_reason("s1", td.path(), tau_proto::SessionStartReason::Resume)
            .expect("cold finish repair");
    let restored_cid = restored
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("restored route");
    assert_eq!(
        restored.agent_runtime.agent_registry.agents[&restored_cid]
            .execution
            .context_input_tokens,
        sibling_usage_before
    );
    assert!(
        restored
            .session_runtime
            .agent_store
            .agent_events(source.agent_id.as_str())
            .expect("restored records")
            .iter()
            .any(|record| matches!(record.event, Event::AgentOuterTurnFinished(_)))
    );
    restored.shutdown().expect("restored shutdown");
}

/// Cancellation while the reserved context rejection is intercepted rewrites
/// that response into the sole canonical Cancelled continuation terminal. It
/// closes the outer turn without claiming or delivering compaction work.
#[test]
fn output_length_reactive_rejection_cancelled_before_commit_never_dispatches() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.submit_user_prompt(test_session_id("s1"), "cancel recovery".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
        .expect("source plan");
    let successor = read_nth_prompt_created(&h, 1);
    let _interceptor = connect_test_tool(&mut h, "reactive-rejection-interceptor");
    h.handle_extension_event(
        "reactive-rejection-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register rejection interceptor");
    h.handle_provider_response_finished(context_overflow_response(&successor))
        .expect("park rejection");
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: Some(successor.agent_prompt_id.clone()),
        },
    );
    h.handle_extension_event(
        "reactive-rejection-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit rejection after cancellation");
    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("records");
    assert!(!records.iter().any(|record| matches!(
        record.event,
        Event::AgentStandaloneCompactionStarted(_) | Event::AgentStandaloneCompactionFailed(_)
    )));
    let cancelled = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == successor.agent_prompt_id
                    && matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationTerminal {
                            outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                            outer_turn_finish_owed: true,
                            ..
                        }
                    ) =>
            {
                Some(response)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled.len(), 1);
    assert!(cancelled[0].output_items.is_empty());
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
                    automatic_compaction_decision: None,
                    disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                    ..
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .all(|agent| agent.dispatch.pending_cancel.is_none())
    );
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::HarnessNotice(notice)
            if notice.message.contains("model")
                || notice.message.contains("policy")
                || notice.message.contains("branch")
    )));
    h.shutdown().expect("shutdown");
}

/// A selected sibling committed while the reserved context rejection is parked
/// remains selected; releasing the rejection records one stale recovery failure
/// and never delivers compaction work.
#[test]
fn output_length_reactive_rejection_parked_across_branch_move_fails_closed() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.submit_user_prompt(test_session_id("s1"), "move during recovery".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
        .expect("source plan");
    let successor = read_nth_prompt_created(&h, 1);
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    let _interceptor = connect_test_tool(&mut h, "reactive-branch-interceptor");
    h.handle_extension_event(
        "reactive-branch-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register rejection interceptor");
    h.handle_provider_response_finished(context_overflow_response(&successor))
        .expect("park rejection");
    let sibling = h
        .append_direct_agent_semantic_event(
            source.agent_id.as_str(),
            tau_core::AgentEventParent::Root,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: source.agent_id.clone(),
                text: "selected recovery sibling".to_owned(),
                inference_activation: false,
                message_class: Default::default(),
            }),
        )
        .expect("append sibling")
        .selected_head_id
        .expect("sibling node");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("source agent")
        .identity
        .head = Some(sibling);
    h.publish_for_agent(
        &cid,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: source.agent_id.clone(),
            head: tau_proto::AgentHead::Node(sibling),
        }),
    );
    h.handle_extension_event(
        "reactive-branch-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit off-selected rejection");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid].identity.head,
        Some(sibling)
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2
    );
    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason
                        == tau_proto::StandaloneCompactionFailureReason::StaleBranch
            ))
            .count(),
        1,
        "off-branch records: {:?}",
        event_log_events(&h)
            .iter()
            .filter_map(|event| match event {
                Event::HarnessNotice(notice) => Some(notice.message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    h.shutdown().expect("shutdown");
}

/// Cancellation while an off-branch reactive Start or its staged failure is
/// parked rewrites the one retained failure to Cancelled before commit.
#[test]
fn output_length_reactive_staged_failure_arbitrates_cancellation() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    h.submit_user_prompt(test_session_id("s1"), "cancel staged recovery".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
        .expect("source plan");
    let successor = read_nth_prompt_created(&h, 1);
    let cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    let _response_interceptor = connect_test_tool(&mut h, "staged-response");
    let _transaction_interceptor = connect_test_tool(&mut h, "staged-transaction");
    h.handle_extension_event(
        "staged-response",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register response intercept");
    h.handle_extension_event(
        "staged-transaction",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_STANDALONE_COMPACTION_STARTED),
                EventSelector::Exact(tau_proto::EventName::AGENT_STANDALONE_COMPACTION_FAILED),
            ],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register transaction intercept");
    h.handle_provider_response_finished(context_overflow_response(&successor))
        .expect("park response");
    let sibling = h
        .append_direct_agent_semantic_event(
            source.agent_id.as_str(),
            tau_core::AgentEventParent::Root,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: source.agent_id.clone(),
                text: "cancel sibling".to_owned(),
                inference_activation: false,
                message_class: Default::default(),
            }),
        )
        .expect("append sibling")
        .selected_head_id
        .expect("sibling");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("source agent")
        .identity
        .head = Some(sibling);
    h.handle_extension_event(
        "staged-response",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit response");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentStandaloneCompactionStarted(_))
    ));
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: Some(successor.agent_prompt_id.clone()),
        },
    );
    h.handle_extension_event(
        "staged-transaction",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit start");
    assert!(matches!(
        h.runtime_io
            .publication
            .pending_intercept
            .as_ref()
            .map(|pending| &pending.event),
        Some(Event::AgentStandaloneCompactionFailed(_))
    ));
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: Some(successor.agent_prompt_id),
        },
    );
    h.handle_extension_event(
        "staged-transaction",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit failure");
    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("records");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason
                        == tau_proto::StandaloneCompactionFailureReason::Cancelled
            ))
            .count(),
        1
    );
    assert!(!records.iter().any(|record| matches!(
        &record.event,
        Event::AgentStandaloneCompactionFailed(failed)
            if failed.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch
    )));
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_cancel
            .is_none()
    );
    h.shutdown().expect("shutdown");
}

/// A rejected settled-finish append must retain the exact open turn until the
/// finish commits, then allow ordinary work on the same agent.
#[test]
fn output_length_finish_append_failure_retries_before_new_work() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    let _interceptor = connect_test_tool(&mut h, "length-finish-interceptor");
    h.handle_extension_event(
        "length-finish-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_OUTER_TURN_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register finish interceptor");
    h.submit_user_prompt(test_session_id("s1"), "finish with retry".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 5))
        .expect("source response");
    let successor = read_nth_prompt_created(&h, 1);
    let mut completed = provider_text_response(
        &successor.agent_prompt_id,
        successor.agent_id.clone(),
        "done",
    );
    completed.usage = Some(tau_proto::ProviderTokenUsage {
        response_received_tokens: 7,
        ..Default::default()
    });
    h.handle_provider_response_finished(completed)
        .expect("successor response");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "settled finish is parked"
    );

    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "length-finish-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release finish into append failure");
    let retained_outer_turn_id = h
        .agent_runtime
        .agent_registry
        .agents
        .values()
        .find_map(|agent| match &agent.turn.outer_turn {
            OuterTurnRuntimeState::FinishRetry(outer_turn_id)
                if matches!(
                    agent.turn.output_length_continuation,
                    OutputLengthContinuationState::Spent { .. }
                ) =>
            {
                Some(outer_turn_id.clone())
            }
            _ => None,
        })
        .expect("append rejection retains exact finish");
    h.submit_user_prompt(test_session_id("s1"), "new work".to_owned())
        .expect("queue work behind finish");
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2,
        "new work remains deferred while the old finish is retryable"
    );

    h.retry_pending_agent_publications();
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "retried finish is interceptable"
    );
    h.handle_extension_event(
        "length-finish-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release retried finish");
    assert!(
        h.agent_runtime.agent_registry.agents.values().all(|agent| {
            !matches!(
                agent.turn.outer_turn,
                crate::agent::OuterTurnRuntimeState::FinishInFlight(_)
                    | crate::agent::OuterTurnRuntimeState::FinishRetry(_)
            )
        }),
        "states: {:?}, intercept: {:?}",
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .map(|agent| (
                &agent.turn.outer_turn,
                &agent.turn.output_length_continuation,
            ))
            .collect::<Vec<_>>(),
        h.runtime_io.publication.pending_intercept.is_some()
    );
    h.handle_disconnect(&crate::test_connection_id("length-finish-interceptor"));
    let next = read_nth_prompt_created(&h, 2);
    assert_ne!(next.agent_prompt_id, successor.agent_prompt_id);
    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(source.agent_id.as_str())
            .expect("durable events after retry")
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentOuterTurnFinished(finished)
                    if finished.outer_turn_id == retained_outer_turn_id
                        && finished.disposition
                            == tau_proto::AgentOuterTurnDisposition::Settled
            ))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// Cancellation accepted while a post-start route failure is parked must
/// rewrite the one canonical terminal to Cancelled without duplicating
/// prompt-start.
#[test]
fn output_length_post_start_route_failure_race_prefers_cancelled_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "post-start route loss".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let source_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    h.report_agent_work_status(
        &source_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "route-bound work".to_owned(),
        )
        .expect("working report"),
    )
    .expect("record working status");
    let _interceptor = connect_test_tool(&mut h, "length-created-interceptor");
    h.handle_extension_event(
        "length-created-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_CREATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register prompt-created interceptor");
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 5))
        .expect("source length response");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "successor delivery is parked"
    );
    h.provider_runtime.model_routes.remove(&source.model);
    h.runtime_io
        .publication
        .interceptors
        .replace_for_connection(
            &crate::test_connection_id("length-created-interceptor"),
            crate::test_extension_name("length-created-interceptor"),
            vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            InterceptionPriority::new(0),
        );
    h.handle_extension_event(
        "length-created-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release successor after route loss");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "local Failed terminal is parked"
    );
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    h.handle_extension_event(
        "length-created-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release cancellation-owned terminal");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let successor_id = records
        .iter()
        .find_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => {
                match &response.output_length_disposition {
                    tau_proto::OutputLengthDisposition::ContinuationPlanned {
                        successor_agent_prompt_id,
                        ..
                    } => Some(successor_agent_prompt_id.clone()),
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("reserved successor");
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentPromptStarted(started)
                    if started.agent_prompt_id == successor_id
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
                    if response.agent_prompt_id == successor_id
                        && matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                                ..
                            }
                        )
            ))
            .count(),
        1
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .all(|agent| agent.dispatch.pending_cancel.is_none())
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&source_cid]
            .turn
            .work_status
            .phase(),
        tau_proto::AgentWorkStatusPhase::Unknown
    );
    h.shutdown().expect("shutdown");
}

/// Cancellation while a synthetic pre-delivery prompt-start is parked must
/// choose Cancelled instead of racing a second Failed terminal.
#[test]
fn output_length_pre_delivery_failure_race_prefers_cancellation_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "cancel local failure".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    let provider_route = h
        .provider_runtime
        .model_routes
        .get(&source.model)
        .cloned()
        .expect("provider route");
    let source_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    h.report_agent_work_status(
        &source_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "cancel-bound work".to_owned(),
        )
        .expect("working report"),
    )
    .expect("record working status");
    let _interceptor = connect_test_tool(&mut h, "length-failure-race");
    h.handle_extension_event(
        "length-failure-race",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_PROMPT_STARTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register prompt-start interceptor");
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 5))
        .expect("source response");
    h.provider_runtime.model_routes.remove(&source.model);
    h.handle_extension_event(
        "length-failure-race",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release normal prompt-start after route loss");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "synthetic failure prompt-start is parked"
    );
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    h.handle_extension_event(
        "length-failure-race",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit synthetic prompt-start and arbitrate cancellation");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let terminals = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response) => {
                match &response.output_length_disposition {
                    tau_proto::OutputLengthDisposition::ContinuationTerminal {
                        outcome, ..
                    } => Some(*outcome),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        terminals,
        vec![tau_proto::OutputLengthContinuationOutcome::Cancelled]
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .all(|agent| agent.dispatch.pending_cancel.is_none())
    );
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&source_cid]
            .turn
            .work_status
            .phase(),
        tau_proto::AgentWorkStatusPhase::Unknown
    );
    h.provider_runtime
        .model_routes
        .insert(source.model.clone(), provider_route);
    h.handle_disconnect(&crate::test_connection_id("length-failure-race"));
    h.submit_user_prompt(
        test_session_id("s1"),
        "same agent remains live after cancellation".to_owned(),
    )
    .expect("submit post-cancel prompt");
    let post_cancel = read_nth_prompt_created(&h, 1);
    assert_ne!(post_cancel.agent_prompt_id, source.agent_prompt_id);
    h.shutdown().expect("shutdown");
}

/// Cancellation before a real successor terminal commits must preserve its
/// response-local accounting while replacing all semantic output and follow-up
/// authority with one Cancelled terminal.
#[test]
fn output_length_real_terminal_race_cancels_with_exact_accounting() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "cancel real terminal".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
        .expect("source response");
    let successor = read_nth_prompt_created(&h, 1);
    let source_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    h.report_agent_work_status(
        &source_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "status-gated completion".to_owned(),
        )
        .expect("working report"),
    )
    .expect("record working status");
    let _interceptor = connect_test_tool(&mut h, "length-real-terminal-race");
    h.handle_extension_event(
        "length-real-terminal-race",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register terminal interceptor");
    let public_id = durable_agent_id_for_conversation(&h, &source_cid);
    let mut terminal = reasoning_only_length_response(&successor, 7);
    terminal.usage = Some(tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 19,
        prompt_cached_tokens: 2,
        response_received_tokens: 7,
        ..Default::default()
    });
    h.handle_provider_response_finished(terminal)
        .expect("park real successor terminal");
    assert!(
        h.runtime_io.publication.pending_intercept.is_some(),
        "real terminal is parked"
    );
    assert!(!matches!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .get(public_id.as_str())
            .map(|status| &status.state),
        Some(tau_proto::AgentWatchProviderState::TerminalIncomplete { .. })
    ));
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: None,
        },
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&source_cid]
            .dispatch
            .pending_prompts
            .is_empty()
            && !h.agent_runtime.agent_registry.agents[&source_cid]
                .dispatch
                .pending_replay_activation,
        "cancellation must consume queued successor activations: {:?}",
        h.agent_runtime.agent_registry.agents[&source_cid]
            .dispatch
            .pending_prompts
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2,
        "cancellation cannot dispatch before terminal commit"
    );
    h.handle_extension_event(
        "length-real-terminal-race",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit cancellation-owned terminal");

    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let cancelled = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == successor.agent_prompt_id =>
            {
                Some(response)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled.len(), 1);
    let cancelled = cancelled[0];
    assert!(cancelled.output_items.is_empty());
    assert!(matches!(
        cancelled.output_length_disposition,
        tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
            outer_turn_finish_owed: true,
            ..
        }
    ));
    let usage = cancelled.usage.as_ref().expect("provider usage retained");
    assert_eq!(usage.prompt_sent_tokens, 19);
    assert_eq!(usage.prompt_cached_tokens, 2);
    assert_eq!(usage.response_received_tokens, 7);
    assert!(cancelled.estimated_api_cost_rates.is_some());
    assert!(cancelled.estimated_api_cost_increment.is_some());
    assert!(!matches!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .get(public_id.as_str())
            .map(|status| &status.state),
        Some(tau_proto::AgentWatchProviderState::TerminalIncomplete { .. })
    ));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&source_cid]
            .turn
            .work_status
            .phase(),
        tau_proto::AgentWorkStatusPhase::Unknown
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&source_cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| !prompt.text.contains("status"))
    );
    assert!(!event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentMessageReceived(message)
            if message.message.contains("must not become visible")
    )));
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2,
        "cancellation cannot dispatch a status challenge or third request: {:?}",
        events
            .iter()
            .filter_map(|event| match event {
                Event::AgentPromptCreated(prompt) =>
                    Some((prompt.agent_prompt_id.clone(), prompt.context.clone(),)),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentInferenceDispatchStarted(_)))
            .count(),
        2
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, Event::AgentPromptStarted(_)))
            .count(),
        2
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentMessageReceived(message)
            if message.kind == tau_proto::AgentMessageKind::WatchResponse
    )));
    h.shutdown().expect("shutdown");
}

/// Cancellation must claim a successor terminal retained after append
/// rejection, then repair it as one Cancelled terminal with no stale
/// completion.
#[test]
fn output_length_append_rejected_terminal_cancellation_repairs_once() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("start");
    h.submit_user_prompt(test_session_id("s1"), "cancel rejected terminal".to_owned())
        .expect("submit");
    let source = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
        .expect("source response");
    let successor = read_nth_prompt_created(&h, 1);
    let _interceptor = connect_test_tool(&mut h, "length-rejected-terminal");
    h.handle_extension_event(
        "length-rejected-terminal",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register terminal interceptor");
    let source_cid = h
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(source.agent_id.as_str())
        .cloned()
        .expect("source route");
    let public_id = durable_agent_id_for_conversation(&h, &source_cid);
    let terminal = reasoning_only_length_response(&successor, 5);
    h.handle_provider_response_finished(terminal)
        .expect("park terminal");

    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "length-rejected-terminal",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("release terminal into append failure");
    assert!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .values()
            .any(|completion| completion.owns_output_length_terminal(&successor.agent_prompt_id))
    );
    assert!(!matches!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .get(public_id.as_str())
            .map(|status| &status.state),
        Some(tau_proto::AgentWatchProviderState::TerminalIncomplete { .. })
    ));
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: h.session_runtime.current_session_id.clone(),
            target_agent_id: Some(source.agent_id.clone()),
            agent_prompt_id: None,
        },
    );

    h.retry_pending_agent_publications();
    assert!(
        h.runtime_io.publication.pending_intercept.is_none(),
        "approved exact retry is not intercepted a second time"
    );
    assert!(
        !h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .contains_key(
                h.agent_runtime
                    .agent_registry
                    .agent_routes
                    .get(source.agent_id.as_str())
                    .expect("source route")
            )
    );
    let records = h
        .session_runtime
        .agent_store
        .agent_events(source.agent_id.as_str())
        .expect("durable events");
    let cancelled = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderResponseFinished(response)
                if response.agent_prompt_id == successor.agent_prompt_id
                    && response.output_items.is_empty()
                    && matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationTerminal {
                            outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                            ..
                        }
                    ) =>
            {
                Some(response)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cancelled.len(), 1);
    assert!(!matches!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .get(public_id.as_str())
            .map(|status| &status.state),
        Some(tau_proto::AgentWatchProviderState::TerminalIncomplete { .. })
    ));
    let tau_proto::OutputLengthDisposition::ContinuationTerminal { outer_turn_id, .. } =
        &cancelled[0].output_length_disposition
    else {
        unreachable!("filtered cancellation terminal");
    };
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::AgentOuterTurnFinished(finished)
                    if finished.outer_turn_id == *outer_turn_id
                        && finished.disposition
                            == tau_proto::AgentOuterTurnDisposition::Settled
            ))
            .count(),
        1
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .values()
            .all(|agent| agent.dispatch.pending_cancel.is_none())
    );
    h.shutdown().expect("shutdown");
}

/// Replay eligibility is limited to ordinary user work on the exact adapter
/// whose retained reasoning representation is known to be replay-safe.
#[test]
fn reasoning_only_length_rejects_other_adapters_and_side_conversations() {
    let cases = [
        (
            tau_proto::ProviderBackendKind::Responses,
            tau_proto::PromptOriginator::User,
        ),
        (
            tau_proto::ProviderBackendKind::ChatCompletions,
            tau_proto::PromptOriginator::Extension {
                name: crate::test_extension_name("side-agent-owner"),
                query_id: "side-query".to_owned(),
            },
        ),
    ];
    for (index, (backend_kind, originator)) in cases.into_iter().enumerate() {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path()).expect("start");
        h.submit_user_prompt(test_session_id("s1"), format!("eligibility case {index}"))
            .expect("submit");
        let source = read_nth_prompt_created(&h, 0);
        if !originator.is_user() {
            let cid = h
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(source.agent_id.as_str())
                .cloned()
                .expect("source route");
            h.agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("source agent")
                .identity
                .originator = originator.clone();
        }
        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            agent_prompt_id: source.agent_prompt_id.clone(),
            agent_id: source.agent_id.clone(),
            output_items: vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "retained reasoning".to_owned(),
            })],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: Some(tau_proto::ProviderBackend {
                kind: backend_kind,
                base_url: "https://example.invalid/v1".to_owned(),
                transport: tau_proto::ProviderBackendTransport::HttpSse,
                stale_chain_fallback: false,
            }),
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("length terminal accepted");

        assert!(
            h.session_runtime
                .agent_store
                .agent_events(source.agent_id.as_str())
                .expect("durable events")
                .iter()
                .filter_map(|record| match &record.event {
                    Event::ProviderResponseFinished(response) => Some(response),
                    _ => None,
                })
                .all(|response| response.output_length_disposition
                    == tau_proto::OutputLengthDisposition::None),
            "case {index} must not reserve a successor"
        );
        h.shutdown().expect("shutdown");
    }
}

/// Eligibility for the one replay-safe continuation is exact: only ordinary
/// user Chat Completions inference with non-empty Full reasoning and no
/// assistant message or tool call may plan a successor. Every other Length
/// remains a visible semantic failure without reserving or dispatching
/// anything, and no non-Inference operation or adapter becomes replay
/// authority.
#[test]
fn output_length_eligibility_matrix_is_exact() {
    struct Case {
        name: &'static str,
        output_items: Vec<ContextItem>,
        stop_reason: tau_proto::ProviderStopReason,
        error: Option<String>,
        failure_kind: Option<tau_proto::ProviderFailureKind>,
        backend: Option<tau_proto::ProviderBackend>,
        originator: tau_proto::PromptOriginator,
        operation: Option<tau_proto::PromptOperation>,
        expect_plan: bool,
    }
    let chat = || tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::ChatCompletions,
        base_url: "https://example.invalid/v1".to_owned(),
        transport: tau_proto::ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    };
    let full_reasoning = ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Full,
        text: "retained reasoning".to_owned(),
    });
    let call = ContextItem::ToolCall(ToolCallItem {
        call_id: "eligibility-call".into(),
        name: ToolName::new("eligibility_tool"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
        raw_arguments_json: None,
        responses_envelope: None,
    });
    let cases = [
        Case {
            name: "full reasoning",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: true,
        },
        Case {
            name: "summary-only reasoning",
            output_items: vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Summary,
                text: "summary".to_owned(),
            })],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "empty full reasoning",
            output_items: vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: String::new(),
            })],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "mixed full reasoning and assistant message",
            output_items: vec![
                full_reasoning.clone(),
                ContextItem::Message(MessageItem {
                    role: tau_proto::ContextRole::Assistant,
                    content: vec![tau_proto::ContentPart::Text {
                        text: "partial prose".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                }),
            ],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "full reasoning with tool call",
            output_items: vec![full_reasoning.clone(), call.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "provider error",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: Some("boom".to_owned()),
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "failure kind",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: None,
            failure_kind: Some(tau_proto::ProviderFailureKind::RequestRejected),
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "end turn stop",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "missing backend",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: None,
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "responses backend",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(tau_proto::ProviderBackend {
                kind: tau_proto::ProviderBackendKind::Responses,
                base_url: "https://example.invalid/v1".to_owned(),
                transport: tau_proto::ProviderBackendTransport::HttpSse,
                stale_chain_fallback: false,
            }),
            originator: tau_proto::PromptOriginator::User,
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "side conversation",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::Extension {
                name: crate::test_extension_name("side-agent-eligibility"),
                query_id: "side-eligibility".to_owned(),
            },
            operation: None,
            expect_plan: false,
        },
        Case {
            name: "standalone compaction",
            output_items: vec![full_reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            failure_kind: None,
            backend: Some(chat()),
            originator: tau_proto::PromptOriginator::User,
            operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
            expect_plan: false,
        },
    ];
    for (index, case) in cases.into_iter().enumerate() {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path()).expect("start");
        // Register the eligibility tool so any erroneous dispatch would be
        // observable in the frames and pending-tool state.
        connect_test_tool(&mut h, "eligibility-tool");
        h.tool_routing.registry.register(
            &crate::test_connection_id("eligibility-tool"),
            staged_tool_spec("eligibility_tool"),
        );
        h.submit_user_prompt(test_session_id("s1"), format!("eligibility row {index}"))
            .expect("submit");
        let source = read_nth_prompt_created(&h, 0);
        if let Some(operation) = case.operation {
            h.prompt_coordination
                .prompt_runtime
                .operations
                .insert(source.agent_prompt_id.clone(), (operation, false));
        }
        if !case.originator.is_user() {
            let cid = h
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(source.agent_id.as_str())
                .cloned()
                .expect("source route");
            h.agent_runtime
                .agent_registry
                .agents
                .get_mut(&cid)
                .expect("source agent")
                .identity
                .originator = case.originator.clone();
        }
        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            agent_prompt_id: source.agent_prompt_id.clone(),
            agent_id: source.agent_id.clone(),
            output_items: case.output_items,
            stop_reason: case.stop_reason,
            error: case.error,
            failure_kind: case.failure_kind,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: case.originator,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: case.backend,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("length terminal accepted");
        let planned = h
            .session_runtime
            .agent_store
            .agent_events(source.agent_id.as_str())
            .expect("durable events")
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::ProviderResponseFinished(response)
                        if matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                        )
                )
            })
            .count();
        let created = event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count();
        assert_eq!(
            planned,
            usize::from(case.expect_plan),
            "row {index} ({}) plan count",
            case.name
        );
        assert_eq!(
            created,
            if case.expect_plan { 2 } else { 1 },
            "row {index} ({}) successor prompt",
            case.name
        );
        assert!(
            h.tool_routing.tool_runtime.tool_turn.is_empty(),
            "row {index} ({}) must never dispatch a call",
            case.name
        );
        assert!(
            !h.tool_routing
                .tool_runtime
                .pending_tools
                .contains_key("eligibility-call"),
            "row {index} ({}) must not register a pending call",
            case.name
        );
        h.shutdown().expect("shutdown");
    }
}

/// The reserved successor terminal matrix is exact: normal answers and real
/// tool calls complete while the finish bit follows the actual post-suppression
/// tool continuation, and length, errors, and repetition stay visibly
/// incomplete or failed without executing calls.
#[test]
fn output_length_successor_terminal_matrix_is_exact() {
    struct Case {
        name: &'static str,
        output_items: Vec<ContextItem>,
        stop_reason: tau_proto::ProviderStopReason,
        error: Option<String>,
        expected_outcome: tau_proto::OutputLengthContinuationOutcome,
        expected_finish_owed: bool,
        expects_dispatch: bool,
    }
    let call = ContextItem::ToolCall(ToolCallItem {
        call_id: "successor-call".into(),
        name: ToolName::new("successor_tool"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
        raw_arguments_json: None,
        responses_envelope: None,
    });
    let reasoning = ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
        kind: tau_proto::ReasoningTextKind::Full,
        text: "successor reasoning".to_owned(),
    });
    let assistant_message = ContextItem::Message(MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: "finished".to_owned(),
        }],
        phase: None,
        responses_raw_json: None,
    });
    let cases = [
        Case {
            name: "end turn assistant",
            output_items: vec![assistant_message.clone()],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            expected_finish_owed: true,
            expects_dispatch: false,
        },
        Case {
            name: "length",
            output_items: vec![reasoning.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Incomplete,
            expected_finish_owed: true,
            expects_dispatch: false,
        },
        Case {
            name: "length with truncated call",
            output_items: vec![reasoning.clone(), call.clone()],
            stop_reason: tau_proto::ProviderStopReason::Length,
            error: None,
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Incomplete,
            expected_finish_owed: true,
            expects_dispatch: false,
        },
        Case {
            name: "provider error",
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: Some("boom".to_owned()),
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
            expected_finish_owed: true,
            expects_dispatch: false,
        },
        Case {
            name: "repetition",
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::RepetitionDetected,
            error: None,
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
            expected_finish_owed: true,
            expects_dispatch: false,
        },
        Case {
            name: "tool calls",
            output_items: vec![call.clone()],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            expected_finish_owed: false,
            expects_dispatch: true,
        },
        Case {
            name: "tool calls with zero calls",
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            expected_finish_owed: true,
            expects_dispatch: false,
        },
        Case {
            name: "end turn with calls",
            output_items: vec![call.clone()],
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            expected_outcome: tau_proto::OutputLengthContinuationOutcome::Completed,
            expected_finish_owed: false,
            expects_dispatch: true,
        },
    ];
    for (index, case) in cases.into_iter().enumerate() {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path()).expect("start");
        connect_test_tool(&mut h, "successor-tool");
        h.tool_routing.registry.register(
            &crate::test_connection_id("successor-tool"),
            staged_tool_spec("successor_tool"),
        );
        h.submit_user_prompt(test_session_id("s1"), format!("successor matrix {index}"))
            .expect("submit");
        let source = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(reasoning_only_length_response(&source, 3))
            .expect("source response");
        let successor = read_nth_prompt_created(&h, 1);
        let terminal = ProviderResponseFinished {
            automatic_compaction_decision: None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            agent_prompt_id: successor.agent_prompt_id.clone(),
            agent_id: successor.agent_id.clone(),
            output_items: case.output_items,
            stop_reason: case.stop_reason,
            error: case.error,
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: Some(tau_proto::ProviderBackend {
                kind: tau_proto::ProviderBackendKind::ChatCompletions,
                base_url: "https://example.invalid/v1".to_owned(),
                transport: tau_proto::ProviderBackendTransport::HttpSse,
                stale_chain_fallback: false,
            }),
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        };
        h.handle_provider_response_finished(terminal)
            .expect("successor terminal");
        let records = h
            .session_runtime
            .agent_store
            .agent_events(source.agent_id.as_str())
            .expect("durable events");
        let terminals = records
            .iter()
            .filter_map(|record| match &record.event {
                Event::ProviderResponseFinished(response)
                    if response.agent_prompt_id == successor.agent_prompt_id =>
                {
                    Some(response)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            terminals.len(),
            1,
            "row {index} ({}) one successor terminal",
            case.name
        );
        let tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outcome,
            outer_turn_finish_owed,
            ..
        } = &terminals[0].output_length_disposition
        else {
            panic!(
                "row {index} ({}) expected a continuation terminal, got {:?}",
                case.name, terminals[0].output_length_disposition
            );
        };
        assert_eq!(
            *outcome, case.expected_outcome,
            "row {index} ({}) outcome",
            case.name
        );
        assert_eq!(
            *outer_turn_finish_owed, case.expected_finish_owed,
            "row {index} ({}) finish bit",
            case.name
        );
        assert_eq!(
            h.tool_routing.tool_runtime.tool_turn.is_empty(),
            !case.expects_dispatch,
            "row {index} ({}) tool dispatch",
            case.name
        );
        assert_eq!(
            h.tool_routing
                .tool_runtime
                .pending_tools
                .contains_key("successor-call"),
            case.expects_dispatch,
            "row {index} ({}) pending call",
            case.name
        );
        h.shutdown().expect("shutdown");
    }
}

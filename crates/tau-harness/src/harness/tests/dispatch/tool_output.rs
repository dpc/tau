//! Ordered output policy, publication custody, and recovery.

use super::*;

/// Incremental source is ordinary activating input: it never interrupts a live
/// provider, but contributes ordinary queued-input recovery authority. Final
/// source alone is passive; independent ordinary interrupted-inference retry
/// still applies to the uncertain owner in both cases.
#[test]
fn output_recovery_preserves_live_inference_and_final_activation_boundary() {
    for incremental in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let mut h = quiet_provider_harness(&state).expect("start");
        let connection = crate::test_connection_id("stream-worker");
        connect_ready_configured_extension(
            &mut h,
            "stream-worker",
            "stream-worker",
            tau_proto::ClientKind::Tool,
        );
        let mut spec = instant_background_test_tool_spec("streaming");
        spec.tags
            .push(tau_proto::ToolTag::new(tau_proto::TOOL_OUTPUT_STREAM_TAG));
        h.tool_routing.registry.register(&connection, spec.clone());
        let cid = ensure_test_user_agent(&mut h);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        h.prompt_coordination
            .prompt_runtime
            .tool_specs
            .insert(test_agent_prompt_id("sp-recovery-stream"), vec![spec]);
        start_background_tool_and_finish_placeholder_turn(
            &mut h,
            &cid,
            "recovery-stream",
            "streaming",
        );
        // Hold the marked dispatch before AgentPromptStarted, the uncertain
        // restart cut. Live runtime still owns an active inference.
        let owner = test_agent_prompt_id("stream-active-owner");
        seed_agent_thinking(&mut h, &cid, owner.as_str());
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .dispatch
            .in_flight_prompt = Some(owner.clone());
        h.prompt_coordination
            .prompt_runtime
            .agents
            .insert(owner.clone(), cid.clone());
        let through = h
            .session_runtime
            .agent_store
            .agent(agent.as_str())
            .expect("tree")
            .head()
            .expect("head");
        h.session_runtime
            .agent_store
            .append_agent_event_at(
                agent.as_str(),
                None,
                tau_core::AgentEventParent::InheritHead,
                Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                    output_length_continuation: None,
                    agent_id: agent.clone(),
                    transaction_id: None,
                    agent_prompt_id: owner.clone(),
                    through: tau_proto::AgentHead::Node(through),
                    model: "test/model".into(),
                    operation: tau_proto::PromptOperation::Inference,
                    activation_cut: tau_proto::AgentHead::Root,
                }),
                tau_proto::UnixMicros::now(),
            )
            .expect("seed durable dispatch cut without starting its provider");
        let before = event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count();
        if incremental {
            h.handle_extension_event_inner(
                &connection,
                Event::ToolOutputReported(tau_proto::ToolOutputReport {
                    call_id: "recovery-stream".into(),
                    tool_name: ToolName::new("streaming"),
                    sequence: 0,
                    text: "incremental-recovery-sentinel".to_owned(),
                    omitted_bytes: 0,
                }),
            )
            .expect("live incremental input");
        }
        // Capture the legitimate canonical-terminal/before-notice crash cut.
        // The final source has no independent activation authority.
        h.publish_for_agent(
            &cid,
            Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                captured_output: None,
                call_id: "recovery-stream".into(),
                tool_name: ToolName::new("streaming"),
                tool_type: tau_proto::ToolType::Function,
                result: CborValue::Text("final-recovery-sentinel".to_owned()),
                originator: tau_proto::PromptOriginator::User,
                display: None,
            }),
        );
        assert_eq!(
            active_prompt_for(&h, &cid),
            owner,
            "never interrupt live inference"
        );
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
                .count(),
            before
        );
        assert_eq!(
            h.session_runtime
                .agent_store
                .agent(agent.as_str())
                .expect("tree")
                .marked_inference_has_deferred_prompt_activation(&owner),
            incremental
        );
        let queued_wakes = &h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_message_wakes;
        assert_eq!(
            queued_wakes
                .iter()
                .filter(|wake| matches!(
                    wake.source,
                    crate::agent::PendingMessageWakeSource::ToolOutput {
                        terminal: false,
                        ..
                    }
                ))
                .count(),
            usize::from(incremental)
        );
        assert!(
            !queued_wakes.iter().any(|wake| matches!(
                wake.source,
                crate::agent::PendingMessageWakeSource::ToolOutput { terminal: true, .. }
            )),
            "final source must not synthesize activation"
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent.as_str())
            .expect("crash prefix");
        assert!(
            records.iter().any(|record| matches!(&record.event,
                Event::AgentInferenceDispatchStarted(mark) if mark.agent_prompt_id == owner
            )),
            "marked owner missing: {:?}",
            records
                .iter()
                .map(|record| record.event.name())
                .collect::<Vec<_>>()
        );
        h.shutdown().expect("release live store");
        wait_for_session_unlock(&state, "s1");
        rewrite_agent_records(&state, &agent, &records);
        let mut restored =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("cold restore");
        let events = event_log_events(&restored);
        let successors = events
            .iter()
            .filter_map(|event| match event {
                Event::AgentPromptCreated(prompt) => Some(prompt),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            successors.len(),
            1,
            "ordinary interrupted recovery dispatches once"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event,
                    Event::ProviderResponseFinished(response) if response.agent_prompt_id == owner
                        && response.interrupted_retry
                ))
                .count(),
            1
        );
        let context = serde_json::to_string(&successors[0].context).expect("context");
        assert_eq!(context.matches("final-recovery-sentinel").count(), 1);
        if incremental {
            assert_eq!(context.matches("incremental-recovery-sentinel").count(), 1);
            assert!(
                context.find("incremental-recovery-sentinel")
                    < context.find("final-recovery-sentinel")
            );
        }
        let cid = test_user_agent(&restored);
        restored
            .handle_provider_response_finished(provider_text_response(
                &successors[0].agent_prompt_id,
                agent.clone(),
                "recovered",
            ))
            .expect("close recovered owner");
        assert!(matches!(
            restored.agent_runtime.agent_registry.agents[&cid]
                .turn
                .turn_state,
            AgentTurnState::Idle
        ));
        restored.shutdown().expect("shutdown restored");
        wait_for_session_unlock(&state, "s1");
        let mut again =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("second restore");
        assert!(
            !event_log_events(&again)
                .iter()
                .any(|event| matches!(event, Event::AgentPromptCreated(_))),
            "accepted occurrence must not dispatch twice"
        );
        again.shutdown().expect("second shutdown");
    }
}

fn start_stream_with_foreground_sibling(h: &mut Harness, cid: &AgentId) {
    start_stream_round(
        h,
        cid,
        vec![("stream-call", "streaming"), ("block-call", "blocking")],
    );
}

#[test]
fn output_recovery_host_enforces_owner_and_selected_branch() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let owner = ensure_test_user_agent(&mut h);
    let other = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    start_stream_with_foreground_sibling(&mut h, &owner);
    h.handle_extension_event_inner(
        &crate::test_connection_id("stream-worker"),
        Event::ToolOutputReported(tau_proto::ToolOutputReport {
            call_id: "stream-call".into(),
            tool_name: ToolName::new("streaming"),
            sequence: 0,
            text: "private accepted output".into(),
            omitted_bytes: 0,
        }),
    )
    .expect("output");
    let call_id = ToolCallId::from("stream-call");
    let page = crate::InternalToolHost::new(&mut h)
        .tool_output_page(
            &owner,
            crate::ToolOutputTarget::DisplayId(call_id.clone()),
            0,
        )
        .expect("own display lookup");
    assert!(page.text.contains("private accepted output"));
    assert_eq!(
        crate::InternalToolHost::new(&mut h)
            .tool_output_page(&owner, crate::ToolOutputTarget::Reference(page.call), 0)
            .expect("own exact lookup"),
        page
    );
    assert!(
        crate::InternalToolHost::new(&mut h)
            .tool_output_page(
                &other,
                crate::ToolOutputTarget::DisplayId(call_id.clone()),
                0
            )
            .is_err()
    );
    assert!(
        crate::InternalToolHost::new(&mut h)
            .tool_output_page(&other, crate::ToolOutputTarget::Reference(page.call), 0)
            .is_err()
    );
    let head = h.agent_runtime.agent_registry.agents[&owner]
        .identity
        .head
        .expect("selected head");
    let agent_id = durable_agent_id_for_conversation(&h, &owner);
    h.publish_for_agent(
        &owner,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id: agent_id.clone(),
            head: tau_proto::AgentHead::Root,
        }),
    );
    assert!(
        crate::InternalToolHost::new(&mut h)
            .tool_output_page(
                &owner,
                crate::ToolOutputTarget::DisplayId(call_id.clone()),
                0
            )
            .is_err()
    );
    assert!(
        crate::InternalToolHost::new(&mut h)
            .tool_output_page(&owner, crate::ToolOutputTarget::Reference(page.call), 0)
            .is_err()
    );
    h.publish_for_agent(
        &owner,
        Event::AgentHeadMoved(tau_proto::AgentHeadMoved {
            agent_id,
            head: tau_proto::AgentHead::Node(head),
        }),
    );
    assert_eq!(
        crate::InternalToolHost::new(&mut h)
            .tool_output_page(&owner, crate::ToolOutputTarget::Reference(page.call), 0)
            .expect("returned branch"),
        page
    );
}

/// Arbitrary opted-in producers cannot move an unbounded terminal payload into
/// retained publication custody. The canonical bounded text keeps final
/// diagnostics and process status while ignoring forged harness capture fields.
#[test]
fn streamed_terminal_capture_bounds_payload_and_preserves_process_outcome() {
    for error in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path().join("state")).expect("start");
        h.config.selected_model = Some("test/model".into());
        let cid = ensure_test_user_agent(&mut h);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        start_stream_with_foreground_sibling(&mut h, &cid);
        let details = CborValue::Map(vec![
            (
                CborValue::Text("status".into()),
                CborValue::Integer(7.into()),
            ),
            (
                CborValue::Text("termination_reason".into()),
                CborValue::Text("exit".into()),
            ),
            (
                CborValue::Text("output".into()),
                CborValue::Text(format!(
                    "first-diagnostic {} last-diagnostic",
                    "x".repeat(1024 * 1024),
                )),
            ),
        ]);
        let forged = Some(tau_proto::ToolFinalOutput {
            text: "forged capture is ignored".into(),
            omitted_bytes: 0,
        });
        let event = if error {
            Event::ToolErrorReported(tau_proto::ToolError {
                captured_output: forged,
                call_id: "stream-call".into(),
                tool_name: ToolName::new("streaming"),
                tool_type: tau_proto::ToolType::Function,
                message: "producer failed".into(),
                details: Some(details),
                presentation: Default::default(),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            })
        } else {
            let mut result = final_tool_result("stream-call", "streaming", "");
            result.result = details;
            result.captured_output = forged;
            Event::ToolResultReported(result)
        };
        h.handle_extension_event_inner(&crate::test_connection_id("stream-worker"), event)
            .expect("producer terminal");
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent.as_str())
            .expect("records");
        let terminal = records
            .iter()
            .find_map(|record| match &record.event {
                Event::ProviderToolResult(result) if result.call_id.as_str() == "stream-call" => {
                    Some(&record.event)
                }
                Event::ProviderToolError(error) if error.call_id.as_str() == "stream-call" => {
                    Some(&record.event)
                }
                _ => None,
            })
            .expect("canonical terminal");
        let captured = match terminal {
            Event::ProviderToolResult(result) => result.captured_output.as_ref(),
            Event::ProviderToolError(error) => error.captured_output.as_ref(),
            _ => unreachable!(),
        }
        .expect("harness capture");
        assert!(captured.text.len() <= tau_proto::MAX_TOOL_OUTPUT_FINAL_BYTES);
        assert!(captured.omitted_bytes > 1000 * 1024);
        assert!(captured.text.contains("first-diagnostic"));
        assert!(captured.text.contains("last-diagnostic"));
        assert!(!captured.text.contains("forged capture"));
        assert!(serde_json::to_vec(terminal).expect("terminal size").len() < 20 * 1024);
        assert_eq!(
            tau_proto::ShellProcessOutcome::from_terminal_event(terminal)
                .expect("process metadata")
                .exit_code(),
            Some(7)
        );
        let cold = tau_core::AgentTree::try_from_events(agent, &records).expect("cold capture");
        let call = cold
            .tool_output_call(&"stream-call".into(), cold.head())
            .expect("call");
        let page = cold.tool_output_page(call, cold.head(), 0).expect("source");
        assert!(page.text.contains("first-diagnostic"));
        assert!(!page.text.contains("forged capture"));
    }
}

/// Foreground cancellation captures available diagnostics within the final
/// allowance, rejects forged capture authority, and replays the same source.
#[test]
fn streamed_cancellation_preserves_bounded_available_final_source() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    start_stream_with_foreground_sibling(&mut h, &cid);
    h.handle_extension_event_inner(
        &crate::test_connection_id("stream-worker"),
        Event::ToolCancelledReported(tau_proto::ToolCancelled {
            output: Some(format!(
                "first-cancel-diagnostic {} last-cancel-diagnostic",
                "x".repeat(1024 * 1024)
            )),
            captured_output: Some(tau_proto::ToolFinalOutput {
                text: "forged source".into(),
                omitted_bytes: 0,
            }),
            call_id: "stream-call".into(),
            tool_name: ToolName::new("streaming"),
            tool_type: tau_proto::ToolType::Function,
            presentation: Default::default(),
            display: None,
        }),
    )
    .expect("cancelled report");
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("records");
    let cancelled = records
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "stream-call" => {
                Some(cancelled)
            }
            _ => None,
        })
        .expect("canonical cancellation");
    assert!(
        cancelled.output.is_none(),
        "raw producer text must leave terminal custody"
    );
    let capture = cancelled.captured_output.as_ref().expect("capture");
    assert!(capture.text.len() <= tau_proto::MAX_TOOL_OUTPUT_FINAL_BYTES);
    assert!(capture.omitted_bytes > 1000 * 1024);
    assert!(capture.text.contains("first-cancel-diagnostic"));
    assert!(capture.text.contains("last-cancel-diagnostic"));
    assert!(!capture.text.contains("forged source"));
    let cold = tau_core::AgentTree::try_from_events(agent, &records).expect("replay");
    let call = cold
        .tool_output_call(&"stream-call".into(), cold.head())
        .expect("declaration");
    let page = cold
        .tool_output_page(call, cold.head(), 0)
        .expect("source page");
    assert!(page.text.contains("first-cancel-diagnostic"));
    assert!(!page.text.contains("final capture unavailable"));
}

fn start_stream_round(h: &mut Harness, cid: &AgentId, calls: Vec<(&str, &str)>) {
    let connection = crate::test_connection_id("stream-worker");
    connect_ready_configured_extension(
        h,
        "stream-worker",
        "stream-worker",
        tau_proto::ClientKind::Tool,
    );
    let mut stream = shared_test_tool_spec("streaming");
    stream
        .tags
        .push(tau_proto::ToolTag::new(tau_proto::TOOL_OUTPUT_STREAM_TAG));
    let sibling = shared_test_tool_spec("blocking");
    for spec in [&stream, &sibling] {
        h.tool_routing.registry.register(&connection, spec.clone());
    }
    let prompt = test_agent_prompt_id("stream-with-sibling");
    seed_agent_thinking(h, cid, prompt.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(prompt.clone(), cid.clone());
    h.prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(prompt.clone(), vec![stream, sibling]);
    let mut response =
        provider_text_response(&prompt, durable_agent_id_for_conversation(h, cid), "");
    response.output_items = calls
        .into_iter()
        .map(|(id, name)| {
            ContextItem::ToolCall(ToolCallItem {
                call_id: id.into(),
                name: ToolName::new(name),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })
        })
        .collect();
    response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    h.handle_provider_response_finished(response)
        .expect("declare sibling round");
}

/// Reports must belong to the configured invocation owner. Peers cannot bypass
/// that check by publishing an already-canonical source occurrence.
#[test]
fn tool_output_rejects_foreign_reports_and_peer_canonical_facts() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    start_stream_with_foreground_sibling(&mut h, &cid);
    connect_ready_configured_extension(
        &mut h,
        "other-worker",
        "other-worker",
        tau_proto::ClientKind::Tool,
    );
    let reference = h
        .persisted_tool_call_ref(&cid, &"stream-call".into())
        .expect("declaration");
    let report = tau_proto::ToolOutputReport {
        call_id: "stream-call".into(),
        tool_name: ToolName::new("producer_alias"),
        sequence: 0,
        text: "owned bytes".to_owned(),
        omitted_bytes: 0,
    };
    h.handle_extension_event_inner(
        &crate::test_connection_id("other-worker"),
        Event::ToolOutputReported(report.clone()),
    )
    .expect("foreign report");
    h.handle_extension_event_inner(
        &crate::test_connection_id("stream-worker"),
        Event::ToolOutputUpdated(tau_proto::ToolOutputUpdate {
            call: reference,
            output: report.clone(),
        }),
    )
    .expect("peer canonical attempt");
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("records");
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, Event::ToolOutputUpdated(_)))
    );
    h.handle_extension_event_inner(
        &crate::test_connection_id("stream-worker"),
        Event::ToolOutputReported(report.clone()),
    )
    .expect("owned report");
    h.handle_extension_event_inner(
        &crate::test_connection_id("stream-worker"),
        Event::ToolOutputReported(report),
    )
    .expect("duplicate owned report");
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("accepted records");
    let updates: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolOutputUpdated(update) => Some(update),
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].output.tool_name.as_str(), "streaming");
}

/// Every wait mode applies the same owner-wide nonterminal mask, including
/// output queued before registration. Capture continues and actual completion
/// still settles the wait exactly once.
#[test]
fn complete_only_filters_queued_and_live_updates_in_each_wait_mode() {
    for mode in ["exact", "all", "any", "input"] {
        for queued_first in [false, true] {
            for complete_only in [false, true] {
                let td = TempDir::new().expect("tempdir");
                let mut h = echo_harness(td.path().join("state")).expect("start");
                h.config.selected_model = Some("test/model".into());
                let connection = crate::test_connection_id("stream-worker");
                connect_ready_configured_extension(
                    &mut h,
                    "stream-worker",
                    "stream-worker",
                    tau_proto::ClientKind::Tool,
                );
                let mut spec = instant_background_test_tool_spec("streaming");
                spec.tags
                    .push(tau_proto::ToolTag::new(tau_proto::TOOL_OUTPUT_STREAM_TAG));
                h.tool_routing.registry.register(&connection, spec.clone());
                let cid = ensure_test_user_agent(&mut h);
                h.prompt_coordination
                    .prompt_runtime
                    .tool_specs
                    .insert(test_agent_prompt_id("sp-background-stream"), vec![spec]);
                start_background_tool_and_finish_placeholder_turn(
                    &mut h,
                    &cid,
                    "background-stream",
                    "streaming",
                );
                seed_assistant_tool_round(&mut h, &cid, &[("waiting-stream", "wait")]);
                let update = Event::ToolOutputReported(tau_proto::ToolOutputReport {
                    call_id: "background-stream".into(),
                    tool_name: ToolName::new("streaming"),
                    sequence: 0,
                    text: "visible even with completion-only mask".to_owned(),
                    omitted_bytes: 0,
                });
                if queued_first {
                    h.handle_extension_event_inner(&connection, update.clone())
                        .expect("queued update");
                }
                let mut arguments = vec![(
                    CborValue::Text("complete_only".to_owned()),
                    CborValue::Bool(complete_only),
                )];
                match mode {
                    "exact" => arguments.push((
                        CborValue::Text("tool_call_id".to_owned()),
                        CborValue::Text("background-stream".to_owned()),
                    )),
                    "all" => arguments.push((
                        CborValue::Text("tool_call_ids".to_owned()),
                        CborValue::Array(vec![CborValue::Text("background-stream".to_owned())]),
                    )),
                    "input" => arguments.push((
                        CborValue::Text("timeout_minutes".to_owned()),
                        CborValue::Integer(1.into()),
                    )),
                    "any" => {}
                    _ => unreachable!("closed test modes"),
                }
                let wait = AgentToolCall {
                    call_ref: h.persisted_tool_call_ref(&cid, &"waiting-stream".into()),
                    id: "waiting-stream".into(),
                    name: ToolName::new("wait"),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(arguments),
                };
                h.handle_wait_tool_call(&cid, &wait, ToolName::new("wait"))
                    .expect("install wait");
                if !queued_first {
                    h.handle_extension_event_inner(&connection, update)
                        .expect("live update");
                }
                assert_eq!(
                    tool_result_count(&h, "waiting-stream"),
                    usize::from(!complete_only),
                    "{mode}, queued={queued_first}, complete_only={complete_only}"
                );
                assert_eq!(h.suppresses_tool_update_wakes(&cid), complete_only);
                if !complete_only && mode != "input" {
                    assert!(
                        event_log_contains_any_source(&h, |event| matches!(
                            event,
                            Event::ToolResult(result) if result.call_id.as_str() == "waiting-stream"
                                && result.result == CborValue::Text(
                                    "outcome: interrupted\nreason: partial_tool_output\ncause: background-stream".into())
                        )),
                        "{mode}, queued_first={queued_first}: exact source receipt"
                    );
                }
                let agent = durable_agent_id_for_conversation(&h, &cid);
                let tree = h
                    .session_runtime
                    .agent_store
                    .agent(agent.as_str())
                    .expect("tree");
                let call = tree
                    .tool_output_call(&"background-stream".into(), tree.head())
                    .expect("source declaration");
                assert!(
                    tree.tool_output_page(call, tree.head(), 0)
                        .expect("captured source")
                        .text
                        .contains("visible even with completion-only mask")
                );
                h.handle_extension_event_inner(
                    &connection,
                    Event::ToolResultReported(final_tool_result(
                        "background-stream",
                        "streaming",
                        "completed source",
                    )),
                )
                .expect("actual completion");
                assert_eq!(
                    tool_result_count(&h, "waiting-stream"),
                    1,
                    "{mode}: terminal once"
                );
                assert!(!h.suppresses_tool_update_wakes(&cid), "{mode}: retire mask");
            }
        }
    }
}

/// Excess declarations produce bounded admission controls, not additional
/// capture buffers or effects. These controls are declaration-sized overhead,
/// separate from the eight captured-source context slots.
#[test]
fn excess_stream_declarations_reject_before_effects_with_tiny_controls() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    let ids: Vec<_> = (0..128).map(|index| format!("stream-{index}")).collect();
    start_stream_round(
        &mut h,
        &cid,
        ids.iter().map(|id| (id.as_str(), "streaming")).collect(),
    );
    assert_eq!(h.tool_routing.tool_runtime.output_lanes.len(), 8);
    assert_eq!(h.tool_routing.tool_runtime.output_residencies.len(), 8);
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("records");
    let rejected: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProviderToolError(error) => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(rejected.len(), 120);
    for error in &rejected {
        assert_eq!(
            error.message,
            "Concurrent tool-output capacity is full; wait for a running call before retrying."
        );
        assert!(error.details.is_none());
        assert!(
            error
                .display
                .as_ref()
                .is_none_or(|display| display.args.is_empty())
        );
        assert!(serde_json::to_vec(error).expect("tiny rejection").len() < 1024);
        assert!(
            !h.tool_routing
                .tool_runtime
                .pending_tool_providers
                .contains_key(&error.call_id)
        );
    }
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, Event::ToolOutputUpdated(_)))
    );
    let cold = tau_core::AgentTree::try_from_events(agent, &records).expect("cold rejected round");
    for error in rejected {
        let call = cold
            .tool_output_call(&error.call_id, cold.head())
            .expect("rejected declaration");
        let page = cold
            .tool_output_page(call, cold.head(), 0)
            .expect("rejection control");
        assert!(page.terminal);
        assert!(
            page.text.len() < 512,
            "control must not copy arguments or captured output"
        );
    }
}

/// Runtime removal releases capture slots even if a terminal had left deferred
/// context behind another foreground call.
#[test]
fn removing_stream_owner_releases_its_output_slots() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    start_stream_with_foreground_sibling(&mut h, &cid);
    h.handle_extension_event_inner(
        &crate::test_connection_id("stream-worker"),
        Event::ToolResultReported(final_tool_result(
            "stream-call",
            "streaming",
            "deferred tail",
        )),
    )
    .expect("stream final");
    assert_eq!(h.tool_routing.tool_runtime.output_residencies.len(), 1);
    h.remove_agent(&cid);
    assert!(h.tool_routing.tool_runtime.output_residencies.is_empty());
    assert!(h.tool_routing.tool_runtime.output_lanes.is_empty());
}

/// A later completion cannot discard an accepted report while its canonical
/// append waits for capacity. Retry reuses interceptor approval exactly once.
#[test]
fn rejected_stream_head_retries_before_terminal_without_reinterception() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    start_stream_with_foreground_sibling(&mut h, &cid);
    let connection = crate::test_connection_id("stream-worker");
    let _interceptor = connect_test_tool(&mut h, "output-interceptor");
    h.handle_extension_event(
        "output-interceptor",
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::TOOL_OUTPUT_UPDATED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("intercept canonical output");
    h.handle_extension_event_inner(
        &connection,
        Event::ToolOutputReported(tau_proto::ToolOutputReport {
            call_id: "stream-call".into(),
            tool_name: ToolName::new("streaming"),
            sequence: 0,
            text: "retained before completion".to_owned(),
            omitted_bytes: 0,
        }),
    )
    .expect("report output");
    assert!(h.runtime_io.publication.pending_intercept.is_some());
    reject_next_semantic_admission(&h);
    h.handle_extension_event(
        "output-interceptor",
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("approve rejected output");
    h.handle_extension_event_inner(
        &connection,
        Event::ToolResultReported(final_tool_result(
            "stream-call",
            "streaming",
            "terminal after retained head",
        )),
    )
    .expect("report terminal");
    h.handle_extension_event_inner(
        &connection,
        Event::ToolOutputReported(tau_proto::ToolOutputReport {
            call_id: "stream-call".into(),
            tool_name: ToolName::new("streaming"),
            sequence: 1,
            text: "late report is not accepted".to_owned(),
            omitted_bytes: 0,
        }),
    )
    .expect("late report");
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("blocked records");
    assert!(!records.iter().any(|record| matches!(&record.event,
        Event::ProviderToolResult(result) if result.call_id.as_str() == "stream-call"
    )));
    h.retry_tool_outputs(&cid);
    assert!(
        h.runtime_io.publication.pending_intercept.is_none(),
        "retry must not reintercept"
    );
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("accepted records");
    let ordered: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolOutputUpdated(update) if update.output.call_id.as_str() == "stream-call" => {
                Some("output")
            }
            Event::ProviderToolResult(result) if result.call_id.as_str() == "stream-call" => {
                Some("terminal")
            }
            _ => None,
        })
        .collect();
    assert_eq!(ordered, ["output", "terminal"]);
    let reference = records
        .iter()
        .find_map(|record| match &record.event {
            Event::ToolOutputUpdated(update) => Some(update.call),
            _ => None,
        })
        .expect("accepted source");
    let cold = tau_core::AgentTree::try_from_events(agent, &records).expect("cold output");
    let page = cold
        .tool_output_page(reference, cold.head(), 0)
        .expect("recover retained output");
    assert_eq!(page.text.matches("retained before completion").count(), 1);
    assert_eq!(page.text.matches("terminal after retained head").count(), 1);
    assert!(!page.text.contains("late report"));
    assert!(
        page.text.find("retained before completion")
            < page.text.find("terminal after retained head")
    );
}

/// Completion cannot reuse output capacity while another native call still
/// prevents its accepted source occurrences from entering the context.
#[test]
fn streamed_terminal_holds_residency_until_foreground_sibling_closes() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let cid = ensure_test_user_agent(&mut h);
    let agent = durable_agent_id_for_conversation(&h, &cid);
    start_stream_with_foreground_sibling(&mut h, &cid);
    let connection = crate::test_connection_id("stream-worker");
    let reference = h
        .persisted_tool_call_ref(&cid, &"stream-call".into())
        .expect("stream declaration");
    assert_eq!(h.tool_routing.tool_runtime.output_residencies.len(), 1);
    h.handle_extension_event_inner(
        &connection,
        Event::ToolOutputReported(tau_proto::ToolOutputReport {
            call_id: "stream-call".into(),
            tool_name: ToolName::new("streaming"),
            sequence: 0,
            text: "first accepted bytes".to_owned(),
            omitted_bytes: 0,
        }),
    )
    .expect("output");
    h.handle_extension_event_inner(
        &connection,
        Event::ToolResultReported(final_tool_result(
            "stream-call",
            "streaming",
            "final diagnostic",
        )),
    )
    .expect("stream terminal");
    assert!(
        !h.tool_routing
            .tool_runtime
            .output_lanes
            .contains_key("stream-call")
    );
    assert_eq!(h.tool_routing.tool_runtime.output_residencies.len(), 1);
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("deferred records");
    let cold =
        tau_core::AgentTree::try_from_events(agent.clone(), &records).expect("cold deferred");
    assert!(cold.tool_output_needs_residency(reference));
    let page = cold
        .tool_output_page(reference, cold.head(), 0)
        .expect("deferred page");
    assert!(page.terminal);
    assert!(page.text.contains("first accepted bytes"));
    assert!(page.text.contains("final diagnostic"));
    // Cold custody blocks new captures without minting slots or processes.
    h.tool_routing.tool_runtime.output_residencies.clear();
    let session = h.session_runtime.current_session_id.clone();
    h.restore_tool_output_residencies(&session);
    assert!(h.tool_routing.tool_runtime.output_residencies.is_empty());
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .restored_tool_output_debt
    );
    h.handle_extension_event_inner(
        &connection,
        Event::ToolResultReported(final_tool_result(
            "block-call",
            "blocking",
            "sibling complete",
        )),
    )
    .expect("sibling terminal");
    assert!(h.tool_routing.tool_runtime.output_residencies.is_empty());
    assert!(
        !h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .restored_tool_output_debt
    );
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent.as_str())
        .expect("closed records");
    let cold = tau_core::AgentTree::try_from_events(agent, &records).expect("cold closed");
    assert!(!cold.tool_output_needs_residency(reference));
    assert_eq!(
        cold.tool_output_page(reference, cold.head(), 0)
            .expect("closed page"),
        page
    );
}

/// A provider cannot grant itself streaming authority. A silent opted-in call
/// keeps its frozen policy across registry changes and runtime-free repair.
#[test]
fn silent_stream_policy_is_frozen_and_repair_uses_durable_declaration() {
    for streamed in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = echo_harness(td.path().join("state")).expect("start");
        h.config.selected_model = Some("test/model".into());
        connect_ready_configured_extension(
            &mut h,
            "stream-worker",
            "stream-worker",
            tau_proto::ClientKind::Tool,
        );
        let connection = crate::test_connection_id("stream-worker");
        let mut spec = shared_test_tool_spec("silent_work");
        if streamed {
            spec.tags
                .push(tau_proto::ToolTag::new(tau_proto::TOOL_OUTPUT_STREAM_TAG));
        }
        h.tool_routing.registry.register(&connection, spec.clone());
        let cid = ensure_test_user_agent(&mut h);
        let agent = durable_agent_id_for_conversation(&h, &cid);
        let prompt = test_agent_prompt_id("silent-stream");
        seed_agent_thinking(&mut h, &cid, prompt.as_str());
        h.prompt_coordination
            .prompt_runtime
            .agents
            .insert(prompt.clone(), cid.clone());
        h.prompt_coordination
            .prompt_runtime
            .tool_specs
            .insert(prompt.clone(), vec![spec]);
        let mut response = provider_text_response(&prompt, agent.clone(), "");
        response.output_items = vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "silent-call".into(),
            name: ToolName::new("silent_work"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })];
        response.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
        // The untrusted claim is deliberately opposite to the frozen spec.
        response.streamed_tool_items = if streamed { Vec::new() } else { vec![0] };
        h.handle_provider_response_finished(response)
            .expect("declare silent call");
        let reference = h
            .persisted_tool_call_ref(&cid, &"silent-call".into())
            .expect("durable declaration");
        assert_eq!(
            h.tool_call_streams_output(&cid, &"silent-call".into()),
            streamed
        );
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent.as_str())
            .expect("records");
        let cold = tau_core::AgentTree::try_from_events(agent.clone(), &records)
            .expect("cold declaration");
        assert_eq!(cold.tool_output_is_streamed(reference), streamed);
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, Event::ToolOutputUpdated(_)))
        );

        h.tool_routing.registry.unregister_connection(&connection);
        h.tool_routing.tool_runtime.output_lanes.clear();
        let session = h.session_runtime.current_session_id.clone();
        assert_eq!(h.repair_restored_foreground_tool_calls(&session), 1);
        let records = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent.as_str())
            .expect("repaired records");
        let errors: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                Event::ProviderToolError(error) if error.call_id.as_str() == "silent-call" => {
                    Some(error)
                }
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].presentation == tau_proto::ToolResultPresentation::ToolStreamReceipt,
            streamed,
        );
        let cold = tau_core::AgentTree::try_from_events(agent, &records).expect("cold repair");
        let page = cold
            .tool_output_page(reference, cold.head(), 0)
            .expect("retained output");
        assert_eq!(!page.text.is_empty(), streamed);
        assert!(page.terminal);
        assert_eq!(h.repair_restored_foreground_tool_calls(&session), 0);
    }
}

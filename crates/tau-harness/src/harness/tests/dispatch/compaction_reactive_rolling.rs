//! Prefix fitting uses backend rejection; fresh ordinary inference owns
//! recurrence.

use super::*;

/// Build one clean local-summary length terminal without publishing context.
fn local_summary_length_response(
    prompt: &tau_proto::AgentPromptCreated,
    text: &str,
) -> ProviderResponseFinished {
    let mut response =
        provider_text_response(&prompt.agent_prompt_id, prompt.agent_id.clone(), text);
    response.originator = prompt.originator.clone();
    response.stop_reason = tau_proto::ProviderStopReason::Length;
    response.backend = Some(tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::ChatCompletions,
        base_url: "http://localhost/v1".to_owned(),
        transport: Default::default(),
        stale_chain_fallback: false,
    });
    response
}

/// Cancellation owns the summary even when a delayed output-limit terminal
/// arrives afterward; neither that draft nor another successor may be
/// published.
#[test]
fn local_summary_length_cancel_rejects_late_successor_authority() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_backend_capacity_compaction(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    append_capacity_history(&mut h, &cid, "history");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation".to_owned()))
        .expect("inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("recovery");
    let first = read_nth_prompt_created(&h, 1);
    h.handle_provider_response_finished(local_summary_length_response(&first, "draft"))
        .expect("continue");
    let second = read_nth_prompt_created(&h, 2);
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(second.agent_id.clone()),
            agent_prompt_id: Some(second.agent_prompt_id.clone()),
        },
    );
    h.handle_provider_response_finished(local_summary_length_response(&second, "late draft"))
        .expect("late terminal");
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        3
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AgentCompacted(_)))
    );
    h.shutdown().expect("shutdown");
}

/// Repeated output limits preserve one original cut and grouped provisional
/// output; a later canonical capacity rejection discards that chain and
/// retreats.
#[test]
fn local_summary_length_chain_replays_then_retreats_original_prefix() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_backend_capacity_compaction(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    append_capacity_history(&mut h, &cid, "old-A");
    append_capacity_history(&mut h, &cid, "old-B");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation".to_owned()))
        .expect("inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("recovery");
    let original = read_nth_prompt_created(&h, 1);
    for index in 1..=2 {
        let prompt = read_nth_prompt_created(&h, index);
        assert_eq!(prompt.context, original.context);
        assert_eq!(prompt.local_summary_continuation.len(), index - 1);
        let mut response = provider_text_response(
            &prompt.agent_prompt_id,
            prompt.agent_id.clone(),
            &format!("fragment-{index}"),
        );
        response.originator = prompt.originator.clone();
        response.stop_reason = tau_proto::ProviderStopReason::Length;
        response.backend = Some(tau_proto::ProviderBackend {
            kind: tau_proto::ProviderBackendKind::ChatCompletions,
            base_url: "http://localhost/v1".to_owned(),
            transport: Default::default(),
            stale_chain_fallback: false,
        });
        h.handle_provider_response_finished(response)
            .expect("length successor");
    }
    let continuation = read_nth_prompt_created(&h, 3);
    assert_eq!(continuation.local_summary_continuation.len(), 2);
    let events = event_log_events(&h);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AgentCompacted(_)))
    );
    assert_eq!(events.iter().filter(|event| matches!(event,
        Event::AgentStandaloneCompactionFailed(failed)
            if failed.reason == tau_proto::StandaloneCompactionFailureReason::OutputLengthExceeded
                && failed.output_length_continuation.is_some()
    )).count(), 2);
    let tree = h
        .session_runtime
        .agent_store
        .agent(continuation.agent_id.as_str())
        .expect("tree");
    assert_eq!(
        tree.local_summary_continuation_responses(&continuation.agent_prompt_id),
        continuation
            .local_summary_continuation
            .iter()
            .map(|step| step.response.clone())
            .collect::<Vec<_>>(),
    );
    let records = h
        .session_runtime
        .agent_store
        .agent_events(continuation.agent_id.as_str())
        .expect("records");
    let cold = tau_core::AgentTree::from_events(continuation.agent_id.clone(), &records);
    assert_eq!(
        cold.local_summary_continuation_responses(&continuation.agent_prompt_id),
        tree.local_summary_continuation_responses(&continuation.agent_prompt_id),
    );
    h.handle_provider_response_finished(context_overflow_response(&continuation))
        .expect("retreat");
    let retreated = read_nth_prompt_created(&h, 4);
    assert!(retreated.local_summary_continuation.is_empty());
    assert_ne!(retreated.context, original.context);
    assert!(
        !serde_json::to_string(&retreated.context)
            .expect("context")
            .contains("fragment-")
    );
    h.handle_provider_response_finished(provider_text_response(
        &retreated.agent_prompt_id,
        retreated.agent_id.clone(),
        "completed-summary",
    ))
    .expect("summary success");
    assert_eq!(
        read_nth_prompt_created(&h, 5).operation,
        tau_proto::PromptOperation::Inference
    );
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentCompacted(_)))
            .count(),
        1
    );
    h.shutdown().expect("shutdown");
}

/// A crash after the committed length failure repairs its reserved successor
/// once; a second restart must not redispatch that already-started summary.
#[test]
fn local_summary_length_restart_claims_unstarted_successor_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, started, partial);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_backend_capacity_compaction(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
        append_capacity_history(&mut h, &cid, "old-A");
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation".to_owned()))
            .expect("inference");
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("recovery");
        let prompt = read_nth_prompt_created(&h, 1);
        started = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentStandaloneCompactionStarted(started) => Some(started),
                _ => None,
            })
            .expect("start");
        partial = provider_text_response(&prompt.agent_prompt_id, prompt.agent_id, "provisional");
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let plan = tau_proto::LocalSummaryContinuationPlan {
        transaction_id: tau_proto::CompactionTransactionId::parse("ct-length-successor")
            .expect("id"),
        compact_prompt_id: tau_proto::AgentPromptId::parse("ap-length-successor").expect("id"),
    };
    let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
                agent_id: agent_id.clone(),
                transaction_id: started.transaction_id.clone(),
                cut: started.cut,
                resume_through: started.resume_through,
                reason: tau_proto::StandaloneCompactionFailureReason::OutputLengthExceeded,
                context_retreat: None,
                output_length_continuation: Some(plan.clone()),
                incomplete_response: Some(Box::new(tau_proto::StandaloneCompactionIncomplete {
                    agent_prompt_id: started.compact_prompt_id,
                    output_items: partial.output_items,
                    usage: None,
                    provider_response_id: None,
                    provider_attempt: Default::default(),
                    backend: tau_proto::ProviderBackend {
                        kind: tau_proto::ProviderBackendKind::ChatCompletions,
                        base_url: "http://localhost/v1".to_owned(),
                        transport: Default::default(),
                        stale_chain_fallback: false,
                    },
                })),
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("failure crash cut");
    drop(store);
    for restart in 0..2 {
        let mut resumed =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let events = event_log_events(&resumed);
        let starts = events.iter().filter(|event| matches!(event,
            Event::AgentStandaloneCompactionStarted(start) if start.transaction_id == plan.transaction_id
        )).count();
        assert_eq!(starts, usize::from(restart == 0));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::AgentCompacted(_)))
        );
        resumed.shutdown().expect("shutdown");
        wait_for_session_unlock(&state, "s1");
    }
}

/// Locate a provider request with compact, content-free recovery diagnostics.
fn read_nth_prompt_created(h: &Harness, index: usize) -> tau_proto::AgentPromptCreated {
    let events = event_log_events(h);
    let prompt = events
        .iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) => Some(prompt.clone()),
            _ => None,
        })
        .nth(index);
    assert!(
        prompt.is_some(),
        "missing prompt {index}; recovery={:?}",
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(_)
                    | Event::AgentStandaloneCompactionFailed(_)
                    | Event::AgentInferenceDispatchStarted(_)
            ))
            .collect::<Vec<_>>()
    );
    prompt.expect("checked prompt")
}

/// Enable standalone work without pretending the harness knows token size.
fn enable_backend_capacity_compaction(h: &mut Harness) {
    enable_remote_compaction_for_test_model(h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = None;
    info.standalone_compaction_prefix_budget = None;
}

/// Append eligible history without independently starting inference.
fn append_capacity_history(h: &mut Harness, cid: &AgentId, text: &str) {
    let agent_id = durable_agent_id_for_conversation(h, cid);
    h.publish_for_agent(
        cid,
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
        }),
    );
}

/// Without a byte cap, the first attempt includes all eligible current input,
/// rather than protecting the activation or an obsolete original target.
#[test]
fn reactive_context_overflow_without_byte_budget_dispatches_whole_context() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_backend_capacity_compaction(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    append_capacity_history(&mut h, &cid, "old-A");
    append_capacity_history(&mut h, &cid, "old-B");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow activation".to_owned()))
        .expect("inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("recovery");
    let compact = read_nth_prompt_created(&h, 1);
    let context = serde_json::to_string(&compact.context).expect("context");
    for marker in ["old-A", "old-B", "overflow activation"] {
        assert!(context.contains(marker), "missing {marker}");
    }
    h.shutdown().expect("shutdown");
}

/// A synthetic backend independently measures complete requests. Each success
/// returns to ordinary inference, whose fresh rejection can authorize another
/// pass. Stop with useful original suffix retained once inference fits.
#[test]
fn reactive_capacity_oracle_repeats_only_after_fresh_inference_rejection() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_backend_capacity_compaction(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    for marker in ["oracle-A", "oracle-B", "oracle-C", "oracle-D"] {
        append_capacity_history(&mut h, &cid, marker);
    }
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("oracle activation".to_owned()))
        .expect("inference");
    let mut index = 0;
    let mut summaries = 0;
    let mut ordinary_rejections = 0;
    let mut compact_rejections = 0;
    loop {
        // A test guard, not a production retry budget.
        assert!(index < 30, "capacity fixture did not converge");
        let prompt = read_nth_prompt_created(&h, index);
        let context = serde_json::to_string(&prompt.context).expect("context");
        let units = [
            "oracle-A",
            "oracle-B",
            "oracle-C",
            "oracle-D",
            "oracle-late",
        ]
        .into_iter()
        .filter(|marker| context.contains(marker))
        .count()
            * 2
            + usize::from(context.contains("oracle-summary-"))
            + usize::from(context.contains("oracle activation"));
        match prompt.operation {
            tau_proto::PromptOperation::Inference if units <= 5 => {
                assert_eq!(summaries, 3);
                assert_eq!(ordinary_rejections, 3);
                assert!(compact_rejections > 0);
                assert!(
                    context.contains("oracle-late"),
                    "useful unsummarized suffix survives"
                );
                assert!(context.matches("oracle activation").count() <= 1);
                h.handle_provider_response_finished(provider_text_response(
                    &prompt.agent_prompt_id,
                    prompt.agent_id,
                    "done",
                ))
                .expect("ordinary success");
                break;
            }
            tau_proto::PromptOperation::Inference => {
                ordinary_rejections += 1;
                h.handle_provider_response_finished(context_overflow_response(&prompt))
                    .expect("fresh inference overflow");
            }
            tau_proto::PromptOperation::StandaloneCompaction if 4 < units => {
                compact_rejections += 1;
                h.handle_provider_response_finished(context_overflow_response(&prompt))
                    .expect("smaller prefix");
            }
            tau_proto::PromptOperation::StandaloneCompaction => {
                summaries += 1;
                if summaries == 1 {
                    append_capacity_history(&mut h, &cid, "oracle-late");
                }
                h.handle_provider_response_finished(provider_text_response(
                    &prompt.agent_prompt_id,
                    prompt.agent_id,
                    &format!("oracle-summary-{summaries}"),
                ))
                .expect("summary");
                let next = read_nth_prompt_created(&h, index + 1);
                assert_eq!(
                    next.operation,
                    tau_proto::PromptOperation::Inference,
                    "success must not drain the suffix or react to arrivals alone"
                );
            }
        }
        index += 1;
    }
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::AgentCompacted(_)))
            .count(),
        summaries
    );
    h.shutdown().expect("shutdown");
}

/// Rejection after restart repairs exactly one planned smaller-prefix attempt;
/// ambiguous provider work is not resent and retained input is still present.
#[test]
fn canonical_standalone_rejection_restart_repairs_retreat_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, rejected, rejected_transaction);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_backend_capacity_compaction(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
        append_capacity_history(&mut h, &cid, "old-A");
        append_capacity_history(&mut h, &cid, "old-B");
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation".to_owned()))
            .expect("inference");
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("recovery");
        rejected = read_nth_prompt_created(&h, 1);
        rejected_transaction = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentStandaloneCompactionStarted(started)
                    if started.compact_prompt_id == rejected.agent_prompt_id =>
                {
                    Some(started.transaction_id)
                }
                _ => None,
            })
            .expect("transaction");
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(context_overflow_response(&rejected)),
            tau_proto::UnixMicros::now(),
        )
        .expect("canonical rejection crash cut");
    drop(store);
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let events = event_log_events(&resumed);
    assert_eq!(events.iter().filter(|event| matches!(event,
        Event::AgentStandaloneCompactionFailed(failed)
            if failed.transaction_id == rejected_transaction && failed.context_retreat.is_some()
    )).count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event,
                Event::AgentStandaloneCompactionStarted(started)
                    if started.supersedes.as_ref() == Some(&rejected_transaction)
            ))
            .count(),
        1
    );
    resumed.shutdown().expect("shutdown");
}

/// A committed partial summary resumes inference after restart rather than
/// rolling to an old target. A later backend rejection can retreat all the way
/// to the summary alone without dropping or resending its preserved suffix.
#[test]
fn partial_compaction_restart_and_replacement_only_retreat_preserve_suffix() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, started, suffix_end);
    {
        let mut h = quiet_standalone_provider_harness_for_with_start_reason_and_storage_mode(
            "s1",
            &state,
            tau_proto::SessionStartReason::Initial,
            crate::HarnessStorageMode::Durable,
        )
        .expect("start");
        enable_backend_capacity_compaction(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
        append_capacity_history(&mut h, &cid, "old-A");
        append_capacity_history(&mut h, &cid, "retained-B");
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("retained activation".to_owned()))
            .expect("inference");
        let mut prompt = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&prompt))
            .expect("recovery");
        let mut index = 1;
        loop {
            prompt = read_nth_prompt_created(&h, index);
            let text = serde_json::to_string(&prompt.context).expect("context");
            if !text.contains("retained-B") {
                break;
            }
            h.handle_provider_response_finished(context_overflow_response(&prompt))
                .expect("retreat");
            index += 1;
            assert!(index < 10);
        }
        started = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentStandaloneCompactionStarted(started)
                    if started.compact_prompt_id == prompt.agent_prompt_id =>
                {
                    Some(started)
                }
                _ => None,
            })
            .expect("prefix transaction");
        suffix_end = h.selected_head_for_agent(&cid).expect("selected head");
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                agent_id: agent_id.clone(),
                transaction_id: Some(started.transaction_id),
                compact_prompt_id: Some(started.compact_prompt_id.clone()),
                model: Some(started.model),
                operation: Some(started.operation),
                cut: Some(started.cut),
                suffix_end: Some(suffix_end),
                original_input_tokens: None,
                compaction_output_tokens: None,
                replacement_window: provider_text_response(
                    &started.compact_prompt_id,
                    agent_id.clone(),
                    "large summary",
                )
                .output_items,
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("success before checkpoint crash cut");
    drop(store);
    let mut h = quiet_standalone_provider_harness_for_with_start_reason_and_storage_mode(
        "s1",
        &state,
        tau_proto::SessionStartReason::Resume,
        crate::HarnessStorageMode::Durable,
    )
    .expect("resume");
    enable_backend_capacity_compaction(&mut h);
    let inference = read_nth_prompt_created(&h, 0);
    assert_eq!(inference.operation, tau_proto::PromptOperation::Inference);
    let text = serde_json::to_string(&inference.context).expect("context");
    for marker in ["large summary", "retained-B", "retained activation"] {
        assert_eq!(text.matches(marker).count(), 1, "{marker}");
    }
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("fresh overflow");
    let mut index = 1;
    loop {
        let compact = read_nth_prompt_created(&h, index);
        let text = serde_json::to_string(&compact.context).expect("context");
        assert!(text.contains("large summary"));
        if !text.contains("retained-B") && !text.contains("retained activation") {
            h.handle_provider_response_finished(provider_text_response(
                &compact.agent_prompt_id,
                compact.agent_id,
                "small summary",
            ))
            .expect("replacement-only success");
            break;
        }
        h.handle_provider_response_finished(context_overflow_response(&compact))
            .expect("retreat");
        index += 1;
        assert!(index < 15);
    }
    let next = read_nth_prompt_created(&h, index + 1);
    assert_eq!(next.operation, tau_proto::PromptOperation::Inference);
    let text = serde_json::to_string(&next.context).expect("context");
    assert!(!text.contains("large summary"));
    for marker in ["small summary", "retained-B", "retained activation"] {
        assert_eq!(text.matches(marker).count(), 1, "{marker}");
    }
    let records = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("records");
    let cold = tau_core::AgentTree::from_events(agent_id.clone(), &records);
    let live = h
        .session_runtime
        .agent_store
        .agent(agent_id.as_str())
        .expect("live tree");
    let cold_context = crate::prompt::assemble_prompt_context_from(&cold, cold.head()).context;
    assert_eq!(
        cold_context,
        crate::prompt::assemble_prompt_context_from(live, live.head()).context
    );
    let cold_text = serde_json::to_string(&cold_context).expect("cold context");
    for marker in ["small summary", "retained-B", "retained activation"] {
        assert_eq!(cold_text.matches(marker).count(), 1, "{marker}");
    }
    h.shutdown().expect("shutdown");
}

/// Idle explicit requests have no resume watermark. Their immutable active
/// window still includes the installed summary when the chosen suffix cut is
/// physically older than that summary, both live and after restart.
#[test]
fn idle_explicit_compaction_anchors_logical_suffix_at_start_parent() {
    for kind in ["ui", "cross"] {
        for restart in [false, true] {
            let td = TempDir::new().expect("tempdir");
            let state = td.path().join("state");
            let mut h = quiet_provider_harness(&state).expect("harness");
            enable_backend_capacity_compaction(&mut h);
            let mut caller = ensure_test_user_agent(&mut h);
            let caller_id = durable_agent_id_for_conversation(&h, &caller);
            let (mut target, target_id) = if kind == "cross" {
                install_manual_compaction_target(&mut h, "anchor-target")
            } else {
                (caller.clone(), caller_id.clone())
            };
            append_capacity_history(&mut h, &target, "removed original");
            let cut = h.selected_head_for_agent(&target).expect("prefix cut");
            append_capacity_history(&mut h, &target, "preserved suffix");
            let suffix = h.selected_head_for_agent(&target).expect("suffix");
            h.publish_for_agent(
                &target,
                Event::AgentStandaloneCompactionStarted(
                    tau_proto::AgentStandaloneCompactionStarted {
                        agent_id: target_id.clone(),
                        transaction_id: tau_proto::CompactionTransactionId::parse("ct-anchor-seed")
                            .expect("transaction"),
                        compact_prompt_id: "ap-anchor-seed".parse().expect("prompt"),
                        cut,
                        resume_through: None,
                        model: "test/model".into(),
                        operation: tau_proto::PromptOperation::StandaloneCompaction,
                        originator: tau_proto::PromptOriginator::User,
                        supersedes: None,
                        trigger: tau_proto::StandaloneCompactionTrigger::Manual,
                    },
                ),
            );
            let first = read_nth_prompt_created(&h, 0);
            h.handle_provider_response_finished(provider_text_response(
                &first.agent_prompt_id,
                first.agent_id,
                "installed summary",
            ))
            .expect("install partial summary");
            if restart {
                h.shutdown().expect("shutdown before explicit request");
                wait_for_session_unlock(&state, "s1");
                h = quiet_provider_harness_with_start_reason(
                    &state,
                    tau_proto::SessionStartReason::Resume,
                )
                .expect("restart");
                enable_backend_capacity_compaction(&mut h);
                caller = h
                    .runtime_agent_id_for_target_agent(Some(caller_id.as_str()))
                    .expect("caller");
                target = h
                    .runtime_agent_id_for_target_agent(Some(target_id.as_str()))
                    .expect("target");
            }
            let boundary = h.selected_head_for_agent(&target).expect("boundary");
            if kind == "cross" {
                let call = register_manual_cross_compaction_call(&mut h, &caller, "call-anchor");
                h.request_agent_tool_compaction(
                    &caller,
                    &call,
                    ToolName::new("agent_compact"),
                    Some(&target_id),
                );
            } else {
                h.handle_compact_request(
                    crate::harness::harness_connection_id(),
                    test_session_id("s1"),
                    Some(target_id.as_str()),
                );
            }
            let started = event_log_events(&h)
                .into_iter()
                .filter_map(|event| match event {
                    Event::AgentStandaloneCompactionStarted(started)
                        if started.agent_id == target_id =>
                    {
                        Some(started)
                    }
                    _ => None,
                })
                .next_back()
                .expect("explicit start");
            assert_eq!(started.cut, suffix, "{kind} restart={restart}");
            assert_eq!(started.resume_through, None);
            let prompt = event_log_events(&h)
                .into_iter()
                .find_map(|event| match event {
                    Event::AgentPromptCreated(prompt)
                        if prompt.agent_prompt_id == started.compact_prompt_id =>
                    {
                        Some(prompt)
                    }
                    _ => None,
                })
                .expect("explicit prompt");
            let text = serde_json::to_string(&prompt.context).expect("context");
            assert!(
                !text.contains("removed original"),
                "{kind} restart={restart}"
            );
            for marker in ["installed summary", "preserved suffix"] {
                assert_eq!(
                    text.matches(marker).count(),
                    1,
                    "{marker}: {kind} restart={restart}"
                );
            }
            let records = h
                .session_runtime
                .agent_store
                .agent_events(target_id.as_str())
                .expect("records");
            let cold = tau_core::AgentTree::from_events(target_id, &records);
            assert_eq!(
                cold.standalone_compaction_active_head(&started.transaction_id),
                Some(boundary)
            );
            let cold_context = crate::prompt::assemble_prompt_context_prefix_from(
                &cold,
                boundary.as_option(),
                suffix,
            )
            .expect("cold logical prefix")
            .context;
            let text = serde_json::to_string(&cold_context).expect("cold context");
            assert!(!text.contains("removed original"));
            for marker in ["installed summary", "preserved suffix"] {
                assert_eq!(text.matches(marker).count(), 1);
            }
            h.shutdown().expect("shutdown");
        }
    }
}

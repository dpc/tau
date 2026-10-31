//! Unusable summaries retreat safely without poisoning future inference.

use super::*;

/// Construct the narrow adapter terminal; private reasoning never crosses here.
fn unusable_summary(prompt: &tau_proto::AgentPromptCreated) -> ProviderResponseFinished {
    let mut response = context_overflow_response(prompt);
    response.failure_kind = Some(tau_proto::ProviderFailureKind::LocalSummaryUnusable);
    response.error = Some("summary compactor did not return exactly one message".to_owned());
    response.backend = Some(tau_proto::ProviderBackend {
        kind: tau_proto::ProviderBackendKind::ChatCompletions,
        base_url: "http://localhost/v1".to_owned(),
        transport: Default::default(),
        stale_chain_fallback: false,
    });
    response
}

/// Configure a synthetic standalone backend with no proactive threshold.
fn enable_summary(h: &mut Harness) {
    enable_remote_compaction_for_test_model(h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_prefix_budget = None;
}

/// Read the latest provider request, including its exact standalone cut owner.
fn latest_prompt(h: &Harness) -> tau_proto::AgentPromptCreated {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentPromptCreated(prompt) => Some(prompt),
            _ => None,
        })
        .last()
        .expect("prompt")
}

/// Derive counts only from canonical start facts, not transient provider
/// updates.
fn starts(h: &Harness) -> Vec<tau_proto::AgentStandaloneCompactionStarted> {
    event_log_events(h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some(started),
            _ => None,
        })
        .collect()
}

/// A reasoning-only summary now reaches the smaller-prefix path. Capacity and
/// length successors cannot reset its four-successor allowance; exhaustion
/// blocks polling and ordinary inference until explicit recovery.
#[test]
fn unusable_summary_budget_survives_mixed_chain_and_requires_explicit_recovery() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("start");
    enable_summary(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    for _ in 0..12 {
        seed_reactive_compaction_prefix(&mut h, &cid);
    }
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let original = starts(&h)[0].clone();
    for successor in 0..=tau_proto::LOCAL_SUMMARY_UNUSABLE_MAX_SUCCESSORS {
        let compact = latest_prompt(&h);
        assert_eq!(
            compact.operation,
            tau_proto::PromptOperation::StandaloneCompaction
        );
        let before = starts(&h);
        let current = before.last().expect("current").clone();
        let response = unusable_summary(&compact);
        h.handle_provider_response_finished(response.clone())
            .expect("unusable");
        h.handle_provider_response_finished(response)
            .expect("duplicate terminal");
        if successor == tau_proto::LOCAL_SUMMARY_UNUSABLE_MAX_SUCCESSORS {
            assert_eq!(starts(&h).len(), before.len(), "allowance exhausted");
            break;
        }
        let next = starts(&h).last().expect("retreat").clone();
        assert_eq!(starts(&h).len(), before.len() + 1);
        assert_ne!(next.cut, current.cut);
        assert_eq!(next.resume_through, original.resume_through);
        assert_eq!(next.model, original.model);
        assert!(
            h.session_runtime
                .agent_store
                .agent(agent_id.as_str())
                .expect("tree")
                .is_ancestor_head(next.cut, current.cut)
        );
        if successor == 0 {
            h.handle_provider_response_finished(context_overflow_response(&latest_prompt(&h)))
                .expect("capacity retreat");
            let prompt = latest_prompt(&h);
            let mut length =
                provider_text_response(&prompt.agent_prompt_id, prompt.agent_id.clone(), "draft");
            length.stop_reason = tau_proto::ProviderStopReason::Length;
            length.backend = unusable_summary(&prompt).backend;
            let count = starts(&h).len();
            h.handle_provider_response_finished(length)
                .expect("length continuation");
            assert_eq!(starts(&h).len(), count + 1, "same-cut continuation");
        }
        let events = h
            .session_runtime
            .agent_store
            .snapshot_agent_events_for_test(agent_id.as_str())
            .expect("records");
        let cold =
            tau_core::AgentTree::try_from_events(agent_id.clone(), &events).expect("cold replay");
        assert_eq!(
            cold.local_summary_unusable_can_retreat(
                &starts(&h).last().expect("start").transaction_id
            ),
            successor + 1 < tau_proto::LOCAL_SUMMARY_UNUSABLE_MAX_SUCCESSORS,
        );
    }
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentCompacted(_))),
        0
    );
    let records = h
        .session_runtime
        .agent_store
        .snapshot_agent_events_for_test(agent_id.as_str())
        .expect("records");
    let failure_index = records
        .iter()
        .rposition(|r| matches!(r.event, Event::AgentStandaloneCompactionFailed(_)))
        .expect("failure");
    let mut cold =
        tau_core::AgentTree::try_from_events(agent_id.clone(), &records[..failure_index])
            .expect("before final failure");
    let mut forged = records[failure_index].clone();
    let Event::AgentStandaloneCompactionFailed(failed) = &mut forged.event else {
        unreachable!()
    };
    let last = starts(&h).last().expect("last start").clone();
    let roll_through = match last.trigger {
        tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
            roll_through, ..
        } => roll_through,
        _ => panic!("last start must be a retreat"),
    };
    failed.context_retreat = Some(tau_proto::ContextRetreatPlan {
        transaction_id: tau_proto::CompactionTransactionId::parse("ct-forged-extra").expect("id"),
        compact_prompt_id: test_agent_prompt_id("ap-forged-extra"),
        cut: cold
            .previous_provider_closed_cut_in_active_window(
                cold.head()
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                last.cut,
            )
            .expect("still reducible"),
        roll_through,
        model: last.model,
        originator: last.originator,
        resume_through: last.resume_through,
    });
    assert!(
        cold.apply_persisted_record(&forged).is_err(),
        "core must reject a fifth unusable-authorized successor"
    );
    let count = starts(&h).len();
    for _ in 0..3 {
        h.reconcile_pending_context_recoveries(true);
        h.drain_publish_idle_dispatches();
        assert!(!h.schedule_standalone_auto_compaction(&cid));
    }
    assert_eq!(starts(&h).len(), count);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("fresh activation".to_owned()))
        .expect("inference");
    assert!(h.dispatch_blocked_for(&cid));
    assert_eq!(starts(&h).len(), count, "ingress alone cannot rearm");
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    assert_eq!(starts(&h).len(), count + 1);
    let new = starts(&h).last().expect("new episode").clone();
    assert!(
        h.session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .expect("tree")
            .local_summary_unusable_can_retreat(&new.transaction_id)
    );
    let compact = latest_prompt(&h);
    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id,
        "valid summary",
    ))
    .expect("success");
    h.drain_publish_idle_dispatches();
    h.try_advance_queue();
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::AgentCompacted(_))),
        1
    );
    let resumed = latest_prompt(&h);
    assert_eq!(resumed.operation, tau_proto::PromptOperation::Inference);
    h.handle_provider_response_finished(provider_text_response(
        &resumed.agent_prompt_id,
        resumed.agent_id,
        "done",
    ))
    .expect("finish");
    assert!(
        h.matching_durable_failed_recovery(
            agent_id.as_str(),
            &"test/model".into(),
            h.selected_head_for_agent(&cid).expect("head")
        )
        .is_none()
    );
    h.shutdown().expect("shutdown");
}

/// Retreat must cross an entire tool round, retain its exact call/result
/// suffix, and resume the owed inference once without running either tool
/// again.
#[test]
fn unusable_summary_retreat_preserves_closed_tool_round_suffix() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path()).expect("start");
    enable_summary(&mut h);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    seed_reactive_compaction_prefix(&mut h, &cid);
    let prefix = h.selected_head_for_agent(&cid).expect("prefix");
    let mut calls = provider_text_response(&test_agent_prompt_id("ap-tool-history"), agent_id, "");
    calls.stop_reason = tau_proto::ProviderStopReason::ToolCalls;
    calls.output_items = vec![ContextItem::ToolCall(ToolCallItem {
        call_id: "call-kept".into(),
        name: ToolName::new("kept_tool"),
        tool_type: tau_proto::ToolType::Custom,
        arguments: CborValue::Text("kept input".to_owned()),
        raw_arguments_json: None,
        responses_envelope: None,
    })];
    h.publish_for_agent(&cid, Event::ProviderResponseFinished(calls));
    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-kept".into(),
            tool_name: ToolName::new("kept_tool"),
            tool_type: tau_proto::ToolType::Custom,
            result: CborValue::Text("kept result".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    h.dispatch_activation_after_publish_idle(&cid);
    let inference = latest_prompt(&h);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("overflow");
    let compact = latest_prompt(&h);
    let first = starts(&h)[0].clone();
    let original_round: Vec<_> = compact
        .context
        .flatten_iter()
        .filter(|item| matches!(item, ContextItem::ToolCall(_) | ContextItem::ToolResult(_)))
        .collect();
    assert_eq!(original_round.len(), 2);
    h.handle_provider_response_finished(unusable_summary(&compact))
        .expect("retreat");
    // The discovery message and empty rejected inference each supply a closed
    // boundary. Cross those before the indivisible round; no cut may split it.
    for _ in 0..3 {
        if starts(&h).last().expect("retreat").cut == prefix {
            break;
        }
        let compact = latest_prompt(&h);
        let input = serde_json::to_string(&compact.context).expect("input");
        assert!(input.contains("kept input") && input.contains("kept result"));
        h.handle_provider_response_finished(unusable_summary(&compact))
            .expect("retreat past complete tool round");
    }
    let retreated = starts(&h).last().expect("retreat").clone();
    assert_eq!(retreated.cut, prefix, "never split call from result");
    assert_eq!(retreated.resume_through, first.resume_through);
    let compact = latest_prompt(&h);
    let input = serde_json::to_string(&compact.context).expect("input");
    assert!(!input.contains("kept input"));
    assert!(!input.contains("kept result"));
    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id,
        "replacement summary",
    ))
    .expect("success");
    let resumed = latest_prompt(&h);
    assert_eq!(resumed.operation, tau_proto::PromptOperation::Inference);
    let input = serde_json::to_string(&resumed.context).expect("input");
    assert_eq!(input.matches("replacement summary").count(), 1);
    let preserved_round: Vec<_> = resumed
        .context
        .flatten_iter()
        .filter(|item| matches!(item, ContextItem::ToolCall(_) | ContextItem::ToolResult(_)))
        .collect();
    assert_eq!(
        preserved_round, original_round,
        "exactly one unchanged call/result round"
    );
    assert_eq!(
        event_log_count(&h, |e| matches!(e, Event::ProviderToolResult(_))),
        1
    );
    h.shutdown().expect("shutdown");
}

/// The canonical unusable terminal is enough to repair the failure/start cut
/// after restart, while a committed successor dispatch remains ambiguous and is
/// never resent. This protects the normal two-event publication boundary.
#[test]
fn unusable_summary_canonical_rejection_restart_claims_one_retreat() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, rejected, transaction);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_summary(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
        for _ in 0..3 {
            seed_reactive_compaction_prefix(&mut h, &cid);
        }
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(agent_id.as_str()),
        );
        rejected = latest_prompt(&h);
        transaction = starts(&h)[0].transaction_id.clone();
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut store = tau_core::AgentStore::open_fixture(state.join("agents")).expect("store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(unusable_summary(&rejected)),
            tau_proto::UnixMicros::now(),
        )
        .expect("canonical crash cut");
    drop(store);
    let mut h =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert_eq!(
        starts(&h)
            .iter()
            .filter(|s| s.supersedes.as_ref() == Some(&transaction))
            .count(),
        1
    );
    let count = starts(&h).len();
    for _ in 0..3 {
        h.reconcile_pending_context_recoveries(true);
        h.drain_publish_idle_dispatches();
    }
    assert_eq!(starts(&h).len(), count);
    h.shutdown().expect("shutdown");
}

/// Cancellation and publicly accepted semantic output must not be laundered
/// into the new private-summary retry authority, even with a forged typed kind.
#[test]
fn unusable_summary_preserves_cancellation_and_public_progress_guards() {
    for cancel in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut h = quiet_provider_harness(td.path()).expect("start");
        enable_summary(&mut h);
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        for _ in 0..3 {
            seed_reactive_compaction_prefix(&mut h, &cid);
        }
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(agent_id.as_str()),
        );
        let compact = latest_prompt(&h);
        if cancel {
            h.handle_cancel_prompt(
                crate::harness::harness_connection_id(),
                &tau_proto::UiCancelPrompt {
                    session_id: test_session_id("s1"),
                    target_agent_id: Some(compact.agent_id.clone()),
                    agent_prompt_id: Some(compact.agent_prompt_id.clone()),
                },
            );
        } else {
            h.prompt_coordination
                .prompt_runtime
                .semantic_output
                .insert(compact.agent_prompt_id.clone());
        }
        h.handle_provider_response_finished(unusable_summary(&compact))
            .expect("late or unsafe terminal");
        h.drain_publish_idle_dispatches();
        assert_eq!(starts(&h).len(), 1);
        assert_eq!(
            event_log_count(&h, |e| matches!(e, Event::AgentCompacted(_))),
            0
        );
        h.shutdown().expect("shutdown");
    }
}

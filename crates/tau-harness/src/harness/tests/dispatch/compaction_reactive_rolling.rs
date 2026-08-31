//! Focused bounded rolling recovery after provider context rejection.

use super::*;

/// Absence of a byte work cap is not absence of recovery capability: the
/// harness dispatches the exact normalized provider-closed target and lets a
/// canonical provider rejection authorize retreat.
#[test]
fn reactive_context_overflow_without_byte_budget_dispatches_exact_target() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = None;
    info.standalone_compaction_prefix_budget = None;
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    for marker in ["old-A", "old-B"] {
        h.publish_for_agent(
            &cid,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: marker.to_owned(),
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
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow activation".to_owned()))
        .expect("dispatch rejected inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("provider rejection authorizes no-cap recovery");

    let compact = read_nth_prompt_created(&h, 1);
    let context = serde_json::to_string(&compact.context).expect("compact context");
    assert!(context.contains("old-A"));
    assert!(context.contains("old-B"));
    assert!(!context.contains("overflow activation"));
    assert!(event_log_events(&h).iter().all(|event| !matches!(
        event,
        Event::AgentStandaloneCompactionFailed(failed)
            if failed.reason == tau_proto::StandaloneCompactionFailureReason::RouteFailed
    )));
    h.shutdown().expect("shutdown");
}

/// A restart after the canonical standalone rejection but before its typed
/// failure must derive and commit the exact retreat successor once.
#[test]
fn canonical_standalone_rejection_restart_repairs_retreat_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, rejected, rejected_transaction);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = None;
        info.standalone_compaction_prefix_budget = None;
        let cid = ensure_test_user_agent(&mut h);
        agent_id = durable_agent_id_for_conversation(&h, &cid);
        for marker in ["old-A", "old-B"] {
            h.publish_for_agent(
                &cid,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    inference_activation: false,
                    agent_id: agent_id.clone(),
                    text: marker.to_owned(),
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
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("activation".to_owned()))
            .expect("dispatch rejected inference");
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("start reactive compact");
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
            .expect("reactive transaction");
        h.shutdown().expect("shutdown");
    }

    wait_for_session_unlock(&state, "s1");
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::ProviderResponseFinished(context_overflow_response(&rejected)),
            tau_proto::UnixMicros::now(),
        )
        .expect("append canonical crash-cut rejection");
    drop(store);

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume rejection crash cut");
    let events = event_log_events(&resumed);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.transaction_id == rejected_transaction
                        && failed.reason
                            == tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
                        && failed.context_retreat.is_some()
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        &started.trigger,
                        tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
                            failed_transaction_id,
                            ..
                        } if failed_transaction_id == &rejected_transaction
                    )
            ))
            .count(),
        1
    );
    resumed.shutdown().expect("shutdown");
}

/// Provider-authorized recovery remains independent of proactive token
/// evidence after one partial pass. The reactive chain keeps consuming fitting
/// closed prefixes up to its original activation cut before it retries
/// inference with the activating input retained.
#[test]
fn reactive_context_overflow_rolls_fitting_prefixes_without_token_evidence() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    info.standalone_compaction_prefix_budget = Some(tau_proto::ByteCount::new(1_000));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    for marker in ["old-A", "old-B"] {
        h.publish_for_agent(
            &cid,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: format!("{marker}:{}", "x".repeat(600)),
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
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent");
    agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1));
    agent.execution.context_usage_model = Some("test/model".into());
    agent.execution.context_usage_prompt_id = Some(test_agent_prompt_id("ap-test-provider-usage"));

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow activation".to_owned()))
        .expect("dispatch rejected inference");
    let inference = read_nth_prompt_created(&h, 0);
    assert_eq!(inference.operation, tau_proto::PromptOperation::Inference);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("plan reactive recovery");

    let first = read_nth_prompt_created(&h, 1);
    let first_context = serde_json::to_string(&first.context).expect("first context");
    assert!(first_context.contains("old-A"));
    assert!(!first_context.contains("old-B"));
    h.handle_provider_response_finished(provider_text_response(
        &first.agent_prompt_id,
        first.agent_id,
        "summary-A",
    ))
    .expect("accept first pass");

    let second = read_nth_prompt_created(&h, 2);
    assert_eq!(
        second.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    let second_context = serde_json::to_string(&second.context).expect("second context");
    assert!(second_context.contains("summary-A"));
    assert!(second_context.contains("old-B"));
    assert!(!second_context.contains("overflow activation"));
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_threshold = Some(tau_proto::TokenCount::new(0));
    h.handle_provider_response_finished(provider_text_response(
        &second.agent_prompt_id,
        second.agent_id,
        "summary-B",
    ))
    .expect("accept second pass");

    let resumed = read_nth_prompt_created(&h, 3);
    assert_eq!(resumed.operation, tau_proto::PromptOperation::Inference);
    let resumed_context = serde_json::to_string(&resumed.context).expect("resumed context");
    assert!(resumed_context.contains("summary-B"));
    assert!(resumed_context.contains("overflow activation"));
    assert!(!resumed_context.contains("old-A"));
    assert!(!resumed_context.contains("old-B"));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                    )
            ))
            .count(),
        1,
        "one linked rolling pass must finish the provider-authorized chain"
    );
    h.shutdown().expect("shutdown");
}

/// A reactive rolling pass that cannot fit its replacement plus one remaining
/// closed group must commit one typed local failure without provider dispatch.
#[test]
fn reactive_context_overflow_terminalizes_unfitting_rolling_prefix() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    info.standalone_compaction_prefix_budget = Some(tau_proto::ByteCount::new(1_000));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    for marker in ["old-A", "old-B"] {
        h.publish_for_agent(
            &cid,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: format!("{marker}:{}", "x".repeat(600)),
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
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent");
    agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1));
    agent.execution.context_usage_model = Some("test/model".into());
    agent.execution.context_usage_prompt_id = Some(test_agent_prompt_id("ap-test-provider-usage"));
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("retained activation".to_owned()))
        .expect("dispatch rejected inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("plan reactive recovery");
    let first = read_nth_prompt_created(&h, 1);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .standalone_compaction_prefix_budget = Some(tau_proto::ByteCount::new(1));
    h.handle_provider_response_finished(provider_text_response(
        &first.agent_prompt_id,
        first.agent_id,
        "summary",
    ))
    .expect("terminalize unfitting continuation");

    let events = event_log_events(&h);
    let local_start = events.iter().find_map(|event| match event {
        Event::AgentStandaloneCompactionStarted(started)
            if matches!(
                started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                    reason: tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
                    ..
                }
            ) =>
        {
            Some(started)
        }
        _ => None,
    });
    let local_start = local_start.expect("linked typed preflight start");
    assert!(events.iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionFailed(failed)
            if failed.transaction_id == local_start.transaction_id
                && failed.reason
                    == tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentPromptCreated(_)))
            .count(),
        2,
        "the local terminal must not dispatch another compact or inference prompt"
    );
    let tree = agent_tree_for_conversation(&h, &cid);
    assert!(
        tree.has_user_input_text_on_branch(tree.head(), "retained activation"),
        "the rejected activation stays durable after terminal no-progress"
    );
    h.shutdown().expect("shutdown");
}

/// A committed partial reactive success must restart from its durable
/// predecessor link and retain the rejected activation below local threshold.
#[test]
fn reactive_context_overflow_partial_success_rolls_after_cold_replay() {
    fn copy_tree(source: &Path, destination: &Path) {
        std::fs::create_dir_all(destination).expect("create copied state directory");
        for entry in std::fs::read_dir(source).expect("read copied state directory") {
            let entry = entry.expect("state entry");
            let target = destination.join(entry.file_name());
            if entry.file_type().expect("state entry type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).expect("copy state file");
            }
        }
    }

    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let first_transaction_id;
    let first_started;
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
        info.standalone_compaction_prefix_budget = Some(tau_proto::ByteCount::new(2_000));
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        for marker in ["old-A", "old-B"] {
            h.publish_for_agent(
                &cid,
                Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                    inference_activation: false,
                    agent_id: agent_id.clone(),
                    text: format!("{marker}:{}", "x".repeat(600)),
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
        h.publish_for_agent(
            &cid,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id.clone(),
                text: format!("later-C:{}", "x".repeat(600)),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: Default::default(),
                display_name: None,
                ctx_id: None,
            }),
        );
        let agent = h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent");
        agent.execution.context_input_tokens = Some(tau_proto::TokenCount::new(1));
        agent.execution.context_usage_model = Some("test/model".into());
        agent.execution.context_usage_prompt_id =
            Some(test_agent_prompt_id("ap-test-provider-usage"));
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("replayed activation".to_owned()))
            .expect("dispatch rejected inference");
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("plan first reactive pass");
        let reactive = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                    ) =>
                {
                    Some(started)
                }
                _ => None,
            })
            .expect("first reactive start");
        let rejected_compact = read_nth_prompt_created(&h, 1);
        let head_before_rejection = agent_tree_for_conversation(&h, &cid).head();
        h.handle_provider_response_finished(context_overflow_response(&rejected_compact))
            .expect("commit canonical rejection and start immediate retreat");
        assert_eq!(
            agent_tree_for_conversation(&h, &cid).head(),
            head_before_rejection,
            "standalone rejection evidence must not mutate provider history"
        );
        let events = event_log_events(&h);
        let response_index = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    Event::ProviderResponseFinished(response)
                        if response.agent_prompt_id == rejected_compact.agent_prompt_id
                )
            })
            .expect("canonical standalone provider rejection");
        let failure_index = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    Event::AgentStandaloneCompactionFailed(failed)
                        if failed.transaction_id == reactive.transaction_id
                )
            })
            .expect("typed standalone failure");
        assert!(response_index < failure_index);
        let retreat = events
            .into_iter()
            .find_map(|event| match event {
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat { .. }
                    ) =>
                {
                    Some(started)
                }
                _ => None,
            })
            .expect("immediate strict-predecessor retreat");
        first_transaction_id = retreat.transaction_id.clone();
        first_started = retreat;
        h.shutdown().expect("shutdown");
    }

    wait_for_session_unlock(&state, "s1");
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    let suffix_end = tau_proto::AgentHead::Node(
        store
            .agent(first_started.agent_id.as_str())
            .and_then(tau_core::AgentTree::head)
            .expect("partial success parent"),
    );
    store
        .append_agent_event_at(
            first_started.agent_id.as_str(),
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                original_input_tokens: None,
                compaction_output_tokens: None,
                agent_id: first_started.agent_id.clone(),
                replacement_window: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content: vec![ContentPart::Text {
                        text: "reactive summary".to_owned(),
                    }],
                    phase: None,
                    responses_raw_json: None,
                })],
                transaction_id: Some(first_started.transaction_id),
                cut: Some(first_started.cut),
                suffix_end: Some(suffix_end),
                compact_prompt_id: Some(first_started.compact_prompt_id),
                model: Some(first_started.model),
                operation: Some(first_started.operation),
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("append committed partial success");
    drop(store);
    let loss_state = td.path().join("loss-state");
    copy_tree(&state, &loss_state);

    {
        let mut loss = quiet_provider_harness_for_with_start_reason_and_storage_mode(
            "s2",
            &loss_state,
            tau_proto::SessionStartReason::Initial,
            crate::HarnessStorageMode::Durable,
        )
        .expect("start route-loss harness");
        loss.provider_runtime
            .model_info
            .remove(&"test/model".into());
        loss.provider_runtime
            .model_routes
            .remove(&"test/model".into());
        for models in loss.provider_runtime.models_by_extension.values_mut() {
            models.retain(|model| model.id != "test/model".into());
        }
        loss.switch_session(test_session_id("s1"), tau_proto::SessionStartReason::Resume)
            .expect("resume without captured capability");
        let events = event_log_events(&loss);
        let local_start = events.iter().find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                        reason: tau_proto::StandaloneCompactionFailureReason::RouteFailed,
                        ..
                    } | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                ) =>
            {
                Some(started)
            }
            _ => None,
        });
        let local_start = local_start
            .unwrap_or_else(|| panic!("route loss terminalizes linked rolling work: {events:#?}"));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::AgentStandaloneCompactionFailed(failed)
                if failed.transaction_id == local_start.transaction_id
                    && failed.reason == tau_proto::StandaloneCompactionFailureReason::RouteFailed
        )));
        assert!(events.iter().all(|event| !matches!(
            event,
            Event::AgentInferenceDispatchStarted(started)
                if started.transaction_id.as_ref() == Some(&first_transaction_id)
        )));
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, Event::AgentPromptCreated(_))),
            "route loss must terminalize without provider dispatch"
        );
        loss.shutdown().expect("shutdown loss harness");
    }

    let mut resumed = quiet_provider_harness_for_with_start_reason_and_storage_mode(
        "s2",
        &state,
        tau_proto::SessionStartReason::Initial,
        crate::HarnessStorageMode::Durable,
    )
    .expect("start cold continuation harness");
    let info = resumed
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(u64::MAX));
    info.standalone_compaction_prefix_budget = Some(tau_proto::ByteCount::new(1_000));
    resumed
        .switch_session(test_session_id("s1"), tau_proto::SessionStartReason::Resume)
        .expect("resume partial reactive success");
    let events = event_log_events(&resumed);
    let continuation = events
        .iter()
        .find_map(|event| match event {
            Event::AgentStandaloneCompactionStarted(started)
                if matches!(
                    &started.trigger,
                    tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                        previous_transaction_id
                    } if previous_transaction_id == &first_transaction_id
                ) =>
            {
                Some(started)
            }
            _ => None,
        })
        .expect("cold replay starts one linked rolling pass");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::AgentStandaloneCompactionStarted(started)
                    if matches!(
                        started.trigger,
                        tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                    )
            ))
            .count(),
        1
    );
    let compact = read_nth_prompt_created(&resumed, 0);
    let context = serde_json::to_string(&compact.context).expect("compact context");
    assert!(context.contains("reactive summary"));
    assert!(context.contains("old-B"));
    assert!(!context.contains("later-C"));
    assert!(!context.contains("replayed activation"));
    assert_ne!(
        continuation.cut,
        continuation.resume_through.expect("resume")
    );
    resumed.shutdown().expect("shutdown");
}

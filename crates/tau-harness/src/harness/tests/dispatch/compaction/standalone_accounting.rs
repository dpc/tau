//! Focused durable standalone backend-attempt accounting oracles.

use tau_config::settings::AgentWatchRetryNotificationPolicy;

use super::*;

fn report_standalone_retry(h: &mut Harness, prompt: &tau_proto::AgentPromptCreated, attempt: u32) {
    let provider = h
        .provider_runtime
        .pending_prompts
        .get(&prompt.agent_prompt_id)
        .cloned()
        .expect("provider owner");
    h.handle_extension_event(
        provider.as_str(),
        TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
            ProviderResponseUpdated {
                agent_prompt_id: prompt.agent_prompt_id.clone(),
                agent_id: prompt.agent_id.clone(),
                deltas: Vec::new(),
                compaction: None,
                status: Some(tau_proto::ProviderResponseStatusUpdate {
                    text: "retrying".to_owned(),
                    clear_response: true,
                    retry: Some(tau_proto::ProviderRetryStatus {
                        category: tau_proto::ProviderRetryCategory::Transport,
                        attempt,
                        next_retry_delay_secs: 1,
                    }),
                }),
                response_stats: None,
                originator: prompt.originator.clone(),
            },
        )),
    )
    .expect("record retry");
}

/// A failed standalone provider terminal must replace its retry snapshot for
/// current and warm late watchers, while restart drops the runtime-only state.
#[test]
fn standalone_failure_terminalizes_warm_provider_watch_status() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let watched_id = {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        let info = h
            .provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model");
        info.supports_compaction = false;
        info.supports_standalone_compaction = true;
        h.config
            .accepted_harness_settings
            .agent_watch_retry_notification_threshold =
            AgentWatchRetryNotificationPolicy::from_raw(0);
        let watched_cid = ensure_test_user_agent(&mut h);
        let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
        let watcher_cid = h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        );
        let late_cid = h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        );
        let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
        let late_id = durable_agent_id_for_conversation(&h, &late_cid).to_string();
        h.set_agent_watch(
            &watcher_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        );

        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(&watched_id),
        );
        let prompt = read_nth_prompt_created(&h, 0);
        report_standalone_retry(&mut h, &prompt, 1);
        assert!(matches!(
            h.agent_runtime.agent_watch.provider_status[&watched_id].state,
            tau_proto::AgentWatchProviderState::Retrying { attempt: 1, .. }
        ));

        h.handle_provider_response_finished(ProviderResponseFinished {
            automatic_compaction_decision: None,
            output_length_disposition: tau_proto::OutputLengthDisposition::None,
            estimated_api_cost_rates: None,
            estimated_api_cost_increment: None,
            agent_prompt_id: prompt.agent_prompt_id.clone(),
            agent_id: prompt.agent_id,
            output_items: Vec::new(),
            stop_reason: tau_proto::ProviderStopReason::Error,
            error: Some("private provider failure".to_owned()),
            failure_kind: None,
            context_limit_telemetry: None,
            recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
            originator: prompt.originator,
            usage: None,
            compaction_original_input_tokens: None,
            compaction_output_tokens: None,
            backend: None,
            provider_attempt: Default::default(),
            provider_response_id: None,
            ws_pool_delta: None,
        })
        .expect("record terminal failure");
        assert!(matches!(
            h.agent_runtime.agent_watch.provider_status[&watched_id].state,
            tau_proto::AgentWatchProviderState::TerminalError {
                failure_kind: tau_proto::ProviderFailureKind::Unknown,
                attempt: 2,
            }
        ));
        let watcher_statuses = session_agent_message_received_events(&h)
            .into_iter()
            .filter(|message| {
                message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                    && message.recipient_id.as_str() == watcher_id
            })
            .collect::<Vec<_>>();
        assert_eq!(watcher_statuses.len(), 2);
        assert!(matches!(
            watcher_statuses[1]
                .watch_provider_status
                .as_ref()
                .expect("terminal status")
                .state,
            tau_proto::AgentWatchProviderState::TerminalError {
                failure_kind: tau_proto::ProviderFailureKind::Unknown,
                attempt: 2,
            }
        ));

        h.set_agent_watch(
            &late_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        );
        let late_status = session_agent_message_received_events(&h)
            .into_iter()
            .rev()
            .find(|message| {
                message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                    && message.recipient_id.as_str() == late_id
            })
            .expect("warm late-watch snapshot");
        let late_status = late_status
            .watch_provider_status
            .expect("typed warm late-watch status");
        assert!(late_status.initial);
        assert!(matches!(
            late_status.state,
            tau_proto::AgentWatchProviderState::TerminalError {
                failure_kind: tau_proto::ProviderFailureKind::Unknown,
                attempt: 2,
            }
        ));
        h.shutdown().expect("shutdown");
        watched_id
    };

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert!(
        !resumed
            .agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&watched_id),
        "provider-watch state remains runtime-only after restart"
    );
    let cold_late_cid = resumed.create_durable_user_agent(
        resumed.session_runtime.current_session_id.clone(),
        &resumed.config.selected_role.clone(),
    );
    let cold_late_id = durable_agent_id_for_conversation(&resumed, &cold_late_cid).to_string();
    let provider_status_count = |h: &Harness| {
        session_agent_message_received_events(h)
            .into_iter()
            .filter(|message| {
                message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                    && message.recipient_id.as_str() == cold_late_id
            })
            .count()
    };
    let before = provider_status_count(&resumed);
    resumed.set_agent_watch(
        &cold_late_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    assert_eq!(
        provider_status_count(&resumed),
        before,
        "cold late watch must not replay a runtime-only provider terminal"
    );
}

/// User cancellation must retire the exact standalone retry snapshot without
/// projecting cancellation as a provider terminal error to current or late
/// watchers.
#[test]
fn standalone_cancellation_retires_warm_provider_watch_status() {
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
    h.config
        .accepted_harness_settings
        .agent_watch_retry_notification_threshold = AgentWatchRetryNotificationPolicy::from_raw(0);
    let watched_cid = ensure_test_user_agent(&mut h);
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let late_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    let late_id = durable_agent_id_for_conversation(&h, &late_cid).to_string();
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&watched_id),
    );
    let prompt = read_nth_prompt_created(&h, 0);
    report_standalone_retry(&mut h, &prompt, 1);
    let watcher_provider_status_count = |h: &Harness| {
        session_agent_message_received_events(h)
            .into_iter()
            .filter(|message| {
                message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                    && message.recipient_id.as_str() == watcher_id
            })
            .count()
    };
    assert_eq!(watcher_provider_status_count(&h), 1);

    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(crate::parse_agent_id(&watched_id)),
            agent_prompt_id: Some(prompt.agent_prompt_id.clone()),
        },
    );

    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&watched_id),
        "cancelled standalone prompt must not remain retrying"
    );
    assert_eq!(
        watcher_provider_status_count(&h),
        1,
        "cancellation is not a provider terminal error"
    );
    h.set_agent_watch(
        &late_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    assert!(
        session_agent_message_received_events(&h)
            .into_iter()
            .all(|message| {
                message.kind != tau_proto::AgentMessageKind::WatchProviderStatus
                    || message.recipient_id.as_str() != late_id
            }),
        "warm late watcher must not receive the retired retry snapshot"
    );
}

/// A standalone-capable model must bound retry accounting at 64, reserve
/// attempt 65 for its terminal, and accept validated output as one replacement
/// boundary.
#[test]
fn manual_standalone_compact_installs_one_boundary() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    info.standalone_compaction_threshold = Some(tau_proto::TokenCount::new(900));
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h.agent_runtime.agent_registry.agents[&cid]
        .identity
        .agent_id
        .clone()
        .expect("durable agent");

    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(&agent_id),
    );
    let prompt = read_nth_prompt_created(&h, 0);
    assert_eq!(
        prompt.operation,
        tau_proto::PromptOperation::StandaloneCompaction
    );
    let events = event_log_events(&h);
    let (started_index, started) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match event {
            Event::AgentStandaloneCompactionStarted(started) => Some((index, started.clone())),
            _ => None,
        })
        .expect("durable start");
    let prompt_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentPromptCreated(created)
                    if created.agent_prompt_id == prompt.agent_prompt_id
            )
        })
        .expect("provider prompt");
    assert!(
        started_index < prompt_index,
        "durable start must commit before provider dispatch"
    );
    assert_eq!(started.compact_prompt_id, prompt.agent_prompt_id);
    assert_eq!(started.model, prompt.model);
    assert_eq!(started.operation, prompt.operation);
    let provider = h
        .provider_runtime
        .pending_prompts
        .get(&prompt.agent_prompt_id)
        .cloned()
        .expect("provider owner");
    for attempt in [1, 1, tau_proto::MAX_STANDALONE_RETRY_ATTEMPTS, u32::MAX] {
        let text = if attempt == u32::MAX {
            "retry-overbound"
        } else {
            "retrying"
        };
        h.handle_extension_event(
            provider.as_str(),
            TestProtocolItem::Event(Event::ProviderResponseUpdatedReported(
                ProviderResponseUpdated {
                    agent_prompt_id: prompt.agent_prompt_id.clone(),
                    agent_id: prompt.agent_id.clone(),
                    deltas: Vec::new(),
                    compaction: None,
                    status: Some(tau_proto::ProviderResponseStatusUpdate {
                        text: text.to_owned(),
                        clear_response: true,
                        retry: Some(tau_proto::ProviderRetryStatus {
                            category: tau_proto::ProviderRetryCategory::Transport,
                            attempt,
                            next_retry_delay_secs: 1,
                        }),
                    }),
                    response_stats: None,
                    originator: prompt.originator.clone(),
                },
            )),
        )
        .expect("ingest duplicate or jumped retry terminal");
    }
    assert_eq!(
        h.prompt_coordination
            .standalone_accounting
            .highest_retry_attempt
            .get(&prompt.agent_prompt_id),
        Some(&tau_proto::MAX_STANDALONE_RETRY_ATTEMPTS),
        "out-of-contract retry status must not become accounting authority"
    );
    assert!(
        h.prompt_coordination
            .standalone_accounting
            .rejected_retry_bounds
            .contains(&prompt.agent_prompt_id),
        "the first out-of-contract status records one bounded diagnostic"
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::ProviderResponseUpdated(updated)
            if updated.status.as_ref().is_some_and(|status|
                status.text == "retry-overbound" && status.retry.is_none())
    )));
    assert!(
        h.agent_runtime
            .agent_watch
            .provider_status
            .values()
            .any(|status| {
                status.agent_prompt_id == prompt.agent_prompt_id
                    && matches!(
                        status.state,
                        tau_proto::AgentWatchProviderState::Retrying {
                            attempt: tau_proto::MAX_STANDALONE_RETRY_ATTEMPTS,
                            ..
                        }
                    )
            })
    );
    let response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: prompt.agent_prompt_id,
        agent_id: crate::parse_agent_id(&agent_id),
        output_items: vec![ContextItem::Message(tau_proto::MessageItem {
            role: tau_proto::ContextRole::Assistant,
            content: vec![tau_proto::ContentPart::Text {
                text: "summary".to_owned(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 226_200,
            response_received_tokens: 4_500,
            ..Default::default()
        }),
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    h.handle_provider_response_finished(response.clone())
        .expect("accept compact response");
    h.handle_provider_response_finished(response)
        .expect("ignore duplicate compact response");
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(agent_id.as_str()),
        "successful standalone terminal must retire the retry snapshot"
    );

    let compacted: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentCompacted(compacted) => Some(compacted),
            _ => None,
        })
        .collect();
    assert_eq!(compacted.len(), 1);
    let accounting = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::ProviderStandaloneExecutionAccounted(accounted) => Some(accounted),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(accounting.len(), 65);
    for attempt in 1..=tau_proto::MAX_STANDALONE_RETRY_ATTEMPTS {
        assert!(accounting.iter().any(|accounted| {
            accounted.logical_attempt.get() == attempt
                && accounted.usage == tau_proto::StandaloneExecutionUsage::Unknown
                && accounted.output == tau_proto::StandaloneExecutionOutput::Rejected
        }));
    }
    let accounting = accounting
        .iter()
        .find(|accounted| accounted.logical_attempt.get() == 65)
        .expect("terminal attempt accounting");
    assert_eq!(accounting.session_id, test_session_id("s1"));
    assert_eq!(accounting.transaction_id, started.transaction_id);
    assert_eq!(
        accounting.output,
        tau_proto::StandaloneExecutionOutput::Accepted
    );
    assert!(matches!(
        &accounting.usage,
        tau_proto::StandaloneExecutionUsage::Known(usage)
            if usage.prompt_sent_tokens == 226_200
                && usage.response_received_tokens == 4_500
    ));
    assert_eq!(
        (
            compacted[0].original_input_tokens,
            compacted[0].compaction_output_tokens,
        ),
        (
            Some(tau_proto::TokenCount::new(226_200)),
            Some(tau_proto::TokenCount::new(4_500)),
        ),
        "the durable boundary must retain canonical provider usage"
    );
    assert_eq!(
        (
            compacted[0].compact_prompt_id.as_ref(),
            compacted[0].model.as_ref(),
            compacted[0].operation,
        ),
        (
            Some(&started.compact_prompt_id),
            Some(&started.model),
            Some(started.operation),
        ),
        "terminal correlation must be copied from the durable start"
    );
    assert!(matches!(
        agent_tree_for_conversation(&h, &cid).current_branch().last(),
        Some(tau_core::AgentEntry::Compaction {
            replacement_window, ..
        })
            if replacement_window.len() == 1
    ));
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(&agent_id)
            .expect("durable events")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::ProviderStandaloneExecutionAccounted(accounted)
                    if accounted.estimated_api_cost_increment.is_some()
            )),
        "canonical standalone accounting must be durable"
    );
    let live_usage = h.session_runtime.current_session_state.token_usage.clone();
    h.shutdown().expect("shutdown");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    let resumed_cid = ensure_test_user_agent(&mut resumed);
    let _ = resumed_cid;
    assert_eq!(
        resumed.session_runtime.current_session_state.token_usage, live_usage,
        "cold replay must rebuild the same standalone accounting ledger"
    );
    resumed.shutdown().expect("shutdown resumed harness");
}

/// Accounting and compaction outcomes retain independent exact publications
/// when both semantic admissions reject, then each commits once after recovery.
#[test]
fn standalone_accounting_and_outcome_append_rejections_recover_independently() {
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
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let compact = read_nth_prompt_created(&h, 0);

    reject_semantic_admissions(&h, 2);
    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact).expect("valid compact response"),
    )
    .expect("terminal processing retains rejected publications");

    assert_eq!(
        h.prompt_coordination.standalone_accounting.retained.len(),
        1,
        "accounting owns its own retained slot"
    );
    assert_eq!(
        h.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .len(),
        1,
        "the independent outcome retains its existing agent slot"
    );

    h.retry_pending_agent_publications();
    let events = event_log_events(&h);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ProviderStandaloneExecutionAccounted(accounted)
                    if accounted.agent_prompt_id == compact.agent_prompt_id
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AgentCompacted(_)))
            .count(),
        1
    );
    let accounting_record = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent events")
        .into_iter()
        .find(|record| {
            matches!(
                &record.event,
                Event::ProviderStandaloneExecutionAccounted(accounted)
                    if accounted.agent_prompt_id == compact.agent_prompt_id
            )
        })
        .expect("durable accounting record");
    assert_eq!(
        accounting_record.source,
        Some(tau_core::PersistedEventSource::Connection(
            crate::harness::harness_connection_id().clone()
        )),
        "retained retry preserves canonical harness source"
    );
    assert!(
        h.prompt_coordination
            .standalone_accounting
            .retained
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

/// An append-rejected cancellation observation must commit before its already
/// received late-terminal correction, without blocking the Cancelled outcome.
#[test]
fn rejected_cancellation_accounting_orders_late_terminal_correction() {
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
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let compact = read_nth_prompt_created(&h, 0);
    reject_next_semantic_admission(&h);
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            agent_prompt_id: Some(compact.agent_prompt_id.clone()),
        },
    );
    assert!(event_log_events(&h).iter().any(|event| matches!(
        event,
        Event::AgentStandaloneCompactionFailed(failed)
            if failed.reason == tau_proto::StandaloneCompactionFailureReason::Cancelled
    )));
    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact).expect("valid late terminal"),
    )
    .expect("queue correction behind rejected initial");
    assert_eq!(
        h.prompt_coordination.standalone_accounting.retained.len(),
        1
    );
    assert_eq!(
        h.prompt_coordination
            .standalone_accounting
            .pending_corrections
            .len(),
        1
    );

    h.retry_pending_agent_publications();
    let events = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent events");
    let initial = events
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::ProviderStandaloneExecutionAccounted(accounted)
                    if accounted.agent_prompt_id == compact.agent_prompt_id
            )
        })
        .expect("committed initial");
    let correction = events
        .iter()
        .position(|record| {
            matches!(
                &record.event,
                Event::ProviderStandaloneExecutionAccountingCorrected(corrected)
                    if corrected.agent_prompt_id == compact.agent_prompt_id
            )
        })
        .expect("committed correction");
    assert!(initial < correction);
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        1
    );
    h.shutdown().expect("shutdown");
}

/// Session switch and process shutdown must fail before invalidating an
/// accounting fact parked at an interceptor, then succeed after that exact fact
/// commits.
#[test]
fn parked_standalone_accounting_blocks_lifecycle_teardown_until_committed() {
    for lifecycle in ["switch", "shutdown"] {
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
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(agent_id.as_str()),
        );
        let compact = read_nth_prompt_created(&h, 0);
        let interceptor = format!("parked-accounting-{lifecycle}");
        connect_test_tool(&mut h, &interceptor);
        h.handle_extension_event(
            &interceptor,
            TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                selectors: vec![EventSelector::Exact(
                    tau_proto::EventName::PROVIDER_STANDALONE_EXECUTION_ACCOUNTED,
                )],
                priority: InterceptionPriority::new(0),
            })),
        )
        .expect("register accounting interceptor");
        h.handle_provider_response_finished(
            strict_fake_compact_response(&compact).expect("valid compact response"),
        )
        .expect("park accounting publication");
        assert!(h.has_unsettled_standalone_accounting_publication());

        let error = if lifecycle == "switch" {
            h.switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
                .expect_err("switch must not discard parked accounting")
        } else {
            h.shutdown()
                .expect_err("shutdown must not discard parked accounting")
        };
        assert!(
            error.to_string().contains("accounting remains uncommitted"),
            "{lifecycle}: {error}"
        );
        assert_eq!(
            h.session_runtime.current_session_id,
            test_session_id("s1"),
            "failed lifecycle must preserve the accounting session"
        );

        h.handle_extension_event(
            &interceptor,
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("commit parked accounting");
        assert!(!h.has_unsettled_standalone_accounting_publication());
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::ProviderStandaloneExecutionAccounted(accounted)
                        if accounted.agent_prompt_id == compact.agent_prompt_id
                ))
                .count(),
            1
        );
        if lifecycle == "switch" {
            h.switch_session(test_session_id("s2"), tau_proto::SessionStartReason::New)
                .expect("switch after accounting commit");
        }
        h.shutdown().expect("shutdown after accounting commit");
    }
}

/// Restore filters a shared durable agent journal by required session identity
/// and remains idempotent across repeated scans and finalization.
#[test]
fn standalone_accounting_restore_is_session_scoped_and_idempotent() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut store = tau_core::AgentStore::open(state.join("agents")).expect("agent store");
    store
        .append_agent_event_at(
            "parent",
            None,
            tau_core::AgentEventParent::InheritHead,
            Event::AgentStarted(tau_proto::AgentStarted {
                creator: Some(tau_proto::AgentCreator::User),
                parent_agent: None,
                agent_id: crate::parse_agent_id("parent"),
                role: "engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
                ephemeral: false,
            }),
            tau_proto::UnixMicros::now(),
        )
        .expect("seed parent");
    append_seed_agent_event(
        &mut store,
        Event::AgentStarted(tau_proto::AgentStarted {
            creator: Some(tau_proto::AgentCreator::Agent {
                session_id: test_session_id("s1"),
                agent_id: crate::parse_agent_id("parent"),
            }),
            parent_agent: None,
            agent_id: crate::parse_agent_id("main"),
            role: "engineer".to_owned(),
            display_name: None,
            metadata: Vec::new(),
            ephemeral: false,
        }),
    );
    append_seed_standalone_accounting(&mut store, "s1", "one", 101);
    append_seed_standalone_accounting(&mut store, "s2", "two", 202);
    drop(store);

    let mut h = quiet_provider_harness(&state).expect("start");
    for (session, expected_cost) in [("s1", 101), ("s2", 202)] {
        if h.session_runtime.current_session_id.as_str() != session {
            h.switch_session(test_session_id(session), tau_proto::SessionStartReason::New)
                .expect("switch session");
        }
        h.publish_event(
            None,
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                agent_initialization_id: tau_proto::AgentInitializationId::parse(format!(
                    "init-{session}"
                ))
                .expect("valid initialization id"),
                session_id: test_session_id(session),
                agent_id: crate::parse_agent_id("main"),
                ephemeral: false,
            }),
        );
        if session == "s1" {
            h.publish_event(
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    agent_initialization_id: tau_proto::AgentInitializationId::parse(
                        "init-memory-only-history",
                    )
                    .expect("valid initialization id"),
                    session_id: test_session_id("s1"),
                    agent_id: crate::parse_agent_id("memory-only-history"),
                    ephemeral: true,
                }),
            );
            h.publish_event(
                None,
                Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                    session_id: test_session_id("s1"),
                    agent_id: crate::parse_agent_id("memory-only-history"),
                }),
            );
            h.publish_event(
                None,
                Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                    session_id: test_session_id("s1"),
                    agent_id: crate::parse_agent_id("main"),
                }),
            );
            h.publish_event(
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    agent_initialization_id: tau_proto::AgentInitializationId::parse("init-parent")
                        .expect("valid initialization id"),
                    session_id: test_session_id("s1"),
                    agent_id: crate::parse_agent_id("parent"),
                    ephemeral: false,
                }),
            );
        }
        h.restore_standalone_execution_accounting();
        h.restore_standalone_execution_accounting();
        h.finalize_restored_standalone_costs();
        h.finalize_restored_standalone_costs();
        if session == "s1" {
            tau_core::AgentJournalSnapshot::capture(
                &state.join("agents"),
                [crate::parse_agent_id("main")],
            )
            .expect("historical accounting snapshot must release its journal lock");
            let collision = tau_proto::ProviderStandaloneExecutionAccounted {
                session_id: test_session_id("s1"),
                agent_id: crate::parse_agent_id("other-journal"),
                agent_prompt_id: test_agent_prompt_id("accounting-one"),
                logical_attempt: tau_proto::ProviderAttempt::ONE,
                transaction_id: tau_proto::CompactionTransactionId::parse("ct-one")
                    .expect("valid transaction"),
                model: "test/model".into(),
                backend: None,
                usage: tau_proto::StandaloneExecutionUsage::Known(tau_proto::ProviderTokenUsage {
                    prompt_sent_tokens: 999,
                    ..Default::default()
                }),
                estimated_api_cost_rates: Some(tau_proto::ESTIMATED_API_COST_FALLBACK),
                estimated_api_cost_increment: Some(tau_proto::EstimatedApiCost::from_picodollars(
                    999,
                )),
                output: tau_proto::StandaloneExecutionOutput::Rejected,
                finality: tau_proto::StandaloneExecutionAccountingFinality::Final,
            };
            h.fold_committed_standalone_accounting(&collision, None, false);
            h.finalize_restored_standalone_costs();
        }

        assert_eq!(
            h.session_runtime
                .current_session_state
                .token_usage
                .total
                .requests,
            1,
            "{session}"
        );
        if session == "s1" {
            assert_eq!(
                h.agent_runtime
                    .agent_registry
                    .cost_ledger
                    .creator_subtree_cost(&crate::parse_agent_id("parent"))
                    .as_picodollars(),
                expected_cost,
                "unloaded child cost propagates to its loaded creator"
            );
        }
        assert_eq!(
            h.agent_runtime
                .agent_registry
                .cost_ledger
                .self_cost(&crate::parse_agent_id("main"))
                .as_picodollars(),
            expected_cost,
            "{session}"
        );
    }
    h.switch_session(test_session_id("s1"), tau_proto::SessionStartReason::New)
        .expect("sequence-continuing New restores first session accounting");
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        1
    );
    enable_remote_compaction_for_test_model(&mut h);
    let info = h
        .provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model");
    info.supports_compaction = false;
    info.supports_standalone_compaction = true;
    let new_cid = ensure_test_user_agent(&mut h);
    let new_agent_id = durable_agent_id_for_conversation(&h, &new_cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(new_agent_id.as_str()),
    );
    let new_compact = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(
        strict_fake_compact_response(&new_compact).expect("new accounting terminal"),
    )
    .expect("commit new accounting");
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        2,
        "New appends accounting after restoring its predecessor prefix"
    );
    h.switch_session(test_session_id("s3"), tau_proto::SessionStartReason::New)
        .expect("switch away");
    h.switch_session(test_session_id("s1"), tau_proto::SessionStartReason::Resume)
        .expect("resume after sequence-continuing New");
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        2
    );
    assert_eq!(
        h.agent_runtime
            .agent_registry
            .cost_ledger
            .self_cost(&crate::parse_agent_id("main"))
            .as_picodollars(),
        101,
        "warm switch/resume finalizes restored costs"
    );
    h.shutdown().expect("shutdown");
}

/// Seed one terminal standalone transaction and its session-correlated spend.
fn append_seed_standalone_accounting(
    store: &mut tau_core::AgentStore,
    session: &str,
    suffix: &str,
    picodollars: u64,
) {
    let agent_id = crate::parse_agent_id("main");
    let session_id = test_session_id(session);
    let prompt_id = test_agent_prompt_id(format!("accounting-{suffix}"));
    let transaction_id = tau_proto::CompactionTransactionId::parse(format!("ct-{suffix}"))
        .expect("valid transaction id");
    let model: tau_proto::ModelId = "test/model".into();
    let rates = tau_proto::EstimatedApiCostRates {
        uncached_input: tau_proto::EstimatedUsdPerMillion::from_micro_usd(1),
        cached_input: tau_proto::EstimatedUsdPerMillion::from_micro_usd(1),
        cache_write_input: None,
        output: tau_proto::EstimatedUsdPerMillion::from_micro_usd(1),
        storage_per_million_token_hour: None,
    };
    let usage = tau_proto::ProviderTokenUsage {
        model: Some(model.clone()),
        prompt_sent_tokens: picodollars,
        ..Default::default()
    };
    append_seed_agent_event(
        store,
        Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
            agent_id: agent_id.clone(),
            transaction_id: transaction_id.clone(),
            compact_prompt_id: prompt_id.clone(),
            cut: tau_proto::AgentHead::Root,
            resume_through: Some(tau_proto::AgentHead::Root),
            model: model.clone(),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::Manual,
        }),
    );
    append_seed_agent_event(
        store,
        Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            agent_prompt_id: prompt_id.clone(),
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            model: model.clone(),
            model_params: None,
            outer_turn_id: None,
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    append_seed_agent_event(
        store,
        Event::ProviderStandaloneExecutionAccounted(
            tau_proto::ProviderStandaloneExecutionAccounted {
                session_id,
                agent_id: agent_id.clone(),
                agent_prompt_id: prompt_id,
                logical_attempt: tau_proto::ProviderAttempt::ONE,
                transaction_id: transaction_id.clone(),
                model,
                backend: None,
                usage: tau_proto::StandaloneExecutionUsage::Known(usage.clone()),
                estimated_api_cost_rates: Some(rates),
                estimated_api_cost_increment: Some(tau_proto::EstimatedApiCost::for_usage(
                    &usage, rates,
                )),
                output: tau_proto::StandaloneExecutionOutput::Rejected,
                finality: tau_proto::StandaloneExecutionAccountingFinality::Final,
            },
        ),
    );
    append_seed_agent_event(
        store,
        Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
            agent_id,
            transaction_id,
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::Cancelled,
            resume_through: Some(tau_proto::AgentHead::Root),
            context_retreat: None,
            incomplete_response: None,
        }),
    );
}

/// UI cancellation during reactive compaction publishes one durable Cancelled
/// outcome; a late provider terminal and cold replay cannot duplicate it.
#[test]
fn reactive_context_overflow_ui_cancel_is_terminal_once() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (agent_id, compact, live_usage);
    {
        let mut h = quiet_provider_harness(&state).expect("start");
        enable_remote_compaction_for_test_model(&mut h);
        h.provider_runtime
            .model_info
            .get_mut(&"test/model".into())
            .expect("test model")
            .supports_standalone_compaction = true;
        let cid = ensure_test_user_agent(&mut h);
        seed_reactive_compaction_prefix(&mut h, &cid);
        agent_id = h.agent_runtime.agent_registry.agents[&cid]
            .identity
            .agent_id
            .clone()
            .expect("agent id");
        h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("overflow".to_owned()))
            .expect("dispatch inference");
        let inference = read_nth_prompt_created(&h, 0);
        h.handle_provider_response_finished(context_overflow_response(&inference))
            .expect("start recovery");
        compact = read_nth_prompt_created(&h, 1);
        let provider = h
            .provider_runtime
            .pending_prompts
            .get(&compact.agent_prompt_id)
            .cloned()
            .expect("provider owner");
        h.handle_cancel_prompt(
            crate::harness::harness_connection_id(),
            &tau_proto::UiCancelPrompt {
                session_id: test_session_id("s1"),
                target_agent_id: Some(crate::parse_agent_id(&agent_id)),
                agent_prompt_id: Some(compact.agent_prompt_id.clone()),
            },
        );
        let mut late = context_overflow_response(&compact);
        late.usage = Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens: 10,
            response_received_tokens: 2,
            ..Default::default()
        });
        h.handle_provider_response_finished(late)
            .expect("discard late response");
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::AgentStandaloneCompactionFailed(failed)
                        if failed.reason
                            == tau_proto::StandaloneCompactionFailureReason::Cancelled
                ))
                .count(),
            1
        );
        let accounting = event_log_events(&h)
            .into_iter()
            .filter_map(|event| match event {
                Event::ProviderStandaloneExecutionAccounted(accounted)
                    if accounted.agent_prompt_id == compact.agent_prompt_id =>
                {
                    Some(accounted)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(accounting.len(), 1);
        assert_eq!(accounting[0].session_id, test_session_id("s1"));
        assert_eq!(
            accounting[0].output,
            tau_proto::StandaloneExecutionOutput::Rejected
        );
        assert!(matches!(
            accounting[0].usage,
            tau_proto::StandaloneExecutionUsage::Unknown
        ));
        assert_eq!(
            accounting[0].finality,
            tau_proto::StandaloneExecutionAccountingFinality::AwaitingCancelledTerminal
        );
        let corrections = event_log_events(&h)
            .into_iter()
            .filter_map(|event| match event {
                Event::ProviderStandaloneExecutionAccountingCorrected(corrected)
                    if corrected.agent_prompt_id == compact.agent_prompt_id =>
                {
                    Some(corrected)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(corrections.len(), 1);
        assert!(matches!(
            corrections[0].usage,
            tau_proto::StandaloneExecutionUsage::Known(_)
        ));
        let original =
            Event::ProviderStandaloneExecutionAccountingCorrected(corrections[0].clone());
        let mut forged = corrections[0].clone();
        forged.usage = tau_proto::StandaloneExecutionUsage::Unknown;
        forged.estimated_api_cost_increment = None;
        assert!(
            crate::harness::interception::immutable_protected_fact_was_modified(
                &original,
                &Event::ProviderStandaloneExecutionAccountingCorrected(forged)
            ),
            "interception cannot replace correction payload"
        );
        h.handle_extension_event(provider.as_str(), TestProtocolItem::Event(original))
            .expect("direct canonical authoring is ignored");
        assert_eq!(
            event_log_events(&h)
                .iter()
                .filter(|event| matches!(
                    event,
                    Event::ProviderStandaloneExecutionAccountingCorrected(corrected)
                        if corrected.agent_prompt_id == compact.agent_prompt_id
                ))
                .count(),
            1,
            "configured provider cannot author a canonical correction directly"
        );
        live_usage = h.session_runtime.current_session_state.token_usage.clone();
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert!(
        !event_log_events(&resumed).iter().any(|event| matches!(
            event,
            Event::AgentPromptStarted(prompt)
                if prompt.agent_prompt_id == compact.agent_prompt_id
        )),
        "terminal cancelled compaction is not replay-dispatched"
    );
    assert!(
        resumed
            .agent_runtime
            .agent_watch
            .provider_status
            .get(agent_id.as_str())
            .is_none_or(|status| !matches!(
                status.state,
                tau_proto::AgentWatchProviderState::Blocked { .. }
            )),
        "a durable cancelled terminal must replay as usable"
    );
    assert_eq!(
        resumed
            .session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        1,
        "the correction must not count a second standalone request"
    );
    assert_eq!(
        resumed
            .session_runtime
            .current_session_state
            .token_usage
            .total
            .sent_tokens,
        10
    );
    assert_eq!(live_usage.total.requests, 2);
    resumed.shutdown().expect("shutdown");
}

/// A dispatched cancellation with no later provider terminal still counts one
/// unknown request, and restart restores no authority to correct it.
#[test]
fn dispatched_cancel_without_terminal_replays_awaiting_accounting() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let prompt_id;
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
        let cid = ensure_test_user_agent(&mut h);
        let agent_id = durable_agent_id_for_conversation(&h, &cid);
        h.handle_compact_request(
            crate::harness::harness_connection_id(),
            test_session_id("s1"),
            Some(agent_id.as_str()),
        );
        let compact = read_nth_prompt_created(&h, 0);
        prompt_id = compact.agent_prompt_id.clone();
        h.handle_cancel_prompt(
            crate::harness::harness_connection_id(),
            &tau_proto::UiCancelPrompt {
                session_id: test_session_id("s1"),
                target_agent_id: Some(agent_id),
                agent_prompt_id: Some(prompt_id.clone()),
            },
        );
        let accounted = event_log_events(&h)
            .into_iter()
            .find_map(|event| match event {
                Event::ProviderStandaloneExecutionAccounted(accounted)
                    if accounted.agent_prompt_id == prompt_id =>
                {
                    Some(accounted)
                }
                _ => None,
            })
            .expect("awaiting accounting");
        assert_eq!(
            accounted.finality,
            tau_proto::StandaloneExecutionAccountingFinality::AwaitingCancelledTerminal
        );
        assert!(matches!(
            accounted.usage,
            tau_proto::StandaloneExecutionUsage::Unknown
        ));
        assert_eq!(
            h.session_runtime
                .current_session_state
                .token_usage
                .total
                .requests,
            1
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");
    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert_eq!(
        resumed
            .session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        1
    );
    assert!(
        !resumed
            .prompt_coordination
            .standalone_accounting
            .owners
            .contains_key(&prompt_id),
        "durable awaiting state must not recreate live provider authority"
    );
    resumed.shutdown().expect("shutdown resumed harness");
}

/// Provider disconnect and graceful shutdown close every still-dispatched
/// standalone owner as one final Unknown request before authority disappears.
#[test]
fn provider_loss_and_shutdown_finalize_active_standalone_accounting() {
    for teardown in ["provider_disconnect", "agent_unload", "shutdown"] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let prompt_id;
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
            let cid = ensure_test_user_agent(&mut h);
            let agent_id = durable_agent_id_for_conversation(&h, &cid);
            h.handle_compact_request(
                crate::harness::harness_connection_id(),
                test_session_id("s1"),
                Some(agent_id.as_str()),
            );
            let compact = read_nth_prompt_created(&h, 0);
            prompt_id = compact.agent_prompt_id.clone();
            let provider = h
                .provider_runtime
                .pending_prompts
                .get(&prompt_id)
                .cloned()
                .expect("provider owner");
            let unload_interceptor = "active-accounting-unload-interceptor";
            if teardown == "agent_unload" {
                connect_test_tool(&mut h, unload_interceptor);
                h.handle_extension_event(
                    unload_interceptor,
                    TestProtocolItem::Message(TestMessage::Intercept(Intercept {
                        selectors: vec![EventSelector::Exact(
                            tau_proto::EventName::PROVIDER_STANDALONE_EXECUTION_ACCOUNTED,
                        )],
                        priority: InterceptionPriority::new(0),
                    })),
                )
                .expect("register unload accounting interceptor");
            }
            if teardown != "shutdown" {
                if teardown == "provider_disconnect" {
                    h.handle_disconnect(&provider);
                } else {
                    h.remove_agent(&cid);
                    assert!(
                        h.runtime_io.publication.pending_intercept.is_some(),
                        "unload must wait for parked accounting"
                    );
                    assert!(h.agent_runtime.agent_registry.agents.contains_key(&cid));
                    h.handle_extension_event(
                        unload_interceptor,
                        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                            action: InterceptAction::Pass(None),
                        })),
                    )
                    .expect("commit accounting before unload");
                }
                let accounted = h
                    .session_runtime
                    .agent_store
                    .agent_events(agent_id.as_str())
                    .expect("agent events")
                    .into_iter()
                    .find_map(|record| match record.event {
                        Event::ProviderStandaloneExecutionAccounted(accounted)
                            if accounted.agent_prompt_id == prompt_id =>
                        {
                            Some(accounted)
                        }
                        _ => None,
                    })
                    .expect("disconnect accounting");
                assert_eq!(
                    accounted.finality,
                    tau_proto::StandaloneExecutionAccountingFinality::Final
                );
                assert!(matches!(
                    accounted.usage,
                    tau_proto::StandaloneExecutionUsage::Unknown
                ));
                h.shutdown().expect("shutdown after provider loss");
            } else {
                h.shutdown().expect("graceful shutdown");
            }
        }
        wait_for_session_unlock(&state, "s1");
        let mut resumed =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        assert_eq!(
            resumed
                .session_runtime
                .current_session_state
                .token_usage
                .total
                .requests,
            1,
            "{teardown}"
        );
        assert!(
            !resumed
                .prompt_coordination
                .standalone_accounting
                .owners
                .contains_key(&prompt_id)
        );
        resumed.shutdown().expect("shutdown resumed harness");
    }
}

/// Lifecycle loss after a cancellation-time initial uses the sole Unknown
/// correction to revoke authority without counting another request.
#[test]
fn lifecycle_loss_finalizes_awaiting_cancellation_without_second_request() {
    for teardown in ["provider_disconnect", "agent_unload", "shutdown"] {
        let td = TempDir::new().expect("tempdir");
        let state = td.path().join("state");
        let (prompt_id, durable_agent_id);
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
            let cid = ensure_test_user_agent(&mut h);
            let agent_id = durable_agent_id_for_conversation(&h, &cid);
            durable_agent_id = agent_id.clone();
            h.handle_compact_request(
                crate::harness::harness_connection_id(),
                test_session_id("s1"),
                Some(agent_id.as_str()),
            );
            let compact = read_nth_prompt_created(&h, 0);
            prompt_id = compact.agent_prompt_id.clone();
            let provider = h
                .provider_runtime
                .pending_prompts
                .get(&prompt_id)
                .cloned()
                .expect("provider owner");
            h.publish_awaiting_cancelled_standalone_accounting(&prompt_id);
            match teardown {
                "provider_disconnect" => h.handle_disconnect(&provider),
                "agent_unload" => h.remove_agent(&cid),
                "shutdown" => {}
                _ => unreachable!("fixed teardown cases"),
            }
            h.shutdown().expect("shutdown after lifecycle loss");
        }
        wait_for_session_unlock(&state, "s1");
        let mut resumed =
            quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
                .expect("resume");
        let records = resumed
            .session_runtime
            .agent_store
            .agent_events(durable_agent_id.as_str())
            .expect("agent events");
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    &record.event,
                    Event::ProviderStandaloneExecutionAccountingCorrected(corrected)
                        if corrected.agent_prompt_id == prompt_id
                            && matches!(
                                corrected.usage,
                                tau_proto::StandaloneExecutionUsage::Unknown
                            )
                ))
                .count(),
            1,
            "{teardown}"
        );
        assert_eq!(
            resumed
                .session_runtime
                .current_session_state
                .token_usage
                .total
                .requests,
            1,
            "{teardown}"
        );
        resumed.shutdown().expect("shutdown resumed harness");
    }
}

/// Unload must wait when terminal accounting was already parked before the
/// unload request removed its live owner.
#[test]
fn unload_waits_for_preparked_terminal_accounting() {
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
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let compact = read_nth_prompt_created(&h, 0);
    let interceptor = "preparked-terminal-accounting";
    connect_test_tool(&mut h, interceptor);
    h.handle_extension_event(
        interceptor,
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_STANDALONE_EXECUTION_ACCOUNTED,
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register accounting interceptor");
    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact).expect("valid terminal"),
    )
    .expect("park terminal accounting");
    h.remove_agent(&cid);
    assert!(h.agent_runtime.agent_registry.agents.contains_key(&cid));
    h.handle_extension_event(
        interceptor,
        TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        })),
    )
    .expect("commit terminal accounting");
    assert!(
        h.session_runtime
            .agent_store
            .agent_events(agent_id.as_str())
            .expect("agent events")
            .iter()
            .any(|record| matches!(
                &record.event,
                Event::ProviderStandaloneExecutionAccounted(accounted)
                    if accounted.agent_prompt_id == compact.agent_prompt_id
            ))
    );
    h.shutdown().expect("shutdown");
}

/// An unload barrier remains until every accounting obligation for one agent
/// commits, even when one closure parks the others behind interception.
#[test]
fn unload_waits_for_all_standalone_accounting_obligations() {
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
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let first = read_nth_prompt_created(&h, 0);
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            agent_prompt_id: Some(first.agent_prompt_id.clone()),
        },
    );
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let second = read_nth_prompt_created(&h, 1);
    let interceptor = "multiple-accounting-unload";
    connect_test_tool(&mut h, interceptor);
    h.handle_extension_event(
        interceptor,
        TestProtocolItem::Message(TestMessage::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix(
                "provider.standalone_execution".to_owned(),
            )],
            priority: InterceptionPriority::new(0),
        })),
    )
    .expect("register accounting interceptor");
    h.remove_agent(&cid);
    assert!(h.agent_runtime.agent_registry.agents.contains_key(&cid));
    while h.runtime_io.publication.pending_intercept.is_some() {
        h.handle_extension_event(
            interceptor,
            TestProtocolItem::Message(TestMessage::InterceptReply(InterceptReply {
                action: InterceptAction::Pass(None),
            })),
        )
        .expect("advance accounting interception");
    }
    let records = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent events");
    for prompt_id in [&first.agent_prompt_id, &second.agent_prompt_id] {
        assert!(records.iter().any(|record| match &record.event {
            Event::ProviderStandaloneExecutionAccounted(accounted) => {
                &accounted.agent_prompt_id == prompt_id
            }
            Event::ProviderStandaloneExecutionAccountingCorrected(corrected) => {
                &corrected.agent_prompt_id == prompt_id
            }
            _ => false,
        }));
    }
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        2
    );
    h.shutdown().expect("shutdown");
}

/// Retrying two append-rejected accounting closures keeps the second visible
/// to the unload barrier while the first commits.
#[test]
fn unload_waits_for_dual_retained_accounting_retry_batch() {
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
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let first = read_nth_prompt_created(&h, 0);
    h.handle_cancel_prompt(
        crate::harness::harness_connection_id(),
        &tau_proto::UiCancelPrompt {
            session_id: test_session_id("s1"),
            target_agent_id: Some(agent_id.clone()),
            agent_prompt_id: Some(first.agent_prompt_id.clone()),
        },
    );
    h.handle_compact_request(
        crate::harness::harness_connection_id(),
        test_session_id("s1"),
        Some(agent_id.as_str()),
    );
    let second = read_nth_prompt_created(&h, 1);
    reject_semantic_admissions(&h, 2);
    h.remove_agent(&cid);
    assert_eq!(
        h.prompt_coordination.standalone_accounting.retained.len(),
        2
    );
    assert!(h.agent_runtime.agent_registry.agents.contains_key(&cid));
    h.retry_pending_agent_publications();
    assert!(
        h.prompt_coordination
            .standalone_accounting
            .retained
            .is_empty()
    );
    let records = h
        .session_runtime
        .agent_store
        .agent_events(agent_id.as_str())
        .expect("agent events");
    for prompt_id in [&first.agent_prompt_id, &second.agent_prompt_id] {
        assert!(records.iter().any(|record| match &record.event {
            Event::ProviderStandaloneExecutionAccounted(accounted) => {
                &accounted.agent_prompt_id == prompt_id
            }
            Event::ProviderStandaloneExecutionAccountingCorrected(corrected) => {
                &corrected.agent_prompt_id == prompt_id
            }
            _ => false,
        }));
    }
    assert_eq!(
        h.session_runtime
            .current_session_state
            .token_usage
            .total
            .requests,
        2
    );
    h.shutdown().expect("shutdown");
}

//! Tests for status reporting behavior.

use super::*;

/// An exact current prompt snapshot without status must not inherit stale
/// status availability from a prior terminal.
#[test]
fn after_response_alert_prefers_frozen_status_absence_over_stale_terminal_state() {
    let td = TempDir::new().expect("tempdir");
    let mut h = quiet_provider_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let prompt_id = tau_proto::AgentPromptId::parse("ap-no-status").expect("prompt");
    h.prompt_coordination
        .prompt_runtime
        .tool_specs
        .insert(prompt_id.clone(), Vec::new());
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .terminal_status_was_available = true;
    let alerts = path_std_collections::BTreeMap::from([(
        "working".to_owned(),
        tau_config::settings::ContextSizeAlert {
            threshold: path_tau_config_settings::ContextSizeAlertThreshold::new(100)
                .expect("positive test threshold"),
            enable: true,
            message: "working-only".to_owned(),
            when: tau_config::settings::ContextPolicyWhen {
                at: path_tau_config_settings::ContextPolicyPoint::AfterResponse,
                statuses: Some(vec![tau_proto::AgentWorkStatusPhase::Working]),
            },
        },
    )]);
    h.queue_crossed_context_size_alerts_for_prompt(&cid, &prompt_id, Some(200), &alerts);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .any(PendingPrompt::is_context_size_alert)
    );
    h.shutdown().expect("shutdown");
}

/// Provider-watch fanout must suppress retries through the configured
/// threshold, dedupe later retry storms without freezing the late-watcher
/// snapshot, preserve phase/category transitions, and stop when the relation or
/// session is removed.
#[test]
fn agent_watch_provider_status_fanout_dedupes_and_cleans_up() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let late_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    let late_id = durable_agent_id_for_conversation(&h, &late_cid).to_string();
    for cid in [&watcher_cid, &late_cid] {
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(cid)
            .expect("watcher")
            .turn
            .turn_state = AgentTurnState::AgentThinking {
            agent_prompt_id: test_agent_prompt_id(format!("busy-{cid}")),
        };
    }
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    let session_id = h.session_runtime.current_session_id.clone();
    let status = |state| tau_proto::AgentWatchProviderStatusNotification {
        session_id: session_id.clone(),
        subscription_id: String::new(),
        turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(4),
        agent_prompt_id: test_agent_prompt_id("sp-provider-watch"),
        state,
        initial: false,
    };
    for attempt in 1..=50 {
        h.update_agent_watch_provider_status(
            &watched_id,
            status(tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Transport,
                attempt,
                next_retry_delay_secs: attempt,
            }),
        );
    }
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Transport,
            attempt: 3,
            next_retry_delay_secs: 3,
        }),
    );
    assert!(matches!(
        h.agent_runtime.agent_watch.provider_status[&watched_id].state,
        tau_proto::AgentWatchProviderState::Retrying { attempt: 50, .. }
    ));
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Throttle,
            attempt: 51,
            next_retry_delay_secs: 60,
        }),
    );
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 51 }),
    );
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::TerminalError {
            failure_kind: tau_proto::ProviderFailureKind::ContextWindowExceeded,
            attempt: 51,
        }),
    );

    let watcher_statuses: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                && message.recipient_id.as_str() == watcher_id
        })
        .collect();
    assert_eq!(watcher_statuses.len(), 4);
    assert!(matches!(
        watcher_statuses[0]
            .watch_provider_status
            .as_ref()
            .expect("retry status")
            .state,
        tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Transport,
            attempt: 6,
            ..
        }
    ));
    assert!(matches!(
        h.agent_runtime.agent_watch.provider_status[&watched_id].state,
        tau_proto::AgentWatchProviderState::TerminalError { attempt: 51, .. }
    ));
    assert!(
        watcher_statuses
            .iter()
            .all(|message| !message.message.contains("secret-provider-body")),
        "only closed categories may cross the watch boundary"
    );

    h.set_agent_watch(
        &late_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let late = session_agent_message_received_events(&h)
        .into_iter()
        .rev()
        .find(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                && message.recipient_id.as_str() == late_id
        })
        .expect("late status snapshot");
    let late_status = late.watch_provider_status.expect("late typed status");
    assert!(late_status.initial);
    assert!(matches!(
        late_status.state,
        tau_proto::AgentWatchProviderState::TerminalError { attempt: 51, .. }
    ));
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get(&late_cid)
            .expect("late watcher")
            .dispatch
            .pending_prompts
            .is_empty(),
        "initial snapshots are client-visible but never model prompts"
    );
    assert_eq!(
        h.agent_watch_provider_status_summary(&watched_id)
            .as_deref(),
        Some("terminal error (context_window_exceeded)")
    );
    h.set_agent_watch(
        &late_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let reenabled_initials = session_agent_message_received_events(&h)
        .iter()
        .filter(|message| {
            message.recipient_id.as_str() == late_id
                && message
                    .watch_provider_status
                    .as_ref()
                    .is_some_and(|status| status.initial)
        })
        .count();
    assert_eq!(reenabled_initials, 2);
    assert!(
        h.agent_runtime.agent_registry.agents[&late_cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );

    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );
    assert!(
        h.agent_runtime
            .agent_watch
            .provider_deliveries
            .keys()
            .all(|subscription_id| {
                h.agent_runtime
                    .agent_watch
                    .subscriptions
                    .values()
                    .any(|active| active == subscription_id)
            }),
        "disable must remove only the retired subscription's dedupe keys"
    );
    let before = session_agent_message_received_events(&h).len();
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::Blocked {
            category: tau_proto::AgentWatchProviderCategory::Compaction,
        }),
    );
    let after_disable = session_agent_message_received_events(&h);
    assert_eq!(
        after_disable
            .iter()
            .skip(before)
            .filter(|message| message.recipient_id.as_str() == watcher_id)
            .count(),
        0
    );
    h.prune_agent_watch(&late_id, &watched_id);
    assert!(h.watchers_for_agent(&watched_id).is_empty());
    assert!(
        h.agent_runtime.agent_watch.provider_deliveries.is_empty(),
        "pruning the final relation drops its delivery bucket"
    );

    h.switch_session(
        test_session_id("watch-status-next"),
        tau_proto::SessionStartReason::New,
    )
    .expect("switch session");
    assert!(h.agent_runtime.agent_watch.provider_status.is_empty());
    assert!(h.agent_runtime.agent_watch.provider_deliveries.is_empty());
    assert!(h.agent_runtime.agent_watch.subscriptions.is_empty());
    h.shutdown().expect("shutdown");
}

/// Unloading a watched target during context recovery must remove its current
/// provider snapshot and every subscription/dedupe bucket. The authoritative
/// watch operation must reject the stopped target without mutating any of its
/// five state maps, and reload must require one explicit fresh subscription.
/// A late terminal update must neither recreate stale reload state nor append
/// to the watcher.
#[test]
fn unloading_watched_agent_clears_status_and_stops_durable_fanout() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let session_id = h.session_runtime.current_session_id.clone();
    let status = |state| tau_proto::AgentWatchProviderStatusNotification {
        session_id: session_id.clone(),
        subscription_id: String::new(),
        turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
        agent_prompt_id: test_agent_prompt_id("target-unload-prompt"),
        state,
        initial: false,
    };
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::Retrying {
            category: tau_proto::AgentWatchProviderCategory::Transport,
            attempt: 1,
            next_retry_delay_secs: 1,
        }),
    );
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 2 }),
    );
    let retired_subscription = h.agent_runtime.agent_watch.subscriptions
        [&(watcher_id.clone(), watched_id.clone())]
        .clone();
    let snapshots_before_unload = event_log_events(&h)
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::AgentWatchesUpdated(snapshot)
                    if snapshot.watcher_id.as_str() == watcher_id
            )
        })
        .count();

    h.remove_agent(&watched_cid);
    assert!(h.watchers_for_agent(&watched_id).is_empty());
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&watched_id)
    );
    let watches_after_unload = h.agent_runtime.agent_watch.forward.clone();
    let watchers_after_unload = h.agent_runtime.agent_watch.reverse.clone();
    let subscriptions_after_unload = h.agent_runtime.agent_watch.subscriptions.clone();
    let provider_status_after_unload = h.agent_runtime.agent_watch.provider_status.clone();
    let provider_deliveries_after_unload = h.agent_runtime.agent_watch.provider_deliveries.clone();
    let enable_error = path_crate_internal_tools::InternalToolHost::new(&mut h)
        .try_set_agent_watch(
            &watcher_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        )
        .expect_err("an unloaded target must reject a new watch");
    assert!(enable_error.contains("not live"));
    assert_eq!(h.agent_runtime.agent_watch.forward, watches_after_unload);
    assert_eq!(h.agent_runtime.agent_watch.reverse, watchers_after_unload);
    assert_eq!(
        h.agent_runtime.agent_watch.subscriptions,
        subscriptions_after_unload
    );
    assert_eq!(
        h.agent_runtime.agent_watch.provider_status,
        provider_status_after_unload
    );
    assert_eq!(
        h.agent_runtime.agent_watch.provider_deliveries,
        provider_deliveries_after_unload
    );
    let unknown_error = path_crate_internal_tools::InternalToolHost::new(&mut h)
        .try_set_agent_watch(
            &watcher_id,
            "agent-never-loaded",
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        )
        .expect_err("an unknown target must reject a new watch");
    assert!(unknown_error.contains("unknown agent"));
    assert_eq!(h.agent_runtime.agent_watch.forward, watches_after_unload);
    assert_eq!(h.agent_runtime.agent_watch.reverse, watchers_after_unload);
    assert_eq!(
        h.agent_runtime.agent_watch.subscriptions,
        subscriptions_after_unload
    );
    assert_eq!(
        h.agent_runtime.agent_watch.provider_status,
        provider_status_after_unload
    );
    assert_eq!(
        h.agent_runtime.agent_watch.provider_deliveries,
        provider_deliveries_after_unload
    );
    // Exercise the local fallback after the committed unload reaction.
    h.retire_agent_watch_endpoint(
        &watched_id,
        Some(tau_proto::AgentWatchLifecycleReason::UnexpectedUnload),
    );
    let lifecycle_messages: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.recipient_id.as_str() == watcher_id
                && message.sender_id.as_str() == watched_id
                && message.kind == tau_proto::AgentMessageKind::WatchLifecycle
        })
        .collect();
    assert_eq!(
        lifecycle_messages.len(),
        1,
        "the committed unload must notify a surviving watcher exactly once"
    );
    assert_eq!(lifecycle_messages[0].message, "");
    assert_eq!(
        lifecycle_messages[0].watch_lifecycle,
        Some(tau_proto::AgentWatchLifecycleNotification {
            state: tau_proto::AgentWatchLifecycleState::Stopped,
            reason: tau_proto::AgentWatchLifecycleReason::UnexpectedUnload,
        })
    );
    let durable_before = h
        .session_runtime
        .agent_store
        .agent_events(&watcher_id)
        .expect("watcher durable log")
        .len();
    h.update_agent_watch_provider_status(
        &watched_id,
        status(tau_proto::AgentWatchProviderState::TerminalError {
            failure_kind: tau_proto::ProviderFailureKind::ContextWindowExceeded,
            attempt: 2,
        }),
    );
    assert!(
        h.publish_agent_watch_response_from_agent(
            &watched_cid,
            watcher_id.clone(),
            "late final response".to_owned(),
        )
        .is_err(),
        "an unloaded sender cannot publish final-response watch content"
    );

    assert_eq!(
        h.session_runtime
            .agent_store
            .agent_events(&watcher_id)
            .expect("watcher durable log")
            .len(),
        durable_before,
        "post-unload terminal status must not append a recipient fact"
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(&watched_id)
    );
    assert!(h.watchers_for_agent(&watched_id).is_empty());
    assert!(
        h.agent_runtime
            .agent_watch
            .subscriptions
            .keys()
            .all(|(_, watched)| watched != &watched_id)
    );
    assert!(h.agent_runtime.agent_watch.provider_deliveries.is_empty());
    assert_eq!(h.agent_watch_provider_status_summary(&watched_id), None);
    let replacement_snapshots: Vec<_> = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentWatchesUpdated(snapshot) if snapshot.watcher_id.as_str() == watcher_id => {
                Some(snapshot)
            }
            _ => None,
        })
        .skip(snapshots_before_unload)
        .collect();
    assert_eq!(replacement_snapshots.len(), 1);
    assert!(replacement_snapshots[0].watched_agent_ids.is_empty());
    assert_eq!(
        replacement_snapshots[0].cause,
        tau_proto::AgentWatchUpdateCause::WatcherPruned
    );

    let reloaded_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let minted_id = durable_agent_id_for_conversation(&h, &reloaded_cid).to_string();
    h.agent_runtime
        .agent_registry
        .agent_routes
        .remove(&minted_id);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&reloaded_cid)
        .expect("reloaded agent")
        .identity
        .agent_id = Some(watched_id.clone());
    h.agent_runtime
        .agent_registry
        .agent_routes
        .insert(watched_id.clone(), reloaded_cid.clone());
    h.ensure_loaded_agent_for_agent(&reloaded_cid, &watched_id);
    assert!(h.watchers_for_agent(&watched_id).is_empty());
    assert_eq!(h.agent_watch_provider_status_summary(&watched_id), None);
    assert!(
        !h.agent_runtime
            .agent_watch
            .forward
            .contains_key(&watcher_id)
    );
    path_crate_internal_tools::InternalToolHost::new(&mut h)
        .try_set_agent_watch(
            &watcher_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        )
        .expect("a freshly reloaded live target accepts a new watch");
    assert_eq!(h.watchers_for_agent(&watched_id), vec![watcher_id.clone()]);
    assert_eq!(h.agent_runtime.agent_watch.subscriptions.len(), 1);
    assert_ne!(
        h.agent_runtime.agent_watch.subscriptions[&(watcher_id.clone(), watched_id.clone())],
        retired_subscription,
        "same-session reload requires a freshly minted subscription"
    );
    h.shutdown().expect("shutdown");
}

/// Restart must preserve an already delivered provider-status fact as
/// transcript context without reconstructing memory-only retry state or
/// re-fanning it out.
#[test]
fn agent_watch_provider_status_replay_preserves_context_without_refanout() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let (watched_id, watcher_id);
    {
        let mut h = echo_harness(&sp).expect("start");
        let watched_cid = ensure_test_user_agent(&mut h);
        let watcher_cid = h.create_durable_user_agent(
            h.session_runtime.current_session_id.clone(),
            &h.config.selected_role.clone(),
        );
        watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
        watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&watcher_cid)
            .expect("watcher")
            .turn
            .turn_state = AgentTurnState::AgentThinking {
            agent_prompt_id: test_agent_prompt_id("watcher-busy"),
        };
        h.set_agent_watch(
            &watcher_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        );
        h.update_agent_watch_provider_status(
            &watched_id,
            tau_proto::AgentWatchProviderStatusNotification {
                session_id: h.session_runtime.current_session_id.clone(),
                subscription_id: String::new(),
                turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
                agent_prompt_id: test_agent_prompt_id("sp-replay-status"),
                state: tau_proto::AgentWatchProviderState::Retrying {
                    category: tau_proto::AgentWatchProviderCategory::Throttle,
                    attempt: 8,
                    next_retry_delay_secs: 9,
                },
                initial: false,
            },
        );
        h.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&sp, "s1");

    let mut resumed =
        echo_harness_with_start_reason("s1", &sp, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert!(resumed.agent_runtime.agent_watch.provider_status.is_empty());
    assert!(
        resumed
            .agent_runtime
            .agent_watch
            .provider_deliveries
            .is_empty()
    );
    assert!(resumed.agent_runtime.agent_watch.forward.is_empty());
    let watcher_cid = resumed
        .agent_runtime
        .agent_registry
        .agent_routes
        .get(&watcher_id)
        .cloned()
        .expect("watcher restored");
    assert!(
        agent_tree_for_conversation(&resumed, &watcher_cid)
            .nodes()
            .iter()
            .any(|node| matches!(
                &node.entry,
                AgentEntry::AgentMessage {
                    kind: tau_proto::AgentMessageKind::WatchProviderStatus,
                    watch_provider_status: Some(status),
                    watch_work_status,
                    watch_long_wait,
                    ..
                } if status.agent_prompt_id.as_str() == "sp-replay-status"
                    && watch_work_status.is_none()
                    && watch_long_wait.is_none()
            )),
        "durable live status remains transcript context"
    );
    assert!(
        resumed.agent_runtime.agent_registry.agents[&watcher_cid]
            .dispatch
            .pending_prompts
            .is_empty(),
        "replay must not queue the historical status as fresh model input"
    );
    resumed
        .handle_authenticated_ui_prompt_submitted(
            crate::harness::harness_connection_id(),
            UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: "continue after restart".to_owned(),
                agent_id: crate::parse_agent_id(&watcher_id),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            },
        )
        .expect("activate restored watcher");
    let prompt = read_nth_prompt_created(&resumed, 0);
    let context = serde_json::to_string(&prompt.context).expect("serialize prompt context");
    assert_eq!(
        context.matches("provider status: retrying").count(),
        1,
        "durable status appears exactly once in the next provider context"
    );
    resumed.shutdown().expect("shutdown");
}

/// A watch receives nonactivating current status and isolated live transitions.
#[test]
fn agent_watch_reports_structured_work_status() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let watched_cid = ensure_test_user_agent(&mut h);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&h, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid).to_string();
    h.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let initial = session_agent_message_received_events(&h)
        .into_iter()
        .find(|message| message.kind == tau_proto::AgentMessageKind::WatchWorkStatus)
        .and_then(|message| message.watch_work_status)
        .expect("initial status");
    assert!(initial.initial);
    assert_eq!(initial.phase, tau_proto::AgentWorkStatusPhase::Unreported);
    assert!(
        h.agent_runtime.agent_registry.agents[&watcher_cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );

    assert!(
        h.report_agent_work_status(
            &watched_cid,
            crate::WorkStatusReport::new(
                tau_proto::AgentWorkStatusPhase::Working,
                "trace lifecycle".to_owned()
            )
            .expect("valid work status report"),
        )
        .expect("status report")
    );
    assert!(
        !h.report_agent_work_status(
            &watched_cid,
            crate::WorkStatusReport::new(
                tau_proto::AgentWorkStatusPhase::Working,
                "trace lifecycle".to_owned()
            )
            .expect("valid work status report"),
        )
        .expect("idempotent status report")
    );
    let live: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| message.kind == tau_proto::AgentMessageKind::WatchWorkStatus)
        .collect();
    assert_eq!(live.len(), 2, "identical updates do not fan out");
    let update = live[1].watch_work_status.as_ref().expect("live status");
    assert!(!update.initial);
    assert_eq!(
        update.status_epoch,
        tau_proto::AgentWorkStatusEpoch::from_raw(1)
    );
    assert_eq!(update.title.as_deref(), Some("trace lifecycle"));

    let late_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let late_id = durable_agent_id_for_conversation(&h, &late_cid).to_string();
    h.set_agent_watch(
        &late_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let late = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchWorkStatus
                && message.recipient_id.as_str() == late_id
        })
        .collect::<Vec<_>>();
    assert_eq!(late.len(), 1);
    let late_status = late[0]
        .watch_work_status
        .as_ref()
        .expect("late current status");
    assert!(late_status.initial);
    assert_eq!(late_status.phase, tau_proto::AgentWorkStatusPhase::Working);
    assert_eq!(late_status.title.as_deref(), Some("trace lifecycle"));
    h.shutdown().expect("shutdown");
}

/// Watch prompts activate like direct messages and final responses, while
/// status/progress notifications remain isolated.
#[test]
fn agent_message_status_activation_class_covers_watch_prompt() {
    let make = |kind| tau_proto::AgentMessageReceived {
        message_id: "message-1".parse().expect("message id"),
        sender_id: "sender".parse().expect("sender id"),
        sender_session_id: None,
        recipient_id: "recipient".parse().expect("recipient id"),
        kind,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: String::new(),
    };
    for kind in [
        tau_proto::AgentMessageKind::Message,
        tau_proto::AgentMessageKind::WatchResponse,
        tau_proto::AgentMessageKind::WatchPrompt,
    ] {
        assert_eq!(
            agent_message_activation_class(&make(kind)),
            Some(crate::agent::AgentMessageActivationClass::OrdinaryAgentInput)
        );
    }
}

/// Isolated watched-agent progress remains model-visible without steering the
/// watcher to report Working for activity that is not user-addressed work.
#[test]
fn isolated_watch_notification_does_not_request_status_acknowledgement() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let prompt_id = test_agent_prompt_id("isolated-watch-status");
    h.prompt_coordination.prompt_runtime.tool_specs.insert(
        prompt_id.clone(),
        vec![
            shared_test_tool_spec("status"),
            shared_test_tool_spec("skill"),
        ],
    );
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt("isolated-watch-call".into(), prompt_id);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .lifecycle_notification_only_turn = true;
    h.execute_agent_tool_call(
        &cid,
        &AgentToolCall {
            call_ref: None,
            id: "isolated-watch-call".into(),
            name: ToolName::new("skill"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("dpc".to_owned()),
            )]),
        },
    )
    .expect("admit isolated substantive call");

    h.queue_working_reminder_if_needed(&cid);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .lifecycle_notification_only_turn = false;
    h.queue_working_reminder_if_needed(&cid);

    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty(),
        "isolated watched-agent progress must not demand Working status"
    );
    h.shutdown().expect("shutdown");
}

/// Work-status fanout follows only the direct watch edge and cannot cascade.
#[test]
fn watch_chain_work_status_does_not_cascade() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let b_cid = ensure_test_user_agent(&mut h);
    let a_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let c_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let a_id = durable_agent_id_for_conversation(&h, &a_cid).to_string();
    let b_id = durable_agent_id_for_conversation(&h, &b_cid).to_string();
    let c_id = durable_agent_id_for_conversation(&h, &c_cid).to_string();
    h.set_agent_watch(
        &a_id,
        &b_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        &c_id,
        &a_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.report_agent_work_status(
        &b_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "direct".to_owned(),
        )
        .expect("valid work status report"),
    )
    .expect("report");
    let live: Vec<_> = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchWorkStatus
                && message
                    .watch_work_status
                    .as_ref()
                    .is_some_and(|status| !status.initial)
        })
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].recipient_id.as_str(), a_id);
    h.shutdown().expect("shutdown");
}

#[test]
fn watch_chain_provider_status_turn_does_not_fan_out_final_response() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.config
        .accepted_harness_settings
        .agent_watch_retry_notification_threshold = 0;
    h.config.selected_model = Some("test/model".into());
    let a_cid = ensure_test_user_agent(&mut h);
    let b_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let c_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let a_id = durable_agent_id_for_conversation(&h, &a_cid).to_string();
    let b_id = durable_agent_id_for_conversation(&h, &b_cid).to_string();
    let c_id = durable_agent_id_for_conversation(&h, &c_cid).to_string();
    h.set_agent_watch(
        &a_id,
        &b_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.set_agent_watch(
        &b_id,
        &c_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.update_agent_watch_provider_status(
        &c_id,
        tau_proto::AgentWatchProviderStatusNotification {
            session_id: h.session_runtime.current_session_id.clone(),
            subscription_id: String::new(),
            turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
            agent_prompt_id: test_agent_prompt_id("sp-a-retry"),
            state: tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Transport,
                attempt: 1,
                next_retry_delay_secs: 1,
            },
            initial: false,
        },
    );
    let response_prompt_id = match &h.agent_runtime.agent_registry.agents[&b_cid]
        .turn
        .turn_state
    {
        AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
        state => panic!("status notification should dispatch b: {state:?}"),
    };
    assert!(
        h.agent_runtime.agent_registry.agents[&b_cid]
            .turn
            .lifecycle_notification_only_turn
    );
    h.handle_provider_response_finished(provider_text_response(
        &response_prompt_id,
        crate::parse_agent_id(&b_id),
        "acknowledged status",
    ))
    .expect("finish status-only turn");

    assert!(
        !session_agent_message_received_events(&h)
            .iter()
            .any(|message| {
                message.kind == tau_proto::AgentMessageKind::WatchResponse
                    && message.sender_id.as_str() == b_id
                    && message.recipient_id.as_str() == a_id
            }),
        "successful status-only completion must not cascade up the watch chain"
    );
    h.shutdown().expect("shutdown");
}

/// Reminder rendering names the required current phase directly.
#[test]
fn substantive_tool_reminder_uses_approved_wording() {
    assert_eq!(
        STATUS_REMINDER,
        "Set your status to `working` before continuing substantive tool work. Batch the `status` call with other tool calls when possible."
    );
}

/// The production dispatch boundary records only admitted substantive calls
/// from a frozen status-capable tool surface.
#[test]
fn working_reminder_is_recorded_at_substantive_tool_admission() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let unavailable_prompt = test_agent_prompt_id("working-reminder-status-unavailable");
    h.prompt_coordination.prompt_runtime.tool_specs.insert(
        unavailable_prompt.clone(),
        vec![shared_test_tool_spec("skill")],
    );
    let unavailable_surface_call = AgentToolCall {
        call_ref: None,
        id: "status-unavailable-work".into(),
        name: ToolName::new("skill"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("query".to_owned()),
            CborValue::Text("dpc".to_owned()),
        )]),
    };
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(unavailable_surface_call.id.clone(), unavailable_prompt);
    {
        let status = &mut h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status;
        assert!(!status.record_input_wait_timeout());
        assert!(!status.record_input_wait_timeout());
        assert!(status.record_input_wait_timeout());
    }
    h.execute_agent_tool_call(&cid, &unavailable_surface_call)
        .expect("accept status-unavailable skill");
    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status
            .take_working_reminder()
    );
    {
        let status = &mut h
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status;
        assert!(!status.record_input_wait_timeout());
        assert!(!status.record_input_wait_timeout());
        assert!(
            status.record_input_wait_timeout(),
            "status-unavailable substantive admission resets the wait guard"
        );
    }

    let prompt_id = test_agent_prompt_id("working-reminder-admission");
    h.prompt_coordination.prompt_runtime.tool_specs.insert(
        prompt_id.clone(),
        vec![
            shared_test_tool_spec("status"),
            shared_test_tool_spec("skill"),
        ],
    );
    let call = AgentToolCall {
        call_ref: None,
        id: "accepted-task-work".into(),
        name: ToolName::new("skill"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("query".to_owned()),
            CborValue::Text("dpc".to_owned()),
        )]),
    };
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(call.id.clone(), prompt_id.clone());
    h.execute_agent_tool_call(&cid, &call)
        .expect("accept substantive skill call");
    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status
            .take_working_reminder()
    );

    let wait = AgentToolCall {
        call_ref: None,
        id: "lifecycle-wait".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(wait.id.clone(), prompt_id.clone());
    h.execute_agent_tool_call(&cid, &wait)
        .expect("accept lifecycle wait");
    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status
            .take_working_reminder()
    );

    let unavailable = AgentToolCall {
        id: "rejected-task-work".into(),
        name: ToolName::new("unknown_tool"),
        ..call
    };
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(unavailable.id.clone(), prompt_id);
    h.execute_agent_tool_call(&cid, &unavailable)
        .expect("reject unknown call in band");
    assert!(
        !h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status
            .take_working_reminder()
    );

    h.install_internal_tool_handlers(vec![std::sync::Arc::new(RejectingStatusTool)]);
    let rejected_status_prompt = test_agent_prompt_id("working-reminder-rejected-status");
    h.prompt_coordination.prompt_runtime.tool_specs.insert(
        rejected_status_prompt.clone(),
        vec![shared_test_tool_spec("status")],
    );
    let rejected_status = AgentToolCall {
        call_ref: None,
        id: "rejected-status-only".into(),
        name: ToolName::new("status"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![
            (
                CborValue::Text("state".to_owned()),
                CborValue::Text("invalid".to_owned()),
            ),
            (
                CborValue::Text("task_name".to_owned()),
                CborValue::Text("Rejected status-only round".to_owned()),
            ),
        ]),
    };
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(rejected_status.id.clone(), rejected_status_prompt);
    h.execute_agent_tool_call(&cid, &rejected_status)
        .expect("settle rejected status-only call");
    h.queue_working_reminder_if_needed(&cid);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .iter()
            .all(|prompt| prompt.text != STATUS_REMINDER),
        "a rejected status-only round must not queue a Working reminder"
    );
    h.shutdown().expect("shutdown");
}

/// Cancelling a substantive nonworking round retires its reminder obligation,
/// so a later exempt round cannot inherit it.
#[test]
fn cancelled_tool_round_does_not_leak_working_reminder() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .turn
        .work_status
        .record_substantive_tool_admission();

    h.finalize_cancelled_tool_turn(&cid);
    h.queue_working_reminder_if_needed(&cid);

    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// An activating background-completion prompt remains subject to the ordinary
/// substantive-tool reminder policy.
#[test]
fn background_completion_substantive_tool_admission_records_working_reminder() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(
        &cid,
        PendingPrompt::activating_background_completion("background work ready".to_owned()),
    )
    .expect("dispatch activating background completion");
    let prompt_id = match &h.agent_runtime.agent_registry.agents[&cid].turn.turn_state {
        AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
        state => panic!("background completion did not activate: {state:?}"),
    };
    h.prompt_coordination.prompt_runtime.tool_specs.insert(
        prompt_id.clone(),
        vec![
            shared_test_tool_spec("status"),
            shared_test_tool_spec("skill"),
        ],
    );
    let call = AgentToolCall {
        call_ref: None,
        id: "background-substantive-work".into(),
        name: ToolName::new("skill"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("query".to_owned()),
            CborValue::Text("dpc".to_owned()),
        )]),
    };
    h.prompt_coordination
        .prompt_runtime
        .record_tool_call_prompt(call.id.clone(), prompt_id);

    h.execute_agent_tool_call(&cid, &call)
        .expect("admit background substantive work");

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status
            .take_working_reminder(),
        "activating background substantive work must request Working"
    );
    h.shutdown().expect("shutdown");
}
/// Self metadata uses the exact prompt-owned model parameters rather than
/// mutable role selection and exposes the initial unreported status explicitly.
#[test]
fn self_info_uses_prompt_authority_and_current_runtime_status() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = echo_harness(&state).expect("start");
    let recorded = path_std_sync::Arc::new(RecordingSelfInfoTool(path_std_sync::Mutex::new(None)));
    h.install_internal_tool_handlers(vec![recorded.clone()]);
    let cid = ensure_test_user_agent(&mut h);
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("inspect self".to_owned()))
        .expect("dispatch prompt");
    let prompt = read_nth_prompt_created(&h, 0);

    h.handle_provider_response_finished(provider_tool_response(
        &prompt,
        "self-info-call",
        "record_self_info",
        CborValue::Map(Vec::new()),
    ))
    .expect("run self-info call");

    let info = recorded
        .0
        .lock()
        .expect("self-info recorder")
        .clone()
        .expect("recorded metadata");
    assert_eq!(info.agent_id, prompt.agent_id);
    assert_eq!(info.session_id, prompt.session_id);
    assert_eq!(info.model, prompt.model);
    assert_eq!(info.effort, prompt.model_params.effort);
    assert_eq!(
        info.session_dir,
        Some(tau_config::settings::sessions_dir_of(&state).join(prompt.session_id.as_str()))
    );
    assert_eq!(
        info.work_status.phase(),
        tau_proto::AgentWorkStatusPhase::Unreported
    );
    assert_eq!(info.work_status.title(), None);
    h.shutdown().expect("shutdown");
}

/// The production response handler challenges two successful finals while
/// Working, then accepts the third through the bounded escape.
#[test]
fn working_final_gate_uses_bounded_escape() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "finish gate".to_owned(),
        )
        .expect("valid work status report"),
    )
    .expect("working");
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let agent_id = durable_agent_id_for_conversation(&h, &cid);
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    finish_test_agent_context_wait(&mut h, &watcher_id);
    h.set_agent_watch(
        watcher_id.as_str(),
        agent_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("finish gate".to_owned()))
        .expect("dispatch production user prompt");
    let outer_generation = h.agent_runtime.agent_registry.agents[&cid]
        .turn
        .turn_generation;
    for index in 0..2 {
        let prompt_id = match &h.agent_runtime.agent_registry.agents[&cid].turn.turn_state {
            AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
            state => panic!("candidate {index} lacks an active prompt: {state:?}"),
        };
        h.handle_provider_response_finished(provider_text_response(
            &prompt_id,
            agent_id.clone(),
            &format!("candidate {index}"),
        ))
        .expect("finish candidate");
        assert!(
            session_agent_message_received_events(&h)
                .iter()
                .all(|message| {
                    message.kind != tau_proto::AgentMessageKind::WatchResponse
                        || message.sender_id != agent_id
                        || message.recipient_id != watcher_id
                }),
            "Working candidate {index} must not reach its watcher"
        );
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
            AgentTurnState::AgentThinking { .. }
        ));
        assert!(
            !h.agent_runtime.agent_registry.agents[&cid]
                .turn
                .terminal_notice_eligible,
            "challenged final must not arm an outer-finish notice"
        );
    }
    let final_prompt_id = match &h.agent_runtime.agent_registry.agents[&cid].turn.turn_state {
        AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
        state => panic!("escape continuation lacks an active prompt: {state:?}"),
    };
    assert!(
        !h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .lifecycle_notification_only_turn,
        "Working challenge continuation remains an ordinary outer turn"
    );
    h.handle_provider_response_finished(provider_text_response(
        &final_prompt_id,
        agent_id.clone(),
        "accepted by bounded escape",
    ))
    .expect("finish through bounded escape");
    let agent = h
        .agent_runtime
        .agent_registry
        .agents
        .get(&cid)
        .expect("agent");
    assert_eq!(
        agent.turn.work_status.phase(),
        tau_proto::AgentWorkStatusPhase::Unknown
    );
    let watch_responses = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchResponse
                && message.sender_id == agent_id
                && message.recipient_id == watcher_id
        })
        .collect::<Vec<_>>();
    assert_eq!(
        watch_responses.len(),
        1,
        "the bounded escape releases exactly one later final response to the watcher"
    );
    assert_eq!(
        watch_responses[0].message, "accepted by bounded escape",
        "challenged candidate text must remain permanently withheld"
    );
    assert_eq!(agent.turn.turn_generation, outer_generation);
    assert!(matches!(agent.turn.turn_state, AgentTurnState::Idle));
    assert_eq!(
        event_log_events(&h)
            .iter()
            .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
            .count(),
        3
    );
    h.shutdown().expect("shutdown");
}

/// Unreported agents with a frozen status-capable prompt surface receive two
/// final challenges, while a prompt whose frozen surface lacks status finishes
/// immediately.
#[test]
fn unreported_final_gate_uses_frozen_status_availability_and_bounded_escape() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.install_internal_tool_handlers(vec![std::sync::Arc::new(RejectingStatusTool)]);
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = durable_agent_id_for_conversation(&h, &cid);

    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("report status".to_owned()))
        .expect("dispatch status-capable prompt");
    let mut completed_prompt_ids = Vec::new();
    for index in 0..2 {
        let prompt_id = match &h.agent_runtime.agent_registry.agents[&cid].turn.turn_state {
            AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
            state => panic!("candidate {index} lacks an active prompt: {state:?}"),
        };
        assert!(
            h.prompt_coordination.prompt_runtime.tool_specs[&prompt_id]
                .iter()
                .any(|spec| h.tool_model_visible_name(spec).as_str() == "status"),
            "the immutable dispatched prompt surface must expose status"
        );
        h.handle_provider_response_finished(provider_text_response(
            &prompt_id,
            agent_id.clone(),
            &format!("unreported candidate {index}"),
        ))
        .expect("challenge unreported final");
        completed_prompt_ids.push(prompt_id);
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
            AgentTurnState::AgentThinking { .. }
        ));
    }
    let escape_prompt_id = match &h.agent_runtime.agent_registry.agents[&cid].turn.turn_state {
        AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
        state => panic!("escape lacks an active prompt: {state:?}"),
    };
    h.handle_provider_response_finished(provider_text_response(
        &escape_prompt_id,
        agent_id,
        "accepted unreported final",
    ))
    .expect("accept bounded unreported escape");
    completed_prompt_ids.push(escape_prompt_id);
    assert!(matches!(
        h.agent_runtime.agent_registry.agents[&cid].turn.turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .work_status
            .phase(),
        tau_proto::AgentWorkStatusPhase::Unreported
    );
    assert!(
        completed_prompt_ids.iter().all(|prompt_id| !h
            .prompt_coordination
            .prompt_runtime
            .tool_specs
            .contains_key(prompt_id)),
        "every completed no-tool prompt must release its frozen tool surface"
    );

    h.shutdown().expect("shutdown status-capable harness");

    let mut statusless_h =
        echo_harness(td.path().join("statusless-state")).expect("start status-less harness");
    let statusless_cid = ensure_test_user_agent(&mut statusless_h);
    let statusless_id = durable_agent_id_for_conversation(&statusless_h, &statusless_cid);
    statusless_h
        .dispatch_prompt_for_agent(
            &statusless_cid,
            PendingPrompt::user("finish without status tool".to_owned()),
        )
        .expect("dispatch status-less prompt");
    let statusless_prompt_id = match &statusless_h.agent_runtime.agent_registry.agents
        [&statusless_cid]
        .turn
        .turn_state
    {
        AgentTurnState::AgentThinking { agent_prompt_id } => agent_prompt_id.clone(),
        state => panic!("status-less prompt is not active: {state:?}"),
    };
    assert!(
        statusless_h.prompt_coordination.prompt_runtime.tool_specs[&statusless_prompt_id]
            .iter()
            .all(|spec| statusless_h.tool_model_visible_name(spec).as_str() != "status"),
        "the immutable dispatched prompt surface must not expose status"
    );
    statusless_h
        .handle_provider_response_finished(provider_text_response(
            &statusless_prompt_id,
            statusless_id,
            "status-less final",
        ))
        .expect("accept status-less final");
    assert!(matches!(
        statusless_h.agent_runtime.agent_registry.agents[&statusless_cid]
            .turn
            .turn_state,
        AgentTurnState::Idle
    ));
    assert_eq!(
        event_log_events(&statusless_h)
            .iter()
            .filter(|event| matches!(event, Event::ProviderResponseFinished(_)))
            .count(),
        1
    );
    statusless_h
        .shutdown()
        .expect("shutdown status-less harness");
}

/// Two Working finals from a tool-backed delegate project neither a delegated
/// result nor detachment; the bounded escape releases the third result and
/// detaches the child.
#[test]
fn delegated_working_final_projects_after_bounded_escape() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path().join("state")).expect("start");
    h.config.selected_model = Some("test/model".into());
    let frames = connect_test_tool(&mut h, "conn-working-final");
    let parent_cid = ensure_test_user_agent(&mut h);
    h.tool_routing
        .tool_runtime
        .tool_agents
        .insert("parent-agent-start".into(), parent_cid);
    h.handle_start_agent_request(
        &crate::test_connection_id("conn-working-final"),
        StartAgentRequest {
            trusted_internal_spans: Vec::new(),
            parent_agent: None,
            query_id: "q-working-final".to_owned(),
            instruction: "Perform delegated work.".to_owned(),
            role: None,
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: Some("parent-agent-start".into()),
            task_name: None,
        },
    )
    .expect("start delegated agent");
    let child_cid = ext_query_cid(&h, "q-working-final").expect("delegated child");
    let child_agent_id = durable_agent_id_for_conversation(&h, &child_cid);
    h.report_agent_work_status(
        &child_cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "delegated final".to_owned(),
        )
        .expect("valid Working report"),
    )
    .expect("accept Working");
    let originator = tau_proto::PromptOriginator::Extension {
        name: crate::test_extension_name("conn-working-final"),
        query_id: "q-working-final".to_owned(),
    };

    for index in 0..2 {
        let prompt_id = h
            .prompt_coordination
            .prompt_runtime
            .agents
            .iter()
            .find_map(|(prompt_id, prompt_cid)| {
                (prompt_cid == &child_cid).then(|| prompt_id.clone())
            })
            .expect("delegated prompt");
        let mut response = provider_text_response(
            &prompt_id,
            child_agent_id.clone(),
            &format!("candidate {index}"),
        );
        response.originator = originator.clone();
        h.handle_provider_response_finished(response)
            .expect("challenge delegated Working final");
        assert!(matches!(
            h.agent_runtime.agent_registry.agents[&child_cid]
                .turn
                .turn_state,
            AgentTurnState::AgentThinking { .. }
        ));
        assert!(
            h.agent_runtime.agent_registry.agents[&child_cid]
                .identity
                .parent_tool_call_id
                .is_some()
        );
        assert!(frames.lock().expect("frames").iter().all(|routed| {
            !matches!(
                peel_inner_event(&routed.frame),
                Some(Event::StartAgentResult(result)) if result.query_id == "q-working-final"
            )
        }));
    }

    let final_prompt_id = h
        .prompt_coordination
        .prompt_runtime
        .agents
        .iter()
        .find_map(|(prompt_id, prompt_cid)| (prompt_cid == &child_cid).then(|| prompt_id.clone()))
        .expect("post-Done delegated prompt");
    let mut response =
        provider_text_response(&final_prompt_id, child_agent_id.clone(), "delegated answer");
    response.originator = originator;
    h.handle_provider_response_finished(response)
        .expect("finish delegated work after Done");

    let results = frames
        .lock()
        .expect("frames")
        .iter()
        .filter_map(|routed| match peel_inner_event(&routed.frame) {
            Some(Event::StartAgentResult(result)) if result.query_id == "q-working-final" => {
                Some(result.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].text, "delegated answer");
    let child = &h.agent_runtime.agent_registry.agents[&child_cid];
    assert!(child.identity.parent_tool_call_id.is_none());
    assert!(child.identity.parent_agent_id.is_none());
    assert!(matches!(child.turn.turn_state, AgentTurnState::Idle));
    assert_eq!(
        child.turn.work_status.phase(),
        tau_proto::AgentWorkStatusPhase::Unknown
    );
    assert!(
        h.agent_runtime
            .agent_registry
            .session_loaded
            .contains(&child_agent_id)
    );
    h.shutdown().expect("shutdown");
}

/// The production response handler makes an unsuccessful terminal Unknown
/// exactly once without scheduling a Working continuation.
#[test]
fn unsuccessful_working_terminal_bypasses_reminders() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "failing".to_owned(),
        )
        .expect("valid work status report"),
    )
    .expect("working");
    let prompt_id = test_agent_prompt_id("status-error");
    seed_agent_thinking(&mut h, &cid, prompt_id.as_str());
    h.prompt_coordination
        .prompt_runtime
        .agents
        .insert(prompt_id.clone(), cid.clone());
    let response = ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,
        agent_prompt_id: prompt_id,
        agent_id: durable_agent_id_for_conversation(&h, &cid),
        output_items: Vec::new(),
        stop_reason: tau_proto::ProviderStopReason::Error,
        error: Some("provider failed".to_owned()),
        failure_kind: Some(tau_proto::ProviderFailureKind::Unknown),
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    };
    h.handle_provider_response_finished(response)
        .expect("finish unsuccessful terminal");
    assert_eq!(
        h.agent_runtime.agent_registry.agents[&cid]
            .turn
            .work_status
            .phase(),
        tau_proto::AgentWorkStatusPhase::Unknown
    );
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// Repeated synthetic dispatch terminalization invalidates Working once and
/// never installs a final-response reminder.
#[test]
fn synthetic_dispatch_terminal_emits_one_unknown_without_reminder() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let watched_id = durable_agent_id_for_conversation(&h, &cid);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    h.set_agent_watch(
        watcher_id.as_str(),
        watched_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "synthetic failure".to_owned(),
        )
        .expect("valid work status report"),
    )
    .expect("working");
    h.agent_runtime
        .agent_registry
        .agents
        .get_mut(&cid)
        .expect("agent")
        .dispatch
        .activation_dispatch = path_crate_agent::ActivationDispatchState::DispatchUncertain {
        owner: path_crate_agent::InferenceCheckpointOwner::Inference,
        agent_prompt_id: test_agent_prompt_id("synthetic-terminal"),
        through: tau_proto::AgentHead::Root,
        model: Some("test/model".into()),
        operation: Some(tau_proto::PromptOperation::Inference),
        activation_cut: None,
    };
    h.terminalize_owned_dispatch_error(&cid, "route failed".to_owned());
    h.terminalize_owned_dispatch_error(&cid, "duplicate route failure".to_owned());

    let unknowns = session_agent_message_received_events(&h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchWorkStatus
                && message.watch_work_status.as_ref().is_some_and(|status| {
                    !status.initial && status.phase == tau_proto::AgentWorkStatusPhase::Unknown
                })
        })
        .count();
    assert_eq!(unknowns, 1);
    assert!(
        h.agent_runtime.agent_registry.agents[&cid]
            .dispatch
            .pending_prompts
            .is_empty()
    );
    h.shutdown().expect("shutdown");
}

/// Settling substantive work while nonworking queues one reminder, while an
/// accepted Working report suppresses it.
#[test]
fn foreground_settlement_follows_current_working_status() {
    for working in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let sp = td.path().join("state");
        let mut h = echo_harness(&sp).expect("start");
        let cid = ensure_test_user_agent(&mut h);
        if working {
            h.report_agent_work_status(
                &cid,
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "already working".to_owned(),
                )
                .expect("valid Working report"),
            )
            .expect("accept Working");
        }
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("agent")
            .turn
            .work_status
            .record_substantive_tool_admission();
        h.queue_working_reminder_if_needed(&cid);
        assert_eq!(
            h.agent_runtime.agent_registry.agents[&cid]
                .dispatch
                .pending_prompts
                .iter()
                .filter(|prompt| prompt.text == STATUS_REMINDER)
                .count(),
            usize::from(!working)
        );
        h.shutdown().expect("shutdown");
    }
}

/// The intrinsic production handler derives nondurable storage and the current
/// status reducer value before publishing its exact seven-line tool result.
#[test]
fn self_info_production_dispatch_reports_memory_only_current_status() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness_memory_only(td.path().join("state")).expect("start memory-only");
    let cid = ensure_test_user_agent(&mut h);
    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "Inspect runtime identity".to_owned(),
        )
        .expect("status"),
    )
    .expect("report status");
    h.dispatch_prompt_for_agent(&cid, PendingPrompt::user("inspect self".to_owned()))
        .expect("dispatch prompt");
    let prompt = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_tool_response(
        &prompt,
        "production-self-info-call",
        "self_info",
        CborValue::Map(Vec::new()),
    ))
    .expect("run intrinsic self-info");

    let result = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::ToolResult(result) if result.call_id.as_str() == "production-self-info-call" => {
                result.result.as_text().map(ToOwned::to_owned)
            }
            _ => None,
        })
        .expect("intrinsic self-info result");
    assert_eq!(
        result,
        format!(
            "agent_id: {}\nsession_id: s1\nsession_dir: (none)\nmodel: {}\neffort: {}\nstatus: working\nstatus_task_name: Inspect runtime identity",
            prompt.agent_id,
            prompt.model,
            prompt.model_params.effort.as_str()
        )
    );
    assert_eq!(result.lines().count(), 7);
    h.shutdown().expect("shutdown");
}

/// Frozen status availability selects explicit phases, while a settled
/// no-status turn infers Done and an accepted unresolved Working final becomes
/// Unknown.
#[test]
fn outer_finish_policy_status_matrix_is_closed_and_policy_only() {
    use tau_proto::AgentWorkStatusPhase::{Blocked, Done, Unknown, Unreported, Waiting, Working};

    assert_eq!(
        Harness::finalizing_outer_turn_policy_status(false, Working),
        Done
    );
    for phase in [Done, Waiting, Blocked, Unknown, Unreported] {
        assert_eq!(
            Harness::finalizing_outer_turn_policy_status(true, phase),
            phase
        );
    }
    assert_eq!(
        Harness::finalizing_outer_turn_policy_status(true, Working),
        Unknown
    );
}

/// Final-status steering uses the concise generic wording approved for both
/// unresolved phases.
#[test]
fn final_status_reminders_use_approved_wording() {
    assert_eq!(
        final_status_reminder(&crate::agent::FinalStatusChallenge::Working {
            title: "STATUS-WATCH-4D8B".to_owned(),
        }),
        "Your `status` is set to `working` on \"STATUS-WATCH-4D8B\". Set it to `done`, `waiting`, or `blocked` to finish or call `wait` when waiting for external events."
    );
    assert_eq!(
        final_status_reminder(&crate::agent::FinalStatusChallenge::Unreported),
        "You have not reported `status`. Set it to `done`, `waiting`, or `blocked` to finish or call `wait` when waiting for external events."
    );
}

/// Work-status reports and invalidations publish complete live and replay stats
/// snapshots, preserving the last canonical title when Working becomes Unknown.
#[test]
fn agent_stats_snapshots_publish_work_status_transitions_and_replay() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let live = connect_test_tool(&mut h, "work-status-live");
    h.complete_subscription(
        &crate::test_connection_id("work-status-live"),
        Vec::new(),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
    )
    .expect("subscribe");
    drain_stats_updated(&live);
    let cid = ensure_test_user_agent(&mut h);
    let public_id = durable_agent_id_for_conversation(&h, &cid);
    drain_stats_updated(&live);

    h.report_agent_work_status(
        &cid,
        crate::WorkStatusReport::new(
            tau_proto::AgentWorkStatusPhase::Working,
            "publish lifecycle".to_owned(),
        )
        .expect("valid working report"),
    )
    .expect("publish working status");
    let working = drain_stats_updated(&live);
    assert!(working.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.work_status.phase() == tau_proto::AgentWorkStatusPhase::Working
            && snapshot.work_status.title() == Some("publish lifecycle")
    }));

    assert!(
        h.agent_runtime
            .agent_registry
            .agents
            .get_mut(&cid)
            .expect("loaded agent")
            .turn
            .work_status
            .invalidate_working(),
        "working state must invalidate"
    );
    h.notify_work_status_transition(&cid);
    let unknown = drain_stats_updated(&live);
    assert!(unknown.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.work_status.phase() == tau_proto::AgentWorkStatusPhase::Unknown
            && snapshot.work_status.title() == Some("publish lifecycle")
    }));

    let replay = connect_test_tool(&mut h, "work-status-replay");
    h.complete_subscription(
        &crate::test_connection_id("work-status-replay"),
        vec![EventSelector::Exact(
            tau_proto::EventName::AGENT_STATS_UPDATED,
        )],
        Vec::new(),
    )
    .expect("subscribe for replay");
    let replayed = drain_stats_updated(&replay);
    assert!(replayed.iter().any(|snapshot| {
        snapshot.agent_id == public_id
            && snapshot.work_status.phase() == tau_proto::AgentWorkStatusPhase::Unknown
            && snapshot.work_status.title() == Some("publish lifecycle")
    }));
    h.shutdown().expect("shutdown");
}

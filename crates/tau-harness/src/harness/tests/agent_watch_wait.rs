use std::collections::HashSet;

use super::*;

/// Return durable long-wait deliveries in publication order.
fn long_wait_deliveries(harness: &Harness) -> Vec<tau_proto::AgentMessageReceived> {
    event_log_events(harness)
        .into_iter()
        .filter_map(|event| match event {
            Event::AgentMessageReceived(message)
                if message.kind == tau_proto::AgentMessageKind::WatchLongWait =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect()
}

/// Build one activating-input wait call with the maximum effective timeout.
fn input_wait_call(call_id: &str) -> AgentToolCall {
    AgentToolCall {
        call_ref: None,
        id: call_id.into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("timeout_minutes".to_owned()),
            CborValue::Integer(60.into()),
        )]),
    }
}

/// Install one exact wait through the production handler at a supplied clock.
fn install_exact_wait_at(
    harness: &mut Harness,
    owner: &AgentId,
    target_call_id: &str,
    wait_call_id: &str,
    now: Instant,
) -> ToolCallId {
    let target_call_id = ToolCallId::from(target_call_id);
    harness
        .tool_agents
        .insert(target_call_id.clone(), owner.clone());
    harness.pending_tools.insert(
        target_call_id.clone(),
        super::super::PendingTool {
            name: ToolName::new("slow"),
            internal_name: ToolName::new("slow"),
            tool_type: tau_proto::ToolType::Function,
            allows_provider_image: false,
        },
    );
    harness.record_wait_tool_request(&target_call_id);
    let wait_call = AgentToolCall {
        call_ref: None,
        id: wait_call_id.into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(vec![(
            CborValue::Text("tool_call_id".to_owned()),
            CborValue::Text(target_call_id.to_string()),
        )]),
    };
    harness
        .handle_wait_tool_call_at(owner, &wait_call, ToolName::new("wait"), now)
        .expect("install exact wait");
    wait_call.id
}

/// Build one ordinary final tool result for a tracked wait source.
fn wait_source_result(call_id: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new("slow"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("complete".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }
}

/// The monotonic scheduler advances every approved threshold exactly once,
/// including without watchers, and watchers receive only future crossings.
#[test]
fn long_wait_thresholds_use_fake_monotonic_deadlines_without_late_replay() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let watched_cid = ensure_test_user_agent(&mut harness);
    let watcher_cid = harness.create_durable_user_agent(
        harness.current_session_id.clone(),
        &harness.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&harness, &watched_cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&harness, &watcher_cid).to_string();
    let start = Instant::now() + Duration::from_secs(60);
    assert_eq!(
        harness.agents[&watched_cid].work_status.phase(),
        tau_proto::AgentWorkStatusPhase::Unreported
    );
    assert!(
        harness
            .agents
            .get_mut(&watched_cid)
            .expect("watched agent")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "awaiting review".to_owned(),
                )
                .expect("valid report"),
                start,
                false,
            )
    );
    install_exact_wait_at(
        &mut harness,
        &watched_cid,
        "long-running-target",
        "wait-long-running-target",
        start,
    );

    assert_eq!(
        harness.next_work_wait_threshold_deadline(),
        Some(start + Duration::from_secs(15 * 60))
    );
    harness.process_runtime_deadlines_at(start + Duration::from_secs(15 * 60));
    assert!(long_wait_deliveries(&harness).is_empty());
    harness.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    assert!(long_wait_deliveries(&harness).is_empty());

    for threshold in [30_u64, 60] {
        assert_eq!(
            harness.next_work_wait_threshold_deadline(),
            Some(start + Duration::from_secs(threshold * 60))
        );
        harness.process_runtime_deadlines_at(start + Duration::from_secs(threshold * 60));
    }

    let late_cid = harness.create_durable_user_agent(
        harness.current_session_id.clone(),
        &harness.selected_role.clone(),
    );
    let late_id = durable_agent_id_for_conversation(&harness, &late_cid).to_string();
    harness.set_agent_watch(
        &late_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    assert!(
        long_wait_deliveries(&harness)
            .iter()
            .all(|message| message.recipient_id.as_str() != late_id)
    );

    for threshold in [120_u64, 240, 360, 480] {
        harness.process_runtime_deadlines_at(start + Duration::from_secs(threshold * 60));
    }

    let deliveries = long_wait_deliveries(&harness);
    let thresholds_for = |recipient: &str| {
        deliveries
            .iter()
            .filter(|message| message.recipient_id.as_str() == recipient)
            .filter_map(|message| {
                message
                    .watch_long_wait
                    .as_ref()
                    .map(|wait| wait.threshold_minutes)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(thresholds_for(&watcher_id), [30, 60, 120, 240, 360, 480]);
    assert_eq!(thresholds_for(&late_id), [120, 240, 360, 480]);
    assert!(harness.agents[&watched_cid].pending_prompts.is_empty());
    assert_eq!(
        harness.next_work_wait_threshold_deadline(),
        Some(start + Duration::from_secs(600 * 60))
    );
    assert!(deliveries.iter().all(|message| {
        message
            .watch_long_wait
            .as_ref()
            .is_some_and(|wait| wait.status_epoch == 1)
    }));
    harness.shutdown().expect("shutdown");
}

/// Two installed waits count their union: settling one leaves the clock active,
/// and only settling the final waiter pauses the epoch.
#[test]
fn overlapping_waits_accumulate_until_the_last_wait_settles() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let start = Instant::now();
    assert!(
        harness
            .agents
            .get_mut(&cid)
            .expect("agent")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "two waits".to_owned(),
                )
                .expect("valid report"),
                start,
                false,
            )
    );
    install_exact_wait_at(&mut harness, &cid, "overlap-a", "wait-overlap-a", start);
    install_exact_wait_at(&mut harness, &cid, "overlap-b", "wait-overlap-b", start);
    let first_deadline = start + Duration::from_secs(15 * 60);
    assert_eq!(
        harness.next_work_wait_threshold_deadline(),
        Some(first_deadline)
    );

    harness.record_wait_tool_result(wait_source_result("overlap-a"), None);
    assert_eq!(
        harness.next_work_wait_threshold_deadline(),
        Some(first_deadline)
    );
    harness.process_runtime_deadlines_at(first_deadline);
    assert!(harness.next_work_wait_threshold_deadline().is_some());

    harness.record_wait_tool_result(wait_source_result("overlap-b"), None);
    assert_eq!(harness.next_work_wait_threshold_deadline(), None);
    harness.shutdown().expect("shutdown");
}

/// A manual-compaction claim remains semantically installed through delayed
/// rollback and through the eventual committed-cancellation boundary.
#[test]
fn claimed_wait_keeps_accounting_until_rollback_or_commit() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let start = Instant::now();
    assert!(
        harness
            .agents
            .get_mut(&cid)
            .expect("agent")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "claimed wait".to_owned(),
                )
                .expect("valid report"),
                start,
                false,
            )
    );
    let wait_call =
        install_exact_wait_at(&mut harness, &cid, "claimed-target", "claimed-wait", start);
    assert!(harness.claim_wait_for_manual_compaction(&cid, &wait_call));
    let threshold = start + Duration::from_secs(15 * 60);
    assert_eq!(harness.next_work_wait_threshold_deadline(), Some(threshold));
    harness.process_runtime_deadlines_at(threshold);
    assert_eq!(
        harness.next_work_wait_threshold_deadline(),
        Some(start + Duration::from_secs(30 * 60))
    );

    harness.rollback_manual_compaction_wait_claim(&cid, &wait_call);
    assert_eq!(
        harness.next_work_wait_threshold_deadline(),
        Some(start + Duration::from_secs(30 * 60))
    );
    assert!(harness.claim_wait_for_manual_compaction(&cid, &wait_call));
    harness.record_wait_tool_cancelled(&HashSet::from([wait_call]), None);
    assert_eq!(harness.next_work_wait_threshold_deadline(), None);
    harness.shutdown().expect("shutdown");
}

/// A real installed input wait contributes a semantic threshold deadline,
/// while an immediately rejected bare wait contributes no duration.
#[test]
fn installed_waits_feed_the_combined_runtime_deadline_scheduler() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    harness
        .report_agent_work_status(
            &cid,
            crate::WorkStatusReport::new(
                tau_proto::AgentWorkStatusPhase::Working,
                "waiting for input".to_owned(),
            )
            .expect("valid report"),
        )
        .expect("report status");

    let rejected = AgentToolCall {
        call_ref: None,
        id: "wait-rejected".into(),
        name: ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        arguments: CborValue::Map(Vec::new()),
    };
    harness
        .handle_wait_tool_call(&cid, &rejected, ToolName::new("wait"))
        .expect("reject bare wait");
    assert_eq!(harness.next_work_wait_threshold_deadline(), None);

    let installed = input_wait_call("wait-installed");
    harness
        .handle_wait_tool_call(&cid, &installed, ToolName::new("wait"))
        .expect("install input wait");
    let work_deadline = harness
        .next_work_wait_threshold_deadline()
        .expect("semantic wait deadline");
    let input_deadline = harness.next_input_wait_deadline().expect("input deadline");
    assert!(work_deadline < input_deadline);
    assert_eq!(harness.next_runtime_deadline(), Some(work_deadline));
    harness.process_runtime_deadlines_at(work_deadline);
    assert!(harness.input_wait_pending_for(&cid));
    assert_eq!(harness.next_input_wait_deadline(), Some(input_deadline));
    assert!(harness.agents[&cid].pending_prompts.is_empty());
    harness.shutdown().expect("shutdown");
}

/// Unloading an agent removes its runtime-only accounting and deadline without
/// emitting a synthetic threshold during cleanup.
#[test]
fn agent_unload_cancels_semantic_wait_deadline() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let now = Instant::now();
    let wait_call = {
        assert!(
            harness
                .agents
                .get_mut(&cid)
                .expect("agent")
                .work_status
                .report_at(
                    crate::WorkStatusReport::new(
                        tau_proto::AgentWorkStatusPhase::Working,
                        "waiting".to_owned(),
                    )
                    .expect("valid report"),
                    now,
                    false,
                )
        );
        install_exact_wait_at(&mut harness, &cid, "unload-target", "unload-wait", now)
    };
    assert!(harness.next_work_wait_threshold_deadline().is_some());
    assert!(harness.claim_wait_for_manual_compaction(&cid, &wait_call));
    let retired =
        harness.discard_wait_owner_before_teardown_at(&cid, now + Duration::from_secs(20 * 60));
    assert!(retired.contains(&wait_call));
    assert_eq!(harness.next_work_wait_threshold_deadline(), None);
    assert!(long_wait_deliveries(&harness).is_empty());
    harness.remove_agent(&cid);
    assert_eq!(harness.next_work_wait_threshold_deadline(), None);
    assert!(long_wait_deliveries(&harness).is_empty());
    harness.shutdown().expect("shutdown");
}

/// Ordinary, unclaimed wait teardown retires the derived deadline at the same
/// cut and does not turn elapsed cleanup time into a notification.
#[test]
fn agent_unload_retires_ordinary_wait_without_notification() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let now = Instant::now();
    assert!(
        harness
            .agents
            .get_mut(&cid)
            .expect("agent")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "ordinary teardown".to_owned(),
                )
                .expect("valid report"),
                now,
                false,
            )
    );
    let wait_call = install_exact_wait_at(
        &mut harness,
        &cid,
        "ordinary-unload-target",
        "ordinary-unload-wait",
        now,
    );

    let retired =
        harness.discard_wait_owner_before_teardown_at(&cid, now + Duration::from_secs(20 * 60));

    assert!(retired.contains(&wait_call));
    assert_eq!(harness.next_work_wait_threshold_deadline(), None);
    assert!(long_wait_deliveries(&harness).is_empty());
    harness.shutdown().expect("shutdown");
}

/// One scheduler cycle bounds overdue catch-up and retains an immediate
/// deadline so later cycles eventually emit every remaining crossing.
#[test]
fn overdue_wait_threshold_catchup_is_bounded_per_scheduler_cycle() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let watcher_cid = harness.create_durable_user_agent(
        harness.current_session_id.clone(),
        &harness.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&harness, &cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&harness, &watcher_cid).to_string();
    let start = Instant::now();
    assert!(
        harness
            .agents
            .get_mut(&cid)
            .expect("agent")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "waiting".to_owned(),
                )
                .expect("valid report"),
                start,
                false,
            )
    );
    install_exact_wait_at(&mut harness, &cid, "overdue-target", "overdue-wait", start);
    harness.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let far_future = start + Duration::from_secs(20_000 * 60);
    harness.runtime_event_receive_cut = Some(far_future);
    harness
        .tx
        .send(HarnessEvent::Disconnected {
            connection_id: crate::test_connection_id("overdue-queued-event"),
        })
        .expect("queue event");
    assert!(matches!(
        harness.next_runtime_event(),
        super::super::RuntimeEventWait::Event(_)
    ));
    assert_eq!(long_wait_deliveries(&harness).len(), 64);
    assert_eq!(harness.pending_long_wait_notifications.len(), 1);
    harness.process_runtime_deadlines_at(far_future);
    assert_eq!(long_wait_deliveries(&harness).len(), 128);
    while harness.has_pending_long_wait_notifications() {
        harness.process_runtime_deadlines_at(far_future);
    }
    let thresholds = long_wait_deliveries(&harness)
        .into_iter()
        .filter_map(|message| message.watch_long_wait.map(|wait| wait.threshold_minutes))
        .collect::<Vec<_>>();
    assert_eq!(thresholds.len(), 169);
    assert!(thresholds.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(thresholds.last(), Some(&19_920));
    harness.shutdown().expect("shutdown");
}

/// Pruning the sole remaining recipient of an in-progress threshold completes
/// that threshold without rewinding onto an already-materialized subscription.
#[test]
fn pruning_partial_long_wait_batch_preserves_exactly_once_cursor() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let watched_cid = ensure_test_user_agent(&mut harness);
    let watched_id = durable_agent_id_for_conversation(&harness, &watched_cid).to_string();
    let mut watcher_ids = Vec::new();
    for _ in 0..5 {
        let watcher_cid = harness.create_durable_user_agent(
            harness.current_session_id.clone(),
            &harness.selected_role.clone(),
        );
        let watcher_id = durable_agent_id_for_conversation(&harness, &watcher_cid).to_string();
        harness.set_agent_watch(
            &watcher_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        );
        watcher_ids.push(watcher_id);
    }
    let start = Instant::now();
    assert!(
        harness
            .agents
            .get_mut(&watched_cid)
            .expect("watched")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "partial batch".to_owned(),
                )
                .expect("valid report"),
                start,
                false,
            )
    );
    install_exact_wait_at(
        &mut harness,
        &watched_cid,
        "partial-target",
        "partial-wait",
        start,
    );
    let far_future = start + Duration::from_secs(2_000 * 60);
    harness.process_runtime_deadlines_at(far_future);
    assert_eq!(long_wait_deliveries(&harness).len(), 64);

    let (recipient_index, recipients) = harness
        .pending_long_wait_front_for_test()
        .expect("compact backlog");
    assert_eq!(recipient_index, 4);
    let removed_watcher = recipients[4].0.clone();
    let removed_subscription = recipients[4].1.clone();
    harness.set_agent_watch(
        &removed_watcher,
        &watched_id,
        false,
        tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
    );
    while harness.has_pending_long_wait_notifications() {
        harness.process_runtime_deadlines_at(far_future);
    }

    let deliveries = long_wait_deliveries(&harness);
    let pairs = deliveries
        .iter()
        .map(|message| {
            let wait = message.watch_long_wait.as_ref().expect("long-wait payload");
            (wait.subscription_id.clone(), wait.threshold_minutes)
        })
        .collect::<HashSet<_>>();
    assert_eq!(pairs.len(), deliveries.len());
    let removed_thresholds = deliveries
        .iter()
        .filter_map(|message| {
            let wait = message.watch_long_wait.as_ref()?;
            (wait.subscription_id == removed_subscription).then_some(wait.threshold_minutes)
        })
        .collect::<Vec<_>>();
    assert_eq!(removed_thresholds.len(), 12);
    assert!(removed_thresholds.windows(2).all(|pair| pair[0] < pair[1]));
    for watcher_id in watcher_ids
        .iter()
        .filter(|watcher_id| *watcher_id != &removed_watcher)
    {
        let subscription =
            harness.agent_watch_subscriptions[&(watcher_id.clone(), watched_id.clone())].clone();
        let thresholds = deliveries
            .iter()
            .filter_map(|message| {
                let wait = message.watch_long_wait.as_ref()?;
                (wait.subscription_id == subscription).then_some(wait.threshold_minutes)
            })
            .collect::<Vec<_>>();
        assert_eq!(thresholds.len(), 19);
        assert!(thresholds.windows(2).all(|pair| pair[0] < pair[1]));
    }
    harness.shutdown().expect("shutdown");
}

/// A queued event observed exactly when a threshold becomes due cannot overtake
/// threshold cursor advancement or turn a later watch into a historical alert.
#[test]
fn received_event_at_threshold_equality_drains_deadline_first() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let watcher_cid = harness.create_durable_user_agent(
        harness.current_session_id.clone(),
        &harness.selected_role.clone(),
    );
    let watched_id = durable_agent_id_for_conversation(&harness, &cid).to_string();
    let watcher_id = durable_agent_id_for_conversation(&harness, &watcher_cid).to_string();
    let start = Instant::now() + Duration::from_secs(60);
    assert!(
        harness
            .agents
            .get_mut(&cid)
            .expect("agent")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "event equality".to_owned(),
                )
                .expect("valid report"),
                start,
                false,
            )
    );
    install_exact_wait_at(
        &mut harness,
        &cid,
        "equality-target",
        "equality-wait",
        start,
    );
    let threshold = start + Duration::from_secs(15 * 60);
    harness.runtime_event_receive_cut = Some(threshold);
    harness
        .tx
        .send(HarnessEvent::Disconnected {
            connection_id: crate::test_connection_id("queued-at-threshold"),
        })
        .expect("queue event");

    let first = harness.next_runtime_event();
    assert_eq!(
        harness.next_work_wait_threshold_deadline(),
        Some(start + Duration::from_secs(30 * 60))
    );
    let _received = match first {
        super::super::RuntimeEventWait::Event(event) => event,
        super::super::RuntimeEventWait::DeadlineElapsed => match harness.next_runtime_event() {
            super::super::RuntimeEventWait::Event(event) => event,
            _ => panic!("held event must follow bounded deadline catch-up"),
        },
        super::super::RuntimeEventWait::Disconnected => panic!("event channel remains connected"),
    };
    harness.set_agent_watch(
        &watcher_id,
        &watched_id,
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    assert!(long_wait_deliveries(&harness).is_empty());
    harness.shutdown().expect("shutdown");
}

/// A live committed long-wait projection survives cold resume as transcript
/// context without reconstructing a clock, fanout, or activating wake.
#[test]
fn cold_resume_replays_long_wait_context_without_runtime_state() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let (watcher_id, watched_id);
    {
        let mut harness = echo_harness(&state).expect("start");
        let watched_cid = ensure_test_user_agent(&mut harness);
        let watcher_cid = harness.create_durable_user_agent(
            harness.current_session_id.clone(),
            &harness.selected_role.clone(),
        );
        watched_id = durable_agent_id_for_conversation(&harness, &watched_cid).to_string();
        watcher_id = durable_agent_id_for_conversation(&harness, &watcher_cid).to_string();
        let start = Instant::now();
        assert!(
            harness
                .agents
                .get_mut(&watched_cid)
                .expect("watched")
                .work_status
                .report_at(
                    crate::WorkStatusReport::new(
                        tau_proto::AgentWorkStatusPhase::Working,
                        "cold replay".to_owned(),
                    )
                    .expect("valid report"),
                    start,
                    false,
                )
        );
        install_exact_wait_at(
            &mut harness,
            &watched_cid,
            "replay-target",
            "replay-wait",
            start,
        );
        harness.set_agent_watch(
            &watcher_id,
            &watched_id,
            true,
            tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        );
        harness.process_runtime_deadlines_at(start + Duration::from_secs(15 * 60));
        assert_eq!(long_wait_deliveries(&harness).len(), 1);
        harness.shutdown().expect("shutdown");
    }
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        echo_harness_with_start_reason("s1", &state, tau_proto::SessionStartReason::Resume)
            .expect("resume");
    assert_eq!(resumed.next_work_wait_threshold_deadline(), None);
    assert!(resumed.agent_watches.is_empty());
    let watcher_cid = resumed
        .agent_routes
        .get(&watcher_id)
        .cloned()
        .expect("restored watcher");
    assert!(resumed.agents[&watcher_cid].pending_prompts.is_empty());
    assert!(
        agent_tree_for_conversation(&resumed, &watcher_cid)
            .nodes()
            .iter()
            .any(|node| matches!(
                &node.entry,
                AgentEntry::AgentMessage {
                    sender_id,
                    kind: tau_proto::AgentMessageKind::WatchLongWait,
                    watch_long_wait: Some(wait),
                    ..
                } if sender_id.as_str() == watched_id
                    && wait.threshold_minutes == 15
            ))
    );
    resumed.shutdown().expect("shutdown");
}

/// Session rollover discards installed waits and all derived deadline state
/// without synthesizing a cleanup-time long-wait delivery.
#[test]
fn session_rollover_discards_semantic_wait_deadlines() {
    let td = TempDir::new().expect("tempdir");
    let mut harness = echo_harness(td.path().join("state")).expect("start");
    let cid = ensure_test_user_agent(&mut harness);
    let start = Instant::now();
    assert!(
        harness
            .agents
            .get_mut(&cid)
            .expect("agent")
            .work_status
            .report_at(
                crate::WorkStatusReport::new(
                    tau_proto::AgentWorkStatusPhase::Working,
                    "rollover".to_owned(),
                )
                .expect("valid report"),
                start,
                false,
            )
    );
    install_exact_wait_at(
        &mut harness,
        &cid,
        "rollover-target",
        "rollover-wait",
        start,
    );
    assert!(harness.next_work_wait_threshold_deadline().is_some());

    harness
        .switch_session(
            "s2".parse().expect("valid session id"),
            tau_proto::SessionStartReason::New,
        )
        .expect("switch session");
    assert_eq!(harness.next_work_wait_threshold_deadline(), None);
    assert!(long_wait_deliveries(&harness).is_empty());
    harness.shutdown().expect("shutdown");
}

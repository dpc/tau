//! Focused provider-watch coverage for manual and reactive compaction.

use super::*;

/// Return provider-watch notifications sent from one watched agent to one
/// watcher, preserving durable publication order.
fn provider_watch_notifications(
    h: &Harness,
    watched_id: &tau_proto::AgentId,
    watcher_id: &tau_proto::AgentId,
) -> Vec<tau_proto::AgentWatchProviderStatusNotification> {
    session_agent_message_received_events(h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                && message.sender_id == *watched_id
                && message.recipient_id == *watcher_id
        })
        .filter_map(|message| message.watch_provider_status)
        .collect()
}

/// Return durable provider-watch notifications from one watched agent to one
/// watcher after live execution or cold replay.
fn durable_provider_watch_notifications(
    h: &Harness,
    watched_id: &tau_proto::AgentId,
    watcher_id: &tau_proto::AgentId,
) -> Vec<tau_proto::AgentWatchProviderStatusNotification> {
    durable_agent_message_received_events(h)
        .into_iter()
        .filter(|message| {
            message.kind == tau_proto::AgentMessageKind::WatchProviderStatus
                && message.sender_id == *watched_id
                && message.recipient_id == *watcher_id
        })
        .filter_map(|message| message.watch_provider_status)
        .collect()
}

/// Manual agent compaction must not masquerade as reactive context recovery for
/// an existing watcher, a warm late watcher, or durable cold replay.
#[test]
fn manual_agent_compaction_has_no_reactive_provider_watch_projection() {
    let (td, mut h, caller_cid, _target_cid, call, target_id) =
        setup_manual_cross_compaction_test();
    let state = td.path().join("state");
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let late_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    let late_id = durable_agent_id_for_conversation(&h, &late_cid);
    h.set_agent_watch(
        watcher_id.as_str(),
        target_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.request_agent_tool_compaction(
        &caller_cid,
        &call,
        ToolName::new("agent_compact"),
        Some(&target_id),
    );
    assert!(
        !h.agent_runtime
            .agent_watch
            .provider_status
            .contains_key(target_id.as_str())
    );
    assert!(provider_watch_notifications(&h, &target_id, &watcher_id).is_empty());

    h.set_agent_watch(
        late_id.as_str(),
        target_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    assert!(provider_watch_notifications(&h, &target_id, &late_id).is_empty());

    let compact = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(provider_text_response(
        &compact.agent_prompt_id,
        compact.agent_id,
        "manual compact summary",
    ))
    .expect("finish manual compaction");
    h.shutdown().expect("shutdown");
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    assert!(durable_provider_watch_notifications(&resumed, &target_id, &watcher_id).is_empty());
    assert!(durable_provider_watch_notifications(&resumed, &target_id, &late_id).is_empty());
    resumed.shutdown().expect("resumed shutdown");
}

/// Actual context-window recovery must retain its existing live notification,
/// warm late snapshot, and exact durable observations after cold replay.
#[test]
fn reactive_context_recovery_retains_provider_watch_projection() {
    let td = TempDir::new().expect("tempdir");
    let state = td.path().join("state");
    let mut h = quiet_provider_harness(&state).expect("start");
    enable_remote_compaction_for_test_model(&mut h);
    h.provider_runtime
        .model_info
        .get_mut(&"test/model".into())
        .expect("test model")
        .supports_standalone_compaction = true;
    let target_cid = ensure_test_user_agent(&mut h);
    seed_reactive_compaction_prefix(&mut h, &target_cid);
    let target_id = durable_agent_id_for_conversation(&h, &target_cid);
    let watcher_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let late_cid = h.create_durable_user_agent(
        h.session_runtime.current_session_id.clone(),
        &h.config.selected_role.clone(),
    );
    let watcher_id = durable_agent_id_for_conversation(&h, &watcher_cid);
    let late_id = durable_agent_id_for_conversation(&h, &late_cid);
    h.set_agent_watch(
        watcher_id.as_str(),
        target_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );

    h.dispatch_prompt_for_agent(&target_cid, PendingPrompt::user("overflow".to_owned()))
        .expect("dispatch inference");
    let inference = read_nth_prompt_created(&h, 0);
    h.handle_provider_response_finished(context_overflow_response(&inference))
        .expect("start reactive recovery");
    let compact = read_nth_prompt_created(&h, 1);
    let live = provider_watch_notifications(&h, &target_id, &watcher_id);
    assert!(matches!(
        live.as_slice(),
        [tau_proto::AgentWatchProviderStatusNotification {
            state: tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 1 },
            initial: false,
            ..
        }]
    ));

    h.set_agent_watch(
        late_id.as_str(),
        target_id.as_str(),
        true,
        tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    );
    let late = provider_watch_notifications(&h, &target_id, &late_id);
    assert!(matches!(
        late.as_slice(),
        [tau_proto::AgentWatchProviderStatusNotification {
            state: tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 1 },
            initial: true,
            ..
        }]
    ));

    h.handle_provider_response_finished(
        strict_fake_compact_response(&compact).expect("valid compact response"),
    )
    .expect("finish reactive compaction");
    let continuation = read_nth_prompt_created(&h, 2);
    h.handle_provider_response_finished(provider_text_response(
        &continuation.agent_prompt_id,
        continuation.agent_id,
        "continued after recovery",
    ))
    .expect("finish recovery continuation");
    h.shutdown().expect("shutdown");
    wait_for_session_unlock(&state, "s1");

    let mut resumed =
        quiet_provider_harness_with_start_reason(&state, tau_proto::SessionStartReason::Resume)
            .expect("cold resume");
    assert_eq!(
        durable_provider_watch_notifications(&resumed, &target_id, &watcher_id),
        live
    );
    assert_eq!(
        durable_provider_watch_notifications(&resumed, &target_id, &late_id),
        late
    );
    resumed.shutdown().expect("resumed shutdown");
}

//! Tests for event projection behavior.

use super::*;

/// Dynamic action IDs use the bounded ASCII short-ID producer shape accepted by
/// the protocol type.
#[test]
fn action_short_id_producer_stays_within_invocation_id_grammar() {
    for _ in 0..100 {
        let raw = super::super::mint_short_id("action");
        let id = tau_proto::ActionInvocationId::parse(raw.clone())
            .expect("generated action invocation id");
        assert_eq!(id.as_str(), raw);
        assert_eq!(id.as_str().len(), 13);
        assert!(id.as_str().starts_with("action-"));
        assert!(
            id.as_str()[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
        );
    }
}

/// Increasing `:set redraw-history-size` should restore more scrollback
/// immediately by forcing a full redraw, while decreasing it should only affect
/// the next otherwise-needed full redraw.
#[test]
fn redraw_history_size_only_redraws_immediately_when_increased() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    sync(&handle);
    let initial_full_renders = handle.full_render_count();

    renderer.apply_setting("redraw-history-size", "100");
    sync(&handle);
    assert_eq!(handle.redraw_history_size(), 100);
    assert_eq!(handle.full_render_count(), initial_full_renders);

    renderer.apply_setting("redraw-history-size", "101");
    sync(&handle);
    assert_eq!(handle.redraw_history_size(), 101);
    assert_eq!(handle.full_render_count(), initial_full_renders + 1);
}

/// A theme refresh between an optimistic session switch and its authoritative
/// echo must preserve routing, drafts, and the new prompt-context session.
#[test]
fn theme_refresh_preserves_optimistic_session_context() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.set_right_prompt_paths("/tmp/project".into(), None);
    let draft_handle = Arc::new((
        Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    ));
    let active_session = Arc::new(Mutex::new(
        tau_proto::SessionId::parse("old-session").expect("session id"),
    ));
    renderer.set_draft_retargeter(draft_handle.clone(), active_session.clone());

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("old-session"),
        reason: SessionStartReason::Initial,
    }));
    {
        let mut draft = draft_handle.0.lock().expect("draft");
        draft.epoch = 7;
        draft.pending = Some((
            7,
            tau_proto::UiPromptDraft {
                session_id: test_session_id("new-session"),
                target_agent_id: None,
                text: Some("draft".to_owned()),
            },
        ));
    }
    *active_session.lock().expect("active session") =
        tau_proto::SessionId::parse("new-session").expect("session id");
    let themed =
        tau_themes::Theme::parse(r##"{ styles: { "prompt.cwd": { fg: "red", bold: true } } }"##)
            .expect("theme parses");
    renderer.apply_theme(themed);
    sync(&handle);

    assert_eq!(
        active_session.lock().expect("active session").as_str(),
        "new-session"
    );
    let draft = draft_handle.0.lock().expect("draft");
    assert_eq!(draft.epoch, 7);
    assert!(draft.pending.is_some());
    assert!(vt.screen_contains(80, "/tmp/project &new-session"));
    assert!(!vt.screen_contains(80, "&old-session"));
}

/// A typed WatchProviderStatus strips only the canonical production envelope in
/// live, reconstructed, and replayed transcripts; delimiter-like untyped text
/// remains an ordinary message.
#[test]
fn authenticated_internal_notices_are_consistent_live_and_replayed() {
    let agent = agent_id("internal-agent");
    let provider_snapshot = Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("provider-snapshot")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("watched-agent"),
        sender_session_id: None,
        recipient_id: agent.clone(),
        kind: tau_proto::AgentMessageKind::WatchProviderStatus,
        watch_provider_status: Some(tau_proto::AgentWatchProviderStatusNotification {
            session_id: test_session_id("s1"),
            subscription_id: "subscription-1".to_owned(),
            turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
            agent_prompt_id: test_agent_prompt_id("prompt-1"),
            state: tau_proto::AgentWatchProviderState::Retrying {
                category: tau_proto::AgentWatchProviderCategory::Unknown,
                attempt: 1,
                next_retry_delay_secs: 11,
            },
            initial: true,
        }),
        watch_work_status: None,
        watch_long_wait: None,
        watch_lifecycle: None,
        message: format!(
            "{}watch{}",
            tau_proto::TAU_INTERNAL_OPEN,
            tau_proto::TAU_INTERNAL_CLOSE,
        ),
    });
    let mut wrong_kind_provider = provider_snapshot.clone();
    let Event::AgentMessageReceived(message) = &mut wrong_kind_provider else {
        unreachable!("cloned provider event retains its variant");
    };
    message.kind = tau_proto::AgentMessageKind::Message;
    message.message = format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    );
    let mut missing_typed_provider = provider_snapshot.clone();
    let Event::AgentMessageReceived(message) = &mut missing_typed_provider else {
        unreachable!("cloned provider event retains its variant");
    };
    message.watch_provider_status = None;
    message.message = format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    );
    let submitted = Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent.clone(),
        text: "Your `status` is set to `working` on \"Fix Slack mandatory terminal delivery\". Set it to `done`, `waiting`, or `blocked` to finish or call `wait` when waiting for external events.".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    });
    let steered = Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent.clone(),
        text: "replayed internal body".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    });

    let (_live_term, live_handle, live_vt) = setup(100, 24);
    let mut live = marker_test_renderer(live_handle.clone());
    live.switch_agent(agent.as_str().to_owned());
    live.handle(&provider_snapshot);
    live.handle(&wrong_kind_provider);
    live.handle(&missing_typed_provider);
    live.handle(&submitted);
    live.apply_setting("show-internal-prompts", "on");
    sync(&live_handle);
    let live_text = visible_lines(&live_vt, 100).join("\n");
    let provider_frame = format!(
        "{}watch{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    );
    assert!(live_text.contains("□ watch"));
    assert!(live_text.contains("□ Your `status` is set to `working` on \"Fix Slack mandatory"));
    assert!(!live_text.contains("[tau-internal"));
    assert!(!live_text.contains(&provider_frame));
    assert!(live_text.contains("■ Message from @watched-agent:"));
    assert!(live_text.contains(&format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(live_text.contains(&format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(!live_text.contains("[tau-internal]: wrong-kind provider body"));
    assert!(!live_text.contains("[tau-internal]: missing-typed provider body"));
    live.switch_agent("other-agent".to_owned());
    live.switch_agent(agent.as_str().to_owned());
    sync(&live_handle);
    let reconstructed_live_text = visible_lines(&live_vt, 100).join("\n");
    assert_eq!(reconstructed_live_text.matches("□ watch").count(), 1);
    assert!(!reconstructed_live_text.contains(&provider_frame));
    assert!(reconstructed_live_text.contains(&format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(reconstructed_live_text.contains(&format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));

    let (_cold_term, cold_handle, cold_vt) = setup(100, 24);
    let mut cold = marker_test_renderer(cold_handle.clone());
    cold.switch_agent(agent.as_str().to_owned());
    cold.handle_recorded_at(&provider_snapshot, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&wrong_kind_provider, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&missing_typed_provider, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&submitted, tau_proto::UnixMicros::new(1));
    cold.handle_recorded_at(&steered, tau_proto::UnixMicros::new(2));
    cold.apply_setting("show-internal-prompts", "on");
    sync(&cold_handle);
    let cold_text = visible_lines(&cold_vt, 100).join("\n");
    assert!(cold_text.contains("□ watch"));
    assert!(cold_text.contains("□ Your `status` is set to `working` on \"Fix Slack mandatory"));
    assert!(cold_text.contains("□ replayed internal body"));
    assert!(!cold_text.contains("[tau-internal"));
    assert!(!cold_text.contains(&provider_frame));
    assert!(cold_text.contains("■ Message from @watched-agent:"));
    assert!(cold_text.contains(&format!(
        "{}wrong-kind provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(cold_text.contains(&format!(
        "{}missing-typed provider body{}",
        tau_proto::TAU_INTERNAL_OPEN,
        tau_proto::TAU_INTERNAL_CLOSE,
    )));
    assert!(!cold_text.contains("[tau-internal]: wrong-kind provider body"));
    assert!(!cold_text.contains("[tau-internal]: missing-typed provider body"));
}

/// Semantic row markers keep messages and notices visually distinct while
/// preserving the structured-status marker between them.
#[test]
fn semantic_row_markers_match_their_categories() {
    assert_eq!(transcript_markers::MESSAGE, "■ ");
    assert_eq!(transcript_markers::STATUS_UPDATE, "▤ ");
    assert_eq!(transcript_markers::NOTICE, "□ ");
}

/// Streaming response updates append text deltas rather than replacing a full
/// accumulated snapshot, so two chunks should render as one growing response.
#[test]
fn response_delta_updates_append_live_text() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hel",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "lo",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "HelHel"));
}

/// Queue ownership follows broadcast FIFO/class rather than mutable text, so
/// skill expansion or interception rewrites cannot strand an initial marker.
#[test]
fn rewritten_submitted_initial_failure_preserves_newer_queue() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: ":skill expand-me".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("main"),
        text: "expanded skill body".into(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HumanUi,
        display_name: None,
        ctx_id: Some("ctx-rewritten".into()),
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "ordinary B".into(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::AgentPromptFailed(AgentPromptFailed {
        request_id: "create-rewritten".into(),
        agent_id: agent_id("main"),
        ctx_id: "ctx-rewritten".into(),
        stage: tau_proto::AgentPromptFailureStage::Submission,
        message: "rewritten initial failed".into(),
    }));
    sync(&handle);

    assert!(!vt.screen_contains(100, ":skill expand-me (queued)"));
    assert!(vt.screen_contains(100, "ordinary B (queued)"));
}

/// A UI that missed the prompt-created event can still route later deltas by
/// agent id and marks the live text with an ellipsis until the final response.
#[test]
fn late_response_delta_update_uses_ellipsis_prefix() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-late"),
            "world",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "…world"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-late",
        vec![assistant_message_item("hello world")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "hello world"));
    assert!(!vt.screen_contains(80, "…world"));
}

/// Normal streaming after the prompt lifecycle was observed must not reuse the
/// late-subscription ellipsis prefix, which otherwise appears before the first
/// streamed assistant text.
#[test]
fn observed_response_delta_update_does_not_use_ellipsis_prefix() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-observed",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-observed"),
            "hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "hello"));
    assert!(!vt.screen_contains(80, "…hello"));
}

#[test]
fn delayed_clear_after_new_session_keeps_initial_history_adoptable() {
    // The input thread also queues a local ClearSelectedAgent when starting a
    // new session. If the remote SessionStarted(New) wins the race, that delayed
    // clear must not convert the fresh initial screen into an explicit protected
    // no-agent snapshot.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("previous-agent".to_owned());
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    renderer.clear_selected_agent();
    renderer.handle(&Event::ExtensionStarting(tau_proto::ExtensionStarting {
        instance_id: 89.into(),
        extension_name: tau_proto::ExtensionName::parse("std-race")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(456),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-race starting"));

    let full_render_count = handle.full_render_count();
    renderer.switch_agent("fresh-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "extension std-race starting"));
    assert_eq!(handle.full_render_count(), full_render_count);
}

/// Replayed terminals after an existing-session switch must replace, rather
/// than add to, the flat totals from the former session.
#[test]
fn session_stats_reset_before_resumed_session_replay() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-a"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("agent-a-sp-0", "agent-a", 100, 90, 10, "session A response"),
    ));
    assert_eq!(
        renderer.session_token_stats_text(),
        "session token totals: ↑90/100 ↓10"
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-b"),
        reason: SessionStartReason::Resume,
    }));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("agent-b-sp-0", "agent-b", 7, 3, 2, "session B replay"),
    ));

    assert_eq!(
        renderer.session_token_stats_text(),
        "session token totals: ↑3/7 ↓2"
    );
}

/// Message facts for unavailable or invalid targets stay in the no-agent
/// snapshot, while facts for loaded targets belong to that target transcript.
#[test]
fn message_facts_route_to_owned_ui_snapshots_end_to_end() {
    let (_term, handle, vt) = setup(100, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let message_fact = |target: &str, message_id: &str, text: &str| {
        Event::MessageDelivered(tau_proto::MessageDelivered::new(
            tau_proto::MessagePublisherId::parse("bridge-main")
                .expect("canonical publisher id must satisfy the identifier grammar"),
            tau_proto::MessageAgentTarget::new(target),
            tau_proto::MessageFactId::new(message_id),
            tau_proto::MessageParty {
                stable_id: "sender-1".to_owned(),
                display_name: None,
                sender_auth: None,
            },
            None,
            text,
        ))
    };

    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&message_fact(
        "unavailable-agent",
        "unavailable-message",
        "unavailable body",
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "unavailable body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "unavailable body"));
    assert!(vt.screen_contains(100, "for Tau target unavailable-agent"));
    renderer.switch_agent("fresh-after-global".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(100, "unavailable body"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "unavailable body"));

    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&message_fact(
        "../invalid",
        "invalid-message",
        "private invalid body",
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "Unprojectable message fact"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert!(vt.screen_contains(100, "Unprojectable message fact"));
    assert!(!vt.screen_contains(100, "private invalid body"));

    renderer
        .agent_navigation()
        .lock()
        .expect("agent navigation lock")
        .mark_live("loaded-agent");
    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&message_fact(
        "loaded-agent",
        "loaded-message",
        "loaded body",
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "loaded body"));
    renderer.switch_agent("loaded-agent".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(100, "loaded body"));
    assert!(!vt.screen_contains(100, "for Tau target loaded-agent"));
}

/// Dynamic action results must render in the transcript that was viewed when
/// the action was invoked. The result event itself has no agent id, so routing
/// it by the currently selected agent would leak output into whichever
/// transcript the user switched to while the extension was working.
#[test]
fn action_result_routes_to_invocation_snapshot() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("agent-a".to_owned());
    renderer.record_action_invocation(
        tau_proto::ActionInvocationId::parse("action-1")
            .expect("test identifier must satisfy its grammar"),
        Some("agent-a".to_owned()),
    );
    renderer.switch_agent("agent-b".to_owned());
    renderer.handle(&Event::ActionResult(tau_proto::ActionResult {
        invocation_id: tau_proto::ActionInvocationId::parse("action-1")
            .expect("test identifier must satisfy its grammar"),
        action_id: "demo.action".to_owned(),
        output: tau_proto::ActionOutput::Text {
            text: "agent a action output".to_owned(),
        },
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "agent a action output"));

    renderer.switch_agent("agent-a".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "agent a action output"));
}

/// Ensures prompt provenance cannot overwrite a harness-authored mode snapshot.
#[test]
fn extension_replay_reconstructs_active_auto_without_overwriting_override() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let prompt = AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("worker-1"),
        text: "side task".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q-worker".to_owned(),
        },
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    };
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("worker-1"),
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.handle(&Event::AgentPromptSubmitted(prompt.clone()));
    assert_eq!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .mode("worker-1"),
        AgentNavigationState::ActiveAuto,
    );
    apply_test_navigation_mode(&mut renderer, tau_proto::AgentNavigationMode::Active);
    renderer.handle(&Event::AgentPromptSubmitted(prompt));
    assert_eq!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .mode("worker-1"),
        AgentNavigationState::Active,
    );
}

#[test]
fn show_messages_none_leaves_no_visible_message_output() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    sync(&handle);
    let before = visible_lines(&vt, 80);

    renderer.apply_setting("show-messages", "none");
    renderer.handle(&agent_message("agent-a", "agent-b", "secret hidden body"));
    sync(&handle);

    assert_eq!(visible_lines(&vt, 80), before);
    assert!(!vt.screen_contains(80, "Message from"));
    assert!(!vt.screen_contains(80, "secret hidden body"));
}

#[test]
fn show_messages_summary_modes_do_not_show_body() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.apply_setting("show-messages", "all-summary");
    renderer.handle(&agent_message(
        "agent-a",
        "agent-b",
        "secret summarized body",
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Message from @agent-a to @agent-b"));
    assert!(!vt.screen_contains(80, "secret summarized body"));
}

/// Retained history keeps its originating session's name authority when a
/// different resumed session later publishes metadata for the same agent id.
#[test]
fn resumed_session_names_do_not_relabel_prior_message_history() {
    let (_term, handle, vt) = setup(100, 12);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-messages", "all-full");
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-a"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "session A worker".to_owned(),
        },
    ));
    renderer.handle(&agent_message("agent-a", "agent-b", "session A body"));
    sync(&handle);
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (session A worker):"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("session-b"),
        reason: SessionStartReason::Resume,
    }));
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "session B worker".to_owned(),
        },
    ));
    renderer.handle(&agent_message("agent-a", "agent-b", "session B body"));
    sync(&handle);

    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b:"));
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (session B worker):"));
    assert_eq!(
        vt.screen_text(100)
            .iter()
            .filter(|row| row.contains("session B worker"))
            .count(),
        1,
        "only the session-B message may use session-B metadata"
    );
    assert!(vt.screen_contains(100, "session A body"));
    assert!(vt.screen_contains(100, "session B body"));
}

#[test]
fn show_messages_toggle_retroactively_hides_and_shows_history() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.apply_setting("show-messages", "none");
    renderer.handle(&agent_message("agent-a", "agent-b", "retro body"));
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from @agent-a to @agent-b"));
    assert!(!vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "all-full");
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from @agent-a to @agent-b:"));
    assert!(vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "none");
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from @agent-a to @agent-b"));
    assert!(!vt.screen_contains(80, "retro body"));
}

#[test]
fn new_session_clears_session_ui_state() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "old prompt".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            assistant_message_item("old response"),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/lib.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(vec![
            (
                CborValue::Text("path".into()),
                CborValue::Text("src/lib.rs".into()),
            ),
            (
                CborValue::Text("content".into()),
                CborValue::Text("fn main() {}\n".into()),
            ),
        ]),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/lib.rs".into(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old prompt"));
    assert!(vt.screen_contains(80, "old response"));
    assert!(vt.screen_contains(80, "read src/lib.rs"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "old prompt"));
    assert!(!vt.screen_contains(80, "old response"));
    assert!(!vt.screen_contains(80, "read src/lib.rs"));
    assert!(!vt.screen_contains(80, "&s2"));
    assert!(!vt.screen_contains(80, "no role selected"));
}
/// `notice-level=warning` hides routine informational chatter while mandatory
/// warnings such as configuration errors still reach the UI.
#[test]
fn warning_notice_level_hides_diagnostics_but_keeps_alerts() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "warning");

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.info".into(),
        message: "routine lifecycle note".into(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Diagnostic,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "routine lifecycle note"));

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: "test.warning".into(),
        message: "important config error".into(),
        level: tau_proto::NoticeLevel::Warning,
        purpose: tau_proto::NoticePurpose::Alert,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "important config error"));
}

/// Compact mode hides diagnostics, preserves responses and alerts, and restores
/// diagnostics after a hidden-agent transcript round trip.
#[test]
fn compact_mode_reprojects_retained_notices_without_hiding_critical_errors() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());

    for (kind, message, level, purpose) in [
        (
            "test.info",
            "status reminder",
            tau_proto::NoticeLevel::Info,
            tau_proto::NoticePurpose::Diagnostic,
        ),
        (
            "test.warning",
            "mandatory warning",
            tau_proto::NoticeLevel::Warning,
            tau_proto::NoticePurpose::Alert,
        ),
        (
            "test.error",
            "critical harness error",
            tau_proto::NoticeLevel::Critical,
            tau_proto::NoticePurpose::Diagnostic,
        ),
    ] {
        renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
            kind: kind.into(),
            message: message.into(),
            level,
            purpose,
        }));
    }
    sync(&handle);
    for visible in [
        "status reminder",
        "mandatory warning",
        "critical harness error",
    ] {
        assert!(vt.screen_contains(80, visible));
    }

    renderer.toggle_verbose_mode();
    renderer.switch_agent("worker".to_owned());
    renderer.switch_agent("main".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "status reminder"));
    assert!(vt.screen_contains(80, "mandatory warning"));
    assert!(vt.screen_contains(80, "critical harness error"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    for restored in [
        "status reminder",
        "mandatory warning",
        "critical harness error",
    ] {
        assert!(vt.screen_contains(80, restored));
    }
}

/// Locally synthesized manual-compaction acceptance notices follow the same
/// compact projection even when their target agent is currently hidden.
#[test]
fn compact_mode_reprojects_hidden_target_manual_compaction_notice() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    renderer.switch_agent("worker".to_owned());
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-hidden-notice", "call-hidden-notice"),
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "accepted compaction request"));

    renderer.toggle_verbose_mode();
    renderer.switch_agent("main".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(100, "Agent main accepted compaction request"));
}

#[test]
fn critical_notice_level_keeps_harness_failure_alert() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "critical");

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: tau_proto::notice_kind::HARNESS_FAILURE.into(),
        message: "failed to dispatch queued prompt: boom".into(),
        level: tau_proto::NoticeLevel::Warning,
        purpose: tau_proto::NoticePurpose::Alert,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "failed to dispatch queued prompt: boom"));
}

/// Compact mode hides typed lifecycle diagnostics and restores their original
/// position when verbose mode returns.
#[test]
fn compact_mode_reprojects_extension_lifecycle_diagnostic() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 1.into(),
        extension_name: tau_proto::ExtensionName::parse("core-shell")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension core-shell"));

    renderer.switch_agent("worker".to_owned());
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(80, "extension core-shell"));

    renderer.apply_setting("notice-level", "warning");
    sync(&handle);
    assert!(!vt.screen_contains(80, "extension core-shell"));
    renderer.apply_setting("notice-level", "info");
    sync(&handle);
    assert!(vt.screen_contains(80, "extension core-shell"));

    let themed =
        tau_themes::Theme::parse(r#"{ styles: { "extension.lifecycle": { fg: "red" } } }"#)
            .expect("theme parses");
    renderer.apply_theme(themed);
    sync(&handle);
    assert_rendered_ansi_foreground(&vt, 80, "extension core-shell", 9);
}

/// Ensures provider response stats make the standalone live indicator
/// look active without entering the final transcript.
#[test]
fn provider_response_stats_update_suffixes_live_indicator_until_finish() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-progress",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "sp-progress",
            tau_proto::AgentId::parse("main").expect("agent id"),
            0,
            0,
            1_000_000,
            0,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (1s, 0B, Δ0B/s, 0B/s)"),
        "pre-output stats samples must still refresh elapsed time: {:?}",
        vt.screen_text(80)
    );
    let mut first_output = main_provider_response_stats_update("sp-progress", 12 * 1024, 4 * 1024);
    first_output
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(first_output));
    sync(&handle);
    assert!(vt.screen_contains(80, "… (820ms, 2s, 12KB, Δ8KB/s, 6KB/s)"));
    assert!(!vt.screen_contains(80, "shell_command"));
    assert!(!vt.screen_contains(80, "tool args"));
    assert!(!vt.screen_contains(80, "tools,"));

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-progress"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 1,
            kind: tau_proto::ReasoningTextKind::Summary,
            text: "thinking".to_owned(),
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (820ms, 2s, 12KB, Δ8KB/s, 6KB/s)"),
        "updates without a fresh stats sample must not clear cached stats: {:?}",
        vt.screen_text(80)
    );

    let mut repeated = provider_response_stats_update(
        "sp-progress",
        tau_proto::AgentId::parse("main").expect("agent id"),
        12 * 1024,
        12 * 1024,
        3_000_000,
        2_000_000,
    );
    repeated
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(repeated));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (820ms, 3s, 12KB, Δ0B/s, 4KB/s)"),
        "idle stats samples must show elapsed time, zero interval rate, and total rate: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-progress"),
        agent_id: agent_id("main"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: "retrying".to_owned(),
            clear_response: true,
            retry: None,
        }),
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "… (820ms,"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-progress",
        Vec::new(),
    )));
    sync(&handle);
    assert!(!vt.screen_contains(80, "… (820ms,"));
}

/// Compact mode must hide both retained in-progress response statistics and a
/// newly completed turn-stat row, then restore each retained projection in
/// verbose mode.
#[test]
fn compact_mode_hides_live_and_new_turn_statistics() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-compact-stats",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-compact-stats", 12 * 1024, 4 * 1024),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "Δ8KB/s"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "Δ8KB/s"));

    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-compact-stats", 20 * 1024, 12 * 1024),
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "Δ8KB/s"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "… (2s, 20KB, Δ8KB/s, 10KB/s)"));
    assert!(!vt.screen_contains(100, "… (2s, 12KB, Δ8KB/s, 6KB/s)"));

    renderer.toggle_verbose_mode();
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "sp-compact-stats",
            "main",
            20_000,
            10_000,
            500,
            "compact stats answer",
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "compact stats answer"));
    assert!(!vt.screen_contains(100, "Δ"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "Δ"));
}

/// First-output durations switch units at the approved five-second and
/// five-minute boundaries without rendering a placeholder for absence.
#[test]
fn first_output_duration_uses_compact_boundaries() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-first-output-format",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-first-output-format", 0, 0),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "… (2s, 0B, Δ0B/s, 0B/s)"));

    for (micros, expected) in [
        (4_999_999, "… (4999ms, 600s, 1B"),
        (5_000_000, "… (5s, 600s, 1B"),
        (299_999_999, "… (299s, 600s, 1B"),
        (300_000_000, "… (5m, 600s, 1B"),
    ] {
        let mut update = provider_response_stats_update(
            "sp-first-output-format",
            agent_id("main"),
            1,
            0,
            600_000_000,
            0,
        );
        update
            .response_stats
            .as_mut()
            .expect("response stats")
            .first_semantic_output_elapsed_micros = Some(micros);
        renderer.handle(&Event::ProviderResponseUpdated(update));
        sync(&handle);
        assert!(
            vt.screen_contains(100, expected),
            "missing {expected}: {:?}",
            vt.screen_text(100)
        );
    }
}

/// Ensures response-progress stats are scoped to the agent transcript that owns
/// the prompt rather than bleeding into the currently visible transcript.
///
/// A stats sample for a hidden agent must update only that hidden snapshot; the
/// user viewing another agent should not see the live response stats line
/// appear, disappear, or change because of background activity elsewhere.
#[test]
fn hidden_provider_response_stats_do_not_update_visible_response_indicator() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("agent_a".to_owned());
    let mut prompt_a = agent_prompt_created("ap-agent_a-0", "s1");
    prompt_a.agent_id = agent_id("agent_a");
    renderer.handle(&Event::AgentPromptCreated(prompt_a));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_stats_update(
            "ap-agent_a-0",
            agent_id("agent_a"),
            4 * 1024,
            0,
            2_000_000,
            1_000_000,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"));

    renderer.switch_agent("agent_b".to_owned());
    let mut prompt_b = agent_prompt_created("ap-agent_b-0", "s1");
    prompt_b.agent_id = agent_id("agent_b");
    renderer.handle(&Event::AgentPromptCreated(prompt_b));
    renderer.switch_agent("agent_a".to_owned());
    let mut hidden_update = provider_response_stats_update(
        "ap-agent_b-0",
        agent_id("agent_b"),
        12 * 1024,
        4 * 1024,
        2_000_000,
        1_000_000,
    );
    hidden_update
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(hidden_update));
    sync(&handle);

    assert!(
        vt.screen_contains(80, "… (2s, 4KB, Δ4KB/s, 2KB/s)"),
        "visible agent A stats should remain unchanged: {:?}",
        vt.screen_text(80)
    );
    assert!(
        !vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"),
        "hidden agent B stats must not render in agent A's view: {:?}",
        vt.screen_text(80)
    );
    assert!(!vt.screen_contains(80, "… (820ms,"));

    renderer.switch_agent("agent_b".to_owned());
    sync(&handle);
    assert!(
        vt.screen_contains(80, "… (820ms, 2s, 12KB, Δ8KB/s, 6KB/s)"),
        "hidden stats should be visible when switching to their owning agent: {:?}",
        vt.screen_text(80)
    );
}

/// Ensures a stale prompt-associated stats sample received after the final
/// provider response does not recreate an already-finished live response block.
#[test]
fn late_provider_response_stats_after_finish_does_not_recreate_live_indicator() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-progress",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-progress",
        vec![assistant_message_item("done")],
    )));
    let mut stale = main_provider_response_stats_update("sp-progress", 12 * 1024, 4 * 1024);
    stale
        .response_stats
        .as_mut()
        .expect("response stats")
        .first_semantic_output_elapsed_micros = Some(820_000);
    renderer.handle(&Event::ProviderResponseUpdated(stale));
    sync(&handle);

    assert!(vt.screen_contains(80, "done"));
    assert!(!vt.screen_contains(80, "… (2s, 12KB, Δ8KB/s, 6KB/s)"));
    assert!(!vt.screen_contains(80, "… (820ms,"));
    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!renderer.main_agent_turn_active_for_test());
}

/// Ensures visible assistant streaming remains content-focused: provider
/// response stats are not appended to the response text while text is visibly
/// active.
#[test]
fn provider_visible_update_omits_response_stats_suffix() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-visible-progress",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        main_provider_response_stats_update("sp-visible-progress", 5, 0),
    ));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-visible-progress"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: vec![tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text: "Hello".to_owned(),
            phase: None,
        }],
        compaction: None,
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello …"));
    assert!(!vt.screen_contains(80, "Hello … (2s, 5B, Δ5B/s, 2B/s)"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-visible-progress",
        vec![assistant_message_item("Hello")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "(2s, 5B, Δ5B/s, 2B/s)"));
}

#[test]
fn set_show_thinking_round_trip_restores_history() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..agent_prompt_created("sp-0", "s1")
    }));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "the_response",
            Some("the_thinking_text".into()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("the_response")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "the_thinking_text"));
    assert!(vt.screen_contains(80, "the_response"));

    // Off — thinking content disappears, no placeholder, no
    // blank row left behind: the response should be on the same
    // row as the (now-empty) thinking block sat before. We assert
    // this indirectly by counting non-blank lines.
    let lines_before = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    renderer.apply_setting("show-thinking", "false");
    sync(&handle);
    assert!(!vt.screen_contains(80, "the_thinking_text"));
    assert!(!vt.screen_contains(80, "thinking hidden"));
    assert!(vt.screen_contains(80, "the_response"));
    let lines_after = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    // Hiding the one thinking block should remove exactly one
    // visible line of content from the screen.
    assert_eq!(lines_after + 1, lines_before);

    // Back on — original thinking text returns in its original
    // position above the response.
    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("the_thinking_text"))
        .unwrap_or_else(|| panic!("thinking should reappear: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("the_response"))
        .unwrap_or_else(|| panic!("response should still be visible: {lines:?}"));
    assert!(thinking_row < response_row);
}

#[test]
fn thinking_created_while_off_stays_invisible_after_toggle_on() {
    // Blocks that arrive while `show_thinking == false` are
    // never rendered and never tracked, so toggling back on
    // doesn't suddenly resurrect them. Only blocks that were
    // visible at some point round-trip through `set_block`.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-thinking", "false");

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..agent_prompt_created("sp-0", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("answer")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "answer"));
    assert!(!vt.screen_contains(80, "hidden reasoning"));

    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "hidden reasoning"),
        "blocks created while off should not appear after toggle on"
    );
}

/// Contradictory request correlation must preserve the independent lifecycle
/// row rather than merging an unrelated tool call into it.
#[test]
fn mismatched_self_compaction_correlation_fails_open_to_distinct_rows() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.apply_setting("show-tools", "compact");

    let mut tool_start = tool_started("call-mismatch", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-mismatch", "call-mismatch"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started("cr-other", "call-mismatch", "ct-mismatch", "ap-mismatch"),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-mismatch"),
    ));
    sync(&handle);

    let text = vt.screen_text(100).join("\n");
    assert_eq!(text.matches("Compacting…").count(), 1, "{text}");
    assert!(text.contains("compact 0s pending"), "{text}");
}

/// Ensures cold catch-up can fold the durable standalone start and replacement
/// boundary, without the live-only prompt-start fact, without an assistant turn
/// or stale compaction marker.
#[test]
fn standalone_compaction_replay_retires_private_progress() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        standalone_compaction_started("ct-replay", "ap-replay"),
    ));
    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::TokenCount::new(226_200)),
        compaction_output_tokens: Some(tau_proto::TokenCount::new(4_500)),
        agent_id: agent_id("main"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-replay")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-replay")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![assistant_message_item("synthetic checkpoint")],
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "compact #226.2k in / #4.5k out ok"));
    assert!(!vt.screen_contains(100, "Compacting…"));
    assert!(!vt.screen_contains(100, "synthetic checkpoint"));
    assert!(!vt.screen_contains(100, "◆"));
}

/// The normalized wait bound must be visible both while the call is live and
/// after its generic result replaces the pending block.
#[test]
fn wait_timeout_label_survives_live_to_retained_transition() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let arguments = CborValue::Map(vec![(
        CborValue::Text("timeout_minutes".to_owned()),
        CborValue::Integer(75.into()),
    )]);

    renderer.handle_recorded_at(
        &tool_started("wait-timeout", "wait", arguments),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("wait-timeout", "wait", "60m", ""),
        tau_proto::UnixMicros::new(1_100_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "wait 60m"));

    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "wait-timeout".into(),
            tool_name: tau_proto::ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("timed_out: true".to_owned()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "60m".to_owned(),
                status: tau_proto::ToolUseStatus::Warning,
                status_text: "timeout".to_owned(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "wait 60m 1s timeout"));

    // Durable replay does not include transient progress, so the terminal
    // descriptor must remain self-contained.
    let (_replay_term, replay_handle, replay_vt) = setup(80, 24);
    let mut replay = EventRenderer::new(
        replay_handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    replay.handle_recorded_at(
        &tool_started(
            "replayed-wait",
            "wait",
            CborValue::Map(vec![(
                CborValue::Text("timeout_minutes".to_owned()),
                CborValue::Integer(75.into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    replay.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "replayed-wait".into(),
            tool_name: tau_proto::ToolName::new("wait"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Map(vec![(
                CborValue::Text("timed_out".to_owned()),
                CborValue::Bool(true),
            )]),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "60m".to_owned(),
                status: tau_proto::ToolUseStatus::Warning,
                status_text: "timeout".to_owned(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&replay_handle);
    assert!(replay_vt.screen_contains(80, "wait 60m 1s timeout"));
}

/// Every structured progress counter keeps counter priority, including custom
/// and unlabelled counters, while free-form info remains independently lower.
#[test]
fn structured_counters_outrank_generic_info() {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "tool",
        &ToolUseState {
            progress_counters: vec![
                ProgressCounter {
                    label: Some("count".into()),
                    unit: ProgressUnit::Count,
                    complete: Some(1),
                    total: None,
                },
                ProgressCounter {
                    label: None,
                    unit: ProgressUnit::Count,
                    complete: Some(2),
                    total: None,
                },
            ],
            info_chips: vec!["optional".into()],
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    assert!(matches!(display.suffixes[0].status, ToolStatus::Counter));
    assert!(matches!(display.suffixes[1].status, ToolStatus::Counter));
    assert!(matches!(display.suffixes[2].status, ToolStatus::Info));

    let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 18);
    assert_eq!(header, "tool count: 1 2 ok");
}

#[test]
fn synthesize_fallback_display_is_minimal() {
    let ok = synthesize_fallback_display("my_tool", None);
    assert_eq!(ok.args, "");
    assert_eq!(ok.status_text, "ok");
    assert!(matches!(ok.status, tau_proto::ToolUseStatus::Success));

    let err =
        synthesize_fallback_display("my_tool", Some("failure description\nwith trailing line"));
    assert_eq!(err.status_text, "failure description");
    assert!(matches!(err.status, tau_proto::ToolUseStatus::Error));
}
/// A pre-start rejection must return to the target's detached transcript so
/// the selected agent cannot steal its self-compaction correlation.
#[test]
fn hidden_self_compaction_rejection_updates_its_target_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("manager".to_owned());
    renderer.apply_setting("show-tools", "compact");

    let mut tool_start = tool_started("call-hidden-rejected", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-hidden-rejected", "call-hidden-rejected"),
    ));
    renderer.handle(&Event::AgentManualCompactionRequestFailed(
        tau_proto::AgentManualCompactionRequestFailed {
            request_id: tau_proto::CompactionRequestId::parse("cr-hidden-rejected")
                .expect("known-safe request id"),
            target_agent_id: agent_id("main"),
            reason: tau_proto::ManualCompactionRequestFailureReason::Unsupported,
        },
    ));

    renderer.switch_agent("main".to_owned());
    sync(&handle);
    let text = vt.screen_text(100).join("\n");
    assert_eq!(text.matches("err: rejected").count(), 1, "{text}");
}
/// Ensures a middle insertion received while live thinking is hidden
/// invalidates the prior append cache before the retained block is shown again.
#[test]
fn hidden_thinking_middle_insertion_reparses_on_show() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..agent_prompt_created("sp-middle-thinking", "s1")
    }));
    let reasoning_update = |output_index, text: &str| {
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: test_agent_prompt_id("sp-middle-thinking"),
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            deltas: vec![tau_proto::ProviderResponseTextDelta::ReasoningText {
                output_index,
                kind: tau_proto::ReasoningTextKind::Summary,
                text: text.to_owned(),
            }],
            compaction: None,
            status: None,
            response_stats: None,
            originator: tau_proto::PromptOriginator::User,
        })
    };

    renderer.handle(&reasoning_update(1, "later\n"));
    renderer.apply_setting("show-thinking", "false");
    renderer.handle(&reasoning_update(0, "éarlier\n"));
    renderer.apply_setting("show-thinking", "true");
    sync(&handle);

    let lines = vt.screen_text(80);
    let earlier = lines
        .iter()
        .position(|line| line.contains("éarlier"))
        .unwrap_or_else(|| panic!("inserted thinking missing: {lines:?}"));
    let later = lines
        .iter()
        .position(|line| line.contains("later"))
        .unwrap_or_else(|| panic!("original thinking missing: {lines:?}"));
    assert!(earlier < later, "middle insertion rendered out of order");
}

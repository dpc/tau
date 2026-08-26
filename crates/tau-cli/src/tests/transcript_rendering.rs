//! Tests for transcript rendering behavior.

use std::sync::Barrier;

use super::super::markdown_render::markdown_block;
use super::*;

#[test]
fn renderer_starts_without_selected_or_default_agent() {
    // Regression: the UI opens in the start-new-agent state instead of
    // preselecting a synthetic `main` agent.
    let (_term, handle, _vt) = setup(80, 24);
    let renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    assert_eq!(
        *renderer
            .current_agent_state()
            .lock()
            .expect("current agent"),
        None
    );
    assert!(
        renderer
            .known_agents()
            .lock()
            .expect("known agents")
            .is_empty()
    );
    assert!(
        renderer
            .agent_navigation()
            .lock()
            .expect("agent navigation")
            .active_agents()
            .is_empty()
    );
}

/// An assistant response must visibly transition from the hollow streaming
/// marker to the solid completed marker when its final event replaces the live
/// block.
#[test]
fn agent_response_marker_tracks_streaming_and_completed_states() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-marker",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-marker"),
            "marker answer",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "◇ marker answer"));
    assert!(!vt.screen_contains(80, "◆ marker answer"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-marker",
        vec![assistant_message_item("marker answer")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "◆ marker answer"));
    assert!(!vt.screen_contains(80, "◇ marker answer"));
}

/// A visible final must coalesce all of its block, editor, and status redraw
/// requests into one settled wake instead of exposing intermediate final state.
#[test]
fn visible_provider_final_requests_one_settled_redraw() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.apply_setting("show-thinking", "true");
    renderer.apply_setting("show-turn-stats", "true");
    for index in 0..40 {
        handle.print_output(
            format!("atomic-history-{index}"),
            format!("history {index}"),
        );
    }

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-atomic-final",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-atomic-final"),
            "live answer",
            Some("live reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    let generation = vt.frame_generation();
    let editor_context = renderer.editor_context();
    let staging = Arc::new(Barrier::new(2));
    let unrelated = handle.clone();
    let producer_staging = staging.clone();
    let producer = std::thread::spawn(move || {
        producer_staging.wait();
        unrelated.print_output("unrelated-final-race", "unrelated output");
        unrelated.redraw();
    });
    let hook_staging = staging.clone();
    let hook_vt = vt.clone();
    let staging_editor = editor_context.clone();
    renderer.set_finished_staging_hook(Arc::new(move || {
        assert!(
            staging_editor
                .lock()
                .expect("staging editor")
                .last_response
                .is_none()
        );
        hook_staging.wait();
        hook_vt.wait_for_frame_containing_after(generation, "unrelated output");
    }));
    let commit_barrier = Arc::new(Barrier::new(2));
    let commit_producer_barrier = commit_barrier.clone();
    let commit_handle = handle.clone();
    let commit_producer = std::thread::spawn(move || {
        commit_producer_barrier.wait();
        commit_handle.print_output("unrelated-during-commit", "commit output");
        commit_handle.redraw_sync();
        commit_producer_barrier.wait();
    });
    let hook_commit_barrier = commit_barrier.clone();
    let commit_editor = editor_context.clone();
    renderer.set_finished_commit_hook(Arc::new(move || {
        assert!(
            commit_editor
                .lock()
                .expect("commit editor")
                .last_response
                .is_none()
        );
        hook_commit_barrier.wait();
        hook_commit_barrier.wait();
    }));
    let published_editor = editor_context.clone();
    renderer.set_finished_published_hook(Arc::new(move || {
        let editor = published_editor.lock().expect("published editor");
        let response = editor.last_response.as_deref().expect("published response");
        assert!(response.contains("first settled item"));
        assert!(response.contains("settled answer"));
    }));

    let mut finished =
        finished_response_with_usage("sp-atomic-final", "main", 120, 40, 12, "settled answer");
    finished.output_items = vec![
        ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
            kind: tau_proto::ReasoningTextKind::Summary,
            text: "settled reasoning".to_owned(),
        }),
        assistant_message_item("first settled item"),
        assistant_message_item("settled answer"),
        ContextItem::Compaction(OpaqueProviderItem::new(CborValue::Map(vec![]))),
        ContextItem::ToolCall(ToolCallItem {
            call_id: "atomic-placeholder".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        }),
        ContextItem::ToolCall(ToolCallItem {
            call_id: "atomic-placeholder-second".into(),
            name: tau_proto::ToolName::new("search"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        }),
    ];
    finished.stop_reason = ProviderStopReason::ToolCalls;
    renderer.handle(&Event::ProviderResponseFinished(finished));
    producer.join().expect("unrelated producer");
    commit_producer.join().expect("commit producer");

    let settled_generation = vt.wait_for_frame_containing_after(generation, "◆ settled answer");
    let frames = vt.frames.0.lock().expect("frames");
    for frame in &frames[generation..settled_generation] {
        let text = frame.join("\n");
        let complete_live = text.contains("◇ live answer")
            && text.contains("live reasoning")
            && !text.contains("◆ settled answer")
            && !text.contains("◆ first settled item")
            && !text.contains("settled reasoning")
            && !text.contains("compact ok")
            && !text.contains(" ↑120 ↓12")
            && !text.contains("%0/2");
        let complete_settled = text.contains("◆ settled answer")
            && text.contains("◆ first settled item")
            && text.contains("settled reasoning")
            && text.contains("compact ok")
            && text.contains(" ↑120 ↓12")
            && text.contains("@main")
            && text.contains("%0/2")
            && !text.contains("◇ live answer")
            && !text.contains("live reasoning");
        assert!(
            complete_live || complete_settled,
            "frame mixed live and settled final state: {frame:?}"
        );
    }
    assert!(
        frames[settled_generation - 1]
            .iter()
            .any(|row| row.contains("unrelated output"))
    );
    assert!(
        frames[settled_generation - 1]
            .iter()
            .any(|row| row.contains("commit output"))
    );
    drop(frames);
    let editor = editor_context.lock().expect("editor");
    let response = editor.last_response.as_deref().expect("last response");
    assert!(response.contains("first settled item"));
    assert!(response.contains("settled answer"));
    drop(editor);
    assert_eq!(renderer.test_active_tool_count(), 2);
    let placeholder_ids = renderer
        .tool_placeholder_ids_for_test(&["atomic-placeholder", "atomic-placeholder-second"]);
    assert_eq!(placeholder_ids[1], placeholder_ids[0] + 1);
    assert!(vt.scrollback_contains(80, 40, "history 0"));
}

/// A final routed into a detached transcript must remain off-screen and must
/// not wake the visible terminal; selecting it later reveals the settled
/// projection.
#[test]
fn hidden_provider_final_stays_off_screen_without_redraw() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-hidden-final",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-hidden-final"),
            "hidden live answer",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-visible-worker".to_owned(),
        agent_id: agent_id("visible-worker"),
    }));
    renderer.switch_agent("visible-worker".to_owned());
    sync(&handle);
    let redraw_requests = renderer.redraw_request_count_for_test();

    let mut hidden_final =
        finished_response_with_usage("sp-hidden-final", "main", 12, 4, 2, "hidden settled answer");
    hidden_final
        .output_items
        .push(ContextItem::ToolCall(ToolCallItem {
            call_id: "hidden-partial-tool".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some("{".to_owned()),
            responses_envelope: None,
        }));
    hidden_final.stop_reason = ProviderStopReason::Length;
    renderer.handle(&Event::ProviderResponseFinished(hidden_final));
    assert_eq!(renderer.redraw_request_count_for_test(), redraw_requests);
    assert!(!vt.screen_contains(80, "hidden settled answer"));

    renderer.switch_agent("main".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "◆ hidden settled answer"));
    assert!(vt.screen_contains(
        80,
        "Model reached its output-token limit while producing a tool call"
    ));
    assert!(!vt.screen_contains(80, "◇ hidden live answer"));
    assert!(vt.screen_contains(80, "✨ @main"));
    assert!(!vt.screen_contains(80, "%0/"));
    assert_eq!(renderer.test_active_tool_count(), 0);
}

/// Empty and output-length finals must replace the complete live frame together
/// with their terminal placeholder rather than drawing either half first.
#[test]
fn empty_and_output_length_finals_publish_complete_frames() {
    for (prompt_id, finished, final_markers) in [
        (
            "atomic-empty",
            finished_response("atomic-empty", Vec::new()),
            vec!["◆ (provider returned an empty response)"],
        ),
        (
            "atomic-length",
            {
                let mut finished = finished_response(
                    "atomic-length",
                    vec![
                        assistant_message_item("partial terminal"),
                        ContextItem::ToolCall(ToolCallItem {
                            call_id: "partial-atomic-tool".into(),
                            name: tau_proto::ToolName::new("read"),
                            tool_type: tau_proto::ToolType::Function,
                            arguments: CborValue::Map(Vec::new()),
                            raw_arguments_json: Some("{".to_owned()),
                            responses_envelope: None,
                        }),
                    ],
                );
                finished.stop_reason = ProviderStopReason::Length;
                finished
            },
            vec![
                "◆ partial terminal",
                "Model reached its output-token limit while producing a tool call",
            ],
        ),
    ] {
        let (_term, handle, vt) = setup(100, 12);
        let mut renderer = marker_test_renderer(handle.clone());
        renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
            prompt_id, "s1",
        )));
        renderer.handle(&Event::ProviderResponseUpdated(
            provider_response_delta_update(
                test_agent_prompt_id(prompt_id),
                "old live terminal",
                None,
                tau_proto::PromptOriginator::User,
            ),
        ));
        sync(&handle);
        let generation = vt.frame_generation();
        let commit_handle = handle.clone();
        renderer.set_finished_commit_hook(Arc::new(move || {
            commit_handle.redraw_sync();
        }));

        renderer.handle(&Event::ProviderResponseFinished(finished));
        let final_generation = vt.wait_for_frame_containing_after(generation, final_markers[0]);
        let frames = vt.frames.0.lock().expect("frames");
        let final_status = if prompt_id == "atomic-length" {
            "✨ @main"
        } else {
            "💤 @main"
        };
        for frame in &frames[generation..final_generation] {
            let text = frame.join("\n");
            let live = text.contains("◇ old live terminal")
                && final_markers.iter().all(|marker| !text.contains(marker));
            let settled = final_markers.iter().all(|marker| text.contains(marker))
                && text.contains(final_status)
                && !text.contains("%0/")
                && !text.contains("◇ old live terminal")
                && !text.contains("old live terminal");
            assert!(live || settled, "partial terminal frame: {frame:?}");
        }
        assert_eq!(renderer.test_active_tool_count(), 0);
    }
}

/// Deferred discovery catch-up must stage its terminal response after selection
/// and then use the same live-or-settled frame cut as ordinary delivery.
#[test]
fn deferred_initial_discovery_final_uses_atomic_publication_cut() {
    let (_term, handle, vt) = setup(100, 16);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.handle(&Event::HarnessAgentContextInitialized(
        tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("s1"),
            agent_id: agent_id("main"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("main-init")
                .expect("initialization id"),
            listed_skills: Vec::new(),
            agents_files: Vec::new(),
        },
    ));
    renderer.handle(&agent_message("main", "worker", "deferred overview once"));
    renderer.handle(&Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("deferred-bridge").expect("publisher id"),
        tau_proto::MessageAgentTarget::new("main"),
        tau_proto::MessageFactId::new("deferred-owned-fact"),
        tau_proto::MessageParty {
            stable_id: "deferred-sender".to_owned(),
            display_name: None,
            sender_auth: None,
        },
        None,
        "deferred owned fact once",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("deferred-final"),
            "deferred live",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    let finished = finished_response(
        "deferred-final",
        vec![assistant_message_item("deferred settled")],
    );
    renderer.handle(&Event::ProviderResponseFinished(finished));
    sync(&handle);
    let generation = vt.frame_generation();
    let staging_handle = handle.clone();
    let staging_vt = vt.clone();
    renderer.set_finished_staging_hook(Arc::new(move || {
        staging_handle.redraw_sync();
        assert!(staging_vt.screen_contains(100, "deferred live"));
        assert!(staging_vt.screen_contains(100, "✨ @main"));
    }));
    let commit_handle = handle.clone();
    renderer.set_finished_commit_hook(Arc::new(move || commit_handle.redraw_sync()));
    let deferred_editor = renderer.editor_context();
    let published_editor = deferred_editor.clone();
    renderer.set_finished_published_hook(Arc::new(move || {
        assert_eq!(
            published_editor
                .lock()
                .expect("published editor")
                .last_response
                .as_deref(),
            Some("deferred settled")
        );
    }));

    renderer.switch_agent("main".to_owned());
    let final_generation = vt.wait_for_frame_containing_after(generation, "◆ deferred settled");
    let frames = vt.frames.0.lock().expect("frames");
    let mut saw_live = false;
    for frame in &frames[generation..final_generation] {
        let text = frame.join("\n");
        let before_replay = (text.contains("@main") || text.contains("deferred overview once"))
            && !text.contains("deferred live")
            && !text.contains("deferred settled");
        let live = text.contains("deferred live")
            && text.contains("✨ @main")
            && !text.contains("deferred settled");
        let settled = text.contains("◆ deferred settled")
            && text.contains("💤 @main")
            && !text.contains("deferred live");
        if live {
            saw_live = true;
        }
        assert!(
            (!saw_live && before_replay) || live || settled,
            "mixed deferred final frame: {frame:?}"
        );
    }
    drop(frames);
    assert_eq!(
        deferred_editor
            .lock()
            .expect("editor")
            .last_response
            .as_deref(),
        Some("deferred settled")
    );
    let selected_text = vt.screen_text(100).join("\n");
    assert_eq!(selected_text.matches("deferred overview once").count(), 1);
    assert!(!selected_text.contains("deferred owned fact once"));
    renderer.clear_selected_agent();
    sync(&handle);
    assert_eq!(
        vt.screen_text(100)
            .join("\n")
            .matches("deferred overview once")
            .count(),
        1
    );
    assert_eq!(
        vt.screen_text(100)
            .join("\n")
            .matches("deferred owned fact once")
            .count(),
        1
    );
}

#[test]
fn switching_between_displayed_agents_restores_transcripts() {
    // The no-redraw fast path must not hide real transcript switches: moving
    // between two agents still swaps the output snapshot and restores each
    // agent's durable scrollback.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "worker one transcript".into(),
        agent_id: agent_id("worker-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.switch_agent("worker-2".to_owned());
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "worker two transcript".into(),
        agent_id: agent_id("worker-2"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "worker two transcript"));
    assert!(!vt.screen_contains(80, "worker one transcript"));
    let full_render_count = handle.full_render_count();

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);

    assert!(vt.screen_contains(80, "worker one transcript"));
    assert!(!vt.screen_contains(80, "worker two transcript"));
    assert!(handle.full_render_count() > full_render_count);
}

/// Ensures the redraw caused by an agent switch cannot combine the destination
/// transcript with the previously selected agent's input placeholder.
#[test]
fn agent_switch_first_frame_has_matching_transcript_and_placeholder() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let generation = vt.frame_generation();
    handle.with_redraw_suppressed(|| {
        renderer.switch_agent("worker-1".to_owned());
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "worker one transcript".into(),
            agent_id: agent_id("worker-1"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
        renderer.switch_agent("worker-2".to_owned());
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: "worker two transcript".into(),
            agent_id: agent_id("worker-2"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    });
    let generation = vt.wait_for_frame_containing_after(generation, "worker two transcript");
    renderer.switch_agent_after_display_update_for_test("worker-1".to_owned(), || {
        handle.redraw_sync();
    });
    let frame = vt.wait_for_frame_after(generation);

    assert!(
        frame
            .iter()
            .any(|row| row.contains("worker one transcript")),
        "{frame:?}"
    );
    assert!(
        frame
            .iter()
            .any(|row| row.contains("Write a message to worker-1"))
    );
    assert!(
        !frame
            .iter()
            .any(|row| row.contains("Write a message to worker-2"))
    );
}

/// Agent-specific discovery must remain in its owning transcript across
/// background delivery and repeated selection switches.
#[test]
fn agent_context_initialization_is_visible_only_in_selected_agent_transcript() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let initialized = |agent: &str, skill: &str, path: &str| {
        Event::HarnessAgentContextInitialized(tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("session-1"),
            agent_id: agent_id(agent),
            agent_initialization_id: tau_proto::AgentInitializationId::parse(format!(
                "{agent}-init"
            ))
            .expect("test identifier must be valid"),
            listed_skills: vec![tau_proto::DiscoveryEffectiveSkill {
                name: skill.into(),
                description: format!("{skill} description"),
                source: tau_proto::DiscoveryEffectiveSkillSource::BuiltIn,
                add_to_prompt: true,
                user_invocable: true,
                disable_model_invocation: false,
                argument_hint: None,
            }],
            agents_files: vec![tau_proto::DiscoveryAgentsFileSummary {
                file_path: path.into(),
                lines: 6,
                bytes: 550,
            }],
        })
    };

    renderer.switch_agent("agent-1".to_owned());
    renderer.handle(&initialized(
        "agent-1",
        "foreground-skill",
        "/one/AGENTS.md",
    ));
    renderer.handle(&initialized(
        "agent-2",
        "background-skill",
        "/two/AGENTS.md",
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "foreground-skill 1L, 28B"));
    assert!(vt.screen_contains(80, "/one/AGENTS.md 6L, 550B"));
    assert!(!vt.screen_contains(80, "background-skill"));
    assert!(!vt.screen_contains(80, "/two/AGENTS.md"));

    renderer.switch_agent("agent-2".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "background-skill 1L, 28B"));
    assert!(vt.screen_contains(80, "/two/AGENTS.md 6L, 550B"));
    assert!(!vt.screen_contains(80, "foreground-skill"));
    assert!(!vt.screen_contains(80, "/one/AGENTS.md"));

    renderer.switch_agent("agent-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "foreground-skill 1L, 28B"));
    assert!(!vt.screen_contains(80, "background-skill"));
}

/// Renderer stats forwarding preserves exact canonical costs, rejects foreign
/// sessions, and clears the picker projection across New and Resume switches.
#[test]
fn agent_cost_projection_tracks_renderer_session_authority() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let projection = renderer.agent_estimated_api_costs();
    let cost = tau_proto::EstimatedApiCost::from_picodollars(2_140_000_000_000);
    let stats = |session: &str, agent: &str| {
        Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: test_session_id(session),
            agent_id: agent_id(agent),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state: tau_proto::AgentRuntimeState::Idle,
            turn_activity: tau_proto::AgentTurnActivity::Idle,
            tools: tau_proto::AgentToolStats::default(),
            context: tau_proto::AgentContextStats::default(),
            estimated_api_cost: cost,
            creator_subtree_estimated_api_cost: Default::default(),
            work_status: Default::default(),
        })
    };

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&stats("s1", "agent-a"));
    assert_eq!(
        projection.snapshot().get(&agent_id("agent-a")),
        Some(&crate::estimated_cost::AgentCostSnapshot::new(
            cost,
            tau_proto::EstimatedApiCost::default(),
        ))
    );
    renderer.handle(&stats("s2", "foreign"));
    assert!(!projection.snapshot().contains_key(&agent_id("foreign")));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::New,
    }));
    assert!(projection.snapshot().is_empty());
    renderer.handle(&stats("s2", "agent-b"));
    assert_eq!(
        projection.snapshot().get(&agent_id("agent-b")),
        Some(&crate::estimated_cost::AgentCostSnapshot::new(
            cost,
            tau_proto::EstimatedApiCost::default(),
        ))
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s3"),
        reason: SessionStartReason::Resume,
    }));
    assert!(projection.snapshot().is_empty());
    renderer.handle(&stats("s3", "agent-c"));
    assert_eq!(
        projection.snapshot().get(&agent_id("agent-c")),
        Some(&crate::estimated_cost::AgentCostSnapshot::new(
            cost,
            tau_proto::EstimatedApiCost::default(),
        ))
    );
}

#[test]
fn clearing_selected_agent_preserves_previous_transcript() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "worker transcript survives".into(),
        agent_id: agent_id("worker-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.clear_selected_agent();
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-helper".to_owned(),
        agent_id: agent_id("helper-1"),
    }));
    renderer.switch_agent("helper-1".to_owned());
    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);

    assert!(vt.screen_contains(80, "worker transcript survives"));
}

#[test]
fn new_session_resets_agent_transcripts() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    renderer.switch_agent("worker-1".to_owned());
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s2"),
        reason: tau_proto::SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "@worker-1"));
    assert!(
        !renderer
            .known_agents()
            .lock()
            .expect("known agents")
            .iter()
            .any(|agent| agent == "worker-1")
    );
}

#[test]
fn agent_switch_preserves_separate_transcripts() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));

    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("worker-1"),
        originator: originator.clone(),
        ..agent_prompt_created("worker-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator,
        ..finished_response("worker-sp", vec![assistant_message_item("worker answer")])
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker answer"));

    renderer.switch_agent("worker-1".to_owned());
    sync(&handle);
    assert!(vt.screen_contains(80, "worker answer"));
    assert!(vt.screen_contains(80, "@worker-1"));

    renderer.switch_agent("main".to_owned());
    sync(&handle);
    assert!(!vt.screen_contains(80, "worker answer"));
}

/// `AgentMessage` events are normal history entries, not active blocks. They
/// must render for every sender/recipient pair, emphasize `@`-qualified routing
/// identities, and scroll away as history grows.
#[test]
fn agent_messages_render_all_recipients_as_history() {
    let (_term, handle, vt) = setup(120, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("manager_11111111"),
        role: "manager".to_owned(),
        display_name: Some("add-all-agent-overview for @engineer_22222222".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));

    renderer.handle(&agent_message(
        "manager_11111111",
        "engineer_22222222",
        "hello worker",
    ));
    sync(&handle);
    assert!(vt.screen_contains(
        120,
        "Message from @manager_11111111 (add-all-agent-overview for @engineer_22222222) to @engineer_22222222:"
    ));
    assert!(vt.screen_contains(120, "hello worker"));
    let lines = vt.screen_text(120);
    let row = lines
        .iter()
        .position(|line| line.contains("Message from @manager_11111111"))
        .expect("message header row") as u16;
    use unicode_width::UnicodeWidthStr as _;
    let sender_col = lines[row as usize][..lines[row as usize]
        .find("@manager_11111111")
        .expect("sender column")]
        .width() as u16;
    let recipient_col = lines[row as usize][..lines[row as usize]
        .rfind("@engineer_22222222")
        .expect("recipient column")]
        .width() as u16;
    assert!(vt.cell_style(row, sender_col).2);
    assert!(vt.cell_style(row, recipient_col).2);
    assert!(!vt.cell_style(row, sender_col - 1).2);
    let task_context_col = lines[row as usize][..lines[row as usize]
        .find("(add-all-agent-overview for @engineer_22222222)")
        .expect("task-name context column")]
        .width() as u16;
    assert!(!vt.cell_style(row, task_context_col).2);
    let context_id_col = lines[row as usize][..lines[row as usize]
        .find("@engineer_22222222")
        .expect("routing-id text inside task-name context")]
        .width() as u16;
    assert!(!vt.cell_style(row, context_id_col).2);

    for idx in 0..20 {
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: format!("scroll filler {idx}"),
            agent_id: agent_id("engineer_22222222"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    }
    sync(&handle);
    assert!(!vt.screen_contains(
        120,
        "Message from @manager_11111111 (add-all-agent-overview for @engineer_22222222) to @engineer_22222222:"
    ));
}

/// Cross-session labels retain and emphasize the complete session-qualified
/// identity for grammar-valid controlled session identifiers.
#[test]
fn external_agent_messages_render_session_agent_labels() {
    let (_term, handle, vt) = setup(120, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&external_agent_message(
        "manager_11111111",
        "session-2",
        "engineer_22222222",
        "hello external",
    ));
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("msg-inbound-external")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("reviewer_33333333"),
            sender_session_id: Some(test_session_id("my_project-cafe-abc123")),
            recipient_id: agent_id("manager_11111111"),
            kind: tau_proto::AgentMessageKind::Message,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: "hello back".to_owned(),
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(
        120,
        "Message from @manager_11111111 to session-2/@engineer_22222222:"
    ));
    assert!(vt.screen_contains(
        120,
        "Message from my_project-cafe-abc123/@reviewer_33333333 to @manager_11111111:"
    ));
    let lines = vt.screen_text(120);
    let row = lines
        .iter()
        .position(|line| line.contains("my_project-cafe-abc123/@reviewer_33333333"))
        .expect("external message header row") as u16;
    let session_suffix = lines[row as usize]
        .find("project-cafe")
        .expect("session suffix") as u16;
    let remote_agent = lines[row as usize]
        .find("@reviewer_33333333")
        .expect("remote agent id") as u16;
    assert!(vt.cell_style(row, session_suffix).2);
    assert!(vt.cell_style(row, remote_agent).2);
}

/// Late display-name facts must replace an already rendered message label
/// without changing its body or leaving the stale block visible.
#[test]
fn late_agent_names_reproject_visible_message_blocks() {
    let (_term, handle, vt) = setup(100, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-messages", "all-full");
    let message = agent_message("agent-a", "agent-b", "semantic body");
    renderer.handle(&message);
    sync(&handle);
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b:"));

    let generation = vt.frame_generation();
    renderer.handle(&Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "review result".to_owned(),
        },
    ));
    vt.wait_for_frame_containing_after(
        generation,
        "Message from @agent-a to @agent-b (review result):",
    );
    assert!(vt.screen_contains(100, "Message from @agent-a to @agent-b (review result):"));
    assert!(!vt.screen_contains(100, "Message from @agent-a to @agent-b:"));
    assert!(vt.screen_contains(100, "semantic body"));
}

/// A selected model keeps a neutral quota chip once its provider has advertised
/// quota capability, even when the initial current-state snapshot is empty.
#[test]
fn selected_agent_empty_quota_state_renders_unknown() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("other/model".into()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model: model.clone(),
        ..agent_prompt_started("quota-empty-sp", "quota-empty")
    }));
    renderer.handle(&Event::HarnessProviderQuotaChanged(
        tau_proto::HarnessProviderQuotaChanged {
            provider: model.provider.clone(),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-empty")
                .expect("quota epoch"),
            sequence: 1,
            windows: Vec::new(),
            route_bindings: Vec::new(),
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(80, "Q?"));
}
/// Ensures standalone success and failure retire a watched side agent's
/// prompt fallback without waiting for an agent-stats snapshot.
#[test]
fn standalone_compaction_terminals_clear_hidden_watched_activity() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("manager".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("manager"),
            watched_agent_ids: vec![agent_id("engineer")],
            changed_agent_id: Some(agent_id("engineer")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));

    let mut started = standalone_compaction_started("ct-side-success", "ap-side-success");
    started.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentStandaloneCompactionStarted(started));
    let mut prompt = standalone_compaction_prompt_started("ap-side-success");
    prompt.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentPromptStarted(prompt));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 1);
    assert!(vt.screen_contains(100, "❓✨ @engineer"));
    assert!(vt.screen_contains(100, "@1"));

    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: None,
        compacted_input_tokens: None,
        agent_id: agent_id("engineer"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-side-success")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-side-success")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: Vec::new(),
    }));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 0);
    assert!(vt.screen_contains(100, "❓💤 @engineer"));
    assert!(!vt.screen_contains(100, "@1"));

    let mut started = standalone_compaction_started("ct-side-failed", "ap-side-failed");
    started.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentStandaloneCompactionStarted(started));
    let mut prompt = standalone_compaction_prompt_started("ap-side-failed");
    prompt.agent_id = agent_id("engineer");
    renderer.handle(&Event::AgentPromptStarted(prompt));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 1);
    assert!(vt.screen_contains(100, "❓✨ @engineer"));
    assert!(vt.screen_contains(100, "@1"));

    renderer.handle(&Event::AgentStandaloneCompactionFailed(
        AgentStandaloneCompactionFailed {
            agent_id: agent_id("engineer"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-side-failed")
                .expect("known-safe compaction transaction id"),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
        },
    ));
    sync(&handle);
    assert_eq!(renderer.active_side_agent_count_for_test(), 0);
    assert!(vt.screen_contains(100, "❓💤 @engineer"));
    assert!(!vt.screen_contains(100, "@1"));
}

/// Multiple active watched-agent blocks should keep a deterministic order
/// across refreshes even when prompt-start events arrive in a different order.
/// This prevents visually similar `watching` rows from flickering by swapping
/// positions between redraws.
#[test]
fn watched_agent_blocks_are_sorted_by_agent_id() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_b"), agent_id("engineer_a")],
            changed_agent_id: None,
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    for watched in ["engineer_b", "engineer_a"] {
        renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
            model_params: Some(tau_proto::ModelParams::default()),
            outer_turn_id: None,

            session_id: test_session_id("s1"),
            agent_id: agent_id(watched),
            agent_prompt_id: test_agent_prompt_id(format!("ap-{watched}-0")),
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::Inference,
            originator: tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("__harness__")
                    .expect("test identifier must satisfy its grammar"),
                query_id: format!("delegate-{watched}"),
            },
            ctx_id: None,
        }));
    }
    sync(&handle);

    let screen = vt.screen_text(100);
    let first = screen
        .iter()
        .position(|line| line.contains("❓✨ @engineer_a"))
        .expect("engineer_a running row");
    let second = screen
        .iter()
        .position(|line| line.contains("❓✨ @engineer_b"))
        .expect("engineer_b running row");
    assert!(
        first < second,
        "watched-agent rows should be sorted by agent id: {screen:?}"
    );
}

/// Keeps turn-stat Σ totals with the response-owning agent while retaining a
/// separate flat total that cold-attach replay can reconstruct from every
/// durable provider terminal.
#[test]
fn turn_stats_and_session_stats_keep_token_scopes_separate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-turn-stats", "true");
    renderer.switch_agent("worker-1".to_owned());

    let mut first = finished_response_with_usage(
        "worker-1-sp-0",
        "worker-1",
        100,
        50,
        10,
        "first worker response",
    );
    first.usage.as_mut().expect("usage").stats.total = tau_proto::TokenUsageCounts {
        sent_tokens: 100,
        cached_tokens: 50,
        received_tokens: 10,
        ..Default::default()
    };
    renderer.handle(&Event::ProviderResponseFinished(first));

    let mut other_agent = finished_response_with_usage(
        "worker-2-sp-0",
        "worker-2",
        700,
        600,
        80,
        "other worker response",
    );
    other_agent.usage.as_mut().expect("usage").stats.total = tau_proto::TokenUsageCounts {
        sent_tokens: 800,
        cached_tokens: 650,
        received_tokens: 90,
        ..Default::default()
    };
    renderer.handle(&Event::ProviderResponseFinished(other_agent));

    let mut second = finished_response_with_usage(
        "worker-1-sp-1",
        "worker-1",
        50,
        20,
        5,
        "second worker response",
    );
    second.usage.as_mut().expect("usage").stats.total = tau_proto::TokenUsageCounts {
        sent_tokens: 850,
        cached_tokens: 670,
        received_tokens: 95,
        ..Default::default()
    };
    renderer.handle(&Event::ProviderResponseFinished(second));
    sync(&handle);

    assert!(vt.screen_contains(80, "Σ↑70/150 ↓15"));
    assert!(!vt.screen_contains(80, "Σ↑670/850 ↓95"));
    assert_eq!(
        renderer.session_token_stats_text(),
        "session token totals: ↑670/850 ↓95"
    );
    renderer.show_session_token_stats();
    sync(&handle);
    assert!(vt.screen_contains(80, "session token totals: ↑670/850 ↓95"));
}

/// Foreground-ownership fail-stop must bypass generic stderr reporting because
/// the process cannot confirm that it owns the terminal.
#[test]
fn foreground_ownership_failure_suppresses_top_level_terminal_report() {
    let error = CliError::ForegroundOwnershipUnconfirmed("restore failed".to_owned());

    assert!(!error.should_report_to_terminal());
}

/// Static CLI mouse configuration must reach the terminal layer unchanged so
/// the raw terminal can select its ownership-safe capture behavior once.
#[test]
fn static_mouse_setting_propagates_to_terminal_options() {
    let mut settings = path_tau_config_settings::CliSettings::built_in();
    settings.mouse = false;

    assert_eq!(
        terminal_options_from_settings(&settings),
        tau_cli_term::TerminalOptions {
            cursor_shape: tau_cli_term::CursorShape::Bar,
            mouse: false,
        }
    );
}

/// Ensures ANSI emission preserves user/assistant base styling through
/// structural Markdown, including wrapping and the process-wide no-color mode.
#[test]
fn virtual_terminal_markdown_structure_inherits_transcript_colors() {
    let theme = tau_themes::Theme::parse(
        r##"{
            styles: {
                "user.prompt": { fg: "#f0f0f0", bg: "#101010" },
                "agent.response": { fg: "#00d0d0", bg: "#101010" },
                "markdown.heading": { bold: true },
                "markdown.list.marker": { bold: true },
            }
        }"##,
    )
    .expect("valid VT Markdown theme");
    let (_term, handle, vt) = setup(12, 8);
    handle.print_output(
        "markdown-user",
        markdown_block(&theme, tau_themes::names::USER_PROMPT, "# User\n"),
    );
    handle.print_output(
        "markdown-assistant",
        markdown_block(
            &theme,
            tau_themes::names::AGENT_RESPONSE,
            "12. assistant text wraps\n",
        ),
    );
    sync(&handle);

    let rows = vt.screen_text(12);
    let user_row = rows
        .iter()
        .position(|row| row.contains("# User"))
        .expect("user heading row") as u16;
    let user_offset = rows[user_row as usize]
        .find("# User")
        .expect("user heading column");
    let user_col = rows[user_row as usize][..user_offset].chars().count() as u16;
    let assistant_row = rows
        .iter()
        .position(|row| row.contains("12."))
        .expect("assistant list row") as u16;
    let assistant_offset = rows[assistant_row as usize]
        .find("12.")
        .expect("assistant list column");
    let assistant_col = rows[assistant_row as usize][..assistant_offset]
        .chars()
        .count() as u16;
    let continuation_row = rows
        .iter()
        .position(|row| row.contains("t text wraps"))
        .expect("wrapped assistant continuation") as u16;
    let continuation_offset = rows[continuation_row as usize]
        .find("t text wraps")
        .expect("wrapped assistant continuation column");
    let continuation_col = rows[continuation_row as usize][..continuation_offset]
        .chars()
        .count() as u16;
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    assert_eq!(
        vt.cell_style(user_row, user_col),
        if no_color {
            (vt100::Color::Default, vt100::Color::Default, true)
        } else {
            (
                vt100::Color::Rgb(0xf0, 0xf0, 0xf0),
                vt100::Color::Rgb(0x10, 0x10, 0x10),
                true,
            )
        },
        "rows={rows:?}, user row={user_row}, col={user_col}"
    );
    assert_eq!(
        vt.cell_style(assistant_row, assistant_col),
        if no_color {
            (vt100::Color::Default, vt100::Color::Default, true)
        } else {
            (
                vt100::Color::Rgb(0x00, 0xd0, 0xd0),
                vt100::Color::Rgb(0x10, 0x10, 0x10),
                true,
            )
        }
    );
    assert_eq!(
        vt.cell_style(continuation_row, continuation_col),
        if no_color {
            (vt100::Color::Default, vt100::Color::Default, false)
        } else {
            (
                vt100::Color::Rgb(0x00, 0xd0, 0xd0),
                vt100::Color::Rgb(0x10, 0x10, 0x10),
                false,
            )
        }
    );
}

/// Successful compaction lifecycle notices retain their info style and gain the
/// semantic notice marker.
#[test]
fn compaction_lifecycle_notice_uses_info_style() {
    let theme = cli_test_theme();
    let lifecycle = render_harness_notice(
        &theme,
        &tau_proto::HarnessNotice::diagnostic(
            tau_proto::notice_kind::HARNESS_NOTICE,
            "Starting compaction request cr-35-0 for reviewer-sOqj (ct-35)",
            tau_proto::NoticeLevel::Info,
        ),
    );

    assert_eq!(lifecycle.content.spans()[0].style.fg, Some(Color::Blue));
    assert_eq!(
        lifecycle
            .content
            .spans()
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "□ Starting compaction request cr-35-0 for reviewer-sOqj (ct-35)"
    );
}

/// Output-limit warnings must distinguish the one authorized continuation from
/// each terminal truncation shape without presenting an incomplete tool call as
/// executable.
#[test]
fn renderer_output_length_diagnostics_match_disposition_and_visible_output() {
    let cases = [
        (
            vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "retained private reasoning".to_owned(),
            })],
            tau_proto::OutputLengthDisposition::ContinuationPlanned {
                outer_turn_id: tau_proto::AgentOuterTurnId::for_prompt(&test_agent_prompt_id(
                    "length-planned",
                )),
                successor_agent_prompt_id: test_agent_prompt_id("length-successor"),
                ordinal: 1,
                limit: 1,
            },
            "Output limit reached; continuing once from retained reasoning.",
            None,
        ),
        (
            vec![ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                kind: tau_proto::ReasoningTextKind::Full,
                text: "terminal private reasoning".to_owned(),
            })],
            tau_proto::OutputLengthDisposition::None,
            "Model reached its output-token limit before completing the turn. No assistant answer or executable tool call was produced.",
            None,
        ),
        (
            vec![assistant_message_item("visible partial answer")],
            tau_proto::OutputLengthDisposition::None,
            "Model reached its output-token limit before completing the turn. The displayed response may be incomplete.",
            Some("visible partial answer"),
        ),
        (
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "truncated-call".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: Some("{".to_owned()),
                responses_envelope: None,
            })],
            tau_proto::OutputLengthDisposition::None,
            "Model reached its output-token limit while producing a tool call. The incomplete call was not executed.",
            None,
        ),
    ];

    for (index, (output_items, disposition, warning, visible_output)) in
        cases.into_iter().enumerate()
    {
        let (_term, handle, vt) = setup(160, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.apply_setting("show-thinking", "false");
        let mut finished = finished_response(&format!("length-{index}"), output_items);
        finished.stop_reason = ProviderStopReason::Length;
        finished.output_length_disposition = disposition;
        renderer.handle(&Event::ProviderResponseFinished(finished));
        sync(&handle);

        assert!(
            eventually_screen_contains(&vt, 160, warning),
            "missing output-limit warning in case {index}: {:?}",
            vt.screen_text(160)
        );
        if let Some(output) = visible_output {
            assert!(vt.screen_contains(160, output));
        }
        assert!(
            !vt.screen_contains(160, "retained private reasoning")
                && !vt.screen_contains(160, "terminal private reasoning")
        );
        assert_eq!(renderer.test_active_tool_count(), 0);
    }
}

/// Ensures live, final, and cold-replayed assistant tables share the display
/// projection while provider events and editor context retain the raw Markdown.
#[test]
fn markdown_table_response_events_preserve_raw_text_and_replay_projection() {
    let source = concat!(
        "| Scope | Effort |\n",
        "| --- | ---: |\n",
        "| Formed 7-guardian federation, connected gateway, configured/advertising FLIP, log paths, working `fman-cli` | **4–7 engineer-days** |\n",
        "| Complete FI-requested/funded liquidity and register the gateway in federation consensus | **8–15 days total** |\n",
        "| Real `cloud-fman-telemetry` collection | **+3–6 days** |\n",
        "| Throwaway demo script | **2–3 days**, but brittle |\n",
    );
    let finished = finished_response("sp-table", vec![assistant_message_item(source)]);
    let raw_finished = finished.clone();

    let (_term, handle, vt) = setup(160, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-table", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-table"),
            source,
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(160, "◇ | Scope"));
    assert!(vt.screen_contains(160, "                    Effort |"));

    renderer.handle(&Event::ProviderResponseFinished(finished.clone()));
    sync(&handle);
    assert_eq!(
        finished, raw_finished,
        "rendering must not mutate event text"
    );
    assert!(vt.screen_contains(160, "◆ | Scope"));
    let editor_context = renderer.editor_context();
    assert_eq!(
        editor_context
            .lock()
            .expect("editor context")
            .last_response
            .as_deref(),
        Some(source)
    );

    let (_cold_term, cold_handle, cold_vt) = setup(160, 40);
    let mut cold_renderer = EventRenderer::new(
        cold_handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let cold_generation = cold_vt.frame_generation();
    cold_renderer.handle(&Event::ProviderResponseFinished(finished));
    let cold_frame = cold_vt.wait_for_frame_containing_after(cold_generation, "◆ | Scope");
    let cold_frames = cold_vt.frames.0.lock().expect("frames");
    let settled = cold_frames[cold_frame - 1].join("\n");
    assert!(settled.contains("                    Effort |"));
    assert!(settled.contains("\n> "), "{settled}");
    for frame in &cold_frames[cold_generation..cold_frame] {
        let text = frame.join("\n");
        assert!(
            !text.contains("◆ | Scope") || text.contains("\n> "),
            "cold replay published content without final status: {frame:?}"
        );
    }
    drop(cold_frames);
    assert_eq!(
        cold_renderer
            .editor_context()
            .lock()
            .expect("cold editor")
            .last_response
            .as_deref(),
        Some(source)
    );
    assert!(cold_vt.screen_contains(160, "◆ | Scope"));
    assert!(cold_vt.screen_contains(160, "                    Effort |"));
}

/// Ensures streaming Markdown styles are applied as each line completes, so a
/// later blank-line seal does not restyle already-hidden scrollback and force a
/// full redraw.
#[test]
fn live_markdown_blank_line_seal_does_not_full_redraw_scrollback() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-md", "s1",
    )));
    for index in 0..24 {
        renderer.handle(&Event::ProviderResponseUpdated(
            provider_response_delta_update(
                test_agent_prompt_id("sp-md"),
                format!("*line {index}*\n"),
                None,
                tau_proto::PromptOriginator::User,
            ),
        ));
    }
    sync(&handle);
    assert!(vt.screen_contains(80, "*line 23*"));
    let full_render_count = handle.full_render_count();

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-md"),
            "\n",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_render_count);
}

/// The compact delivered-message shape wraps naturally at narrow terminal
/// widths, preserves its immediate body, and styles only publisher provenance
/// as inline code.
#[test]
fn compact_message_fact_wraps_at_narrow_width_with_code_styled_publisher() {
    let (_term, handle, vt) = setup(28, 20);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer
        .agent_navigation()
        .lock()
        .expect("agent navigation lock")
        .mark_live("selected-agent");
    renderer.switch_agent("selected-agent".to_owned());
    renderer.handle(&Event::MessageDelivered(tau_proto::MessageDelivered::new(
        tau_proto::MessagePublisherId::parse("fedi-slack")
            .expect("canonical publisher id must satisfy the identifier grammar"),
        tau_proto::MessageAgentTarget::new("selected-agent"),
        tau_proto::MessageFactId::new("slack-message:opaque"),
        tau_proto::MessageParty {
            stable_id: "slack-sender:opaque".to_owned(),
            display_name: Some("Dawid (dpc)".to_owned()),
            sender_auth: None,
        },
        Some(tau_proto::MessageConversation {
            stable_id: "D123".to_owned(),
            display_name: Some("dpc-dm".to_owned()),
            alias: None,
        }),
        "Can you see this?",
    )));
    sync(&handle);

    let rows = vt.screen_text(28);
    assert!(rows.iter().any(|row| row.contains("External `fedi-slack`")));
    assert!(rows.iter().any(|row| row.contains("Can you see this?")));
    assert!(!rows.iter().any(|row| row.contains("slack-message:opaque")));
    assert!(!rows.iter().any(|row| row.contains("slack-sender:opaque")));
    assert!(!rows.iter().any(|row| row.contains("D123")));
    assert!(!rows.iter().any(|row| row.contains("Text:")));

    if !std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        let publisher_row = rows
            .iter()
            .position(|row| row.contains("`fedi-slack`"))
            .expect("publisher row") as u16;
        let publisher_text = &rows[publisher_row as usize];
        let publisher_byte = publisher_text.find("fedi-slack").expect("publisher column");
        let publisher_col = publisher_text[..publisher_byte].chars().count() as u16;
        let external_byte = publisher_text.find("External").expect("heading column");
        let external_col = publisher_text[..external_byte].chars().count() as u16;
        assert_ne!(
            vt.cell_style(publisher_row, publisher_col).0,
            vt.cell_style(publisher_row, external_col).0,
            "publisher should use the inline-code foreground; rows={rows:?}"
        );
    }
}

/// The interactive renderer preserves one directed tree notice as ordered,
/// distinct lines, including anchor spacing and the selected-head marker.
#[test]
fn tree_notice_renders_multiline_result_without_reformatting() {
    let (_term, handle, vt) = setup(240, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.toggle_verbose_mode();
    renderer.apply_setting("notice-level", "critical");
    let expected = crate::test_support::TREE_PREVIEW_PARITY_NOTICE
        .lines()
        .collect::<Vec<_>>();

    renderer.handle(&Event::HarnessNotice(tau_proto::HarnessNotice {
        kind: tau_proto::notice_kind::HARNESS_NOTICE.into(),
        message: crate::test_support::TREE_PREVIEW_PARITY_NOTICE.into(),
        level: tau_proto::NoticeLevel::Info,
        purpose: tau_proto::NoticePurpose::Response,
    }));
    sync(&handle);

    let rows = vt.screen_text(240);
    let mut previous_row = None;
    let mut rendered_tree_rows = Vec::new();
    for line in &expected {
        let row = rows
            .iter()
            .position(|row| row.contains(line))
            .unwrap_or_else(|| panic!("missing exact tree row {line:?} in {rows:?}"));
        assert!(
            previous_row.is_none_or(|previous| previous < row),
            "tree rows are out of order: {rows:?}"
        );
        previous_row = Some(row);
        rendered_tree_rows.push(
            rows[row]
                .strip_prefix("□ ")
                .unwrap_or(&rows[row])
                .trim_end(),
        );
    }
    assert_eq!(rendered_tree_rows, expected);
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("before first prompt") || row.contains("before prompt"))
            .count(),
        expected.len()
    );
}

#[test]
fn thinking_renders_as_separate_block_above_response() {
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
    sync(&handle);

    // Thinking arrives before the response text. Both should be
    // visible simultaneously, with thinking above response.
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            String::new(),
            Some("planning the answer".into()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "planning the answer"),
        "thinking block should be live: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "actual answer",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "actual answer"));
    assert!(vt.screen_contains(80, "planning the answer"));

    // Order matters even during live streaming: thinking should
    // render ABOVE the response, not below it.
    let live = vt.screen_text(80);
    let live_thinking = live
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("live thinking missing: {live:?}"));
    let live_response = live
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("live response missing: {live:?}"));
    assert!(
        live_thinking < live_response,
        "live thinking should render above live response (thinking @ {live_thinking}, response @ {live_response}); lines: {live:?}",
    );

    // On finish both stick in history.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("actual answer")],
    )));
    sync(&handle);
    // Thinking should appear above the response in the history.
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("thinking should remain in history: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("response should remain in history: {lines:?}"));
    assert!(
        thinking_row < response_row,
        "thinking should render above response (thinking @ {thinking_row}, response @ {response_row}); lines: {lines:?}",
    );
}

#[test]
fn no_thinking_block_when_summary_absent() {
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
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("hello")],
    )));
    sync(&handle);
    // Just make sure we didn't crash and the response is visible.
    assert!(vt.screen_contains(80, "hello"));
}

#[test]
fn streaming_indicator_appends_during_updates() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "…"),
        "prompt creation should not render provider-progress ellipsis before provider bytes: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello …"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("Hello")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "Hello …"));
}

#[test]
fn render_empty_provider_response_placeholder_without_context_item() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: the empty-response notice is a CLI presentation fallback, not
    // a provider-authored assistant message inserted into durable output_items.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-empty",
        Vec::new(),
    )));
    sync(&handle);

    assert!(vt.screen_contains(80, "(provider returned an empty response)"));
}

#[test]
fn render_provider_error_from_non_context_field() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let mut finished = finished_response("sp-error", Vec::new());
    finished.stop_reason = ProviderStopReason::Error;
    finished.error = Some("LLM error: boom".to_owned());

    // Regression: provider/runtime failures should be visible to the user but
    // must not be represented as assistant output_items that replay into the
    // next prompt.
    renderer.handle(&Event::ProviderResponseFinished(finished));
    sync(&handle);

    assert!(vt.screen_contains(80, "LLM error: boom"));
}

/// Ensures failed and terminated standalone lifecycles remove their private
/// progress marker without clearing or rendering any compactor output.
#[test]
fn standalone_compaction_terminal_failures_clear_private_progress() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        standalone_compaction_started("ct-failed", "ap-failed"),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-failed"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionFailed(
        AgentStandaloneCompactionFailed {
            agent_id: agent_id("main"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-failed")
                .expect("known-safe compaction transaction id"),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "compact failed"));
    assert!(!vt.screen_contains(100, "Compacting…"));
    assert!(!renderer.agent_has_active_prompt_for_test("main"));
    assert!(!renderer.main_agent_turn_active_for_test());

    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-terminated"),
    ));
    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_id: agent_id("main"),
        agent_prompt_id: test_agent_prompt_id("ap-terminated"),
        reason: AgentPromptTerminationReason::Canceled,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(100, "compact stopped"));
    assert!(!vt.screen_contains(100, "Compacting…"));
}

/// Ensures normal inference continues to render provider deltas when an
/// unrelated malformed standalone lifecycle does not carry its compact
/// operation.
#[test]
fn malformed_standalone_lifecycle_does_not_hide_inference_stream() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    let mut malformed = standalone_compaction_started("ct-malformed", "ap-inference");
    malformed.operation = tau_proto::PromptOperation::Inference;
    renderer.handle(&Event::AgentStandaloneCompactionStarted(malformed));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "ap-inference",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("ap-inference"),
            "ordinary inference answer",
            Some("ordinary inference reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    assert!(vt.screen_contains(100, "ordinary inference answer"));
    assert!(vt.screen_contains(100, "ordinary inference reasoning"));
}

#[test]
fn render_provider_compaction_update_as_compact_progress() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-compact"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: Some(tau_proto::ProviderResponseCompactionUpdate {
            status: tau_proto::ProviderResponseCompactionStatus::Started,
            original_input_tokens: Some(226_200),
            compacted_input_tokens: None,
        }),
        status: None,
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let progress = format!("compact #226.2k {}", tau_proto::PROGRESS_INDICATOR_TEXT);
    assert!(vt.screen_contains(80, &progress));
    assert!(!vt.screen_contains(80, "compacting"));
}

#[test]
fn render_provider_compaction_item_when_response_finishes() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Regression: a manual trigger event only records the user request. The UI
    // should show compaction after the provider returns the durable compaction
    // item, which means server-side compaction has actually completed.
    let mut finished = finished_response(
        "sp-compact",
        vec![ContextItem::Compaction(OpaqueProviderItem::new(
            CborValue::Map(vec![]),
        ))],
    );
    finished.compaction_original_input_tokens = Some(226_200);
    finished.compaction_compacted_input_tokens = Some(4_500);
    renderer.handle(&Event::ProviderResponseFinished(finished));
    sync(&handle);

    assert!(vt.screen_contains(80, "compact #226.2k ok: #4.5k"));
    assert!(!vt.screen_contains(80, "compacted"));
}

/// Active tools, queued prompts, and watched engineers must keep their category
/// order regardless of whether tools or watches arrive first. This prevents a
/// later update from moving a lower-priority activity row above user work.
#[test]
fn mixed_live_activity_blocks_keep_category_and_internal_order() {
    for tools_arrive_first in [true, false] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.switch_agent("parent_1".to_owned());

        let render_tools = |renderer: &mut EventRenderer| {
            for call_id in ["read_one", "read_two"] {
                renderer.handle(&Event::ToolStarted(tau_proto::ToolStarted {
                    call_id: call_id.into(),
                    tool_name: tau_proto::ToolName::new(call_id),
                    arguments: CborValue::Null,
                    agent_id: agent_id("parent_1"),
                    originator: tau_proto::PromptOriginator::User,
                }));
            }
        };
        let render_watches = |renderer: &mut EventRenderer| {
            renderer.handle(&Event::AgentWatchesUpdated(
                tau_proto::AgentWatchesUpdated {
                    session_id: test_session_id("s1"),
                    watcher_id: agent_id("parent_1"),
                    watched_agent_ids: vec![agent_id("engineer_b"), agent_id("engineer_a")],
                    changed_agent_id: None,
                    cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
                },
            ));
        };

        if tools_arrive_first {
            render_tools(&mut renderer);
        } else {
            render_watches(&mut renderer);
        }
        for text in ["queued-one", "queued-two"] {
            renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
                text: text.to_owned(),
                agent_id: agent_id("parent_1"),
                message_class: tau_proto::PromptMessageClass::User,
            }));
        }
        if tools_arrive_first {
            render_watches(&mut renderer);
        } else {
            render_tools(&mut renderer);
        }

        // Refresh from a differently ordered update to exercise both reordering
        // existing watched rows and the mixed-category anchor placement.
        renderer.handle(&Event::AgentWatchesUpdated(
            tau_proto::AgentWatchesUpdated {
                session_id: test_session_id("s1"),
                watcher_id: agent_id("parent_1"),
                watched_agent_ids: vec![agent_id("engineer_a"), agent_id("engineer_b")],
                changed_agent_id: Some(agent_id("engineer_a")),
                cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
            },
        ));
        sync(&handle);

        let screen = vt.screen_text(100);
        let positions = [
            "read_one 0s pending",
            "read_two 0s pending",
            "queued-one (queued)",
            "queued-two (queued)",
            "❓💤 @engineer_a",
            "❓💤 @engineer_b",
        ]
        .map(|needle| {
            screen
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing `{needle}` in {screen:?}"))
        });
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "expected tools, queued prompts, then sorted watched engineers: {screen:?}"
        );
    }
}

/// A harness-originated message call must leave the live-progress set when its
/// canonical provider terminal arrives, even when the renderer does not receive
/// the redundant transient `tool.result` projection.
#[test]
fn provider_terminal_finishes_harness_originated_message_progress() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("__harness__").expect("extension name"),
        query_id: "peer-auto-start".to_owned(),
    };
    let mut started = match tool_started(
        "message-call",
        "message",
        CborValue::Map(vec![(
            CborValue::Text("recipient_id".into()),
            CborValue::Text("engineer".into()),
        )]),
    ) {
        Event::ToolStarted(started) => started,
        _ => unreachable!("tool_started helper always returns ToolStarted"),
    };
    started.agent_id = agent_id("coordinator");
    started.originator = originator.clone();
    renderer.handle(&Event::ToolStarted(started));
    renderer.handle(&Event::ToolProgress(tau_proto::ToolProgress {
        call_id: "message-call".into(),
        tool_name: tau_proto::ToolName::new("message"),
        message: None,
        progress: None,
        display: Some(tau_proto::ToolUseState {
            args: "engineer".into(),
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
            ..Default::default()
        }),
    }));
    renderer.handle(&Event::ProviderToolResult(tau_proto::ToolResult {
        presentation: Default::default(),
        call_id: "message-call".into(),
        tool_name: tau_proto::ToolName::new("message"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("Message sent".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "message engineer"));
}

#[test]
fn streaming_block_does_not_duplicate_on_finish() {
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
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "hello!",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("hello!")],
    )));
    sync(&handle);

    // Count how many rows contain "hello!".
    let count = vt
        .screen_text(80)
        .iter()
        .filter(|r| r.contains("hello!"))
        .count();
    assert_eq!(
        count,
        1,
        "response should appear exactly once, got {count}: {:?}",
        vt.screen_text(80)
    );
}
/// Workdir get/set labels remain accessible, prefix-safe, and compact in plain
/// and styled output.
#[test]
fn workdir_result_modes_render_consistently() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let long_path = format!("/{}", "segment/".repeat(10));
    let display = ToolUseState {
        mode: "set".into(),
        args: long_path,
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("project_a__workdir", &display);
    let theme = cli_test_theme();
    let block = render_tool_block(&theme, &rendered);
    let cells = priority_header_cells(&block, 120);
    let plain: String = cells.iter().map(|cell| cell.ch).collect();
    let mode_start = plain.find(" set ").expect("structural set mode") + 1;
    let mode_style = cells[mode_start].style;
    assert!(plain.contains("/segment/"));
    assert!(plain.contains('┄'));
    assert!(plain.trim_end().ends_with(" ok"));
    assert_eq!(
        mode_style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_MODE)
    );
    assert_ne!(
        mode_style,
        cells[plain.find("/segment").expect("compacted path")].style
    );
}

/// Action output begins with the notice marker while retaining the dedicated
/// styles that distinguish actionable approval identifiers and labels.
#[test]
fn render_action_output_block_highlights_approval_ids_and_labels() {
    let theme = cli_test_theme();
    let block = render_action_output_block(
        &theme,
        "Incoming approval 7\nstatus: pending\n8 account=personal folder=INBOX\n",
    );
    let spans = block.content.spans();
    let id_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ID);
    let label_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_LABEL);
    let marker_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROMPT_MARKER_SUBMITTED);

    let heading_id = spans
        .iter()
        .find(|span| span.text == "7")
        .expect("heading approval id span");
    let row_id = spans
        .iter()
        .find(|span| span.text == "8")
        .expect("list row approval id span");
    let status_label = spans
        .iter()
        .find(|span| span.text == "status:")
        .expect("status label span");
    let account_label = spans
        .iter()
        .find(|span| span.text == "account=")
        .expect("key-value label span");

    assert_eq!(heading_id.style, id_style);
    assert_eq!(row_id.style, id_style);
    assert_eq!(status_label.style, label_style);
    assert_eq!(account_label.style, label_style);
    assert_eq!(spans[0].text, "□ ");
    assert_eq!(spans[0].style, marker_style);
}

/// Action errors begin with the same notice marker without flattening their
/// identifier and diagnostic styles into the generic feedback style.
#[test]
fn render_action_error_block_uses_action_error_styles() {
    let theme = cli_test_theme();
    let block = render_action_error_block(&theme, "7", "invalid input");
    let spans = block.content.spans();
    let id_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ID);
    let error_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ERROR);
    let marker_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROMPT_MARKER_SUBMITTED);

    assert_eq!(spans[0].text, "□ ");
    assert_eq!(spans[0].style, marker_style);
    assert_eq!(spans[1].text, "7");
    assert_eq!(spans[1].style, id_style);
    assert_eq!(spans[3].text, "invalid input");
    assert_eq!(spans[3].style, error_style);
}

#[test]
fn render_turn_stats_block_uses_dedicated_styles() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        prompt_cached_tokens: 900,
        prompt_cache_read_ceiling_tokens: Some(1_000),
        response_received_tokens: 42,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 2_000,
                cached_tokens: 1_000,
                received_tokens: 100,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ90%");
    assert!(spans[0].style.bold);
    assert_eq!(spans[0].style.fg, Some(Color::DarkGrey));
    assert_eq!(spans[1].text, " 900/1k");
    assert!(!spans[1].style.bold);
    assert_eq!(spans[1].style.fg, Some(Color::Red));
    let sigma = spans
        .iter()
        .find(|span| span.text == " Σ")
        .expect("sigma span is rendered");
    assert!(sigma.style.bold);
    assert_eq!(sigma.style.fg, Some(Color::DarkGrey));
}

#[test]
fn render_turn_stats_block_warns_for_exact_99_percent_efficiency() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_456,
        prompt_cache_read_ceiling_tokens: Some(19_500),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 40_100,
                cached_tokens: 19_456,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 19_500,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ99%");
    assert_eq!(spans[1].text, " 19.4k/19.5k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkYellow));
}

#[test]
fn render_turn_stats_block_warns_cache_hit_above_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_100,
        prompt_cache_read_ceiling_tokens: Some(10_000),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 20_100,
                cached_tokens: 9_100,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ91%");
    assert_eq!(spans[1].text, " 9.1k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkYellow));
}

#[test]
fn render_turn_stats_block_highlights_cache_hit_at_or_below_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_000,
        prompt_cache_read_ceiling_tokens: Some(10_000),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 20_100,
                cached_tokens: 9_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        ..Default::default()
    };
    let block =
        render_turn_stats_block(&cli_test_theme(), &usage, Some(&previous_usage), None, None);
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ90%");
    assert_eq!(spans[1].text, " 9k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::Red));
}

#[test]
fn streaming_block_handles_each_trailing_case() {
    let theme = cli_test_theme();
    let cases = [
        ("", "…"),
        ("Hello", "Hello …"),
        ("Hello ", "Hello …"),
        ("Hello\t", "Hello\t…"),
        ("line\n", "line\n…"),
        ("line\n  ", "line\n  …"),
    ];
    for (input, expected) in cases {
        let block = streaming_block(&theme, tau_themes::names::AGENT_RESPONSE, input);
        let actual: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(actual, expected, "input was {input:?}");
    }
}

/// Emoji (wide characters) in responses must not corrupt the
/// layout. Each emoji occupies 2 terminal columns; if we count
/// them as 1, text after the emoji shifts right and wraps
/// incorrectly.
#[test]
fn emoji_in_response_renders_correctly() {
    let (_term, handle, vt) = setup(40, 24);
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
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // Response with emoji followed by text on next line.
    let response = "Hello! 👋\n\nHow can I help you today?";
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            response,
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(response)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);

    // "Hello! 👋" should be on its own line, not merged with the
    // next line.
    assert!(
        vt.screen_contains(40, "Hello!"),
        "emoji line missing, got: {:?}",
        text
    );
    // The text after \n\n should start at column 0, not offset.
    assert!(
        text.iter().any(|r| r.starts_with("How can I help")),
        "text after emoji should start at column 0, got: {:?}",
        text
    );
    // Prompt must be visible.
    assert!(
        vt.screen_contains(40, "> "),
        "prompt missing, got: {:?}",
        text
    );
}

/// Multiple emoji in a single line must not cause column drift.
#[test]
fn multiple_emoji_no_column_drift() {
    let (_term, handle, vt) = setup(40, 24);
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
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // 3 emoji = 6 columns + "end" = 9 columns total.
    let response = "🎉🎊🎈end\nnext line here";
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(response)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);
    // "next line here" should start at column 0.
    assert!(
        text.iter().any(|r| r.starts_with("next line here")),
        "line after emoji should start at col 0, got: {:?}",
        text
    );
}

/// Replacing a long streaming block with its final settled output
/// must not leave stale partial lines behind, even when the live
/// block overflowed the viewport while streaming.
#[test]
fn overflowing_stream_replaced_cleanly_on_finish() {
    let (_term, handle, vt) = setup(40, 5);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "overflow please".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    let partial = "stream 0\nstream 1\nstream 2\nstream 3\nPARTIAL ONLY";
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            partial,
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(40, "PARTIAL ONLY"),
        "partial overflowed response should be visible before finish, got: {:?}",
        vt.screen_text(40)
    );

    let final_text = "final 0\nfinal 1\nfinal 2";
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(final_text)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);
    assert!(
        vt.screen_contains(40, "final 1"),
        "final response missing, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "final 2"),
        "final response tail missing, got: {:?}",
        text
    );
    assert!(
        !vt.screen_contains(40, "PARTIAL ONLY"),
        "stale partial content should be gone, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "> "),
        "prompt should remain visible, got: {:?}",
        text
    );
}
/// Ensures the CLI uses the running harness announcement as the custom-prompt
/// source of truth, which keeps reattached UIs aligned with daemon startup
/// overrides instead of re-reading local config files.
#[test]
fn renderer_tracks_custom_prompts_from_harness_event() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let prompt = tau_proto::HarnessCustomPrompt {
        id: "review".to_owned(),
        text: "Review this patch carefully".to_owned(),
    };

    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: Vec::new(),
        groups: Vec::new(),
        custom_prompts: vec![prompt.clone()],
    }));

    let prompts = renderer.custom_prompts().lock().expect("prompts").clone();
    assert_eq!(prompts, vec![prompt]);
}

/// Classifies only the static commands rendered locally by the interactive CLI.
#[test]
fn known_static_commands_are_identified_for_history_rendering() {
    assert!(is_known_static_command(":model engineer"));
    assert!(is_known_static_command(":set show-tools compact"));
    assert!(is_known_static_command(":theme dpc"));
    assert!(is_known_static_command(":session-stats"));
    assert!(is_known_static_command(":debug-show-ui-event-stats"));
    assert!(is_known_static_command(":debug-show-event-stats std-shell"));
    assert!(is_known_static_command(":quit"));
    assert!(is_known_static_command(":agent"));
    assert!(is_known_static_command(":agent switch worker-1"));
    assert!(is_known_static_command(":agent suspend"));
    assert!(is_known_static_command(":agent resume worker-1"));
    assert!(is_known_static_command(":agent new"));
    assert!(is_known_static_command(":new"));
    assert!(is_known_static_command(":name Current worker"));
    assert!(is_known_static_command(":suspend"));
    assert!(is_known_static_command(":resume"));
    assert!(is_known_static_command(":new now"));
    assert!(is_known_static_command(":session new"));
    assert!(is_known_static_command(":version"));
    assert!(is_known_static_command(":version now"));
    assert!(is_known_static_command(":skill jujutsu"));
    assert!(is_known_static_command(":skill:jujutsu args"));
    assert!(!is_known_static_command("/skillx jujutsu"));
    assert!(!is_known_static_command("hello :model engineer"));
}

/// Ensures live UI submissions and durable replay render the same Markdown
/// attributes while retaining the exact raw prompt text for routing and
/// history.
#[test]
fn submitted_prompt_markdown_styles_match_live_and_replay_without_mutating_raw_text() {
    let theme = tau_themes::Theme::parse(
        r##"{
            styles: {
                "user.prompt": { fg: "#f0f0f0", bg: "#101010" },
                "markdown.strong": { bold: true },
                "markdown.emphasis": { italic: true },
                "markdown.code": { fg: "#00d000" },
                "markdown.link": { fg: "#d00000", bold: true },
            }
        }"##,
    )
    .expect("valid submitted-prompt Markdown theme");
    let source = "**strong** _emphasis_ `code` [link](https://example.test/docs)".to_owned();

    for replayed in [false, true] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            theme.clone(),
        );
        let event = if replayed {
            Event::AgentPromptSubmitted(AgentPromptSubmitted {
                inference_activation: false,
                agent_id: agent_id("main"),
                text: source.clone(),
                trusted_internal_spans: Vec::new(),
                message_class: tau_proto::PromptMessageClass::User,
                internal_kind: None,
                originator: tau_proto::PromptOriginator::User,
                submission_source: tau_proto::PromptSubmissionSource::HumanUi,
                display_name: None,
                ctx_id: None,
            })
        } else {
            Event::UiPromptSubmitted(UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: source.clone(),
                agent_id: agent_id("main"),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            })
        };
        renderer.handle(&event);
        sync(&handle);

        assert_eq!(
            renderer.last_submitted_user_prompt_text_for_test(),
            Some(source.as_str()),
            "the {} projection must retain exact raw prompt bytes",
            if replayed { "replayed" } else { "live" }
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "**strong**"),
            (
                expected_rendered_color(vt100::Color::Rgb(0xf0, 0xf0, 0xf0)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                true,
                false,
                false,
            )
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "_emphasis_"),
            (
                expected_rendered_color(vt100::Color::Rgb(0xf0, 0xf0, 0xf0)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                false,
                true,
                false,
            )
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "`code`"),
            (
                expected_rendered_color(vt100::Color::Rgb(0x00, 0xd0, 0x00)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                false,
                false,
                false,
            )
        );
        assert_eq!(
            rendered_cell_attributes(&vt, 100, "link"),
            (
                expected_rendered_color(vt100::Color::Rgb(0xd0, 0x00, 0x00)),
                expected_rendered_color(vt100::Color::Rgb(0x10, 0x10, 0x10)),
                true,
                false,
                false,
            )
        );
        assert!(
            !vt.screen_contains(100, "https://example.test/docs"),
            "the display-only OSC 8 link projection must not replace retained raw text"
        );
    }
}

#[test]
fn delayed_prompt_started_does_not_duplicate_live_response_block() {
    // Regression: provider updates can arrive before a delayed
    // `agent.prompt_started` if an interceptor parks that lifecycle event. The
    // delayed start must not create a second live response block alongside the
    // provider-update fallback block.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            " world",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    let lines = visible_lines(&vt, 80);
    let response_lines = lines
        .iter()
        .filter(|line| line.contains("hello"))
        .collect::<Vec<_>>();
    assert_eq!(
        response_lines.len(),
        1,
        "delayed prompt_started must not leave duplicate live response blocks: {lines:?}"
    );
    assert!(
        response_lines[0].contains("hello world"),
        "response should keep accumulating in the single live block: {lines:?}"
    );
}

/// Ensures every submitted-user-prompt projection renders ANSI bright white in
/// the default theme rather than ordinary-white index 7.
#[test]
fn submitted_prompt_projections_render_default_bright_white() {
    let vt = render_submitted_prompt_projections(tau_themes::Theme::builtin());

    for text in [
        "immediate submitted prompt",
        "promoted queued prompt",
        "steered submitted prompt",
        "replayed submitted prompt",
    ] {
        assert_rendered_bright_white(&vt, 100, text);
    }
}

/// Ensures the active `tau-dpc` theme renders every submitted-prompt
/// projection bright white rather than inheriting the terminal-default color.
#[test]
fn submitted_prompt_projections_render_dpc_bright_white() {
    let vt = render_submitted_prompt_projections(tau_themes::Theme::builtin_dpc());

    for text in [
        "immediate submitted prompt",
        "promoted queued prompt",
        "steered submitted prompt",
        "replayed submitted prompt",
    ] {
        assert_rendered_bright_white(&vt, 100, text);
    }
}

/// Timer-created internal prompt submissions render a visible wakeup marker so
/// the following response is attributable to the timer, not an invisible
/// prompt.
#[test]
fn timer_wakeup_prompt_submitted_renders_visible_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `wake` fired: stand up".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: Some("timer:wake:1".to_owned()),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "Timer `wake` woke this agent: stand up"));
    assert!(!vt.screen_contains(100, "woke this agent: Timer `wake` fired"));
}

/// Timer wakeups that were queued during a busy turn render the same marker
/// when folded as steered prompts.
#[test]
fn timer_wakeup_prompt_steered_renders_visible_marker() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "Timer `wake` fired: stand up".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: Some("timer:wake:2".to_owned()),
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "Timer `wake` woke this agent: stand up"));
    assert!(!vt.screen_contains(100, "woke this agent: Timer `wake` fired"));
}

/// Ensures prompt echoes, transcript facts, and terminal events never replace
/// complete harness stats as the CLI's navigation-cache authority.
#[test]
fn prompt_and_terminal_events_do_not_replace_navigation_snapshot() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
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
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "follow up".to_owned(),
        agent_id: agent_id("worker-1"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("worker-1"),
        text: "follow up".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "q-worker".to_owned(),
        text: "done".to_owned(),
        error: None,
    }));
    let navigation = renderer.agent_navigation();
    let navigation = navigation.lock().expect("agent navigation");
    assert_eq!(
        navigation.mode("worker-1"),
        AgentNavigationState::ActiveAuto
    );
    assert!(navigation.is_active("worker-1"));
    drop(navigation);

    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    let navigation = renderer.agent_navigation();
    let navigation = navigation.lock().expect("agent navigation");
    assert_eq!(navigation.mode("worker-1"), AgentNavigationState::Active);
    assert!(navigation.is_active("worker-1"));
}

#[test]
fn deselect_then_first_prompt_for_new_agent_does_not_inherit_prior_transcript() {
    // Regression: `:agent none` must restore an empty no-agent screen. The
    // first prompt that selects a new agent from that state should render into
    // that agent's own fresh transcript rather than appending to the previously
    // selected agent's terminal output.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "agent one prompt".to_owned(),
        agent_id: agent_id("agent-one"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "agent one prompt"));

    renderer.clear_selected_agent();
    sync(&handle);
    assert!(!vt.screen_contains(80, "agent one prompt"));

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "agent two prompt".to_owned(),
        agent_id: agent_id("agent-two"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "agent two prompt"));
    assert!(!vt.screen_contains(80, "agent one prompt"));
}

/// An unknown ordinary prompt terminal must not be mistaken for a standalone
/// compaction merely because no local prompt state exists.
#[test]
fn unknown_ordinary_prompt_termination_does_not_render_compaction() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_prompt_id: test_agent_prompt_id("sp-unknown"),
        agent_id: agent_id("main"),
        reason: AgentPromptTerminationReason::Stale,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let text = vt.screen_text(80).join("\n");
    assert!(!text.contains("compact"), "{text}");
}

#[test]
fn queued_prompt_renders_after_first_completes() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // First prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "first".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // Regression: the production busy-submit path immediately publishes
    // only `AgentPromptQueued`; there may be no preceding local
    // `UiPromptSubmitted` echo for the renderer to replace. The queued
    // event itself must make the user's prompt visible.
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "second".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "second (queued)"),
        "queued indicator should show, got: {:?}",
        vt.screen_text(80)
    );

    // First finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("response one")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "response one"));

    // Second dispatched — "(queued)" should be removed.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-1", "s1",
    )));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "(queued)"),
        "queued indicator should be gone after dispatch, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "> second"),
        "dispatched prompt should show normally, got: {:?}",
        vt.screen_text(80)
    );
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("second"))
            .count(),
        1,
        "queued prompt should be promoted instead of duplicated, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-1"),
            "response two",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two"),
        "second response should stream, got: {:?}",
        vt.screen_text(80)
    );

    // Second finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![assistant_message_item("response two complete")],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two complete"),
        "final second response should show, got: {:?}",
        vt.screen_text(80)
    );
    // First response should still be visible.
    assert!(
        vt.screen_contains(80, "response one"),
        "first response should still show, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn three_queued_prompts_render_sequentially() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // Three rapid prompts.
    for i in 0..3 {
        if i == 0 {
            renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
                literal: false,
                session_id: test_session_id("s1"),
                text: format!("msg-{i}"),
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }));
            renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
                "sp-0", "s1",
            )));
        } else {
            renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
                text: format!("msg-{i}"),
                agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
                message_class: tau_proto::PromptMessageClass::User,
            }));
        }
    }

    // Process all three sequentially, flushing between each.
    for i in 0..3 {
        let spid: tau_proto::AgentPromptId = test_agent_prompt_id(format!("sp-{i}"));
        if 0 < i {
            renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
                agent_prompt_id: spid.clone(),
                ..agent_prompt_created("sp-ignore", "s1")
            }));
        }
        renderer.handle(&Event::ProviderResponseUpdated(
            provider_response_delta_update(
                spid.clone(),
                format!("partial-{i}"),
                None,
                tau_proto::PromptOriginator::User,
            ),
        ));
        renderer.handle(&Event::ProviderResponseFinished(finished_response(
            spid.as_ref(),
            vec![assistant_message_item(format!("response-{i}"))],
        )));
        sync(&handle);
    }

    // All three responses should be visible.
    // Extra flush to catch any delayed renders.
    sync(&handle);
    for i in 0..3 {
        assert!(
            vt.screen_contains(80, &format!("response-{i}")),
            "response-{i} should be visible, got: {:?}",
            vt.screen_text(80)
        );
    }
    // No stale "..." blocks.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}

/// Terminal prompt events tombstone their prompt id so delayed start or create
/// events cannot reactivate a persistent watched status row.
#[test]
fn watched_agent_terminal_event_wins_over_delayed_prompt_start() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent("parent_1".to_owned());
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        agent_id: agent_id("engineer_1"),
        output_items: Vec::new(),
        stop_reason: ProviderStopReason::EndTurn,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("__harness__")
                .expect("test identifier must satisfy its grammar"),
            query_id: "delegate-1".to_owned(),
        },
        ..agent_prompt_created("ap-engineer_1-0", "s1")
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "delayed start/create must retain, not reactivate, the status row: {:?}",
        vt.screen_text(100)
    );
}

/// Reproduces the user-reported bug: send 3 prompts during the
/// first response's streaming. After all responses complete, the
/// prompt must be visible and all 3 responses rendered.
#[test]
fn three_prompts_during_streaming_all_render_correctly() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    // User sends first prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-0", "s1",
    )));

    // Agent starts streaming response 1.
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "Hello",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hello"),
        "streaming should show, got: {:?}",
        vt.screen_text(80)
    );

    // User sends 2nd and 3rd prompts while streaming.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "hi".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
    }));

    // More streaming updates (multi-line, like a real LLM).
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-0"),
            "!\n\nHow can I help you today?",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    // Response 1 finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(
            "Hello!\n\nHow can I help you today?",
        )],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 should be in history, got: {:?}",
        vt.screen_text(80)
    );

    // Second prompt dispatched.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-1", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-1"),
            "Hello again!\n\nHow can I help you?",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![assistant_message_item(
            "Hello again!\n\nHow can I help you?",
        )],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 should be visible, got: {:?}",
        vt.screen_text(80)
    );

    // Third prompt dispatched.
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-2", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-2"),
            "Hi there!\n\nWhat can I help you with?",
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-2",
        vec![assistant_message_item(
            "Hi there!\n\nWhat can I help you with?",
        )],
    )));
    sync(&handle);

    // All three responses should be visible.
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "What can I help you with?"),
        "response 3 missing, got: {:?}",
        vt.screen_text(80)
    );

    // The prompt must be visible at the bottom.
    assert!(
        vt.screen_contains(80, "> "),
        "prompt should be visible after all responses, got: {:?}",
        vt.screen_text(80)
    );

    // No stale streaming blocks should remain.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}
/// Ensures an independent standalone compaction terminal says `compact ok`,
/// never uses a custom success verb, and keeps streamed compactor text out of
/// the transcript and editor context.
#[test]
fn standalone_compaction_stream_is_hidden_from_cli_output() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent("main".to_owned());
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        standalone_compaction_started("ct-private", "ap-private"),
    ));
    renderer.handle(&Event::ProviderPromptSubmitted(
        tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: test_agent_prompt_id("ap-private"),
            originator: tau_proto::PromptOriginator::User,
        },
    ));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("ap-private"),
            "private compactor answer",
            Some("private compactor reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(
        vt.screen_contains(100, "private compactor answer"),
        "the delayed typed start must remove output that earlier generic events rendered"
    );

    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-private"),
    ));
    sync(&handle);

    assert!(vt.screen_contains(100, "compact Compacting…"));
    assert!(!vt.screen_contains(100, "private compactor answer"));
    assert!(!vt.screen_contains(100, "private compactor reasoning"));
    let editor_context = renderer.editor_context();
    let editor_context = editor_context.lock().expect("editor context");
    assert!(editor_context.current_response.is_none());
    assert!(editor_context.last_response.is_none());
    drop(editor_context);

    let generation = vt.frame_generation();
    let standalone_commit_handle = handle.clone();
    renderer.set_finished_commit_hook(Arc::new(move || {
        standalone_commit_handle.redraw_sync();
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "ap-private",
        Vec::new(),
    )));
    let mut next_generation = generation;
    let provider_final_frame = loop {
        let frame = vt.wait_for_frame_after(next_generation).join("\n");
        next_generation += 1;
        if !frame.contains("Compacting…") {
            break frame;
        }
        assert!(!frame.contains("private compactor"));
    };
    assert!(!provider_final_frame.contains("Compacting…"));
    assert!(!provider_final_frame.contains("private compactor"));
    assert!(provider_final_frame.contains("💤 @main"));

    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 226_200,
            provenance: tau_proto::CompactionTokenProvenance::Estimated,
        }),
        compacted_input_tokens: Some(tau_proto::CompactionTokenMeasurement {
            tokens: 4_500,
            provenance: tau_proto::CompactionTokenProvenance::Estimated,
        }),
        agent_id: agent_id("main"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-private")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-private")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: vec![assistant_message_item("private checkpoint")],
    }));
    sync(&handle);

    assert!(vt.screen_contains(100, "compact ~#226.2k → ~#4.5k (2%) ok"));
    assert!(!vt.screen_contains(100, "compact complete"));
    assert!(!vt.screen_contains(100, "Compacting…"));
    assert!(!vt.screen_contains(100, "private compactor answer"));
    assert!(!vt.screen_contains(100, "private compactor reasoning"));
    assert!(!vt.screen_contains(100, "private checkpoint"));
    assert!(!renderer.agent_has_active_prompt_for_test("main"));
    assert!(!renderer.main_agent_turn_active_for_test());
    assert!(
        !renderer
            .agent_in_progress_state()
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}

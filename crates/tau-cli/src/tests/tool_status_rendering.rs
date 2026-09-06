//! Tests for tool status rendering behavior.

use tau_cli_term::RendererDeliveryId;

use super::super::event_renderer::{ToolTimerNotifier, UiIoStats, unix_time_millis};
use super::*;
use crate::tool_render::format_turn_stats_line_with_projection;
use crate::turn_stats_projection::{
    CacheEstimateCalibration, CacheEstimateContext, CacheEstimateGeometry, CacheEstimateModel,
    PreviousTurnUsageProjection,
};

/// Live and attach-reconstructed blocks must use the same mode projection when
/// they move between visible and detached agent transcripts.
#[test]
fn verbose_mode_reprojects_streaming_thinking_hidden_agents_and_attach_tools() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-live",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("main-live"),
            "",
            Some("main live reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "main live reasoning"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(100, "main live reasoning"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "main live reasoning"));

    let mut worker_created = agent_prompt_created("worker-live", "s1");
    worker_created.agent_id = agent_id("worker");
    renderer.handle(&Event::AgentPromptCreated(worker_created));
    let mut worker_update = provider_response_delta_update(
        test_agent_prompt_id("worker-live"),
        "",
        Some("worker hidden reasoning".to_owned()),
        tau_proto::PromptOriginator::User,
    );
    worker_update.agent_id = agent_id("worker");
    renderer.handle(&Event::ProviderResponseUpdated(worker_update));

    renderer.toggle_verbose_mode();
    renderer.switch_agent(agent_id("worker"));
    sync(&handle);
    assert!(!vt.screen_contains(100, "worker hidden reasoning"));
    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(100, "worker hidden reasoning"));

    renderer.toggle_verbose_mode();
    let mut reconstructed = tool_started(
        "attach-tool",
        "read",
        CborValue::Text("SECRET_ATTACH_ARGUMENT".to_owned()),
    );
    let Event::ToolStarted(started) = &mut reconstructed else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("worker");
    renderer.handle_reconstructed_tool_start_socket_delivery(
        &reconstructed,
        &agent_id("worker"),
        tau_proto::UnixMicros::new(1),
        RendererDeliveryId::new(1),
    );
    sync(&handle);
    let pending = vt.screen_text(100).join("\n");
    assert!(pending.contains("read 0s pending"), "{pending}");
    assert!(!pending.contains("SECRET_ATTACH_ARGUMENT"), "{pending}");

    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "attach-tool".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(100, "read 0s pending"));
}

/// Tool lifecycle events must keep the timer notifier keyed by their canonical
/// call identities, without allowing duplicate or unknown terminals to change
/// the active count, and disconnect reset must clear every remaining call.
#[test]
fn tool_timer_notifier_tracks_event_lifecycle_with_typed_call_ids() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let timer = ToolTimerNotifier::new();
    let timer_state = timer.inner();
    renderer.set_tool_timer(timer);

    renderer.handle(&tool_started("timer-first", "read", CborValue::Null));
    renderer.handle(&tool_started("timer-second", "write", CborValue::Null));
    renderer.handle(&tool_started("timer-first", "read", CborValue::Null));
    {
        let (mutex, _) = &*timer_state;
        let state = mutex.lock().expect("timer state lock");
        assert_eq!(state.active_tool_ids.len(), 2);
        assert!(
            state
                .active_tool_ids
                .contains(&tau_proto::ToolCallId::from("timer-first"))
        );
        assert!(
            state
                .active_tool_ids
                .contains(&tau_proto::ToolCallId::from("timer-second"))
        );
    }

    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "timer-first".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "unknown-timer-call".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    {
        let (mutex, _) = &*timer_state;
        let state = mutex.lock().expect("timer state lock");
        assert_eq!(state.active_tool_ids.len(), 1);
        assert!(
            !state
                .active_tool_ids
                .contains(&tau_proto::ToolCallId::from("timer-first"))
        );
        assert!(
            state
                .active_tool_ids
                .contains(&tau_proto::ToolCallId::from("timer-second"))
        );
    }

    renderer.handle_disconnect(None);
    let (mutex, _) = &*timer_state;
    let state = mutex.lock().expect("timer state lock");
    assert!(state.active_tool_ids.is_empty());
}

/// Tool starts carry the owning agent id, so hidden-agent tools must be routed
/// away from the visible transcript even before provider output maps the call.
#[test]
fn renderer_learns_agent_from_tool_started_event() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let event = Event::ToolStarted(tau_proto::ToolStarted {
        invocation_policy: tau_proto::ToolInvocationPolicy::default(),
        call_id: "hidden-tool".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: CborValue::Null,
        agent_id: agent_id("agent-b"),
        originator: tau_proto::PromptOriginator::User,
    });

    assert_eq!(
        renderer.agent_id_for_event_for_test(&event).as_deref(),
        Some("agent-b")
    );

    renderer.handle(&event);

    assert_eq!(
        renderer
            .agent_id_for_event_for_test(&Event::ToolProgress(tau_proto::ToolProgress {
                call_id: "hidden-tool".into(),
                tool_name: tau_proto::ToolName::new("read"),
                message: None,
                progress: None,
                display: None,
            }))
            .as_deref(),
        Some("agent-b")
    );
    assert!(
        renderer
            .known_agents()
            .lock()
            .expect("known agents")
            .contains(&"agent-b".to_owned())
    );
}

/// Complete agent stats must redraw the selected-agent status row with escaped
/// work metadata, retaining the phase when narrow width drops the task title.
#[test]
fn selected_agent_status_row_renders_phase_and_adapts_task_title() {
    let title = "review \u{202e}fix";
    let escaped_title = tau_proto::visible_escape_metadata(title);
    let render_at_width = |width| {
        let (_term, handle, vt) = setup(width, 8);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
            session_id: test_session_id("s1"),
            reason: tau_proto::SessionStartReason::Initial,
        }));
        renderer.switch_agent(agent_id("worker"));
        renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: test_session_id("s1"),
            agent_id: agent_id("worker"),
            work_status: tau_proto::SessionAgentWorkStatus::new(
                tau_proto::AgentWorkStatusPhase::Blocked,
                Some(title.to_owned()),
            )
            .expect("valid work status"),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state: tau_proto::AgentRuntimeState::Idle,
            turn_activity: tau_proto::AgentTurnActivity::Manipulating,
            tools: Default::default(),
            context: Default::default(),
            inner_turns_total: None,
            estimated_api_cost: Default::default(),
            creator_subtree_estimated_api_cost: Default::default(),
        }));
        sync(&handle);
        vt.screen_text(width)
    };

    let wide = render_at_width(80);
    assert!(
        wide.iter()
            .any(|row| row.contains(&format!("⛔️🔨 @worker {escaped_title}"))),
        "wide selected-agent status row should contain phase and escaped title: {wide:?}"
    );
    assert!(
        wide.iter().all(|row| !row.contains(title)),
        "raw structural metadata must not reach the terminal: {wide:?}"
    );

    let narrow = render_at_width(18);
    assert!(
        narrow.iter().any(|row| row.contains("⛔️🔨 @worker")),
        "work phase should survive narrow-width fitting: {narrow:?}"
    );
    assert!(
        narrow.iter().all(|row| !row.contains(&escaped_title)),
        "lower-priority task title should yield before phase: {narrow:?}"
    );
}

#[test]
fn initial_session_started_omits_session_status_and_role_placeholder() {
    // Regression: startup may announce SessionStarted before role selection.
    // The status bar must not duplicate the prompt-context session id or add a
    // misleading no-role placeholder.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("tau-agent-test"),
        reason: SessionStartReason::Initial,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "&tau-agent-test"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

/// Manual-compaction lifecycle status belongs only to the target transcript,
/// including when the target is a descendant of the currently selected agent.
#[test]
fn manual_compaction_lifecycle_status_follows_target_agent_selection() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::AgentManualCompactionRequested(
        AgentManualCompactionRequested {
            request_id: tau_proto::CompactionRequestId::parse("cr-48-0").expect("request id"),
            target_agent_id: agent_id("reviewer-KH50"),
            source: tau_proto::ManualCompactionSource::Tool(
                tau_proto::ManualToolCompactionSource {
                    caller_agent_id: agent_id("manager"),
                    initiating_agent_prompt_id: test_agent_prompt_id("ap-manager-48"),
                    initiating_tool_call_id: "call-48".into(),
                    initiating_tool_name: tau_proto::ManualCompactionTool::AgentCompact,
                    visible_tool_name: tau_proto::ToolName::new("agent_compact"),
                    resume_inference: false,
                },
            ),
            requested_target_head: tau_proto::AgentHead::Root,
            target_generation: tau_proto::MaterializedPromptGeneration::from_inference_generation(
                0,
            ),
            model: "test/model".parse().expect("model id"),
        },
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        AgentStandaloneCompactionStarted {
            agent_id: agent_id("reviewer-KH50"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-48")
                .expect("transaction id"),
            compact_prompt_id: test_agent_prompt_id("ap-reviewer-KH50-48"),
            cut: tau_proto::AgentHead::Root,
            resume_through: None,
            model: "test/model".parse().expect("model id"),
            operation: tau_proto::PromptOperation::StandaloneCompaction,
            originator: tau_proto::PromptOriginator::User,
            supersedes: None,
            trigger: tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                request_id: tau_proto::CompactionRequestId::parse("cr-48-0").expect("request id"),
                caller_agent_id: agent_id("manager"),
                initiating_tool_call_id: "call-48".into(),
            },
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "compaction request cr-48-0"));

    renderer.switch_agent(agent_id("unrelated"));
    sync(&handle);
    assert!(!vt.screen_contains(100, "compaction request cr-48-0"));

    renderer.switch_agent(agent_id("reviewer-KH50"));
    sync(&handle);
    assert!(vt.screen_contains(
        100,
        "Agent manager accepted compaction request for reviewer-KH50 (cr-48-0)"
    ));
    assert!(!vt.screen_contains(100, "Starting compaction request"));
}

/// Ensures start-result delivery cannot replace canonical outer-turn runtime
/// state as the effective-activity authority.
#[test]
fn delegated_agent_effectiveness_follows_stats_not_start_result() {
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
        start_id: tau_proto::StartOperationId(1),
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
        inner_turns_total: None,
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.handle(&Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "q-worker".to_owned(),
        text: "done".to_owned(),
        error: None,
    }));
    let navigation = renderer.agent_navigation();
    assert!(
        navigation
            .lock()
            .expect("agent navigation")
            .is_active(&agent_id("worker-1"))
    );
}

#[test]
fn switched_agent_shows_its_tool_usage() {
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
        start_id: tau_proto::StartOperationId(1),
        query_id: "q-worker".to_owned(),
        agent_id: agent_id("worker-1"),
    }));
    let originator = tau_proto::PromptOriginator::Extension {
        name: tau_proto::ExtensionName::parse("core-subagents")
            .expect("test identifier must satisfy its grammar"),
        query_id: "q-worker".to_owned(),
    };
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        agent_id: agent_id("worker-1"),
        originator: originator.clone(),
        ..finished_response(
            "worker-sp",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "worker-call".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/lib.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )
    }));
    renderer.handle_recorded_at(
        &tool_started(
            "worker-call",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/lib.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("worker-call", "read", "src/lib.rs", ""),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "read src/lib.rs"));

    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/lib.rs"));
}

/// Structured current watch statuses remain exclusive to their watcher
/// transcript and do not appear in the no-agent message overview.
#[test]
fn no_agent_overview_excludes_structured_current_watch_status() {
    let (_term, handle, vt) = setup(100, 20);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let provider_status_body = "watched provider is blocked";
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("watch-provider")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("watched-agent"),
            sender_session_id: None,
            recipient_id: agent_id("watcher-agent"),
            kind: tau_proto::AgentMessageKind::WatchProviderStatus,
            watch_provider_status: Some(tau_proto::AgentWatchProviderStatusNotification {
                session_id: test_session_id("s1"),
                subscription_id: "sub-1".to_owned(),
                turn_generation: tau_proto::AgentOuterTurnGeneration::from_raw(1),
                agent_prompt_id: test_agent_prompt_id("prompt-1"),
                state: tau_proto::AgentWatchProviderState::Blocked {
                    category: tau_proto::AgentWatchProviderCategory::Account,
                },
                initial: false,
            }),
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: provider_status_body.to_owned(),
        },
    ));
    let long_wait_row = "▤ @watched-agent has been working for 5 minutes";
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("watch-long-wait")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("watched-agent"),
            sender_session_id: None,
            recipient_id: agent_id("watcher-agent"),
            kind: tau_proto::AgentMessageKind::WatchLongWait,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: Some(tau_proto::AgentWatchLongWaitNotification {
                session_id: test_session_id("s1"),
                subscription_id: "sub-1".to_owned(),
                status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
                threshold_minutes: 5,
            }),
            watch_lifecycle: None,
            message: String::new(),
        },
    ));
    sync(&handle);
    let provider_status_row = format!("□ {provider_status_body}");
    assert!(!vt.screen_contains(100, &provider_status_row));
    assert!(!vt.screen_contains(100, long_wait_row));

    renderer.switch_agent(agent_id("watcher-agent"));
    sync(&handle);
    assert!(vt.screen_contains(100, &provider_status_row));
    assert!(vt.screen_contains(100, long_wait_row));
}

#[test]
fn new_session_preserves_role_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(100_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "+engineer"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::Initial,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer"));
    assert!(!vt.screen_contains(80, "&s2"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

/// The compact quota status uses redundant ASCII text for every pacing state,
/// so no-color and narrow terminal users do not have to infer meaning from hue.
#[test]
fn quota_status_renders_all_accessible_compact_chips() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some(model.clone()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    let now = unix_time_millis();
    let cases = [
        (1, 1_000, 5_000, 0, "Q-"),
        (2, 5_000, 5_000, 0, "Q="),
        (3, 6_000, 5_000, 0, "Q+"),
        (4, 9_000, 5_000, 0, "Q!"),
        (5, 5_000, 5_000, 16 * 60 * 1_000, "Q?"),
    ];
    for (epoch, used, elapsed, age, expected) in cases {
        let remaining = 604_800_u64 * (10_000 - elapsed) / 10_000;
        renderer.handle(&Event::HarnessProviderQuotaChanged(
            tau_proto::HarnessProviderQuotaChanged {
                provider: model.provider.clone(),
                profile_epoch: tau_proto::ProviderQuotaEpoch::parse(format!("epoch-{epoch}"))
                    .expect("valid quota test value"),
                sequence: tau_proto::ProviderQuotaSequence::new(1),
                windows: vec![tau_proto::ProviderQuotaWindow {
                    key: tau_proto::ProviderQuotaWindowKey {
                        limit_id: tau_proto::ProviderQuotaLimitId::parse("codex")
                            .expect("valid quota test value"),
                        window_id: tau_proto::ProviderQuotaWindowId::parse("secondary")
                            .expect("valid quota test value"),
                    },
                    used_basis_points: used,
                    usage_observed_at_unix_ms: tau_proto::UnixMillis::new(now - age),
                    window_seconds: tau_proto::QuotaWindowSeconds::new(604_800),
                    reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(
                        now / 1_000 + remaining,
                    )),
                    remaining_seconds_at_timing_anchor: Some(tau_proto::SignedSeconds::new(
                        remaining as i64,
                    )),
                    timing_anchor_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(now - age)),
                    server_offset_ms: Some(tau_proto::ServerOffsetMillis::new(0)),
                    server_offset_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(now - age)),
                }],
                route_bindings: vec![tau_proto::ProviderQuotaRouteBinding {
                    model: model.clone(),
                    limit_ids: vec![
                        tau_proto::ProviderQuotaLimitId::parse("codex")
                            .expect("valid quota test value"),
                    ],
                    observed_at_unix_ms: tau_proto::UnixMillis::new(now - age),
                    provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
                }],
            },
        ));
        sync(&handle);
        assert!(vt.screen_contains(80, expected), "missing {expected}");
    }
}

/// A narrow status line with the real default-plus-Bengalfox account shape
/// renders pacing from the exact `codex` binding and does not let the unrelated
/// additional pool's danger state override the selected route.
#[test]
fn quota_status_narrow_two_pool_state_uses_only_bound_default_pool() {
    let (_term, handle, vt) = setup(16, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some(model.clone()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    let now = unix_time_millis();
    let remaining = 604_800_u64 / 2;
    let window = |limit_id: &str, used_basis_points| tau_proto::ProviderQuotaWindow {
        key: tau_proto::ProviderQuotaWindowKey {
            limit_id: tau_proto::ProviderQuotaLimitId::parse(limit_id).expect("pool id"),
            window_id: tau_proto::ProviderQuotaWindowId::parse("primary").expect("window id"),
        },
        used_basis_points,
        usage_observed_at_unix_ms: tau_proto::UnixMillis::new(now),
        window_seconds: tau_proto::QuotaWindowSeconds::new(604_800),
        reset_at_unix_seconds: Some(tau_proto::UnixSeconds::new(now / 1_000 + remaining)),
        remaining_seconds_at_timing_anchor: Some(tau_proto::SignedSeconds::new(remaining as i64)),
        timing_anchor_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(now)),
        server_offset_ms: Some(tau_proto::ServerOffsetMillis::new(0)),
        server_offset_observed_at_unix_ms: Some(tau_proto::UnixMillis::new(now)),
    };
    renderer.handle(&Event::HarnessProviderQuotaChanged(
        tau_proto::HarnessProviderQuotaChanged {
            provider: model.provider.clone(),
            profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-two-pool")
                .expect("quota epoch"),
            sequence: tau_proto::ProviderQuotaSequence::new(2),
            windows: vec![window("codex", 1_000), window("codex_bengalfox", 9_500)],
            route_bindings: vec![tau_proto::ProviderQuotaRouteBinding {
                model,
                limit_ids: vec![
                    tau_proto::ProviderQuotaLimitId::parse("codex").expect("default pool"),
                ],
                observed_at_unix_ms: tau_proto::UnixMillis::new(now),
                provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
            }],
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(16)
        .into_iter()
        .find(|row| row.contains("Q-"))
        .expect("narrow status row");
    assert!(status_row.ends_with("Q-"));
    assert!(!vt.screen_contains(16, "Q!"));
}

#[test]
fn status_identity_matches_no_agent_placeholder_semantics() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: None,
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    sync(&handle);

    // In the no-agent/start-new-agent state, the status bar mirrors the prompt
    // placeholder by showing the selected role.
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row before agent selection");
    assert!(status_row.starts_with("+engineer"));
    assert!(!status_row.contains("@engineer_abc"));

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("engineer_abc").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    // Once an agent is selected, the same slot switches from role to agent id.
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@engineer_abc"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("❓💤 @engineer_abc"));
    assert!(!status_row.contains("+engineer"));

    renderer.clear_selected_agent();
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after clearing agent selection");
    assert!(status_row.starts_with("+engineer"));
    assert!(!status_row.contains("@engineer_abc"));
}

/// An authoritative display name remains supplemental and visible when its text
/// equals the operational role; equality must not be treated as synthetic.
#[test]
fn status_agent_chip_keeps_id_primary_and_display_name_secondary() {
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
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer-junior_b"),
        role: "engineer-junior".to_owned(),
        display_name: Some("engineer-junior".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("engineer-junior_b").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@engineer-junior_b"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("❓💤 @engineer-junior_b (engineer-junior)"));
    assert!(!status_row.contains("@engineer-junior (engineer-junior_b)"));
}

/// A selected agent without an explicit display name must not show its role as
/// a synthesized parenthetical in the status bar.
#[test]
fn status_agent_chip_omits_parenthetical_for_unnamed_agent() {
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
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer-junior_b"),
        role: "engineer-junior".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: agent_id("engineer-junior_b"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@engineer-junior_b"))
        .expect("status row after agent selection");
    assert!(status_row.starts_with("❓💤 @engineer-junior_b"));
    assert!(!status_row.contains("(engineer-junior)"));
    assert!(!status_row.contains("@engineer-junior_b ("));
}

/// Prevents reintroducing the `watched by:` prefix while preserving the watcher
/// id beside the selected-agent label required by
/// `SPEC-tau-cli-agent-message-labels`.
#[test]
fn status_agent_chip_shows_current_agent_watchers() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        parent_agent: None,
        agent_id: agent_id("engineer_child"),
        role: "engineer".to_owned(),
        display_name: Some("fix streaming ellipsis".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "hello".into(),
        agent_id: tau_proto::AgentId::parse("engineer_child").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("manager-AjhD"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(120)
        .into_iter()
        .find(|row| row.contains("@engineer_child"))
        .expect("status row after watch update");
    assert!(status_row.contains("@engineer_child (fix streaming ellipsis)"));
    assert!(status_row.contains("manager-AjhD"));
    assert!(!status_row.contains("watched by:"));
    assert!(!status_row.contains("child of"));
}

/// Prevents reintroducing the `watched by:` prefix while preserving the sorted
/// first watcher id and `+N more agents` truncation required by
/// `SPEC-tau-cli-agent-message-labels`.
#[test]
fn status_agent_chip_truncates_multiple_current_agent_watchers() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.switch_agent(agent_id("engineer_child"));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("manager-AjhD"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("reviewer-Zz99"),
            watched_agent_ids: vec![agent_id("engineer_child")],
            changed_agent_id: Some(agent_id("engineer_child")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(120)
        .into_iter()
        .find(|row| row.contains("@engineer_child"))
        .expect("status row after watch updates");
    assert!(status_row.contains("manager-AjhD, +1 more agents"));
    assert!(!status_row.contains("watched by:"));
    assert!(!status_row.contains("reviewer-Zz99"));
}

/// Main-tool progress and context retain their relative order while quota
/// pacing occupies the final, rightmost status position.
#[test]
fn model_status_shows_main_tools_then_context_then_quota() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    let model = tau_proto::ModelId::from("chatgpt/gpt-5.6-sol");
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some(model.clone()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    renderer.handle(&danger_quota_event(&model));

    // Regression coverage for the bottom status bar: main-agent tool
    // usage should mirror generic tool progress chips (`%complete/total`)
    // and should render immediately before the context chip. Quota remains
    // final, while side-conversation calls stay rolled up under their delegate.
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        automatic_compaction_decision: None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id("side-sp"),
        agent_id: tau_proto::AgentId::parse("q1").expect("agent id"),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "side-call".into(),
            name: tau_proto::ToolName::new("grep"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
        stop_reason: ProviderStopReason::ToolCalls,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q1".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after side response");
    assert!(status_row.ends_with("#12k/200k Q!"));
    assert!(!status_row.contains('%'));

    let mut created = agent_prompt_created("main-sp", "s1");
    created.model = model.clone();
    renderer.handle(&Event::AgentPromptCreated(created));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-sp",
        vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-2".into(),
                name: tau_proto::ToolName::new("grep"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after main response");
    assert!(
        status_row.ends_with("%0/2 #12k/200k -/- Q!"),
        "unexpected status row: {status_row:?}"
    );

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "side-call".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("side result".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q1".to_owned(),
        },

        display: None,
    }));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("main result".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after tool result");
    assert!(status_row.ends_with("%1/2 #12k/200k -/- Q!"));

    // Regression coverage for turn visibility: once an extension/sub-agent
    // prompt becomes active, it must not steal the main transcript's tool chip;
    // main progress stays visible while side-conversation tool calls remain
    // rolled up under their own delegate blocks.
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: tau_proto::AgentId::parse("q2").expect("agent id"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q2".to_owned(),
        },
        ..agent_prompt_created("side-sp-2", "s1")
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after side prompt starts");
    assert!(
        status_row.ends_with("%1/2 @1 #12k/200k -/- Q!"),
        "unexpected status row: {status_row:?}"
    );
    assert!(status_row.contains('%'));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("main result".into()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after second main tool result during side turn");
    assert!(status_row.ends_with("%2/2 @1 #12k/200k -/- Q!"));
    assert!(status_row.contains('%'));

    // Main tool completions that arrive while a side conversation is active
    // update the visible main counters. The side conversation's own tool usage
    // remains hidden from the main status chip.
    let mut follow_up = agent_prompt_created("main-follow-up-sp", "s1");
    follow_up.model = model;
    renderer.handle(&Event::AgentPromptCreated(follow_up));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after main prompt resumes");
    assert!(status_row.ends_with("%2/2 @1 #12k/200k -/- Q!"));

    // The main agent's final no-tool response ends the tool-using turn and
    // hides the chip while preserving context stats.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-final-sp",
        vec![assistant_message_item("done")],
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after final main response");
    assert!(status_row.ends_with("@1 #12k/200k -/- Q!"));
    assert!(!status_row.contains('%'));

    // Starting a new user task in the same session also keeps the chip hidden
    // until the main agent requests tools for that task.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        literal: false,
        session_id: test_session_id("s1"),
        text: "next task".into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after next prompt");
    assert!(status_row.ends_with("@1 #12k/200k -/- Q!"));
    assert!(!status_row.contains('%'));

    renderer.apply_setting("show-ui-io", "true");
    renderer.apply_setting("redraw-counter", "true");
    handle.invalidate_screen();
    sync(&handle);
    let full_render_count = handle.full_render_count();
    renderer.handle_ui_io_sample(UiIoStats {
        uplink_max_bytes_per_sec: 1024,
        downlink_max_bytes_per_sec: 2048,
    });
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row with optional diagnostics");
    assert!(
        status_row.ends_with(&format!(
            "@1 #12k/200k -/- io ↑1K ↓2K {full_render_count} Q!"
        )),
        "unexpected status row: {status_row:?}"
    );
}

/// Ensures cancellation clears active-tool state and preserves a producer's
/// generic attempt-history display while normalizing the terminal status.
#[test]
fn agent_in_progress_clears_when_tool_is_cancelled() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp1", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp1",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));

    // ToolCancelled is a terminal tool event just like ToolResult/ToolError.
    // The Ctrl-D guard must clear it, otherwise a cancelled tool leaves the
    // session looking busy forever after the harness has stopped the tool.
    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        display: Some(tau_proto::ToolUseState {
            args: "query: example".to_owned(),
            info_chips: vec!["✗ Exa → ⊘ Parallel".to_owned()],
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        }),
    }));

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    sync(&handle);
    assert!(vt.screen_contains(80, "✗ Exa → ⊘ Parallel cancelled"));
}

/// Ensures idle watched status rows repaint with self-reported work and stats.
///
/// The initial unreported row must appear before model activity, then update in
/// place when the agent reports working and its counters change.
#[test]
fn watched_agent_stats_redraws_status_row() {
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
    renderer.switch_agent(agent_id("parent_1"));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));

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
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats::default(),
        inner_turns_total: None,
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));

    assert!(
        eventually_screen_contains(&vt, 100, "❓💤 @engineer_1"),
        "watched-agent stats should repaint without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
    assert!(
        eventually_screen_contains(&vt, 100, "❓💤 @engineer_1 %3/3"),
        "watched-agent stats should repaint with tool-call-style counters without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
    renderer.handle(&Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),
        parent_agent: Some(agent_id("parent_1")),
        agent_id: agent_id("engineer_1"),
        role: "engineer".to_owned(),
        display_name: Some("worker display".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&Event::AgentMessageReceived(
        tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse("status-engineer-1")
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("engineer_1"),
            sender_session_id: None,
            recipient_id: agent_id("parent_1"),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: test_session_id("s1"),
                subscription_id: "watch-engineer-1".to_owned(),
                status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
                phase: tau_proto::AgentWorkStatusPhase::Working,
                title: Some("investigate session".to_owned()),
                initial: true,
            }),
            watch_long_wait: None,
            watch_lifecycle: None,
            message: String::new(),
        },
    ));
    assert!(
        eventually_screen_contains(
            &vt,
            100,
            "🚀💤 @engineer_1 (worker display) investigate session %3/3",
        ),
        "the watched row should use the agent's own status title and display name: {:?}",
        vt.screen_text(100)
    );
    renderer.handle(&Event::SessionAgentUnloaded(
        tau_proto::SessionAgentUnloaded {
            session_id: test_session_id("s1"),
            agent_id: agent_id("engineer_1"),
        },
    ));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    assert!(
        eventually_screen_contains(&vt, 100, "❓💤 @engineer_1 (worker display)"),
        "a reloaded same-id row must not retain its former self-reported title: {:?}",
        vt.screen_text(100)
    );
    let reloaded_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| {
            row.trim_start()
                .starts_with("❓💤 @engineer_1 (worker display)")
        })
        .expect("reloaded watched row");
    assert!(!reloaded_row.contains("investigate session"));
    assert!(
        !vt.screen_contains(100, "running tools"),
        "watched-agent block should keep compact tool-block layout, not prose: {:?}",
        vt.screen_text(100)
    );
}

/// Ensures watched-agent rows remain owned by the transcript snapshot across
/// agent switches.
///
/// This prevents restoring a parent transcript that already contains a
/// `watching [...]` row while the renderer has forgotten that row's block id,
/// which would otherwise create a duplicate simultaneous row for the same
/// watched agent on the next refresh.
#[test]
fn watched_agent_status_row_does_not_duplicate_after_agent_switch() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent(agent_id("parent_1"));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
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
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 13,
        },
        context: tau_proto::AgentContextStats::default(),
        inner_turns_total: None,
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);
    assert!(eventually_screen_contains(
        &vt,
        100,
        "❓💤 @engineer_1 %13/13",
    ));

    renderer.switch_agent(agent_id("other_1"));
    renderer.switch_agent(agent_id("parent_1"));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 42,
        },
        context: tau_proto::AgentContextStats::default(),
        inner_turns_total: None,
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);

    let watching_rows: Vec<_> = vt
        .screen_text(100)
        .into_iter()
        .filter(|row| row.contains("❓💤 @engineer_1"))
        .map(|row| row.trim_end().to_owned())
        .collect();
    assert_eq!(
        watching_rows,
        vec!["❓💤 @engineer_1 %42/42"],
        "watched-agent row should update in place after transcript restore: {:?}",
        vt.screen_text(100)
    );
}

/// A provider response must stop only transient activity, not the watched row.
///
/// The row remains available for the agent's task status until the agent
/// reports `done`; a provider terminal cannot make it flicker away between
/// model rounds.
#[test]
fn watched_agent_response_finished_keeps_status_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent(agent_id("parent_1"));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
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
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 0,
            started_total: 15,
        },
        context: tau_proto::AgentContextStats::default(),
        inner_turns_total: None,
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));

    assert!(eventually_screen_contains(
        &vt,
        100,
        "❓💤 @engineer_1 %15/15",
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
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1 %15/15"),
        "outer runtime state should remain running after the inner response finishes: {:?}",
        vt.screen_text(100)
    );
}

/// A direct watched row must use the canonical task-status phase for lifetime.
///
/// Missing status is unreported; working and blocked preserve the same row
/// through turn start and stop, while done is the only phase that removes it.
#[test]
fn watched_agent_status_row_survives_turn_transitions_until_done() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent(agent_id("parent_1"));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    let stats = |runtime_state| {
        Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: test_session_id("s1"),
            agent_id: agent_id("engineer_1"),
            work_status: Default::default(),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state,
            turn_activity: tau_proto::AgentTurnActivity::Idle,
            tools: Default::default(),
            context: Default::default(),
            inner_turns_total: None,
            estimated_api_cost: Default::default(),
            creator_subtree_estimated_api_cost: Default::default(),
        })
    };
    let watch_status = |message_id: &str, phase, title: Option<&str>| {
        Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse(message_id)
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("engineer_1"),
            sender_session_id: None,
            recipient_id: agent_id("parent_1"),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: test_session_id("s1"),
                subscription_id: "watch-1".to_owned(),
                status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
                phase,
                title: title.map(str::to_owned),
                initial: false,
            }),
            watch_long_wait: None,
            watch_lifecycle: None,
            message: String::new(),
        })
    };

    sync(&handle);
    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "an absent status snapshot is canonically unreported"
    );

    renderer.handle(&watch_status(
        "status-unreported",
        tau_proto::AgentWorkStatusPhase::Unreported,
        None,
    ));
    renderer.handle(&stats(tau_proto::AgentRuntimeState::Running));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));
    renderer.handle(&stats(tau_proto::AgentRuntimeState::Idle));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));

    renderer.handle(&watch_status(
        "status-working",
        tau_proto::AgentWorkStatusPhase::Working,
        Some("implement fix"),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "🚀💤 @engineer_1 implement fix"));

    renderer.handle(&watch_status(
        "status-blocked",
        tau_proto::AgentWorkStatusPhase::Blocked,
        Some("await input"),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "⛔️💤 @engineer_1 await input"));

    renderer.handle(&watch_status(
        "status-waiting",
        tau_proto::AgentWorkStatusPhase::Waiting,
        Some("await automation"),
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "⏳💤 @engineer_1 await automation"));

    renderer.handle(&watch_status(
        "status-unknown",
        tau_proto::AgentWorkStatusPhase::Unknown,
        None,
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "❓💤 @engineer_1"));

    renderer.handle(&watch_status(
        "status-done",
        tau_proto::AgentWorkStatusPhase::Done,
        Some("finished"),
    ));
    sync(&handle);
    assert!(
        vt.screen_text(100)
            .iter()
            .all(|row| !row.trim_start().starts_with("@engineer_1 ")),
        "done must remove the watched-agent activity row"
    );
}

/// Provider response updates use their explicit agent id as the active prompt
/// owner, then terminal cleanup clears activity without removing its status
/// row.
///
/// This prevents a provider-update-only path from accidentally marking the
/// current/originator agent active and leaving the watched response owner stale
/// after `provider.response_finished`.
#[test]
fn watched_agent_provider_response_update_keeps_status_row_after_terminal() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent(agent_id("parent_1"));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_id: agent_id("engineer_1"),
        ..provider_response_delta_update(
            test_agent_prompt_id("ap-engineer_1-0"),
            "working",
            None,
            tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("__harness__")
                    .expect("test identifier must satisfy its grammar"),
                query_id: "parent-query".to_owned(),
            },
        )
    }));
    sync(&handle);

    assert!(eventually_screen_contains(&vt, 100, "❓✨ @engineer_1",));

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
            query_id: "parent-query".to_owned(),
        },
        usage: None,
        compaction_original_input_tokens: None,
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "terminal prompt id should clear activity but retain the status row: {:?}",
        vt.screen_text(100)
    );
}

/// Ensures watched-agent rows put self-reported status before stable identity
/// and title while preserving generic telemetry, independent counter styling,
/// and context-first truncation priority.
#[test]
fn watched_agent_display_uses_tool_block_styles_and_counters() {
    let theme = cli_test_theme();
    let stats = tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 1,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats {
            input_tokens: Some(133_400),
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: Some(67),
        },
        inner_turns_total: Some(123),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    };
    let status = tau_proto::AgentWatchWorkStatusNotification {
        session_id: test_session_id("s1"),
        subscription_id: "watch-1".to_owned(),
        status_epoch: tau_proto::AgentWorkStatusEpoch::from_raw(1),
        phase: tau_proto::AgentWorkStatusPhase::Working,
        title: Some("review changes".to_owned()),
        initial: false,
    };

    let display = watched_agent_tool_display(
        Some("review"),
        "engineer_1",
        None,
        Some(&stats),
        WatchedAgentActivity::Running,
        Some(&status),
    );
    assert_eq!(display.tool_name, "@engineer_1");
    assert_eq!(display.args, "");
    let leading: Vec<&str> = display
        .leading_segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    assert_eq!(
        display
            .status_prefix
            .as_ref()
            .map(|(text, _)| text.as_str()),
        Some("🚀💤")
    );
    assert_eq!(leading, vec!["(review)", "review changes"]);
    let texts: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["%2/3", "#133k/200k", "*123"]);

    let block = render_tool_block(&theme, &display);
    assert_eq!(
        priority_header_text(&block, 100),
        "🚀💤 @engineer_1 (review) review changes %2/3 #133k/200k *123"
    );
    let wide_cells = priority_header_cells(&block, 100);
    let identity_start = wide_cells
        .iter()
        .position(|cell| cell.ch == '@')
        .expect("stable identity span");
    assert_eq!(
        wide_cells[identity_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::WATCHING_NAME),
        "stable agent identity keeps the watched-agent style"
    );
    let display_name_start = wide_cells
        .iter()
        .position(|cell| cell.ch == '(')
        .expect("display name span")
        + 1;
    let phase_start = wide_cells
        .iter()
        .position(|cell| cell.ch == '🚀')
        .expect("work phase span");
    let title_start = wide_cells
        .iter()
        .rposition(|cell| cell.ch == 'r')
        .expect("self-reported title span");
    assert_eq!(
        wide_cells[display_name_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO),
        "persisted display names remain informational metadata"
    );
    assert_eq!(
        wide_cells[phase_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROGRESS_INDICATOR),
        "self-reported work phases retain the progress semantic style"
    );
    assert_eq!(
        wide_cells[title_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO),
        "self-reported task titles remain informational metadata"
    );
    let inner_turn_start = wide_cells
        .iter()
        .position(|cell| cell.ch == '*')
        .expect("inner-turn counter span");
    assert_eq!(
        wide_cells[inner_turn_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::STATUS_INNER_TURNS),
        "inner turns retain their independently configurable status style"
    );
    let without_display_name = priority_header_text(&block, 40);
    assert!(!without_display_name.contains("(review)"));
    assert!(
        without_display_name.contains("🚀"),
        "{without_display_name:?}"
    );
    assert!(
        without_display_name.contains("re┄es"),
        "{without_display_name:?}"
    );
    let without_task_title = priority_header_text(&block, 19);
    assert!(
        without_task_title.starts_with("🚀💤 @"),
        "{without_task_title:?}"
    );
    assert!(without_task_title.contains("🚀"), "{without_task_title:?}");
    assert!(
        !without_task_title.contains("review"),
        "{without_task_title:?}"
    );
    let without_inner_turns = priority_header_text(&block, 27);
    assert!(
        without_inner_turns.contains("#133k/200k"),
        "{without_inner_turns:?}"
    );
    assert!(
        !without_inner_turns.contains("*123"),
        "{without_inner_turns:?}"
    );

    let percent_only_stats = tau_proto::AgentStatsUpdated {
        context: tau_proto::AgentContextStats {
            input_tokens: None,
            cached_tokens: None,
            context_window: None,
            percent_used: Some(67),
        },
        ..stats
    };
    let display = watched_agent_tool_display(
        Some("review"),
        "engineer_1",
        None,
        Some(&percent_only_stats),
        WatchedAgentActivity::Watching { witness: "leaf" },
        Some(&status),
    );
    let texts: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(display.tool_name, "@engineer_1");
    assert_eq!(texts, vec!["-> @leaf", "%2/3", "#67%", "*123"]);
    let block = render_tool_block(&theme, &display);
    let watching = priority_header_cells(&block, 100)
        .into_iter()
        .find(|cell| cell.ch == '@')
        .expect("stable identity cell");
    assert_eq!(
        watching.style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::WATCHING_NAME)
    );
    assert_eq!(watching.style.fg, Some(Color::DarkYellow));
}

/// Replay-boundary abandonment removes only the unconfirmed lifecycle from
/// every renderer owner while preserving unrelated and current lifecycles.
#[test]
fn shell_replay_abandonment_covers_renderer_owners_and_collision() {
    let command = |id: &str, text: &str, target: Option<&str>| {
        Event::UiShellCommand(tau_proto::UiShellCommand {
            session_id: test_session_id("s1"),
            command_id: tau_proto::ShellCommandId::parse(id).expect("command id"),
            command: text.to_owned(),
            include_in_context: false,
            target_agent_id: target.map(agent_id),
        })
    };
    let abandoned = |id: &str, target: Option<&str>| ShellStartPresentation {
        command_id: tau_proto::ShellCommandId::parse(id).expect("command id"),
        target_agent_id: target.map(agent_id),
    };

    for (case, target, initially_selected, selected_after) in [
        ("visible", Some("worker"), Some("worker"), Some("worker")),
        ("hidden", Some("worker"), Some("main"), Some("worker")),
        ("no-agent", None, None, None),
    ] {
        let (_term, handle, vt) = setup(100, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
            session_id: test_session_id("s1"),
            reason: tau_proto::SessionStartReason::Initial,
        }));
        if let Some(agent) = initially_selected {
            renderer.switch_agent(agent_id(agent));
        }
        renderer.handle(&command("shell-x", &format!("{case}-remove-X"), target));
        renderer.handle(&command("shell-y", &format!("{case}-retain-Y"), target));
        renderer.abandon_shell_starts(&[abandoned("shell-x", target)]);
        if let Some(agent) = selected_after {
            renderer.switch_agent(agent_id(agent));
        }
        sync(&handle);
        assert!(!vt.screen_contains(100, &format!("{case}-remove-X")));
        assert!(vt.screen_contains(100, &format!("{case}-retain-Y")));
    }

    // Targeted starts can be deferred behind initial discovery. Removing X
    // before agent selection must prevent it from resurrecting when the queue
    // flushes, while unrelated Y survives.
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::HarnessAgentContextInitialized(
        tau_proto::HarnessAgentContextInitialized {
            session_id: test_session_id("s1"),
            agent_id: agent_id("worker"),
            agent_initialization_id: tau_proto::AgentInitializationId::parse("worker-init")
                .expect("initialization id"),
            listed_skills: Vec::new(),
            agents_files: Vec::new(),
        },
    ));
    renderer.handle(&command("shell-x", "deferred-remove-X", Some("worker")));
    renderer.handle(&command("shell-y", "deferred-retain-Y", Some("worker")));
    renderer.abandon_shell_starts(&[abandoned("shell-x", Some("worker"))]);
    renderer.switch_agent(agent_id("worker"));
    sync(&handle);
    assert!(!vt.screen_contains(100, "deferred-remove-X"));
    assert!(vt.screen_contains(100, "deferred-retain-Y"));

    // A colliding historical terminal renders standalone and does not consume
    // the active row subsequently settled by the live terminal.
    renderer.handle(&command(
        "shell-collision",
        "current-collision",
        Some("worker"),
    ));
    let terminal = tau_proto::ShellCommandFinished {
        command_id: tau_proto::ShellCommandId::parse("shell-collision").expect("command id"),
        session_id: test_session_id("s1"),
        command: "historical-collision".to_owned(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker")),
        output: "historical-output".to_owned(),
        exit_code: Some(0),
        cancelled: false,
    };
    renderer.handle_standalone_socket_shell_finished(
        &terminal,
        tau_proto::UnixMicros::new(1),
        RendererDeliveryId::new(1),
    );
    sync(&handle);
    assert!(vt.screen_contains(100, "historical-output"));
    assert!(vt.screen_contains(100, "current-collision"));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command: "current-collision".to_owned(),
            output: "current-output".to_owned(),
            ..terminal
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "historical-output"));
    assert!(vt.screen_contains(100, "current-output"));
}

#[test]
fn running_shell_display_keeps_mode_separate_for_dedicated_style() {
    let theme = cli_test_theme();
    let display = tau_proto::ToolUseState {
        args: "printf hello".to_owned(),
        mode: "rw".to_owned(),
        status: tau_proto::ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("shell", &display);
    assert_eq!(rendered.mode, "rw");
    assert_eq!(rendered.args, "printf hello");

    let block = render_tool_block(&theme, &rendered);
    let cells = priority_header_cells(&block, 80);
    let text: String = cells.iter().map(|cell| cell.ch).collect();
    let mode_start = text.find(" rw ").expect("mode span") + 1;
    assert_eq!(
        cells[mode_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_MODE)
    );
}

/// The real untyped `AgentPromptSteered` carrier used by work-status reminders
/// remains model-visible while compact mode suppresses only its human
/// projection.
#[test]
fn compact_mode_dominates_status_reminder_internal_prompt_subfilter() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = marker_test_renderer(handle.clone());
    renderer.apply_setting("show-internal-prompts", "on");
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "Set your status to `working` before continuing substantive tool work.".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(120, "Set your status to `working`"));

    renderer.apply_setting("notice-level", "warning");
    sync(&handle);
    assert!(!vt.screen_contains(120, "Set your status to `working`"));
    renderer.apply_setting("notice-level", "info");
    sync(&handle);
    assert!(vt.screen_contains(120, "Set your status to `working`"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(!vt.screen_contains(120, "Set your status to `working`"));

    renderer.toggle_verbose_mode();
    sync(&handle);
    assert!(vt.screen_contains(120, "Set your status to `working`"));
}

/// A tagged fresh-turn context-size alert renders exactly once in journal
/// order, while an otherwise identical untagged internal prompt stays hidden.
#[test]
fn context_size_alert_prompt_submitted_renders_internal_history_marker() {
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
    let visible_prompt = |text: &str| {
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        })
    };
    renderer.handle(&visible_prompt("before submitted alert"));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("engineer_abc12345"),
        text: "untagged internal prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: true,
        agent_id: agent_id("engineer_abc12345"),
        text: "Use the `compact` tool after finishing your current task.".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
        originator: tau_proto::PromptOriginator::User,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        display_name: None,
        ctx_id: None,
    }));
    renderer.handle(&visible_prompt("after submitted alert"));
    sync(&handle);

    let relevant_lines = visible_lines(&vt, 100)
        .into_iter()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| line.contains("submitted alert") || line.contains("Use the `compact`"))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_lines,
        [
            "> before submitted alert",
            "□ Use the `compact` tool after finishing your current task.",
            "> after submitted alert",
        ]
    );
    assert!(!vt.screen_contains(100, "untagged internal prompt"));
}

/// A context-size alert folded after tools uses the same exact, ordered history
/// presentation as a fresh-turn delivery.
#[test]
fn context_size_alert_prompt_steered_renders_internal_history_marker() {
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
    let visible_prompt = |text: &str| {
        Event::AgentPromptSubmitted(AgentPromptSubmitted {
            inference_activation: true,
            agent_id: agent_id("engineer_abc12345"),
            text: text.to_owned(),
            trusted_internal_spans: Vec::new(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: tau_proto::PromptSubmissionSource::HumanUi,
            display_name: None,
            ctx_id: None,
        })
    };
    renderer.handle(&visible_prompt("before steered alert"));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: true,
        submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
        agent_id: agent_id("engineer_abc12345"),
        text: "compact after tools".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::Internal,
        internal_kind: Some(tau_proto::InternalPromptKind::ContextSizeAlert),
        ctx_id: None,
    }));
    renderer.handle(&visible_prompt("after steered alert"));
    sync(&handle);

    let relevant_lines = visible_lines(&vt, 100)
        .into_iter()
        .map(|line| line.trim_end().to_owned())
        .filter(|line| line.contains("steered alert") || line.contains("compact after tools"))
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_lines,
        [
            "> before steered alert",
            "□ compact after tools",
            "> after steered alert",
        ]
    );
}

#[test]
fn shell_progress_routes_to_command_owner_after_agent_switch() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent(agent_id("worker-1"));
    renderer.handle(&Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: test_session_id("s1"),
        command_id: tau_proto::ShellCommandId::parse("ui-sh-1")
            .expect("test identifier must satisfy its grammar"),
        command: "printf worker-output".into(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker-1")),
    }));
    renderer.switch_agent(agent_id("main"));

    renderer.handle(&Event::ShellCommandProgress(
        tau_proto::ShellCommandProgress {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-1")
                .expect("test identifier must satisfy its grammar"),
            stream: tau_proto::ShellStream::Stdout,
            chunk: "worker-output".into(),
            target_agent_id: Some(agent_id("worker-1")),
        },
    ));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-1")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "printf worker-output".into(),
            include_in_context: false,
            target_agent_id: Some(agent_id("worker-1")),
            output: "worker-output".into(),
            exit_code: Some(0),
            cancelled: false,
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(90, "worker-output"));

    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);
    assert!(vt.screen_contains(90, "worker-output"));
}

#[test]
fn shell_command_target_field_survives_switch_before_echo_and_replay() {
    let (_term, handle, vt) = setup(90, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.switch_agent(agent_id("main"));

    // Regression: the durable event's target must own the command even if the
    // selected transcript is main by the time the renderer processes the echo.
    renderer.handle(&Event::UiShellCommand(tau_proto::UiShellCommand {
        session_id: test_session_id("s1"),
        command_id: tau_proto::ShellCommandId::parse("ui-sh-race")
            .expect("test identifier must satisfy its grammar"),
        command: "printf race-output".into(),
        include_in_context: false,
        target_agent_id: Some(agent_id("worker-1")),
    }));
    renderer.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-race")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "printf race-output".into(),
            include_in_context: false,
            target_agent_id: Some(agent_id("worker-1")),
            output: "race-output".into(),
            exit_code: Some(0),
            cancelled: false,
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(90, "race-output"));

    renderer.switch_agent(agent_id("worker-1"));
    sync(&handle);
    assert!(vt.screen_contains(90, "race-output"));

    let (_term, handle, vt) = setup(90, 24);
    let mut replay = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    replay.handle(&Event::SessionStarted(tau_proto::SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    replay.handle(&Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: tau_proto::ShellCommandId::parse("ui-sh-replay")
                .expect("test identifier must satisfy its grammar"),
            session_id: test_session_id("s1"),
            command: "printf replay-output".into(),
            include_in_context: true,
            target_agent_id: Some(agent_id("worker-1")),
            output: "replay-output".into(),
            exit_code: Some(0),
            cancelled: false,
        },
    ));
    sync(&handle);
    assert!(!vt.screen_contains(90, "replay-output"));

    replay.switch_agent(agent_id("worker-1"));
    sync(&handle);
    assert!(vt.screen_contains(90, "replay-output"));
}

/// `:role` candidates keep only their model and effort beside the role name,
/// while `:new` and subsequent role-setting completions retain the metadata
/// needed to inspect and edit the configured policy.
#[test]
fn role_completion_labels_stay_compact_without_hiding_role_settings() {
    let (_term, handle, _vt) = setup(80, 24);
    let completion_data = tau_cli_term::CompletionData::new();
    let mut renderer = EventRenderer::new(handle, completion_data.clone(), cli_test_theme());
    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "unused structured details".to_owned(),
            role_description: Some("production implementation".to_owned()),
            details: Some(tau_proto::HarnessRoleDetails {
                inference_compaction: None,
                compactions: Vec::new(),
                model: Some("provider/model".into()),
                params: tau_proto::ModelParams {
                    effort: tau_proto::ReasoningSelection::native(NativeReasoningEffort::High),
                    verbosity: Verbosity::Low,
                    thinking_summary: ThinkingSummary::Concise,
                    service_tier: Some(ServiceTier::Fast),
                },
                tools: Some(vec![tau_proto::ToolName::new("read")]),
                enable_tool_groups: vec![tau_proto::ToolGroupName::new("pim")],
                disable_tool_groups: vec![tau_proto::ToolGroupName::new("shell")],
                enable_tools: vec![tau_proto::ToolName::new("web_search")],
                disable_tools: vec![tau_proto::ToolName::new("shell")],
            }),
        }],
        groups: Vec::new(),
        custom_prompts: Vec::new(),
    }));

    let role_candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(
            ":role",
            "configure role",
        )],
        &completion_data,
        ":role eng",
        ":role eng".len(),
    );
    assert_eq!(role_candidates.len(), 1);
    assert_eq!(role_candidates[0].label, "engineer");
    assert_eq!(
        role_candidates[0].description,
        "provider/model e=0.75→high — production implementation"
    );
    assert_eq!(role_candidates[0].replacement, ":role engineer");

    let new_role_candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(":new", "new agent")],
        &completion_data,
        ":new eng",
        ":new eng".len(),
    );
    assert_eq!(new_role_candidates.len(), 1);
    assert_eq!(
        new_role_candidates[0].description,
        "provider/model e=0.75→high v=low ts=concise st=fast tools=read etg=pim dtg=shell et=web_search dt=shell — production implementation"
    );

    let tool_setting_candidates = tau_cli_term::completion::build_candidates(
        &[tau_cli_term::CommandCompletion::new(
            ":role",
            "configure role",
        )],
        &completion_data,
        ":role engineer ",
        ":role engineer ".len(),
    );
    let labels_and_descriptions = tool_setting_candidates
        .iter()
        .filter(|candidate| {
            matches!(
                candidate.label.as_str(),
                "tools"
                    | "enable-tool-groups"
                    | "disable-tool-groups"
                    | "enable-tools"
                    | "disable-tools"
            )
        })
        .map(|candidate| (candidate.label.as_str(), candidate.description.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        labels_and_descriptions,
        vec![
            ("tools", "read"),
            ("enable-tool-groups", "pim"),
            ("disable-tool-groups", "shell"),
            ("enable-tools", "web_search"),
            ("disable-tools", "shell"),
        ]
    );
}

/// A self-`compact` call and its private standalone transaction must share one
/// evolving tool row with canonical `ok` success before and after the generic
/// background terminal owns the final result.
#[test]
fn self_compaction_reuses_its_tool_row_through_background_completion() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));
    renderer.apply_setting("show-tools", "compact");
    let mut tool_start = tool_started("call-self", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-self", "call-self"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started("cr-self", "call-self", "ct-self", "ap-self"),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-self"),
    ));
    sync(&handle);

    let progress = vt.screen_text(100).join("\n");
    assert!(progress.contains("Compacting…"), "{progress}");
    assert_eq!(progress.matches("Compacting…").count(), 1, "{progress}");

    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::TokenCount::new(226_200)),
        compaction_output_tokens: Some(tau_proto::TokenCount::new(4_500)),
        agent_id: agent_id("main"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-self")
                .expect("known-safe transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-self")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: Vec::new(),
    }));
    sync(&handle);
    let compacted = vt.screen_text(100).join("\n");
    assert!(compacted.contains("#226.2k → ? 0s ok"), "{compacted}");
    assert!(!compacted.contains("complete"), "{compacted}");

    renderer.handle(&Event::ToolBackgroundResult(ToolBackgroundResult {
        call_id: "call-self".into(),
        tool_name: tau_proto::ToolName::new("compact"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let completed = vt.screen_text(100).join("\n");
    assert!(completed.contains("#226.2k → ? 0s ok"), "{completed}");
    assert!(!completed.contains("complete"), "{completed}");
    assert!(!completed.contains("Compacting…"), "{completed}");

    renderer.handle(&Event::AgentInferenceDispatchStarted(
        compaction_continuation_started("main", "ct-self", "ap-after-self"),
    ));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("ap-after-self", "main", 80_189, 0, 100, "continued"),
    ));
    sync(&handle);

    let measured = vt.screen_text(100).join("\n");
    assert!(
        measured.contains("#226.2k → #80.1k (35%) ok 0s"),
        "{measured}"
    );
    assert!(!measured.contains("#4.5k"), "{measured}");
}

/// The standalone row must remain unknown until the exact transaction-owned
/// continuation finishes, then repaint without using generated-item usage.
#[test]
fn standalone_compaction_repaints_from_owned_continuation_usage_only() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        standalone_compaction_started("ct-measure", "ap-compact"),
    ));
    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::TokenCount::new(130_772)),
        compaction_output_tokens: Some(tau_proto::TokenCount::new(2_549)),
        agent_id: agent_id("main"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-measure")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-compact")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: Vec::new(),
    }));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("unrelated", "main", 9_999, 0, 1, "unrelated"),
    ));
    sync(&handle);

    let unknown = vt.screen_text(100).join("\n");
    assert!(unknown.contains("compact #130.7k → ? ok"), "{unknown}");
    assert!(!unknown.contains("#2.5k"), "{unknown}");
    assert!(!unknown.contains("#9.9k"), "{unknown}");

    renderer.handle(&Event::AgentInferenceDispatchStarted(
        compaction_continuation_started("main", "ct-measure", "ap-continuation"),
    ));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("ap-continuation", "main", 80_189, 11_392, 100, "continued"),
    ));
    sync(&handle);

    let measured = vt.screen_text(100).join("\n");
    assert!(
        measured.contains("compact #130.7k → #80.1k (61%) ok"),
        "{measured}"
    );
}

/// Hidden-agent compaction accounting must fold inside that detachable
/// transcript and reconstruct the same measured row when selected later.
#[test]
fn hidden_compaction_continuation_repaints_owning_detached_transcript() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));
    let mut started = standalone_compaction_started("ct-hidden", "ap-hidden-compact");
    started.agent_id = agent_id("worker");
    renderer.handle(&Event::AgentStandaloneCompactionStarted(started));
    renderer.handle(&Event::AgentCompacted(AgentCompacted {
        original_input_tokens: Some(tau_proto::TokenCount::new(100_158)),
        compaction_output_tokens: Some(tau_proto::TokenCount::new(2_243)),
        agent_id: agent_id("worker"),
        transaction_id: Some(
            tau_proto::CompactionTransactionId::parse("ct-hidden")
                .expect("known-safe compaction transaction id"),
        ),
        cut: Some(tau_proto::AgentHead::Root),
        suffix_end: Some(tau_proto::AgentHead::Root),
        compact_prompt_id: Some(test_agent_prompt_id("ap-hidden-compact")),
        model: Some("test/model".parse().expect("model id")),
        operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
        replacement_window: Vec::new(),
    }));
    renderer.handle(&Event::AgentInferenceDispatchStarted(
        compaction_continuation_started("worker", "ct-hidden", "ap-hidden-after"),
    ));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage(
            "ap-hidden-after",
            "worker",
            79_766,
            0,
            100,
            "hidden continuation",
        ),
    ));
    sync(&handle);
    assert!(!vt.screen_contains(100, "compact #100.1k"));

    renderer.switch_agent(agent_id("worker"));
    sync(&handle);
    let attached = vt.screen_text(100).join("\n");
    assert!(
        attached.contains("compact #100.1k → #79.7k (80%) ok"),
        "{attached}"
    );
    assert!(!attached.contains("#2.2k"), "{attached}");
}

/// Provider-prompt fallback must clear activity on terminal without removing
/// the watched row.
///
/// This covers backends or replay paths that omit `agent.prompt_started` before
/// provider work, preventing their terminal event from looking like task done.
#[test]
fn watched_agent_provider_prompt_terminal_keeps_status_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.switch_agent(agent_id("parent_1"));
    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "delegate-1".to_owned(),
        agent_id: agent_id("engineer_1"),
    }));
    renderer.handle(&Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: test_session_id("s1"),
            watcher_id: agent_id("parent_1"),
            watched_agent_ids: vec![agent_id("engineer_1")],
            changed_agent_id: Some(agent_id("engineer_1")),
            cause: tau_proto::AgentWatchUpdateCause::AgentStart,
        },
    ));
    renderer.handle(&Event::ProviderPromptSubmitted(
        tau_proto::ProviderPromptSubmitted {
            agent_prompt_id: test_agent_prompt_id("ap-engineer_1-0"),
            originator: tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("__harness__")
                    .expect("test identifier must satisfy its grammar"),
                query_id: "delegate-1".to_owned(),
            },
        },
    ));
    sync(&handle);

    assert!(eventually_screen_contains(&vt, 100, "❓✨ @engineer_1",));

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
        compaction_output_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);

    assert!(
        vt.screen_contains(100, "❓💤 @engineer_1"),
        "provider-fallback terminal should retain the watched status row: {:?}",
        vt.screen_text(100)
    );
}

/// Ensures the now-immediate `agent_start` completion remains informative even
/// though the child agent's final answer is delivered later through
/// `agent_watch` notifications.
#[test]
fn immediate_agent_start_completion_shows_agent_stats_and_standard_status() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&tool_started(
        "delegate-call",
        "agent_start",
        CborValue::Map(Vec::new()),
    ));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "delegate-call".into(),
        tool_name: tau_proto::ToolName::new("agent_start"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(Vec::new()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "[audit]".into(),
            stats: tau_proto::ToolUseStats {
                matches: None,
                lines: Some(2),
                bytes: Some(12),
            },
            info_chips: vec!["@engineer_child".into()],
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let rows = vt.screen_text(100);
    let line = rows
        .iter()
        .find(|row| row.contains("agent_start"))
        .expect("agent_start completion line");
    assert!(
        line.contains("agent_start [audit] 2L, 12B @engineer_child"),
        "immediate agent_start completion should include spawned id and prompt size: {rows:?}",
    );
    assert!(
        line.contains("ok"),
        "immediate agent_start completion should use the standard success status: {rows:?}",
    );
}

/// Ensures shell-command duration chips retain both an explicit timeout and the
/// provider's effective default after the running call becomes a history row.
#[test]
fn shell_command_duration_shows_effective_timeout() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started(
            "shell-default",
            "gpt_shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text("sleep 300".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "shell-default".into(),
            tool_name: tau_proto::ToolName::new("gpt_shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "sleep 300".into(),
                mode: "rw".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(207_000_000),
    );

    renderer.handle_recorded_at(
        &tool_started(
            "shell-explicit",
            "gpt_shell",
            CborValue::Map(vec![
                (
                    CborValue::Text("command".into()),
                    CborValue::Text("sleep 400".into()),
                ),
                (
                    CborValue::Text("timeout".into()),
                    CborValue::Integer(300.into()),
                ),
            ]),
        ),
        tau_proto::UnixMicros::new(210_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "shell-explicit".into(),
            tool_name: tau_proto::ToolName::new("gpt_shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "sleep 400".into(),
                mode: "rw".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(416_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "gpt_shell rw sleep 300 206/300s ok"));
    assert!(vt.screen_contains(100, "gpt_shell rw sleep 400 206/300s ok"));
}

/// Regression coverage for multiline `shell` calls in `show-tools=full`:
/// the running block must already reserve/show the command body, matching the
/// final result block and avoiding a layout jump when the command finishes.
#[test]
fn running_shell_tool_shows_multiline_command_body_in_full_mode() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let command = "printf hello\nprintf world";

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".into()),
                    CborValue::Text(command.into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text(command.into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            message: None,
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "printf hello".to_owned(),
                mode: "rw".to_owned(),
                status: tau_proto::ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: command.to_owned(),
                }),
                ..Default::default()
            }),
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "shell rw printf hello"));
    assert!(
        vt.screen_text(100)
            .iter()
            .any(|row| row.trim() == "printf world"),
        "running shell command body should be on its own row"
    );

    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "rw printf hello".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: command.into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "shell rw printf hello 1/300s ok"));
    assert!(
        vt.screen_text(100)
            .iter()
            .any(|row| row.trim() == "printf world"),
        "finished shell command body should stay on its own row"
    );
}

#[test]
fn show_tools_summarize_prompt_aggregates_across_tool_followups() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "summarize-prompt");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            stats: tau_proto::ToolUseStats {
                matches: None,
                lines: Some(1),
                bytes: Some(13),
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 1/1 1L, 13B ok: 1"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-2".into(),
            name: tau_proto::ToolName::new("grep"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("pattern".into()),
                CborValue::Text("foo".into()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 1/2 1L, 13B ok: 1 …"));
    assert!(!vt.screen_contains(80, "tools 1/1"));
    assert!(!vt.screen_contains(80, "grep foo"));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "foo".into(),
            stats: tau_proto::ToolUseStats {
                matches: Some(3),
                lines: None,
                bytes: None,
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 2/2 3, 1L, 13B ok: 2"));
    assert!(!vt.screen_contains(80, "read src/main.rs 1L, 13B ok"));
    assert!(!vt.screen_contains(80, "grep foo (3 matches) ok"));
}

#[test]
fn delegate_completion_keeps_input_stats_with_output_stats() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    let cached = ToolUseState {
        args: "[audit]".into(),
        stats: ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let display =
        build_delegate_completion_display(Some(&cached), &CborValue::Text("ok\nmore".into()), None);

    assert_eq!(display.args, "[audit]");
    assert_eq!(display.stats, ToolUseStats::for_text("ok\nmore"));
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn delegate_completion_uses_output_stats_from_duration_result_map() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    let cached = ToolUseState {
        args: "[audit]".into(),
        stats: ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };
    let details = CborValue::Map(vec![
        (
            CborValue::Text("output".into()),
            CborValue::Text("ok\nmore".into()),
        ),
        (
            CborValue::Text("duration_seconds".into()),
            CborValue::Integer(6.into()),
        ),
    ]);

    let display = build_delegate_completion_display(Some(&cached), &details, None);

    assert_eq!(display.args, "[audit]");
    assert_eq!(display.stats, ToolUseStats::for_text("ok\nmore"));
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn delegate_completion_keeps_input_stats_for_empty_output() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    let cached = ToolUseState {
        args: "[audit]".into(),
        stats: ToolUseStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let display =
        build_delegate_completion_display(Some(&cached), &CborValue::Text(String::new()), None);

    assert_eq!(display.stats, ToolUseStats::default());
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolUseStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn render_shell_block_abbreviates_inline_command_and_status_but_preserves_output() {
    let command = "printf 1234567890123456789012345678901234567890";
    let status = "err: command failed after printing a very long diagnostic";
    let output = "full output line one\nfull output line two";
    let block = render_shell_block(&cli_test_theme(), command, output, Some(status));
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains("printf 1234567890123┄12345678901234567890"));
    assert!(text.contains("err: command failed ┄very long diagnostic"));
    assert!(!text.contains(status));
    assert!(text.contains(output));
}

#[test]
fn format_turn_stats_line_shows_zero_hit_when_no_prompt_sent() {
    let usage = tau_proto::ProviderTokenUsage::default();
    let line = format_turn_stats_line(&usage, None, None, None);

    assert_eq!(line, "Δ—? 0/0? ↑0 ↓0 Σ↑0/0 ↓0");
}

/// Context-size magnitudes retain useful precision at small scales, round
/// half-up at each width boundary, and promote instead of growing past three
/// numeric columns.
#[test]
fn context_size_formatter_rounds_and_promotes_within_three_numeric_columns() {
    for (tokens, expected) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1.0k"),
        (4_600, "4.6k"),
        (9_949, "9.9k"),
        (9_950, "10k"),
        (99_499, "99k"),
        (99_500, "100k"),
        (353_400, "353k"),
        (999_499, "999k"),
        (999_500, "1.0m"),
    ] {
        assert_eq!(format_context_token_count(tokens), expected);
    }

    for (unit, suffix, next_suffix) in [
        (1_000_000, "m", "b"),
        (1_000_000_000, "b", "t"),
        (1_000_000_000_000, "t", "q"),
    ] {
        let small_boundary = 10 * unit - unit / 20;
        let medium_boundary = 100 * unit - unit / 2;
        let promotion_boundary = 1_000 * unit - unit / 2;
        assert_eq!(
            format_context_token_count(small_boundary),
            format!("10{suffix}")
        );
        assert_eq!(
            format_context_token_count(medium_boundary),
            format!("100{suffix}")
        );
        assert_eq!(
            format_context_token_count(promotion_boundary),
            format!("1.0{next_suffix}")
        );
    }

    for tokens in [
        1_000,
        9_950,
        99_500,
        999_500,
        9_950_000,
        99_500_000,
        999_500_000,
        u64::MAX,
    ] {
        let rendered = format_context_token_count(tokens);
        let numeric = rendered.trim_end_matches(|character: char| character.is_ascii_alphabetic());
        assert!(numeric.chars().count() <= 3, "{rendered}");
    }
}

/// Extension ready/kept messages are informational lifecycle notices, so a
/// warning threshold should keep them out of live startup preambles.
#[test]
fn warning_notice_level_hides_routine_extension_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("notice-level", "warning");

    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 1.into(),
        extension_name: tau_proto::ExtensionName::parse("core-shell")
            .expect("test identifier must satisfy its grammar"),
        pid: Some(123),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s2"),
        reason: SessionStartReason::Initial,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "extension core-shell"));
}

#[test]
fn model_status_uses_symbol_prefixed_chips() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams {
            verbosity: Verbosity::High,
            ..Default::default()
        },
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("tau-agent-test"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(353_400),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(80)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row");
    assert!(status_row.starts_with("+engineer ~high"));
    assert!(status_row.ends_with("#353k/200k"));
    assert!(!vt.screen_contains(80, "=test/model"));
    assert!(!vt.screen_contains(80, "v=high"));
    assert!(!vt.screen_contains(80, "ctx:"));
}

/// Exact status boundaries must use terminal display width and drop the context
/// chip atomically before the identity, including for a wide Unicode role name.
#[test]
fn model_status_progressively_hides_at_ascii_and_unicode_boundaries() {
    let cases = [
        (17, "engineer", "+engineer", Some("#-/200k")),
        (16, "engineer", "+engineer", None),
        (11, "界", "+界", Some("#-/200k")),
        (10, "界", "+界", None),
    ];

    for (width, role, identity, context) in cases {
        let (_term, handle, vt) = setup(width, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
            model: Some("test/model".into()),
            context_window: Some(200_000),
            role: role.into(),
            baseline_params: None,
            model_params: tau_proto::ModelParams::default(),
        }));
        sync(&handle);

        let status_row = vt
            .screen_text(width)
            .into_iter()
            .find(|row| row.contains(identity))
            .unwrap_or_else(|| panic!("missing {identity:?} status row at width {width}"));
        assert_eq!(
            status_row.contains("#-/200k"),
            context.is_some(),
            "unexpected status row at width {width}: {status_row:?}"
        );
        assert_eq!(
            status_row.trim(),
            context.map_or_else(
                || identity.to_owned(),
                |context| format!("{identity} {context}")
            )
        );
    }
}

#[test]
fn model_status_shows_context_window_until_usage_is_known() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row");
    assert!(status_row.ends_with("#-/200k"));
}

/// The selected-agent status shows the canonical ordinary-turn count, while a
/// narrow status line retains context before the lower-priority turn counter.
#[test]
fn model_status_shows_inner_turns_after_context() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "inner-turns",
        "main",
    )));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("main"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats {
            input_tokens: Some(12_000),
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: Some(6),
        },
        inner_turns_total: Some(123),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    renderer.handle(&Event::HarnessAgentContextUsageChanged(
        tau_proto::HarnessAgentContextUsageChanged {
            agent_id: agent_id("main"),
            input_tokens: Some(12_000),
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: Some(6),
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row");
    assert!(status_row.contains("#12k/200k *123"), "{status_row:?}");
    let status_cells = priority_header_cells(&renderer.build_model_status_block(), 100);
    let context_start = status_cells
        .iter()
        .position(|cell| cell.ch == '#')
        .expect("context status chip");
    let inner_turn_start = status_cells
        .iter()
        .position(|cell| cell.ch == '*')
        .expect("inner-turn status chip");
    assert_eq!(
        status_cells[context_start].style,
        tau_cli_term::resolve::resolve(&cli_test_theme(), tau_themes::names::STATUS_CONTEXT)
    );
    assert_eq!(
        status_cells[inner_turn_start].style,
        tau_cli_term::resolve::resolve(&cli_test_theme(), tau_themes::names::STATUS_INNER_TURNS)
    );
    assert_ne!(
        status_cells[context_start].style, status_cells[inner_turn_start].style,
        "the test theme keeps inner-turn styling independent from context"
    );

    let (_term, narrow_handle, narrow_vt) = setup(24, 24);
    let mut narrow_renderer = EventRenderer::new(
        narrow_handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    narrow_renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: tau_proto::SessionStartReason::Initial,
    }));
    narrow_renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "inner-turns-narrow",
        "main",
    )));
    narrow_renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("main"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats {
            input_tokens: Some(12_000),
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: Some(6),
        },
        inner_turns_total: Some(123),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    narrow_renderer.handle(&Event::HarnessAgentContextUsageChanged(
        tau_proto::HarnessAgentContextUsageChanged {
            agent_id: agent_id("main"),
            input_tokens: Some(12_000),
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: Some(6),
        },
    ));
    sync(&narrow_handle);

    let narrow_status_row = narrow_vt
        .screen_text(24)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("narrow status row");
    assert!(
        narrow_status_row.contains("#12k/200k"),
        "{narrow_status_row:?}"
    );
    assert!(!narrow_status_row.contains("*123"), "{narrow_status_row:?}");
}

/// The selected creator renders independent self and inclusive descendant
/// estimates from the complete stats snapshot rather than collapsing them.
#[test]
fn model_status_shows_selected_creator_cost_pair() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptStarted(agent_prompt_started(
        "cost-sp", "main",
    )));
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("main"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats::default(),
        context: tau_proto::AgentContextStats::default(),
        inner_turns_total: None,
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: tau_proto::EstimatedApiCost::from_picodollars(
            2_140_000_000_000,
        ),
        work_status: Default::default(),
    }));
    sync(&handle);

    assert!(vt.screen_contains(40, "@main"));
    assert!(vt.screen_contains(40, "$.00/$2.1"));
}

/// Estimated cost yields to the selected-agent identity under status-line width
/// pressure instead of wrapping or clipping either element.
#[test]
fn estimated_cost_status_hides_under_width_pressure() {
    let (_term, handle, vt) = setup(13, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: test_session_id("s1"),
        reason: SessionStartReason::Initial,
    }));
    renderer.handle(&Event::AgentPromptStarted(tau_proto::AgentPromptStarted {
        agent_id: agent_id("a"),
        ..agent_prompt_started("cost-sp", "s1")
    }));
    renderer.handle(&Event::HarnessAgentContextUsageChanged(
        tau_proto::HarnessAgentContextUsageChanged {
            agent_id: agent_id("a"),
            input_tokens: None,
            cached_tokens: None,
            context_window: Some(200_000),
            percent_used: None,
        },
    ));
    sync(&handle);

    assert!(vt.screen_contains(13, "@a"));
    assert!(vt.screen_contains(13, "❓✨ @a"));
    assert!(!vt.screen_contains(13, "$"));
}

#[test]
fn delegate_side_conversation_keeps_parent_tool_status_visible() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "main-sp", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-sp",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&tool_started(
        "delegate-call",
        "agent_start",
        CborValue::Map(Vec::new()),
    ));

    renderer.handle(&Event::StartAgentAccepted(tau_proto::StartAgentAccepted {
        start_id: tau_proto::StartOperationId(1),
        query_id: "q1".to_owned(),
        agent_id: agent_id("engineer_1"),
    }));

    // A running parent `agent_start` call is the visible main-agent work while
    // the sub-agent side conversation is active. The side agent is also active
    // while its delegated request is running. Regression coverage: the side
    // prompt lifecycle must not hide `%0/1` from the status bar, because
    // otherwise users lose the only bottom-bar indication that delegation is
    // still in progress.
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        agent_id: agent_id("engineer_1"),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q1".to_owned(),
        },
        ..agent_prompt_created("side-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_id: agent_id("engineer_1"),
        ..provider_response_delta_update(
            test_agent_prompt_id("side-sp"),
            "working",
            None,
            tau_proto::PromptOriginator::Extension {
                name: tau_proto::ExtensionName::parse("core-subagents")
                    .expect("test identifier must satisfy its grammar"),
                query_id: "q1".to_owned(),
            },
        )
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row during delegate side conversation");
    assert!(status_row.ends_with("%0/1 @1 #12k/200k -/-"));

    // Generic watched-agent stats no longer mutate the parent tool status chip.
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("engineer_1"),
        navigation_mode: tau_proto::AgentNavigationMode::Active,
        runtime_state: tau_proto::AgentRuntimeState::Running,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: tau_proto::AgentToolStats {
            in_flight: 2,
            started_total: 3,
        },
        context: tau_proto::AgentContextStats::default(),
        inner_turns_total: None,
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("#12k/200k"))
        .expect("status row after watched-agent stats");
    assert!(status_row.contains("@main"));
    assert!(status_row.ends_with("%0/1 @1 #12k/200k -/-"));

    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "delegate-call".into(),
        tool_name: tau_proto::ToolName::new("agent_start"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    renderer.handle(&Event::StartAgentResult(tau_proto::StartAgentResult {
        query_id: "q1".to_owned(),
        text: String::new(),
        error: None,
    }));
    renderer.handle(&Event::AgentPromptCreated(AgentPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("core-subagents")
                .expect("test identifier must satisfy its grammar"),
            query_id: "q2".to_owned(),
        },
        ..agent_prompt_created("later-side-sp", "s1")
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("@main"))
        .expect("status row after delegate cancellation");
    assert!(status_row.ends_with("@1 #12k/200k -/-"));
}

/// Correlation must survive attach-style ordering where a standalone lifecycle
/// arrives before the reconstructed generic tool start.
#[test]
fn late_self_compaction_tool_start_adopts_retained_lifecycle_status() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));
    renderer.apply_setting("show-tools", "compact");
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-late", "call-late"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started("cr-late", "call-late", "ct-late", "ap-late"),
    ));
    let mut tool_start = tool_started("call-late", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut tool_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&tool_start);
    sync(&handle);

    let text = vt.screen_text(100).join("\n");
    assert!(text.contains("Compacting…"), "{text}");
    assert_eq!(text.matches("Compacting…").count(), 1, "{text}");
}

/// Compaction terminals must report actual request-to-continuation reduction,
/// retain unknown measurements honestly, and avoid overflow in the ratio.
#[test]
fn compaction_success_status_formats_exact_and_partial_measurements() {
    let exact = |tokens| tau_proto::TokenCount::new(tokens);

    assert_eq!(
        EventRenderer::standalone_compaction_success_status(
            Some(exact(226_200)),
            Some(exact(4_500)),
        ),
        "#226.2k → #4.5k (2%) ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(Some(exact(12_000)), None),
        "#12k → ? ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(None, Some(exact(4_500))),
        "? → #4.5k ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(Some(exact(0)), Some(exact(1))),
        "#0 → #1 ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(Some(exact(3)), Some(exact(2))),
        "#3 → #2 (67%) ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(
            Some(exact(u64::MAX)),
            Some(exact(u64::MAX)),
        ),
        "#18446744073709.5m → #18446744073709.5m (100%) ok"
    );
    assert_eq!(
        EventRenderer::standalone_compaction_success_status(None, None),
        "ok"
    );
}

#[test]
fn provider_tool_error_before_tool_started_is_ignored() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "bad-args".into(),
                name: tau_proto::ToolName::new("agent_start"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("unknown_option".into()),
                    CborValue::Text("invalid".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "delegate 0s …"));

    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            presentation: Default::default(),
            call_id: "bad-args".into(),
            tool_name: tau_proto::ToolName::new("agent_start"),
            tool_type: tau_proto::ToolType::Function,
            message: "invalid arguments for tool `agent_start`".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "delegate err: invalid"));
    assert!(!vt.screen_contains(80, "delegate 0s …"));
}
/// Provider-facing errors must not finish live UI tool blocks. The harness is
/// responsible for publishing a logical `ToolError` for user-visible failures.
#[test]
fn provider_tool_error_without_logical_tool_error_does_not_finish_live_tool() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "bad-args".into(),
                name: tau_proto::ToolName::new("strict_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started("bad-args", "strict_tool", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_500_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "strict_tool 0s pending"));
    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            presentation: Default::default(),
            call_id: "bad-args".into(),
            tool_name: tau_proto::ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            message: "invalid arguments: unexpected argument `extra`".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "err: invalid"));
    assert!(vt.screen_contains(80, "strict_tool 0s pending"));
}

/// Ensures an incomplete historical tool call repaired on resume is shown as
/// explicitly uncertain/error rather than silently successful or left active.
#[test]
fn incomplete_dummy_tool_replay_is_repaired_honestly_and_not_active() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-incomplete-prompt",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "opud-incomplete".into(),
            name: tau_proto::ToolName::new("restart_test_dummy"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some("{}".to_owned()),
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: "opud-incomplete".into(),
        tool_name: tau_proto::ToolName::new("restart_test_dummy"),
        tool_type: tau_proto::ToolType::Function,
        message: "Interrupted during restart. Side effects may have occurred.".to_owned(),
        details: None,
        originator: tau_proto::PromptOriginator::User,
        display: None,
    }));
    sync(&handle);
    let row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("restart_test_dummy"))
        .expect("repaired dummy row");
    assert!(row.contains("err"), "{row}");
    assert!(!row.contains(" ok"), "{row}");
    assert!(!row.contains("pending"), "{row}");
    assert_eq!(renderer.test_active_tool_count(), 0);
}

/// A running tool call remains visibly pending until its result arrives.
#[test]
fn running_tool_call_shows_ellipsis_until_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("call-1", "read", "src/main.rs", ""),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs"));
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Map(vec![
                (
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                ),
                (
                    CborValue::Text("content".into()),
                    CborValue::Text("fn main() {}\n".into()),
                ),
            ]),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "src/main.rs".into(),
                stats: tau_proto::ToolUseStats {
                    matches: None,
                    lines: Some(1),
                    bytes: Some(13),
                },
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(3_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 1L, 13B 2s ok"));
    assert!(!vt.screen_contains(80, "read src/main.rs …"));
}

#[test]
fn tool_progress_display_replaces_live_state_generically() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("call-1", "dir_lock", CborValue::Map(vec![])),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("dir_lock"),
            message: None,
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "update /tmp/project".into(),
                info_chips: vec!["dir lock".into()],
                status: tau_proto::ToolUseStatus::InProgress,
                status_text: "waiting".into(),
                ..Default::default()
            }),
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );

    // Regression: ToolProgress.display is a complete ToolUseState replacement.
    // The renderer must preserve generic stats/counters/chips/status instead of
    // treating progress as just a name/args/ellipsis header.
    sync(&handle);
    assert!(vt.screen_contains(80, "dir_lock update /tmp/project"));
    assert!(vt.screen_contains(80, "dir lock"));
    assert!(vt.screen_contains(80, "waiting"));
}

/// An exact canonical progress replacement must not rebuild the live block or
/// wake the terminal, while the next visible state change does each once.
#[test]
fn exact_tool_progress_noop_skips_block_replacement_and_redraw() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "full");
    renderer.handle(&tool_started("call-1", "read", CborValue::Map(vec![])));

    let progress = |status_text: &str| {
        Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            message: Some("still running".to_owned()),
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "src/main.rs".into(),
                status: tau_proto::ToolUseStatus::InProgress,
                status_text: status_text.to_owned(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: "line 1\nline 2".into(),
                }),
                ..Default::default()
            }),
        })
    };

    renderer.handle(&progress("reading"));
    sync(&handle);
    let replacements = renderer.block_replacement_count_for_test();
    let redraws = renderer.redraw_request_count_for_test();

    renderer.handle(&progress("reading"));
    assert_eq!(renderer.block_replacement_count_for_test(), replacements);
    assert_eq!(renderer.redraw_request_count_for_test(), redraws);

    renderer.handle(&progress("checking"));
    assert_eq!(
        renderer.block_replacement_count_for_test(),
        replacements + 1
    );
    assert_eq!(renderer.redraw_request_count_for_test(), redraws + 1);
    sync(&handle);
    assert!(vt.screen_contains(80, "checking"));
}

#[test]
fn backgrounded_tool_stays_visibly_running_until_background_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![
                    (
                        CborValue::Text("command".into()),
                        CborValue::Text("sleep 10".into()),
                    ),
                    (CborValue::Text("mode".into()), CborValue::Text("ro".into())),
                ]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![
                (
                    CborValue::Text("command".into()),
                    CborValue::Text("sleep 10".into()),
                ),
                (CborValue::Text("mode".into()), CborValue::Text("ro".into())),
            ]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress("call-1", "shell", "sleep 10", "ro"),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ProviderToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text(
                "tau_internal: true\n\nTool call `call-1` is running in the background.".into(),
            ),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "shell ro sleep 10"));
    assert!(!vt.screen_contains(80, "shell 1s ok"));
    assert!(vt.screen_contains(80, "0/1"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-final",
        vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "done for now".into(),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "0/1"));

    renderer.handle_recorded_at(
        &Event::ToolBackgroundResult(ToolBackgroundResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".into()),
            display: Some(tau_proto::ToolUseState {
                args: "ro sleep 10".into(),
                status: tau_proto::ToolUseStatus::Error,
                status_text: "false-error".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(4_000_000),
    );
    sync(&handle);
    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "shell ro sleep 10 3/300s ok"));
    assert!(vt.screen_contains(80, "1/1"));
}

#[test]
fn finished_tool_result_preserves_message_and_tool_item_order() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            assistant_message_item("before tool"),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            assistant_message_item("after tool"),
        ],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let lines = vt.screen_text(100);
    let before = lines
        .iter()
        .position(|line| line.contains("before tool"))
        .unwrap_or_else(|| panic!("missing first message: {lines:?}"));
    let tool = lines
        .iter()
        .position(|line| line.contains("read src/main.rs"))
        .unwrap_or_else(|| panic!("missing tool call: {lines:?}"));
    let after = lines
        .iter()
        .position(|line| line.contains("after tool"))
        .unwrap_or_else(|| panic!("missing second message: {lines:?}"));
    assert!(
        before < tool && tool < after,
        "output_items order should be preserved; lines: {lines:?}",
    );
}

#[test]
fn live_tool_timer_updates_do_not_mutate_scrolled_history() {
    // Running tool calls live in the fixed active-tools area above the prompt.
    // Timer ticks should therefore repaint that visible area only, not trigger a
    // hidden-prefix full redraw of old transcript rows that have moved to
    // scrollback.
    let (_term, handle, vt) = setup(80, 5);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-history",
        (0..10)
            .map(|i| assistant_message_item(format!("history line {i}")))
            .collect(),
    )));
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text("src/main.rs".into()),
    )]);
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-tool",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: read_args.clone(),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&tool_started("call-1", "read", read_args));
    renderer.handle(&initial_tool_progress("call-1", "read", "src/main.rs", ""));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs"));

    let full_renders_before = handle.full_render_count();
    renderer.handle_tool_timer_tick();
    sync(&handle);

    assert_eq!(
        handle.full_render_count(),
        full_renders_before,
        "live timer ticks must not full-redraw hidden transcript rows",
    );
    assert!(vt.screen_contains(80, "read src/main.rs"));
}

#[test]
fn live_multiline_payload_tool_uses_static_duration_placeholder() {
    // Multi-line live tool payloads can extend above the visible active-tools
    // area. Updating only the elapsed seconds would force visible churn without
    // changing useful content, so keep the live duration stable until completion.
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "compact");
    let args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text("src/main.rs".into()),
    )]);
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-tool",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: args.clone(),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&tool_started("call-1", "read", args));
    renderer.handle(&Event::ToolProgress(tau_proto::ToolProgress {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        message: None,
        progress: None,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            payload: Some(tau_proto::ToolUsePayload::Text {
                text: "line 1\nline 2".into(),
            }),
            ..Default::default()
        }),
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "read src/main.rs 0s"));

    renderer.apply_setting("show-tools", "full");
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs -s"));
    assert!(vt.screen_contains(80, "line 1"));

    renderer.apply_setting("show-tools", "compact");
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 0s"));

    renderer.apply_setting("show-tools", "full");
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs -s"));
    assert!(vt.screen_contains(80, "line 1"));

    let full_renders_before = handle.full_render_count();
    renderer.handle_tool_timer_tick();
    sync(&handle);

    assert_eq!(handle.full_render_count(), full_renders_before);
    assert!(vt.screen_contains(80, "read src/main.rs -s"));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            payload: Some(tau_proto::ToolUsePayload::Text {
                text: "line 1\nline 2".into(),
            }),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "read src/main.rs 0s ok"));
}

/// Compact mode must preserve each verbose live tool header, including useful
/// arguments and elapsed time, while hiding payloads and removing every row at
/// its terminal outcome; returning to verbose mode restores terminal history.
#[test]
fn verbose_mode_round_trips_thinking_and_overlapping_tool_outcomes() {
    let (_term, handle, vt) = setup(120, 30);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));
    renderer.apply_setting("show-turn-stats", "true");

    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-verbose",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-verbose"),
            "conversation answer",
            Some("private reasoning".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-verbose",
        vec![assistant_message_item("conversation answer")],
    )));
    renderer.handle(&Event::ProviderResponseFinished(
        finished_response_with_usage("sp-stats", "main", 20_000, 10_000, 500, "stats answer"),
    ));

    for (call_id, tool_name, args) in [
        ("call-ok", "read", "READ_ARGUMENT"),
        ("call-error", "search", "SEARCH_ARGUMENT"),
        ("call-cancel", "write", "WRITE_ARGUMENT"),
        ("call-wait", "wait", "60m"),
    ] {
        let mut started = tool_started(call_id, tool_name, CborValue::Null);
        let Event::ToolStarted(started_event) = &mut started else {
            unreachable!("tool_started helper returns a tool start");
        };
        started_event.agent_id = agent_id("main");
        renderer.handle(&started);
        renderer.handle(&initial_tool_progress(
            call_id,
            tool_name,
            args,
            if tool_name == "wait" { "" } else { "LIVE_MODE" },
        ));
    }
    renderer.handle(&Event::ToolProgress(tau_proto::ToolProgress {
        call_id: "call-ok".into(),
        tool_name: tau_proto::ToolName::new("read"),
        message: None,
        progress: None,
        display: Some(tau_proto::ToolUseState {
            args: "READ_ARGUMENT".to_owned(),
            mode: "LIVE_MODE".to_owned(),
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            payload: Some(tau_proto::ToolUsePayload::Text {
                text: "PRIVATE_LIVE_PAYLOAD".to_owned(),
            }),
            ..Default::default()
        }),
    }));
    renderer.handle_tool_timer_tick();
    sync(&handle);

    let verbose = vt.screen_text(120).join("\n");
    assert!(verbose.contains("PRIVATE_LIVE_PAYLOAD"), "{verbose}");
    let verbose_headers =
        ["READ_ARGUMENT", "SEARCH_ARGUMENT", "WRITE_ARGUMENT", "60m"].map(|argument| {
            verbose
                .lines()
                .find(|line| line.contains(argument))
                .unwrap_or_else(|| panic!("missing verbose header for {argument}: {verbose}"))
                .trim()
                .to_owned()
        });
    assert!(
        verbose_headers[3].contains("wait 60m"),
        "{}",
        verbose_headers[3]
    );

    renderer.toggle_verbose_mode();
    sync(&handle);

    let compact = vt.screen_text(120).join("\n");
    assert!(compact.contains("conversation answer"), "{compact}");
    assert!(!compact.contains("private reasoning"), "{compact}");
    assert!(!compact.contains('Δ'), "{compact}");
    assert!(!compact.contains("PRIVATE_LIVE_PAYLOAD"), "{compact}");
    for header in verbose_headers {
        assert!(
            compact.lines().any(|line| line.trim() == header),
            "compact mode changed verbose live header {header:?}: {compact}"
        );
    }
    for tool_name in ["read", "search", "write", "wait"] {
        assert_eq!(compact.matches(tool_name).count(), 1, "{compact}");
    }

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-ok".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("SECRET_RESULT".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "SECRET_ARGUMENT".to_owned(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ToolError(ToolError {
        presentation: Default::default(),
        call_id: "call-error".into(),
        tool_name: tau_proto::ToolName::new("search"),
        tool_type: tau_proto::ToolType::Function,
        message: "SECRET_ERROR".to_owned(),
        details: None,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        presentation: Default::default(),
        call_id: "call-cancel".into(),
        tool_name: tau_proto::ToolName::new("write"),
        tool_type: tau_proto::ToolType::Function,
        display: None,
    }));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-wait".into(),
        tool_name: tau_proto::ToolName::new("wait"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "60m".to_owned(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".to_owned(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let completed_compact = vt.screen_text(120).join("\n");
    for hidden in [
        "read",
        "search",
        "write",
        "wait",
        "SECRET_RESULT",
        "SECRET_ERROR",
        "READ_ARGUMENT",
        "SEARCH_ARGUMENT",
        "WRITE_ARGUMENT",
    ] {
        assert!(!completed_compact.contains(hidden), "{completed_compact}");
    }

    renderer.toggle_verbose_mode();
    sync(&handle);
    let restored = vt.screen_text(120).join("\n");
    assert!(restored.contains("private reasoning"), "{restored}");
    assert!(restored.contains('Δ'), "{restored}");
    assert!(restored.contains("read"), "{restored}");
    assert!(restored.contains("search"), "{restored}");
    assert!(restored.contains("write"), "{restored}");
    assert!(restored.contains("wait"), "{restored}");
    assert!(restored.contains("SECRET_ERROR"), "{restored}");
}

#[test]
fn show_tools_summarize_turn_summarizes_tool_batch() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "summarize-turn");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-2".into(),
                name: tau_proto::ToolName::new("grep"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("pattern".into()),
                    CborValue::Text("foo".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            }),
        ],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 0/2 …"));
    assert!(!vt.screen_contains(80, "read src/main.rs"));

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "src/main.rs".into(),
            stats: tau_proto::ToolUseStats {
                matches: None,
                lines: Some(1),
                bytes: Some(13),
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ToolError(tau_proto::ToolError {
        presentation: Default::default(),
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        message: "nope".into(),
        details: None,
        display: Some(tau_proto::ToolUseState {
            args: "foo".into(),
            status: tau_proto::ToolUseStatus::Error,
            status_text: "err: nope".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 2/2 1L, 13B ok: 1 err: 1"));
    assert!(!vt.screen_contains(80, "read src/main.rs 1L, 13B ok"));
    assert!(!vt.screen_contains(80, "grep foo err: nope"));
}

#[test]
fn show_tools_compact_hides_payload_body() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "compact");

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "src/main.rs".into(),
                stats: tau_proto::ToolUseStats {
                    matches: None,
                    lines: Some(1),
                    bytes: Some(13),
                },
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: "fn main() {}\n".into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 1L, 13B 0s ok"));
    assert!(!vt.screen_contains(80, "fn main()"));
}

/// A bounded one-line tool header keeps its identity, status, and timing while
/// full mode reveals the exact Unicode payload and compact mode hides it.
#[test]
fn show_tools_full_reveals_truncated_one_line_payload() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "full");
    let payload = "αβγδεζηθικλμνξοπρστυφχψω一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";
    let args = "αβγδεζηθικλμνξοπρστυ┄一二三四五六七八九十甲乙丙丁戊己庚辛壬癸";

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".into()),
                    CborValue::Text(payload.into()),
                )]),
                raw_arguments_json: None,
                responses_envelope: None,
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text(payload.into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: args.into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: payload.into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);

    let header = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("shell "))
        .expect("bounded shell header");
    assert!(header.contains('┄'), "{header:?}");
    assert!(header.contains(" 1/300s ok"), "{header:?}");
    assert!(!header.contains(args), "{header:?}");
    assert!(
        vt.screen_text(100).iter().any(|row| row.trim() == payload),
        "full Unicode payload should render beneath the compact header"
    );

    renderer.apply_setting("show-tools", "compact");
    sync(&handle);
    let header = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("shell "))
        .expect("bounded compact shell header");
    assert!(header.contains('┄'), "{header:?}");
    assert!(header.contains(" 1/300s ok"), "{header:?}");
    assert!(
        !vt.screen_text(100).iter().any(|row| row.trim() == payload),
        "compact mode should continue hiding payload bodies"
    );
}

#[test]
fn websearch_tool_result_shows_result_count_and_size() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-web".into(),
        tool_name: tau_proto::ToolName::new("websearch_exa"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(
            "Title: One\nURL: https://one.example\n\nTitle: Two\nURL: https://two.example\n".into(),
        ),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: String::new(),
            stats: tau_proto::ToolUseStats {
                matches: Some(2),
                lines: Some(193),
                bytes: Some(7370),
            },
            status: tau_proto::ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "websearch_exa 2, 193L, 7.2kB ok"));
}

/// A status descriptor keeps its state and title visible beside terminal
/// duration/outcome metadata, and truncates the title before hiding the
/// outcome.
#[test]
fn status_tool_header_preserves_semantics_and_outcome_when_narrow() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let mut display = render_tool_use_state(
        "status",
        &ToolUseState {
            args: "working: implementing focused renderer regression coverage".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let status_index = display.suffixes.len().saturating_sub(1);
    display.suffixes.insert(
        status_index,
        crate::tool_render::tool_duration_suffix(path_std_time::Duration::from_secs(12)),
    );
    let block = render_tool_block(&cli_test_theme(), &display);

    assert_eq!(
        priority_header_text(&block, 100),
        "status working: implementing fo┄rer regression coverage 12s ok"
    );
    assert_eq!(
        priority_header_text(&block, 25),
        "status worki┄erage 12s ok"
    );
    assert_eq!(priority_header_text(&block, 9), "status ok");
}

/// Tool-line elements retain their documented truncation order, including the
/// task-title band that yields after a display name and before telemetry.
#[test]
fn tool_line_priorities_cover_every_element() {
    let priorities = [
        (ToolLineElement::Identity, 0),
        (ToolLineElement::ResultStatus, 10),
        (ToolLineElement::ErrorDetails, 20),
        (ToolLineElement::Arguments, 30),
        (ToolLineElement::AgentId, 40),
        (ToolLineElement::Mode, 50),
        (ToolLineElement::Range, 60),
        (ToolLineElement::Counter, 70),
        (ToolLineElement::WorkTitle, 75),
        (ToolLineElement::Info, 80),
        (ToolLineElement::Duration, 90),
    ];

    for (element, expected) in priorities {
        assert_eq!(element.priority().get(), expected);
    }
}

/// Exact narrow boundaries must retain minimum middle-truncated arguments and
/// agent ids, then drop them by priority while keeping `err` atomic and
/// visible.
#[test]
fn tool_error_line_degrades_at_exact_priority_boundaries() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "extraordinarily_long_tool",
        &ToolUseState {
            mode: "read-write-mode".into(),
            args: "arguments-abcdefghijklmnopqrstuvwxyz".into(),
            range: Some(ToolUseRange {
                start: Some("2026-01-01".into()),
                end: Some("2026-12-31".into()),
            }),
            info_chips: vec![
                "@agent-abcdefghijklmnopqrstuvwxyz".into(),
                "optional-information".into(),
            ],
            status: ToolUseStatus::Error,
            status_text: "permission denied for a very long resource name".into(),
            ..Default::default()
        },
    );
    let block = render_tool_block(&cli_test_theme(), &display);

    let at_all_minima = priority_header_text(&block, 25);
    assert_eq!(at_all_minima, "ex┄l ar┄yz @a┄yz err: ┄me");

    let without_agent = priority_header_text(&block, 24);
    assert!(without_agent.contains("ar┄yz"), "{without_agent:?}");
    assert!(!without_agent.contains('@'), "{without_agent:?}");
    assert!(without_agent.contains("err:"), "{without_agent:?}");

    let without_arguments = priority_header_text(&block, 18);
    assert!(
        !without_arguments.contains("ar┄yz"),
        "{without_arguments:?}"
    );
    assert!(!without_arguments.contains('@'), "{without_arguments:?}");
    assert!(without_arguments.contains("err:"), "{without_arguments:?}");

    let status_only_detail_drop = priority_header_text(&block, 12);
    assert!(status_only_detail_drop.ends_with(" err"));
    assert!(!status_only_detail_drop.contains("permission"));

    assert_eq!(priority_header_text(&block, 7), "");
}

/// Every documented truncatable tool category must enforce its configured
/// maximum at a wide terminal rather than reverting to unbounded content.
#[test]
fn tool_line_truncation_maxima_cover_every_category() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        &"i".repeat(40),
        &ToolUseState {
            mode: "m".repeat(30),
            args: "a".repeat(60),
            range: Some(ToolUseRange {
                start: Some("r".repeat(40)),
                end: None,
            }),
            info_chips: vec![format!("@agent_{}", "g".repeat(40))],
            status: ToolUseStatus::Error,
            status_text: "d".repeat(60),
            ..Default::default()
        },
    );
    assert!(matches!(display.suffixes[0].status, ToolStatus::Agent));
    let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 300);
    let fields: Vec<&str> = header.split_whitespace().collect();

    assert_eq!(fields[0].chars().count(), 32, "{header:?}");
    assert_eq!(fields[1].chars().count(), 16, "{header:?}");
    assert_eq!(fields[2].chars().count(), 48, "{header:?}");
    assert_eq!(fields[3].chars().count(), 32, "{header:?}");
    assert_eq!(fields[4].chars().count(), 32, "{header:?}");
    assert_eq!(fields[5], "err:");
    assert_eq!(fields[6].chars().count(), 46, "{header:?}");
    for field in [
        &fields[0], &fields[1], &fields[2], &fields[3], &fields[4], &fields[6],
    ] {
        assert!(field.contains('┄'), "{field:?} in {header:?}");
    }
}

/// Mode and range retain their exact configured minima together, then the
/// lower-priority range disappears cleanly one column below that boundary.
#[test]
fn tool_line_mode_and_range_minimum_boundaries_are_exact() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "tool",
        &ToolUseState {
            mode: "mode-value".into(),
            range: Some(ToolUseRange {
                start: Some("range-value".into()),
                end: None,
            }),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let block = render_tool_block(&cli_test_theme(), &display);
    let at_minima = priority_header_text(&block, 17);
    assert_eq!(at_minima, "tool m┄e ra┄.. ok");

    let without_range = priority_header_text(&block, 16);
    assert!(without_range.contains("mode"));
    assert!(!without_range.contains("range"));
    assert!(without_range.ends_with(" ok"));
}

/// Success and failure labels must both remain exact essential elements: a
/// terminal too narrow for identity plus status renders no ambiguous tool row.
#[test]
fn tool_result_status_never_truncates_or_disappears_alone() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    for (status, status_text, exact_width, expected) in [
        (ToolUseStatus::Success, "ok", 7, "ab┄z ok"),
        (ToolUseStatus::Error, "failure", 8, "ab┄z err"),
    ] {
        let display = render_tool_use_state(
            "abcdefghijklmnopqrstuvwxyz",
            &ToolUseState {
                status,
                status_text: status_text.into(),
                ..Default::default()
            },
        );
        let block = render_tool_block(&cli_test_theme(), &display);
        assert_eq!(priority_header_text(&block, exact_width), expected);
        assert_eq!(priority_header_text(&block, exact_width - 1), "");
    }
}

/// Empty protocol labels must still produce explicit truthful lifecycle
/// statuses so the essential status band cannot silently vanish.
#[test]
fn empty_tool_status_labels_receive_unambiguous_defaults() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    for (status, expected) in [
        (ToolUseStatus::Success, "ok"),
        (ToolUseStatus::Warning, "warn"),
        (ToolUseStatus::Error, "err"),
        (
            ToolUseStatus::InProgress,
            tau_proto::PROGRESS_INDICATOR_TEXT,
        ),
    ] {
        for supplied in [" \t ", "\u{200b}\u{200d}\u{fe0f}"] {
            let display = render_tool_use_state(
                "tool",
                &ToolUseState {
                    status,
                    status_text: supplied.into(),
                    ..Default::default()
                },
            );
            assert_eq!(
                display.suffixes.last().map(|suffix| suffix.text.as_str()),
                Some(expected)
            );
            let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 80);
            assert_eq!(header, format!("tool {expected}"));
        }
    }
}

/// Tool-line truncation must measure wide graphemes in terminal columns and
/// recompute from full immutable content when the same block is resized.
#[test]
fn tool_line_unicode_resize_restores_bounded_content() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "read",
        &ToolUseState {
            args: "ab界cd界efghijklmnopqrstuvwxyz".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let block = render_tool_block(&cli_test_theme(), &display);
    let wide = priority_header_text(&block, 40);
    let narrow = priority_header_text(&block, 13);
    assert!(wide.contains('界'));
    assert!(narrow.contains('┄'));
    assert!(narrow.ends_with(" ok"));
    assert_eq!(priority_header_text(&block, 40), wide);
}

/// Untrusted tool fields must remain one row and pass through the terminal
/// cell sanitizer while the adaptive header removes embedded line breaks.
#[test]
fn tool_line_preserves_control_character_safety() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "unsafe\nname",
        &ToolUseState {
            args: "alpha\tbeta\u{1b}[2J\nomega".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let header = priority_header_text(&render_tool_block(&cli_test_theme(), &display), 80);

    assert!(!header.contains('\n'));
    assert!(!header.contains('\t'));
    assert!(!header.contains('\u{1b}'));
    assert!(header.contains("unsafe name"));
    assert!(header.contains("alpha beta�[2J omega"));
    assert!(header.ends_with(" ok"));
}

/// A provider retry/status reset hides reasoning from the failed attempt so
/// stale thinking does not remain visible while the replacement attempt runs.
#[test]
fn status_clear_response_removes_live_thinking_block() {
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
            "",
            Some("failed attempt thinking".to_owned()),
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "failed attempt thinking"));

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id("sp-0"),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: "retrying".to_owned(),
            clear_response: true,
            retry: None,
            native_tool: None,
        }),
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "retrying"));
    assert!(!vt.screen_contains(80, "failed attempt thinking"));
}

/// Typed provider-native lifecycle must produce a generic live row and retain
/// its completed row with a visibly distinct native qualifier.
#[test]
fn provider_native_tool_lifecycle_renders_live_and_completed_rows() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    let update = |phase, status, status_text: &str| {
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: test_agent_prompt_id("sp-native"),
            agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
            deltas: Vec::new(),
            compaction: None,
            status: Some(tau_proto::ProviderResponseStatusUpdate {
                text: status_text.to_owned(),
                clear_response: false,
                retry: None,
                native_tool: Some(tau_proto::ProviderNativeToolStatusUpdate {
                    call_id: "ws_1".to_owned(),
                    tool_name: tau_proto::ToolName::new("web_search"),
                    display: tau_proto::ToolUseState {
                        status,
                        status_text: if phase == tau_proto::ProviderNativeToolPhase::Started {
                            String::new()
                        } else {
                            "ok".to_owned()
                        },
                        ..Default::default()
                    },
                    phase,
                }),
            }),
            response_stats: None,
            originator: tau_proto::PromptOriginator::User,
        })
    };

    renderer.handle(&update(
        tau_proto::ProviderNativeToolPhase::Started,
        tau_proto::ToolUseStatus::InProgress,
        "Searching web…",
    ));
    sync(&handle);
    assert!(vt.screen_contains(100, "web_search (native)"));

    renderer.handle(&update(
        tau_proto::ProviderNativeToolPhase::Completed,
        tau_proto::ToolUseStatus::Success,
        "Web search complete…",
    ));
    sync(&handle);
    let screen = vt.screen_text(100);
    assert!(
        screen
            .iter()
            .any(|line| line.contains("web_search (native) ok")),
        "{screen:?}"
    );
}

/// The native qualifier uses the generic informational style rather than the
/// primary tool-name color, keeping the execution distinction visually subdued.
#[test]
fn provider_native_tool_qualifier_uses_info_style() {
    let display = crate::event_renderer::provider_native_tool_display(
        &tau_proto::ProviderNativeToolStatusUpdate {
            call_id: "ws_1".to_owned(),
            tool_name: tau_proto::ToolName::new("web_search"),
            display: tau_proto::ToolUseState::default(),
            phase: tau_proto::ProviderNativeToolPhase::Started,
        },
    );
    assert_eq!(display.leading_segments.len(), 1);
    assert_eq!(display.leading_segments[0].text, "(native)");
    assert!(matches!(
        display.leading_segments[0].status,
        ToolStatus::Info
    ));
}

/// Provider finalization must remove an unfinished native live row rather than
/// leaving it pinned or synthesizing a completed history entry.
#[test]
fn provider_finished_retires_unfinished_native_tool_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&native_tool_started_update("sp-native-finished"));
    sync(&handle);
    assert!(vt.screen_contains(100, "web_search (native)"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-native-finished",
        Vec::new(),
    )));
    sync(&handle);
    assert!(!vt.screen_contains(100, "web_search (native)"));
}

/// Prompt cancellation must remove an unfinished native live row even when no
/// provider terminal or typed native completion arrives.
#[test]
fn prompt_termination_retires_unfinished_native_tool_row() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&native_tool_started_update("sp-native-terminated"));
    sync(&handle);
    assert!(vt.screen_contains(100, "web_search (native)"));

    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_id: agent_id("main"),
        agent_prompt_id: test_agent_prompt_id("sp-native-terminated"),
        reason: AgentPromptTerminationReason::Canceled,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(100, "web_search (native)"));
}

fn native_tool_started_update(prompt_id: &str) -> Event {
    Event::ProviderResponseUpdated(ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id(prompt_id),
        agent_id: agent_id("main"),
        deltas: Vec::new(),
        compaction: None,
        status: Some(tau_proto::ProviderResponseStatusUpdate {
            text: "Searching web…".to_owned(),
            clear_response: false,
            retry: None,
            native_tool: Some(tau_proto::ProviderNativeToolStatusUpdate {
                call_id: "ws_1".to_owned(),
                tool_name: tau_proto::ToolName::new("web_search"),
                display: tau_proto::ToolUseState {
                    status: tau_proto::ToolUseStatus::InProgress,
                    ..Default::default()
                },
                phase: tau_proto::ProviderNativeToolPhase::Started,
            }),
        }),
        response_stats: None,
        originator: tau_proto::PromptOriginator::User,
    })
}

#[test]
fn render_compaction_block_styles_completed_status() {
    let theme = cli_test_theme();

    let block = render_compaction_block(&theme, "ok", CompactionStatus::Success);
    let spans = block.content.spans();
    let success_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_SUCCESS);
    let ok = spans
        .iter()
        .find(|span| span.text == "ok")
        .expect("completed compaction status span");

    assert_eq!(ok.style, success_style);
}

/// Self-compaction metrics must use the generic neutral stats chip while only
/// the terminal `ok` uses the success color.
#[test]
fn self_compaction_tool_row_styles_metrics_as_stats() {
    let theme = cli_test_theme();
    let display = EventRenderer::self_compaction_tool_use_state(
        CompactionStatus::Success,
        "~#110k → ~#27.7k (25%) ok".to_owned(),
    );
    let block = render_tool_block(&theme, &render_tool_use_state("compact", &display));
    let cells = priority_header_cells(&block, 100);
    let text: String = cells.iter().map(|cell| cell.ch).collect();
    let metrics_start = text[..text.find("~#110k").expect("compaction metrics")]
        .chars()
        .count();
    let ok_start = text[..text.rfind("ok").expect("terminal success status")]
        .chars()
        .count();

    assert_eq!(
        cells[metrics_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_INFO)
    );
    assert_eq!(
        cells[ok_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_SUCCESS)
    );
}

#[test]
fn manual_compaction_trigger_does_not_render_progress_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle(&Event::AgentCompactionTriggered(AgentCompactionTriggered {
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
        resume_inference: false,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "compact"));
    assert!(!vt.screen_contains(80, "manual compaction requested"));
}

#[test]
fn logical_and_provider_tool_errors_render_one_terminal_line() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("overlap-edit", "edit", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolError(ToolError {
            presentation: Default::default(),
            call_id: "overlap-edit".into(),
            tool_name: tau_proto::ToolName::new("edit"),
            tool_type: tau_proto::ToolType::Function,
            message: "overlapping edits".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            presentation: Default::default(),
            call_id: "overlap-edit".into(),
            tool_name: tau_proto::ToolName::new("edit"),
            tool_type: tau_proto::ToolType::Function,
            message: "overlapping edits".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,

            display: None,
        }),
        tau_proto::UnixMicros::new(2_100_000),
    );
    sync(&handle);

    let text = vt.screen_text(80).join("\n");
    assert!(text.contains("edit 1s err: overlapping edits"));
    assert_eq!(text.matches("overlapping edits").count(), 1);
}

/// Ensures canonical completed cold-replay facts fold directly to one terminal
/// dummy-tool row and cannot be resurrected by later transcript activity.
#[test]
fn completed_dummy_tool_replay_is_terminal_idle_and_stays_terminal() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-tool-prompt",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "opud-call".into(),
            name: tau_proto::ToolName::new("restart_test_dummy"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
            raw_arguments_json: Some("{}".to_owned()),
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "opud-call".into(),
        tool_name: tau_proto::ToolName::new("restart_test_dummy"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("restart succeeded".to_owned()),
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-final-prompt",
        vec![assistant_message_item("opud-tool-complete")],
    )));
    sync(&handle);
    let row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("restart_test_dummy"))
        .expect("terminal dummy row");
    assert!(row.contains("ok"), "{row}");
    assert!(!row.contains("pending"), "{row}");
    assert_eq!(renderer.test_active_tool_count(), 0);

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "opud-later-prompt",
        vec![assistant_message_item("later response")],
    )));
    sync(&handle);
    assert_eq!(renderer.test_active_tool_count(), 0);
    assert!(!vt.screen_contains(100, "restart_test_dummy 0s pending"));
}

/// A status call's semantic descriptor must survive the complete event-renderer
/// lifecycle, with terminal duration and outcome retained in their real order.
#[test]
fn status_descriptor_survives_terminal_tool_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("status-call", "status", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &initial_tool_progress(
            "status-call",
            "status",
            "working: implementing renderer coverage",
            "",
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            presentation: Default::default(),
            call_id: "status-call".into(),
            tool_name: tau_proto::ToolName::new("status"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("Status accepted".into()),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolUseState {
                args: "working: implementing renderer coverage".into(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(3_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(80, "status working: implementing renderer coverage 2s ok"));
    assert!(!vt.screen_contains(80, "status 2s ok"));
}

#[test]
fn tool_started_renders_pending_until_provider_progress() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &Event::ToolStarted(tau_proto::ToolStarted {
            invocation_policy: tau_proto::ToolInvocationPolicy::default(),
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("fallback.rs".into()),
            )]),
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read 0s pending"));
    assert!(!vt.screen_contains(80, "fallback.rs"));

    renderer.handle_recorded_at(
        &Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            message: None,
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "semantic.rs".into(),
                status: tau_proto::ToolUseStatus::InProgress,
                status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
                ..Default::default()
            }),
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read semantic.rs"));
}

/// Streaming assistant text must stay above already-running tool calls so the
/// tool UI remains pinned near the prompt even when the live response grows
/// taller than the viewport.
#[test]
fn active_tool_stays_below_streaming_response() {
    let (_term, handle, vt) = setup(80, 6);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text("src/main.rs".into()),
    )]);
    renderer.handle(&tool_started("call-1", "read", read_args));
    renderer.handle(&initial_tool_progress("call-1", "read", "src/main.rs", ""));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs"));
    let full_renders_before = handle.full_render_count();

    let long_response = (0..12)
        .map(|i| format!("streaming response line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    renderer.handle(&Event::AgentPromptCreated(agent_prompt_created(
        "sp-streaming",
        "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(
        provider_response_delta_update(
            test_agent_prompt_id("sp-streaming"),
            long_response,
            None,
            tau_proto::PromptOriginator::User,
        ),
    ));
    sync(&handle);

    let lines = visible_lines(&vt, 80);
    let response_line = lines
        .iter()
        .position(|line| line.contains("streaming response line"))
        .unwrap_or_else(|| panic!("missing streaming response line: {lines:?}"));
    let tool_line = lines
        .iter()
        .position(|line| line.contains("read src/main.rs"))
        .unwrap_or_else(|| panic!("missing pinned tool line: {lines:?}"));
    assert!(
        response_line < tool_line,
        "active tool should stay below the streaming response: {lines:?}",
    );
    assert_eq!(
        handle.full_render_count(),
        full_renders_before,
        "pinning live tool calls must not force a full redraw",
    );
}

#[test]
fn show_tools_off_hides_tool_blocks() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.apply_setting("show-tools", "off");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
            raw_arguments_json: None,
            responses_envelope: None,
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        presentation: Default::default(),
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        provider_content: Vec::new(),
        kind: tau_proto::ToolResultKind::Final,
        originator: tau_proto::PromptOriginator::User,

        display: None,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "tools"));
    assert!(!vt.screen_contains(80, "read"));
}

#[test]
fn render_tool_use_state_assembles_chips_in_order() {
    use tau_proto::{ToolUseState, ToolUseStats, ToolUseStatus};

    // grep-style: matches + stats + status.
    let display = ToolUseState {
        args: "\"foo\" in src".into(),
        stats: ToolUseStats {
            matches: Some(3),
            lines: Some(7),
            bytes: Some(120),
        },
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("grep", &display);
    assert_eq!(rendered.tool_name, "grep");
    assert_eq!(rendered.args, "\"foo\" in src");
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["3, 7L, 120B", "ok"]);
    assert!(matches!(
        rendered.suffixes.last().expect("status suffix").status,
        ToolStatus::Success
    ));
}

#[test]
fn render_tool_use_state_keeps_range_separate_from_args() {
    use tau_proto::{ToolUseRange, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "feed/main".into(),
        range: Some(ToolUseRange {
            start: Some("2026-05-29".into()),
            end: Some("2026-05-30".into()),
        }),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };

    let rendered = render_tool_use_state("calendar", &display);
    assert_eq!(rendered.args, "feed/main");
    assert_eq!(rendered.range.as_deref(), Some("2026-05-29..2026-05-30"));
}

#[test]
fn render_tool_block_paints_mode_with_dedicated_style() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let theme = cli_test_theme();
    let display = ToolUseState {
        mode: "rw".into(),
        args: "printf hello".into(),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };

    let rendered = render_tool_use_state("shell", &display);
    assert_eq!(rendered.mode, "rw");
    assert_eq!(rendered.args, "printf hello");

    let block = render_tool_block(&theme, &rendered);
    let cells = priority_header_cells(&block, 80);
    let text: String = cells.iter().map(|cell| cell.ch).collect();
    let mode_start = text.find(" rw ").expect("mode span") + 1;
    assert_eq!(
        cells[mode_start].style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_MODE)
    );
}

/// Context counters use the status bar's compact magnitude policy without
/// changing the broader token-progress formatting policy.
#[test]
fn render_tool_use_state_token_progress_formats_context_like_status_bar() {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "[research]".into(),
        progress_counters: vec![
            ProgressCounter {
                label: Some("ctx".into()),
                unit: ProgressUnit::Tokens,
                complete: Some(133_400),
                total: Some(200_000),
            },
            ProgressCounter {
                label: Some("tokens".into()),
                unit: ProgressUnit::Tokens,
                complete: Some(133_400),
                total: Some(200_000),
            },
        ],
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let rendered = render_tool_use_state("agent_start", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "#133k/200k",
            "tokens: 133.4k/200k",
            tau_proto::PROGRESS_INDICATOR_TEXT,
        ]
    );
}

#[test]
fn render_tool_use_state_text_payload_is_preserved_for_block_rendering() {
    use tau_proto::{ToolUsePayload, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "printf hello".into(),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        payload: Some(ToolUsePayload::Text {
            text: "printf hello\nprintf world".into(),
        }),
        ..Default::default()
    };
    let rendered = render_tool_use_state("shell", &display);
    assert_eq!(rendered.args, "printf hello");
    assert_eq!(rendered.payload, display.payload);
}

#[test]
fn render_tool_use_state_diff_payload_adds_plus_minus_chips() {
    use tau_proto::{DiffSummary, ToolUsePayload, ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "src/main.rs".into(),
        status: ToolUseStatus::Success,
        status_text: "ok".into(),
        payload: Some(ToolUsePayload::Diff(DiffSummary {
            added: 12,
            removed: 3,
            hunks: vec![],
        })),
        ..Default::default()
    };
    let rendered = render_tool_use_state("edit", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["+12", "-3", "ok"]);
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::DiffAdded));
    assert!(matches!(
        rendered.suffixes[1].status,
        ToolStatus::DiffRemoved
    ));
}

#[test]
fn render_diff_tool_block_uses_unified_diff_line_prefixes() {
    use tau_proto::{DiffHunk, DiffLine, DiffSegment, DiffSummary, ToolUseState, ToolUseStatus};

    let display = render_tool_use_state(
        "edit",
        &ToolUseState {
            args: "src/main.rs 10..11".into(),
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        },
    );
    let diff = DiffSummary {
        added: 2,
        removed: 2,
        hunks: vec![DiffHunk {
            old_start: 10,
            old_count: 2,
            new_start: 10,
            new_count: 2,
            lines: vec![
                DiffLine::Equal {
                    text: "    unchanged();".into(),
                },
                DiffLine::Remove {
                    text: "    old();".into(),
                },
                DiffLine::Add {
                    text: "    new();".into(),
                },
                DiffLine::Modify {
                    old: vec![
                        DiffSegment::Equal {
                            text: "let x = ".into(),
                        },
                        DiffSegment::Remove { text: "1".into() },
                        DiffSegment::Equal { text: ";".into() },
                    ],
                    new: vec![
                        DiffSegment::Equal {
                            text: "let x = ".into(),
                        },
                        DiffSegment::Add { text: "2".into() },
                        DiffSegment::Equal { text: ";".into() },
                    ],
                },
            ],
        }],
    };

    let block = render_diff_tool_block(&cli_test_theme(), &display, &diff, true);
    let text: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.starts_with("@@ -10,2 +10,2 @@\n     unchanged();"));
    assert!(text.contains("\n-    old();"));
    assert!(text.contains("\n+    new();"));
    assert!(text.contains("\n-let x = 1;\n+let x = 2;"));
    assert!(!text.contains("\n-     old();"));
    assert!(!text.contains("\n+     new();"));
    let removed_line = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "-    old();")
        .expect("removed line uses one span");
    assert_eq!(removed_line.style.fg, Some(tau_cli_term::Color::DarkRed));

    let added_line = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "+    new();")
        .expect("added line uses one span");
    assert_eq!(added_line.style.fg, Some(tau_cli_term::Color::DarkGreen));

    let changed_removed = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "1")
        .expect("removed changed token is split into its own span");
    assert_eq!(changed_removed.style.fg, Some(tau_cli_term::Color::Red));
    assert!(changed_removed.style.bold);

    let changed_added = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .find(|span| span.text == "2")
        .expect("added changed token is split into its own span");
    assert_eq!(changed_added.style.fg, Some(tau_cli_term::Color::Green));
    assert!(changed_added.style.bold);
}

/// Ensures multi-file mutation payloads keep per-file headers and aggregate
/// diff chips, so apply_patch results can show structured UI diffs for every
/// changed file instead of falling back to plain text summaries.
#[test]
fn render_multi_diff_tool_block_preserves_file_boundaries() {
    use tau_proto::{
        DiffHunk, DiffLine, DiffSummary, FileDiffSummary, ToolUsePayload, ToolUseState,
        ToolUseStatus,
    };

    let files = vec![
        FileDiffSummary {
            path: "a.txt".into(),
            diff: DiffSummary {
                added: 1,
                removed: 0,
                hunks: vec![DiffHunk {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: 1,
                    lines: vec![DiffLine::Add {
                        text: "alpha".into(),
                    }],
                }],
            },
        },
        FileDiffSummary {
            path: "b.txt".into(),
            diff: DiffSummary {
                added: 0,
                removed: 1,
                hunks: vec![DiffHunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: 0,
                    new_count: 0,
                    lines: vec![DiffLine::Remove {
                        text: "beta".into(),
                    }],
                }],
            },
        },
    ];
    let display = render_tool_use_state(
        "apply_patch",
        &ToolUseState {
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            payload: Some(ToolUsePayload::Diffs {
                files: files.clone(),
            }),
            ..Default::default()
        },
    );

    let suffixes: Vec<&str> = display.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(suffixes, vec!["+1", "-1", "ok"]);
    let block = render_multi_diff_tool_block(&cli_test_theme(), &display, &files, true);
    let text: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.starts_with("--- a.txt"));
    assert!(text.contains("\n+alpha"));
    assert!(text.contains("\n--- b.txt"));
    assert!(text.contains("\n-beta"));
}

/// Ensures a one-file `Diffs` payload still labels its hunks, because freeform
/// apply_patch calls have no compact argument path that could identify them.
#[test]
fn render_multi_diff_tool_block_labels_single_file_hunks_once() {
    use tau_proto::{
        DiffHunk, DiffLine, DiffSummary, FileDiffSummary, ToolUsePayload, ToolUseState,
        ToolUseStatus,
    };

    let files = vec![FileDiffSummary {
        path: "src/lib.rs".into(),
        diff: DiffSummary {
            added: 1,
            removed: 1,
            hunks: vec![DiffHunk {
                old_start: 3,
                old_count: 1,
                new_start: 3,
                new_count: 1,
                lines: vec![
                    DiffLine::Remove { text: "old".into() },
                    DiffLine::Add { text: "new".into() },
                ],
            }],
        },
    }];
    let display = render_tool_use_state(
        "apply_patch",
        &ToolUseState {
            status: ToolUseStatus::Success,
            status_text: "ok".into(),
            payload: Some(ToolUsePayload::Diffs {
                files: files.clone(),
            }),
            ..Default::default()
        },
    );
    let block = render_multi_diff_tool_block(&cli_test_theme(), &display, &files, true);
    let text: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert_eq!(text.matches("--- src/lib.rs").count(), 1);
    assert!(text.contains("@@ -3,1 +3,1 @@"));
    assert!(text.contains("\n-old"));
    assert!(text.contains("\n+new"));
}

#[test]
fn fallback_error_status_is_abbreviated_only_by_renderer() {
    let message =
        "failed to access /home/dpc/agent/.agents/skills: No such file or directory (os error 2)";
    let display = synthesize_fallback_display("ls", Some(message));
    assert_eq!(display.status_text, message);
    assert!(!display.status_text.contains("err:"));
    assert!(!display.status_text.contains('…'));

    let rendered = render_tool_use_state("ls", &display);
    let block = render_tool_block(&cli_test_theme(), &rendered);
    let text = priority_header_text(&block, 80);

    assert!(text.contains('┄'));
    assert!(!text.contains('…'));
}

#[test]
fn render_tool_use_state_error_status_picks_error_severity() {
    use tau_proto::{ToolUseState, ToolUseStatus};

    let display = ToolUseState {
        args: "/etc".into(),
        status: ToolUseStatus::Error,
        status_text: "permission denied".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("ls", &display);
    assert_eq!(rendered.suffixes.len(), 1);
    assert_eq!(rendered.suffixes[0].text, "err: permission denied");
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::Error));

    let legacy_display = ToolUseState {
        args: "/etc".into(),
        status: ToolUseStatus::Error,
        status_text: "err: permission denied".into(),
        ..Default::default()
    };
    let rendered = render_tool_use_state("ls", &legacy_display);
    assert_eq!(rendered.suffixes[0].text, "err: permission denied");
}

#[test]
fn render_tool_block_abbreviates_inline_args_and_error_but_preserves_payload() {
    use tau_proto::{ToolUsePayload, ToolUseState, ToolUseStatus};

    let payload = "full payload line one\nfull payload line two".to_owned();
    let display = ToolUseState {
        args: "LOG_MODULE_WALLETV2|LOG_CLIENT_MODULE_WALLETV2 in modules/fedimint-walletv2-server/src modules/fedimint-walletv2-client/src".into(),
        status: ToolUseStatus::Error,
        status_text: "ripgrep error: rg: modules/fedimint-walletv2-server/src modules/fedimint-walletv2-client/src: IO error for operation".into(),
        payload: Some(ToolUsePayload::Text {
            text: payload.clone(),
        }),
        ..Default::default()
    };
    let rendered = render_tool_use_state("grep", &display);
    let block = render_tool_block(&cli_test_theme(), &rendered);
    let header = priority_header_text(&block, 200);
    let body: String = block
        .priority_line_body_content()
        .expect("priority body")
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(header.contains("LOG_MODULE_WALLETV2|"));
    assert!(header.contains("walletv2-client/src"));
    assert!(header.contains("err: ripgrep error:"));
    assert!(header.contains("IO error for operation"));
    assert_eq!(header.matches('┄').count(), 2);
    assert!(!header.contains(&display.args));
    assert!(!header.contains(&display.status_text));
    assert!(body.contains(&payload));
}

#[test]
fn format_turn_stats_line_formats_short_latencies_as_millis() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 17_341,
        prompt_cached_tokens: 16_896,
        prompt_cache_read_ceiling_tokens: Some(17_341),
        response_received_tokens: 29,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 100_000,
                cached_tokens: 50_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 16_000,
        response_received_tokens: 1_341,
        ..Default::default()
    };
    let line = format_turn_stats_line(
        &usage,
        Some(&previous_usage),
        Some(Duration::from_millis(1_240)),
        Some(Duration::from_millis(4_560)),
    );

    assert_eq!(line, "Δ97% 16.8k/17.3k ↑0 ↓29 1240ms Σ↑50k/100k ↓0 4560ms",);
}

#[test]
fn format_turn_stats_line_formats_long_latencies_compactly() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_cache_read_ceiling_tokens: Some(0),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 1_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let line = format_turn_stats_line(
        &usage,
        None,
        Some(Duration::from_millis(18_723)),
        Some(Duration::from_secs(5 * 60 + 1)),
    );

    assert_eq!(line, "Δ— 0/0 ↑0 ↓0 18s Σ↑0/1k ↓0 5m");
}

#[test]
fn format_turn_stats_line_uses_previous_turn_for_hit_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_000,
        prompt_cache_read_ceiling_tokens: Some(20_000),
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 40_100,
                cached_tokens: 19_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_000,
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ95% 19k/20k ↑100 ↓0 Σ↑19k/40.1k ↓0");
}

/// Ensures a missing provider ceiling uses the existing bounded reusable-prefix
/// calculation and visibly marks both the derived ratio and denominator.
#[test]
fn format_turn_stats_line_estimates_unknown_cache_ceiling() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 121_300,
        prompt_cached_tokens: 120_300,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 120_000,
        response_received_tokens: 1_300,
        ..Default::default()
    };

    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ99%? 120.3k/121.3k? ↑0 ↓0 Σ↑0/0 ↓0");
}

fn private_cache_context(model: CacheEstimateModel) -> CacheEstimateContext {
    CacheEstimateContext::private(model, tau_proto::ModelParams::default())
}

fn calibrated_predecessor(
    sent: u64,
    received: u64,
    context: CacheEstimateContext,
    geometry: CacheEstimateGeometry,
) -> PreviousTurnUsageProjection {
    PreviousTurnUsageProjection {
        prompt_sent_tokens: sent,
        response_received_tokens: received,
        cache_estimate_context: context,
        cache_estimate_calibration: CacheEstimateCalibration::Confirmed(geometry),
    }
}

/// Ensures calibration stays within the observed prefix range and preserves
/// the distinct later and earlier 1,024-token phases.
#[test]
fn cache_estimate_geometries_preserve_evidence_boundaries() {
    assert_eq!(CacheEstimateGeometry::Step128Lag182.estimate(9_999), None);
    assert_eq!(
        CacheEstimateGeometry::Step1024Residue256.estimate(23_757),
        Some(22_784)
    );
    assert_eq!(
        CacheEstimateGeometry::Step1024Residue512.estimate(23_757),
        Some(23_040)
    );
}

/// Ensures the motivating Sol row uses a previously observed 128-token regime
/// while retaining the uncertainty marker.
#[test]
fn format_turn_stats_line_uses_calibrated_sol_regime() {
    let context = private_cache_context(CacheEstimateModel::Sol);
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 23_962,
        prompt_cached_tokens: 23_552,
        ..Default::default()
    };

    let line = format_turn_stats_line_with_projection(
        &usage,
        context,
        Some(calibrated_predecessor(
            23_757,
            166,
            context,
            CacheEstimateGeometry::Step128Lag182,
        )),
    );

    assert_eq!(line, "Δ100%? 23.5k/23.5k? ↑410 ↓0 Σ↑0/0 ↓0");
}

/// Ensures the calibrated 1,024/256 regime remains distinct from the Sol
/// 128-token geometry.
#[test]
fn format_turn_stats_line_uses_calibrated_terra_plateau() {
    let context = private_cache_context(CacheEstimateModel::Terra);
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 23_962,
        prompt_cached_tokens: 22_784,
        ..Default::default()
    };

    let line = format_turn_stats_line_with_projection(
        &usage,
        context,
        Some(calibrated_predecessor(
            23_757,
            166,
            context,
            CacheEstimateGeometry::Step1024Residue256,
        )),
    );

    assert_eq!(line, "Δ100%? 22.7k/22.7k? ↑1.1k ↓0 Σ↑0/0 ↓0");
}

/// Ensures a same-model regime change invalidates the inherited calibration
/// instead of making the current read its own 100-percent denominator.
#[test]
fn format_turn_stats_line_falls_back_when_same_model_regime_changes() {
    let context = private_cache_context(CacheEstimateModel::Sol);
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 30_600,
        prompt_cached_tokens: 30_208,
        ..Default::default()
    };

    let line = format_turn_stats_line_with_projection(
        &usage,
        context,
        Some(calibrated_predecessor(
            30_423,
            100,
            context,
            CacheEstimateGeometry::Step1024Residue256,
        )),
    );

    assert_eq!(line, "Δ98%? 30.2k/30.5k? ↑77 ↓0 Σ↑0/0 ↓0");
}

/// Ensures a model/control discontinuity does not apply a calibrated boundary
/// to an unrelated preceding response.
#[test]
fn format_turn_stats_line_uses_generic_estimate_across_scope_discontinuity() {
    let previous_context = private_cache_context(CacheEstimateModel::Sol);
    let current_context = CacheEstimateContext::private(
        CacheEstimateModel::Sol,
        tau_proto::ModelParams {
            service_tier: Some(tau_proto::ServiceTier::Fast),
            ..Default::default()
        },
    );
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 23_962,
        prompt_cached_tokens: 23_552,
        ..Default::default()
    };

    let line = format_turn_stats_line_with_projection(
        &usage,
        current_context,
        Some(calibrated_predecessor(
            23_757,
            166,
            previous_context,
            CacheEstimateGeometry::Step128Lag182,
        )),
    );

    assert_eq!(line, "Δ98%? 23.5k/23.9k? ↑39 ↓0 Σ↑0/0 ↓0");
}

/// Ensures a nonzero reusable prefix with no provider cache read remains a
/// visibly approximate zero-percent estimate rather than an exact cache miss.
#[test]
fn format_turn_stats_line_marks_estimated_zero_cache_read() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 121_300,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 120_000,
        response_received_tokens: 1_300,
        ..Default::default()
    };

    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ0%? 0/121.3k? ↑0 ↓0 Σ↑0/0 ↓0");
}

/// Ensures a provider chain reset cannot show more cacheable tokens than the
/// current full-replay request contains.
#[test]
fn format_turn_stats_line_caps_cache_possible_after_chain_reset() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 13_659,
        prompt_cached_tokens: 3_840,
        prompt_cache_read_ceiling_tokens: Some(13_659),
        response_received_tokens: 116,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 157_101,
        response_received_tokens: 31,
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ28% 3.8k/13.6k ↑0 ↓116 Σ↑0/0 ↓0");
}

#[test]
fn format_turn_stats_line_shows_zero_hit_when_nothing_could_be_cached() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 1_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, None, None, None);

    assert_eq!(line, "Δ—? 0/0? ↑1k ↓0 Σ↑0/1k ↓0");
}

/// Optional diagnostics must stay absent when disabled and, when enabled, hide
/// the lower-value redraw counter before the more useful UI-I/O rates.
#[test]
fn model_status_debug_elements_follow_config_and_priority() {
    for width in [20, 22] {
        let (_term, handle, vt) = setup(width, 24);
        let mut renderer = EventRenderer::new(
            handle.clone(),
            tau_cli_term::CompletionData::new(),
            cli_test_theme(),
        );
        renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
            model: Some("test/model".into()),
            context_window: None,
            role: "engineer".into(),
            baseline_params: None,
            model_params: tau_proto::ModelParams::default(),
        }));
        sync(&handle);
        let status_row = vt
            .screen_text(width)
            .into_iter()
            .find(|row| row.contains("+engineer"))
            .expect("status row without diagnostics");
        assert!(!status_row.contains("io "));

        renderer.apply_setting("show-ui-io", "true");
        renderer.apply_setting("redraw-counter", "true");
        handle.invalidate_screen();
        sync(&handle);
        renderer.handle_ui_io_sample(UiIoStats {
            uplink_max_bytes_per_sec: 1024,
            downlink_max_bytes_per_sec: 2048,
        });
        sync(&handle);

        let full_render_count = handle.full_render_count();
        let status_row = vt
            .screen_text(width)
            .into_iter()
            .find(|row| row.contains("+engineer"))
            .expect("status row with diagnostics");
        assert!(
            status_row.contains("io ↑1K ↓2K"),
            "UI-I/O diagnostics missing at width {width}: {status_row:?}"
        );
        assert_eq!(
            status_row.ends_with(&format!(" {full_render_count}")),
            width == 22,
            "unexpected redraw-counter retention at width {width}: {status_row:?}"
        );
    }
}

/// A self-compaction failure and a pre-start rejection update their owning
/// generic rows instead of creating standalone lifecycle rows.
#[test]
fn self_compaction_failure_and_rejection_reuse_their_tool_rows() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );
    renderer.switch_agent(agent_id("main"));
    renderer.apply_setting("show-tools", "compact");

    let mut failed_start = tool_started("call-failed", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut failed_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&failed_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-failed", "call-failed"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started(
            "cr-failed",
            "call-failed",
            "ct-failed-self",
            "ap-failed-self",
        ),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionFailed(
        AgentStandaloneCompactionFailed {
            agent_id: agent_id("main"),
            transaction_id: tau_proto::CompactionTransactionId::parse("ct-failed-self")
                .expect("known-safe transaction id"),
            cut: tau_proto::AgentHead::Root,
            reason: tau_proto::StandaloneCompactionFailureReason::ProviderError,
            resume_through: None,
            context_retreat: None,
            incomplete_response: None,
        },
    ));

    let mut rejected_start = tool_started("call-rejected", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut rejected_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&rejected_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-rejected", "call-rejected"),
    ));
    renderer.handle(&Event::AgentManualCompactionRequestFailed(
        tau_proto::AgentManualCompactionRequestFailed {
            request_id: tau_proto::CompactionRequestId::parse("cr-rejected")
                .expect("known-safe request id"),
            target_agent_id: agent_id("main"),
            reason: tau_proto::ManualCompactionRequestFailureReason::Unsupported,
        },
    ));

    let mut cancelled_start = tool_started("call-cancelled", "compact", CborValue::Null);
    let Event::ToolStarted(started) = &mut cancelled_start else {
        unreachable!("tool_started helper returns a tool start");
    };
    started.agent_id = agent_id("main");
    renderer.handle(&cancelled_start);
    renderer.handle(&Event::AgentManualCompactionRequested(
        self_compaction_requested("cr-cancelled", "call-cancelled"),
    ));
    renderer.handle(&Event::AgentStandaloneCompactionStarted(
        self_compaction_started(
            "cr-cancelled",
            "call-cancelled",
            "ct-cancelled-self",
            "ap-cancelled-self",
        ),
    ));
    renderer.handle(&Event::AgentPromptStarted(
        standalone_compaction_prompt_started("ap-cancelled-self"),
    ));
    renderer.handle(&Event::AgentPromptTerminated(AgentPromptTerminated {
        automatic_compaction_decision: None,
        agent_id: agent_id("main"),
        agent_prompt_id: test_agent_prompt_id("ap-cancelled-self"),
        reason: AgentPromptTerminationReason::Canceled,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let text = vt.screen_text(100).join("\n");
    assert_eq!(text.matches("err: failed").count(), 1, "{text}");
    assert_eq!(text.matches("err: rejected").count(), 1, "{text}");
    assert_eq!(text.matches("err: stopped").count(), 1, "{text}");
    assert!(!text.contains("compact complete"), "{text}");
}

/// Inter-session receiver rejection must retain its actionable fixed detail in
/// the terminal's ordinary tool-error presentation.
#[test]
fn message_tool_receiver_rejection_renders_actionable_detail() {
    let (_term, handle, vt) = setup(120, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
    );

    renderer.handle_recorded_at(
        &tool_started("message-no-receiver", "message", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolError(ToolError {
            presentation: Default::default(),
            call_id: "message-no-receiver".into(),
            tool_name: tau_proto::ToolName::new("message"),
            tool_type: tau_proto::ToolType::Function,
            message: "target live; no receiver; set `inter_session_receiver`".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,
            display: None,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);

    let text = vt.screen_text(120).join("\n");
    assert!(
        text.contains("target live; no receiv"),
        "terminal did not retain the receiver diagnosis: {text}"
    );
    assert!(
        text.contains("inter_session_receiver"),
        "terminal did not retain the configuration key: {text}"
    );
}

/// Ensures malformed totals and cache counts that exceed the bounded estimate
/// retain invalid rendering instead of displaying an impossible percentage.
#[test]
fn format_turn_stats_line_rejects_invalid_or_inconsistent_cache_counts() {
    let malformed = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        prompt_cached_tokens: 1_001,
        ..Default::default()
    };
    let inconsistent = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_000,
        prompt_cached_tokens: 15_000,
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        response_received_tokens: 1,
        ..Default::default()
    };

    assert_eq!(
        format_turn_stats_line(&malformed, None, None, None),
        "Δ! 1k/? ↑1k ↓0 Σ↑0/0 ↓0"
    );
    assert_eq!(
        format_turn_stats_line(&inconsistent, Some(&previous_usage), None, None),
        "Δ! 15k/? ↑9.9k ↓0 Σ↑0/0 ↓0"
    );
}

/// Ensures cached tokens without any reusable predecessor are treated as
/// invalid for an estimated ceiling, just like an invalid exact ceiling.
#[test]
fn format_turn_stats_line_rejects_cache_without_reusable_prefix() {
    let unknown = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 2_000,
        prompt_cached_tokens: 1_500,
        ..Default::default()
    };
    let invalid = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 2_000,
        prompt_cached_tokens: 1_500,
        prompt_cache_read_ceiling_tokens: Some(1_000),
        ..Default::default()
    };
    assert!(format_turn_stats_line(&unknown, None, None, None).starts_with("Δ! 1.5k/?"));
    assert!(format_turn_stats_line(&invalid, None, None, None).starts_with("Δ! 1.5k/?"));
}

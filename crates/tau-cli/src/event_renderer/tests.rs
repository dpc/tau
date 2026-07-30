use std::collections::{HashMap, HashSet};
use std::sync as path_std_sync;
use std::sync::atomic as path_std_sync_atomic;

use tau_cli_term_raw::Term;
use tau_config::settings as path_tau_config_settings;

use super::{
    AgentActivity, MessageRenderMode, QUEUED_PROJECTION_WINDOW_BYTES, RoleCompletionDetails,
    bounded_queued_line_end, bounded_queued_line_start, queued_prompt_projection,
    role_setting_value_completions, role_value_completion,
};
use crate::chat::{DraftSlot, queue_prompt_draft_snapshot};

fn agent_id(value: &str) -> tau_proto::AgentId {
    tau_proto::AgentId::parse(value).expect("valid test agent id")
}

fn renderer_for_agent_id_tests() -> super::EventRenderer {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    super::EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        crate::tests::cli_test_theme(),
    )
}

/// Queued-prompt layout receives fixed-size source windows even for arbitrarily
/// large ASCII and zero-width Unicode input.
#[test]
fn queued_prompt_source_windows_are_bounded() {
    let ascii = "a".repeat(1024 * 1024);
    let combining = "\u{301}".repeat(1024 * 1024);

    for source in [&ascii, &combining] {
        assert!(bounded_queued_line_start(source).len() <= QUEUED_PROJECTION_WINDOW_BYTES);
        assert!(bounded_queued_line_end(source).len() <= QUEUED_PROJECTION_WINDOW_BYTES);
    }
}

/// The production projection builder must not retain a complete huge prompt or
/// expand any rendered component beyond its fixed source windows.
#[test]
fn queued_prompt_projection_drops_huge_unabridged_content() {
    let source = format!("{}\n{}", "a".repeat(1024 * 1024), "界".repeat(1024 * 1024));
    let projection =
        queued_prompt_projection(&crate::tests::cli_test_theme(), false, "◯ ".into(), &source);

    assert!(projection.unabridged.is_none());
    for excerpt in [&projection.first, &projection.last] {
        let retained: usize = excerpt.spans().iter().map(|span| span.text.len()).sum();
        assert!(retained <= QUEUED_PROJECTION_WINDOW_BYTES);
    }
}

/// Every bottom-status element must keep the ten-point band documented by
/// `ARCH-tau-cli`, including shared operational and optional debug bands.
#[test]
fn status_element_priorities_cover_every_element() {
    use super::StatusElement;

    let priorities = [
        (StatusElement::Identity, 0),
        (StatusElement::Context, 10),
        (StatusElement::Tools, 20),
        (StatusElement::ActiveAgents, 20),
        (StatusElement::Description, 30),
        (StatusElement::ModelAdjustment, 30),
        (StatusElement::Watchers, 40),
        (StatusElement::WeeklyQuota, 50),
        (StatusElement::UiIoDebug, 60),
        (StatusElement::RedrawDebug, 70),
    ];

    for (element, expected) in priorities {
        assert_eq!(element.priority().get(), expected, "{element:?}");
    }
}

fn watch_turn_state_event(
    message_id: &str,
    state: tau_proto::AgentRuntimeState,
) -> tau_proto::Event {
    tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse(message_id)
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("worker"),
        sender_session_id: None,
        recipient_id: agent_id("manager"),
        kind: tau_proto::AgentMessageKind::WatchTurnState,
        watch_turn_state: Some(tau_proto::AgentWatchTurnStateNotification {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            subscription_id: "watch-1".to_owned(),
            state,
            initial: false,
            turn_generation: 1,
        }),
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        message: "non-authoritative compatibility text".to_owned(),
    })
}

/// Structured agent-turn state must drive both directed watch rendering and
/// the aggregate side-agent count after inner prompt activity has ended.
#[test]
fn watched_agent_turn_state_is_authoritative_for_running_counts() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            watcher_id: agent_id("manager"),
            watched_agent_ids: vec![agent_id("worker")],
            changed_agent_id: Some(agent_id("worker")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    renderer.handle(&watch_turn_state_event(
        "running",
        tau_proto::AgentRuntimeState::Running,
    ));

    assert!(renderer.watched_agent_is_running("manager", "worker"));
    assert_eq!(renderer.active_side_agent_count(), 1);

    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            watcher_id: agent_id("reviewer"),
            watched_agent_ids: vec![agent_id("worker")],
            changed_agent_id: Some(agent_id("worker")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    renderer.handle(&watch_turn_state_event(
        "manager-idle",
        tau_proto::AgentRuntimeState::Idle,
    ));
    renderer
        .active_agent_prompts
        .entry("worker".to_owned())
        .or_default()
        .insert("inner-round".to_owned());
    assert_eq!(
        renderer.active_side_agent_count(),
        1,
        "the reviewer edge still uses prompt fallback after the manager edge is idle"
    );
}

/// The global side-agent count must include intermediate watched ancestors in a
/// recursive chain while retaining unique target counting.
#[test]
fn watched_agent_count_projects_recursive_activity() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.watched_agents = HashMap::from([
        ("manager".to_owned(), vec!["reviewer".to_owned()]),
        ("reviewer".to_owned(), vec!["worker".to_owned()]),
    ]);
    renderer.agent_watchers = HashMap::from([
        ("reviewer".to_owned(), vec!["manager".to_owned()]),
        ("worker".to_owned(), vec!["reviewer".to_owned()]),
    ]);
    renderer
        .active_agent_prompts
        .insert("worker".to_owned(), HashSet::from(["prompt".to_owned()]));

    assert_eq!(renderer.active_side_agent_count(), 2);
    renderer.current_agent_id = Some("reviewer".to_owned());
    assert_eq!(
        renderer.active_side_agent_count(),
        1,
        "the existing selected-agent exclusion remains in force"
    );
}

/// An idle edge is terminal for its generation, and removing topology or
/// resetting the session must retire the corresponding cached lifecycle.
#[test]
fn watched_agent_turn_state_rejects_stale_start_and_clears_with_scope() {
    let mut renderer = renderer_for_agent_id_tests();
    let watch_update = tau_proto::AgentWatchesUpdated {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        watcher_id: agent_id("manager"),
        watched_agent_ids: vec![agent_id("worker")],
        changed_agent_id: Some(agent_id("worker")),
        cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    };
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(watch_update.clone()));
    renderer.handle(&watch_turn_state_event(
        "idle",
        tau_proto::AgentRuntimeState::Idle,
    ));
    renderer.handle(&watch_turn_state_event(
        "late-running",
        tau_proto::AgentRuntimeState::Running,
    ));
    assert!(!renderer.watched_agent_is_running("manager", "worker"));

    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            watched_agent_ids: Vec::new(),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
            ..watch_update.clone()
        },
    ));
    assert!(renderer.watched_agent_turn_states.is_empty());

    renderer
        .active_agent_prompts
        .entry("worker".to_owned())
        .or_default()
        .insert("inner-round".to_owned());
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(watch_update));
    assert_eq!(
        renderer.active_side_agent_count(),
        1,
        "re-enabled edges use prompt fallback before their fresh snapshot"
    );
    renderer.handle(&watch_turn_state_event(
        "running-again",
        tau_proto::AgentRuntimeState::Running,
    ));
    renderer.clear_for_new_session();
    assert!(renderer.watched_agent_turn_states.is_empty());
    assert_eq!(renderer.active_side_agent_count(), 0);
}

/// Empty topology snapshots, session changes, and endpoint unload are
/// authoritative scope boundaries even when no matching lifecycle edge arrives.
#[test]
fn watched_agent_turn_state_does_not_survive_scope_boundaries() {
    let mut renderer = renderer_for_agent_id_tests();
    let tau_proto::Event::AgentMessageReceived(orphan) =
        watch_turn_state_event("orphan", tau_proto::AgentRuntimeState::Running)
    else {
        unreachable!()
    };
    renderer
        .watched_agent_turn_states
        .entry("manager".to_owned())
        .or_default()
        .insert(
            "worker".to_owned(),
            orphan.watch_turn_state.expect("watch state"),
        );
    let live_enable = tau_proto::AgentWatchesUpdated {
        session_id: "s1"
            .parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        watcher_id: agent_id("manager"),
        watched_agent_ids: vec![agent_id("worker")],
        changed_agent_id: Some(agent_id("worker")),
        cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
    };
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(live_enable.clone()));
    assert!(
        renderer.watched_agent_turn_states.is_empty(),
        "a fresh live edge must not expose replayed orphan state"
    );
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            watched_agent_ids: Vec::new(),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchDisable,
            ..live_enable.clone()
        },
    ));
    assert!(renderer.watched_agent_turn_states.is_empty());

    let tau_proto::Event::AgentMessageReceived(replay) =
        watch_turn_state_event("replay", tau_proto::AgentRuntimeState::Running)
    else {
        unreachable!()
    };
    renderer
        .watched_agent_turn_states
        .entry("manager".to_owned())
        .or_default()
        .insert(
            "worker".to_owned(),
            replay.watch_turn_state.expect("watch state"),
        );
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            cause: tau_proto::AgentWatchUpdateCause::SessionSnapshot,
            ..live_enable
        },
    ));
    assert_eq!(
        renderer.active_side_agent_count(),
        1,
        "a replay session snapshot preserves its preceding folded lifecycle"
    );

    renderer.clear_for_new_session();
    renderer.current_session_id = Some(
        "s2".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s2"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            watcher_id: agent_id("manager"),
            watched_agent_ids: vec![agent_id("worker")],
            changed_agent_id: Some(agent_id("worker")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    renderer.handle(&watch_turn_state_event(
        "old-session",
        tau_proto::AgentRuntimeState::Running,
    ));
    assert!(renderer.watched_agent_turn_states.is_empty());

    let mut current_session =
        watch_turn_state_event("current-session", tau_proto::AgentRuntimeState::Running);
    let tau_proto::Event::AgentMessageReceived(message) = &mut current_session else {
        unreachable!()
    };
    message
        .watch_turn_state
        .as_mut()
        .expect("watch state")
        .session_id = "s2"
        .parse::<tau_proto::SessionId>()
        .expect("known-safe SessionId must be valid");
    renderer.handle(&current_session);
    assert_eq!(renderer.active_side_agent_count(), 1);
    renderer.handle(&tau_proto::Event::SessionAgentUnloaded(
        tau_proto::SessionAgentUnloaded {
            session_id: "s2"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id("manager"),
        },
    ));
    assert!(renderer.watched_agents.is_empty());
    assert!(renderer.watched_agent_turn_states.is_empty());
    assert_eq!(renderer.active_side_agent_count(), 0);
}

/// Renderer-owned auto-selection from the empty screen must retarget any
/// pending prompt draft, because the input loop is not involved in remote-event
/// selection changes.
#[test]
fn renderer_auto_select_retargets_pending_prompt_draft() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    handle.set_buffer("draft".to_owned(), "draft".len());
    let mut renderer = super::EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        crate::tests::cli_test_theme(),
    );
    let draft_handle = path_std_sync::Arc::new((
        path_std_sync::Mutex::new(DraftSlot::default()),
        path_std_sync::Condvar::new(),
    ));
    let session_id = path_std_sync::Arc::new(path_std_sync::Mutex::new(
        tau_proto::SessionId::parse("s1").expect("session id"),
    ));
    renderer.set_draft_retargeter(draft_handle.clone(), session_id);
    queue_prompt_draft_snapshot(
        draft_handle.as_ref(),
        "s1".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
        None,
        "draft".to_owned(),
    );

    renderer.handle_recorded_at(
        &tau_proto::Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
            inference_activation: false,
            agent_id: agent_id("agent-a"),
            text: "submitted".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            internal_kind: None,
            originator: tau_proto::PromptOriginator::User,
            submission_source: Default::default(),
            display_name: None,
            ctx_id: None,
        }),
        tau_proto::UnixMicros::now(),
    );

    let (mtx, _cv) = draft_handle.as_ref();
    let slot = mtx.lock().expect("draft slot");
    let (epoch, draft) = slot.pending.as_ref().expect("retargeted draft");
    assert_eq!(*epoch, 1);
    assert_eq!(
        draft.session_id,
        tau_proto::SessionId::parse("s1").expect("known-safe SessionId must be valid")
    );
    assert_eq!(draft.target_agent_id, Some(agent_id("agent-a")));
    assert_eq!(draft.text, "draft");
}

fn agent_message(sender_id: &str, recipient: &str, message: &str) -> tau_proto::Event {
    tau_proto::Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::parse(format!("msg-{sender_id}-{recipient}"))
            .expect("test message id must satisfy the identifier grammar"),
        sender_id: agent_id(sender_id),
        recipient: if recipient == "user" {
            tau_proto::AgentMessageRecipient::User
        } else {
            tau_proto::AgentMessageRecipient::Agent {
                agent_id: agent_id(recipient),
            }
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

/// Large hidden transcripts must fold one event without cloning the selected
/// terminal snapshot; this guards the permanent-freeze amplification found
/// under sustained multi-agent traffic.
#[test]
fn generated_multi_agent_load_avoids_hidden_terminal_snapshot_clones() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        "> ",
        Box::new(std::io::sink()),
        tau_cli_term::CursorShape::Bar,
    );
    let mut renderer = super::EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        crate::tests::cli_test_theme(),
    );
    renderer.switch_agent("worker-0".to_owned());

    for agent_index in 0..8 {
        let agent_id = format!("worker-{agent_index}");
        if agent_index == 0 {
            for block_index in 0..6_250 {
                renderer.handle.print_output(
                    "generated-load",
                    tau_cli_term::StyledBlock::new(format!("{agent_id}:{block_index}")),
                );
            }
            continue;
        }
        let mut state = super::AgentUiState::default();
        for block_index in 0..6_250 {
            state.output.print_output(
                "generated-load",
                tau_cli_term::StyledBlock::new(format!("{agent_id}:{block_index}")),
            );
        }
        renderer.agents_ui_state.insert(agent_id, state);
    }

    let snapshots_before = handle.output_snapshot_count();
    let blocks_before = (1..8)
        .map(|agent_index| {
            renderer.agents_ui_state[&format!("worker-{agent_index}")]
                .output
                .block_count()
        })
        .collect::<Vec<_>>();
    for agent_index in 1..8 {
        renderer.handle(&agent_message(
            &format!("worker-{agent_index}"),
            "worker-0",
            "generated update",
        ));
    }

    assert_eq!(handle.output_snapshot_count(), snapshots_before);
    for (agent_index, blocks_before) in (1..8).zip(blocks_before) {
        assert_eq!(
            renderer.agents_ui_state[&format!("worker-{agent_index}")]
                .output
                .block_count(),
            blocks_before + 1
        );
    }
}

/// User-directed agent messages are broadcasts rendered without an owning
/// agent transcript. This guards the agent-id resolver's explicit `None`
/// result so the refactored fallback chain does not accidentally route
/// broadcasts to the current agent.
#[test]
fn agent_id_for_event_preserves_user_broadcast_without_current_agent_fallback() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.current_agent_id = Some("current-agent".to_owned());

    let resolved = renderer.agent_id_for_event_for_test(&agent_message(
        "sender-agent",
        "user",
        "visible broadcast",
    ));

    assert_eq!(resolved, None);
}

/// Tool events may be attributed from prior metadata or from the event's
/// embedded agent id. This keeps both paths covered while splitting the
/// dispatcher into smaller resolver helpers.
#[test]
fn agent_id_for_event_resolves_tool_metadata_and_started_fallback() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer
        .tool_agents
        .insert("known-call".to_owned(), "metadata-agent".to_owned());

    let known_started = tau_proto::Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "known-call".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: tau_proto::CborValue::Null,
        agent_id: agent_id("started-agent"),
        originator: tau_proto::PromptOriginator::User,
    });
    let unknown_started = tau_proto::Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "unknown-call".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: tau_proto::CborValue::Null,
        agent_id: agent_id("started-agent"),
        originator: tau_proto::PromptOriginator::User,
    });

    assert_eq!(
        renderer.agent_id_for_event_for_test(&known_started),
        Some("metadata-agent".to_owned())
    );
    assert_eq!(
        renderer.agent_id_for_event_for_test(&unknown_started),
        Some("started-agent".to_owned())
    );
}

/// Shell progress can omit an explicit target and rely on metadata learned
/// from the command request. The resolver must still use that map after the
/// shell-specific branch was extracted out of the large event match.
#[test]
fn agent_id_for_event_resolves_shell_progress_from_learned_metadata() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer
        .shell_agents
        .insert("cmd-1".to_owned(), "shell-agent".to_owned());

    let progress = tau_proto::Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
        command_id: tau_proto::ShellCommandId::parse("cmd-1")
            .expect("test identifier must satisfy its grammar"),
        stream: tau_proto::ShellStream::Stdout,
        chunk: "output".to_owned(),
        target_agent_id: None,
    });

    assert_eq!(
        renderer.agent_id_for_event_for_test(&progress),
        Some("shell-agent".to_owned())
    );
}

/// UI I/O status values are compact because they live in the status bar.
/// Zero stays bare for the idle `io ↑0 ↓0` display, while nonzero byte
/// rates carry short binary unit suffixes.
#[test]
fn ui_io_rates_format_for_status_bar() {
    assert_eq!(super::format_ui_io_rate(0), "0");
    assert_eq!(super::format_ui_io_rate(999), "999B");
    assert_eq!(super::format_ui_io_rate(1024), "1K");
    assert_eq!(super::format_ui_io_rate(1536), "1.5K");
    assert_eq!(super::format_ui_io_rate(10 * 1024), "10K");
    assert_eq!(super::format_ui_io_rate(1024 * 1024 + 512 * 1024), "1.5M");
}

/// `:set show-messages` must hide, summarize, or fully render durable
/// message events based on whether they involve the user. User-directed
/// messages are broadcasts and always render fully, while agent-to-agent
/// messages still respect the privacy modes. This locks the policy down
/// without needing a terminal renderer fixture.
#[test]
fn show_messages_modes_map_user_and_agent_messages() {
    let user_recipient_message = agent_message("agent", "user", "visible body");
    let agent_message = agent_message("agent-a", "agent-b", "private body");

    let cases = [
        (
            path_tau_config_settings::ShowMessages::None,
            MessageRenderMode::Full,
            MessageRenderMode::Hidden,
        ),
        (
            path_tau_config_settings::ShowMessages::SelfSummary,
            MessageRenderMode::Full,
            MessageRenderMode::Hidden,
        ),
        (
            path_tau_config_settings::ShowMessages::SelfFull,
            MessageRenderMode::Full,
            MessageRenderMode::Hidden,
        ),
        (
            path_tau_config_settings::ShowMessages::AllSummary,
            MessageRenderMode::Full,
            MessageRenderMode::Summary,
        ),
        (
            path_tau_config_settings::ShowMessages::AllFull,
            MessageRenderMode::Full,
            MessageRenderMode::Full,
        ),
    ];

    for (mode, expected_self, expected_agent) in cases {
        assert_eq!(
            super::EventRenderer::message_render_mode(mode, &user_recipient_message),
            expected_self
        );
        assert_eq!(
            super::EventRenderer::message_render_mode(mode, &agent_message),
            expected_agent
        );
    }
}

/// Summary rendering intentionally carries no message body so private
/// content from summarized agent-agent messages cannot leak.
#[test]
fn agent_message_summary_excludes_body() {
    let message = agent_message("agent-a", "agent-b", "secret payload");

    let summary = renderer_for_agent_id_tests().agent_message_summary(&message);

    assert_eq!(summary, "Message from @agent-a to @agent-b");
    assert!(!summary.contains("secret payload"));
}

/// An explicitly named sender keeps its supplemental label even when that name
/// equals its operational role, while a manually created unnamed target is
/// rendered as its routing id without parentheses.
#[test]
fn agent_message_summary_omits_name_for_unnamed_target() {
    let message = agent_message("named-sender", "manual-target", "payload");
    let mut renderer = renderer_for_agent_id_tests();
    renderer.handle(&tau_proto::Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        agent_id: agent_id("named-sender"),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: Some("engineer".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    renderer.handle(&tau_proto::Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        agent_id: agent_id("manual-target"),
        parent_agent: None,
        role: "engineer-junior".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    }));

    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @named-sender (engineer) to @manual-target"
    );
}

/// Local message endpoints independently project authoritative restored agent
/// names while keeping both routing ids visible.
#[test]
fn agent_message_summary_projects_known_names_independently() {
    let message = agent_message("agent-a", "agent-b", "payload");
    let mut renderer = renderer_for_agent_id_tests();
    renderer.handle(&tau_proto::Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        agent_id: agent_id("agent-a"),
        parent_agent: None,
        role: "researcher".to_owned(),
        display_name: Some("something research".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @agent-a (something research) to @agent-b"
    );

    renderer.handle(&tau_proto::Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),

        agent_id: agent_id("agent-b"),
        parent_agent: None,
        role: "reviewer".to_owned(),
        display_name: Some("something else something".to_owned()),
        metadata: Vec::new(),
        ephemeral: false,
    }));
    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @agent-a (something research) to @agent-b (something else something)"
    );
    renderer.handle(&tau_proto::Event::SessionAgentUnloaded(
        tau_proto::SessionAgentUnloaded {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id("agent-b"),
        },
    ));
    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @agent-a (something research) to @agent-b (something else something)",
        "unloading does not discard durable presentation metadata"
    );
}

/// Presentation metadata is visibly escaped, grapheme-safe, and bounded so a
/// task name cannot forge terminal lines or make plain output unbounded.
#[test]
fn agent_message_names_are_sanitized_and_bounded() {
    use unicode_width::UnicodeWidthStr as _;

    let message = agent_message("agent-a", "agent-b", "payload");
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("agent-a", "x)(\\\"");
    let summary = renderer.agent_message_summary(&message);
    assert!(summary.contains(r"agent-a (x\u{0029}\u{0028}\u{005C}\u{0022})"));

    renderer
        .remember_agent_display_name("agent-a", &format!("\n\u{1b}\u{202e}{}", "👩‍🚀".repeat(100)));
    let summary = renderer.agent_message_summary(&message);
    assert!(summary.contains(r"\u{001B}\u{202E}"));
    assert!(summary.contains('…'));
    assert!(!summary.contains('\n'));
    assert!(!summary.contains('\u{1b}'));
    assert!(summary.width() <= 96);
}

/// Names that already contain their routing id are omitted rather than
/// duplicating or obscuring identity.
#[test]
fn agent_message_names_do_not_duplicate_agent_ids() {
    let message = agent_message("agent-a", "agent-b", "payload");
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("agent-a", "agent-a");
    renderer.remember_agent_display_name("agent-b", "review agent-b task");

    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @agent-a to @agent-b"
    );
}

/// Cross-session identities never borrow a same-spelled local agent's name.
#[test]
fn peer_message_names_require_endpoint_authority() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("agent-b", "local worker");
    let event = tau_proto::Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::parse("peer-message")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("agent-a"),
        recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
            session_id: "remote-session"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id("agent-b"),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: "payload".to_owned(),
    });
    assert_eq!(
        renderer.agent_message_summary(&event),
        "Message from @agent-a to remote-session/@agent-b"
    );
}

/// Late authoritative name changes reproject presentation without mutating the
/// immutable semantic message event stored for transcript history.
#[test]
fn late_agent_name_updates_reproject_message_history() {
    let message = agent_message("agent-a", "agent-b", "semantic payload");
    let mut renderer = renderer_for_agent_id_tests();
    renderer.handle(&message);
    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @agent-a to @agent-b"
    );

    renderer.handle(&tau_proto::Event::AgentDisplayNameSet(
        tau_proto::AgentDisplayNameSet {
            agent_id: agent_id("agent-b"),
            display_name: "new task".to_owned(),
        },
    ));
    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @agent-a to @agent-b (new task)"
    );
    let stored = &renderer.message_history[0].event;
    assert_eq!(stored, &message);
    assert_eq!(
        super::EventRenderer::agent_message_body(stored),
        "semantic payload"
    );
}

/// Watch content projections preserve their distinct response/prompt wording
/// while using the same supplemental endpoint labels as explicit messages.
#[test]
fn watch_content_summaries_preserve_wording_with_names() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("worker", "research task");
    renderer.remember_agent_display_name("manager", "coordination");
    let cases = [
        (
            tau_proto::AgentMessageKind::WatchResponse,
            "Response from @worker (research task) to @manager (coordination)",
        ),
        (
            tau_proto::AgentMessageKind::WatchPrompt,
            "Prompt to @worker (research task) observed by @manager (coordination)",
        ),
    ];
    for (kind, expected) in cases {
        let event = tau_proto::Event::AgentMessageSent(tau_proto::AgentMessageSent {
            message_id: tau_proto::AgentMessageId::parse(format!("watch-{kind:?}"))
                .expect("test message id must satisfy the identifier grammar"),
            sender_id: agent_id("worker"),
            recipient: tau_proto::AgentMessageRecipient::Agent {
                agent_id: agent_id("manager"),
            },
            kind,
            message: "content".to_owned(),
        });
        assert_eq!(renderer.agent_message_summary(&event), expected);
    }
}

/// A resumed different session must not inherit a same-spelled agent's local
/// display name from the previously attached session.
#[test]
fn resumed_session_clears_agent_display_name_authority() {
    let message = agent_message("agent-a", "agent-b", "payload");
    let mut renderer = renderer_for_agent_id_tests();
    renderer.handle(&tau_proto::Event::SessionStarted(
        tau_proto::SessionStarted {
            session_id: "session-a"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            reason: tau_proto::SessionStartReason::Initial,
        },
    ));
    renderer.remember_agent_display_name("agent-b", "session A worker");
    assert!(
        renderer
            .agent_message_summary(&message)
            .contains("session A worker")
    );

    renderer.handle(&tau_proto::Event::SessionStarted(
        tau_proto::SessionStarted {
            session_id: "session-b"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            reason: tau_proto::SessionStartReason::Resume,
        },
    ));
    assert_eq!(
        renderer.agent_message_summary(&message),
        "Message from @agent-a to @agent-b"
    );
}

/// Watch lifecycle records are harness-authored statuses, so the renderer must
/// use their typed payload and never label their body as an agent message. See
/// `SPEC-tau-cli-agent-message-labels`.
#[test]
fn watch_turn_state_renders_as_compact_typed_status() {
    let mut event = tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("watch-state-1")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("researcher"),
        sender_session_id: None,
        recipient_id: agent_id("manager"),
        kind: tau_proto::AgentMessageKind::WatchTurnState,
        watch_turn_state: Some(tau_proto::AgentWatchTurnStateNotification {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            subscription_id: "subscription-1".to_owned(),
            state: tau_proto::AgentRuntimeState::Running,
            initial: false,
            turn_generation: 1,
        }),
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        message: "[tau-internal]: stale presentation".to_owned(),
    });

    assert_eq!(
        renderer_for_agent_id_tests()
            .watch_turn_state_summary(&event)
            .as_deref(),
        Some("@researcher · turn started")
    );

    let tau_proto::Event::AgentMessageReceived(message) = &mut event else {
        unreachable!()
    };
    let state = message.watch_turn_state.as_mut().expect("watch state");
    state.state = tau_proto::AgentRuntimeState::Idle;
    assert_eq!(
        renderer_for_agent_id_tests()
            .watch_turn_state_summary(&event)
            .as_deref(),
        Some("@researcher · turn stopped")
    );

    let tau_proto::Event::AgentMessageReceived(message) = &mut event else {
        unreachable!()
    };
    let state = message.watch_turn_state.as_mut().expect("watch state");
    state.initial = true;
    assert_eq!(
        renderer_for_agent_id_tests()
            .watch_turn_state_summary(&event)
            .as_deref(),
        Some("Watching @researcher · idle")
    );

    let tau_proto::Event::AgentMessageReceived(message) = &mut event else {
        unreachable!()
    };
    message
        .watch_turn_state
        .as_mut()
        .expect("watch state")
        .state = tau_proto::AgentRuntimeState::Running;
    assert_eq!(
        renderer_for_agent_id_tests()
            .watch_turn_state_summary(&event)
            .as_deref(),
        Some("Watching @researcher · running")
    );
}

fn tool_call(call_id: &str) -> tau_proto::ContextItem {
    tau_proto::ContextItem::ToolCall(tau_proto::ToolCallItem {
        call_id: call_id.into(),
        name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        arguments: tau_proto::CborValue::Null,
        raw_arguments_json: None,
        responses_envelope: None,
    })
}

/// Ctrl-D must stay guarded across the assistant/tool boundary: a
/// provider response that requests tools means the session is still
/// busy even though the provider turn itself has finished.
#[test]
fn agent_activity_stays_busy_until_requested_tools_finish() {
    let mut activity = AgentActivity::default();
    activity.mark_optimistic_submission();
    assert!(activity.is_in_progress());

    activity.start_prompt(
        &"sp1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
    );
    activity.finish_prompt(
        &"sp1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        &[tool_call("call1")],
    );
    assert!(activity.is_in_progress());

    activity.finish_tool(&"call1".into());
    assert!(!activity.is_in_progress());
}

/// Side conversations use the same lifecycle events as the main chat;
/// the Ctrl-D guard must track them before UI filtering hides their
/// transcript details.
#[test]
fn agent_activity_tracks_side_conversation_prompts() {
    let mut activity = AgentActivity::default();
    activity.start_prompt(
        &"side-sp1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
    );
    assert!(activity.is_in_progress());

    activity.finish_prompt(
        &"side-sp1"
            .parse::<tau_proto::AgentPromptId>()
            .expect("known-safe AgentPromptId must be valid"),
        &[],
    );
    assert!(!activity.is_in_progress());
}

#[test]
fn role_details_abbreviate_description() {
    let details = RoleCompletionDetails::from_description(
        "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off, tools=read_only, enable-tools=web_search",
    );

    assert_eq!(
        details.short_description(),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off tools=read_only et=web_search"
    );
}

/// `:role <name>` completion appends free-form role descriptions after the
/// parsed model/knob summary instead of parsing that user text as settings.
#[test]
fn role_details_append_configured_role_description() {
    let details = RoleCompletionDetails::from_role_info(&tau_proto::HarnessRoleInfo {
        name: "deep".to_owned(),
        description:
            "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off"
                .to_owned(),
        role_description: Some("Investigate deeply, no rush = thorough".to_owned()),
        details: None,
    });

    assert_eq!(
        details.short_description(),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off — Investigate deeply, no rush = thorough"
    );
}

#[test]
fn role_details_prefer_structured_fields_over_description_text() {
    let details = RoleCompletionDetails::from_role_info(&tau_proto::HarnessRoleInfo {
        name: "deep".to_owned(),
        description: "free-form text, not parsed as settings".to_owned(),
        role_description: None,
        details: Some(tau_proto::HarnessRoleDetails {
            model: Some("provider/model".into()),
            params: tau_proto::ModelParams {
                effort: tau_proto::Effort::High,
                verbosity: tau_proto::Verbosity::Low,
                thinking_summary: tau_proto::ThinkingSummary::Concise,
                service_tier: Some(tau_proto::ServiceTier::Fast),
            },
            tools: Some(vec![tau_proto::ToolName::new("read")]),
            enable_tool_groups: vec![tau_proto::ToolGroupName::new("pim")],
            disable_tool_groups: vec![tau_proto::ToolGroupName::new("shell")],
            enable_tools: vec![tau_proto::ToolName::new("web_search")],
            disable_tools: vec![tau_proto::ToolName::new("shell")],
        }),
    });

    assert_eq!(
        details.short_description(),
        "provider/model e=high v=low ts=concise st=fast tools=read etg=pim dtg=shell et=web_search dt=shell"
    );
}

#[test]
fn role_details_structured_role_without_model_renders_as_no_model() {
    let details = RoleCompletionDetails::from_role_info(&tau_proto::HarnessRoleInfo {
        name: "none".to_owned(),
        description: "free-form fallback text".to_owned(),
        role_description: None,
        details: Some(tau_proto::HarnessRoleDetails::default()),
    });

    assert_eq!(details.short_description(), "no model");
    assert_eq!(details.current_description("effort"), "unset");
    assert_eq!(details.current_description("model"), "unset");
}

#[test]
fn role_details_report_single_current_field() {
    let details = RoleCompletionDetails::from_description(
        "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off, service-tier=fast, tools=read_only, enable-tools=web_search",
    );

    assert_eq!(details.current_description("model"), "codex-dpcpw/gpt-5.5");
    assert_eq!(details.current_description("effort"), "xhigh");
    assert_eq!(details.current_description("verbosity"), "medium");
    assert_eq!(details.current_description("thinking-summary"), "off");
    assert_eq!(details.current_description("service-tier"), "fast");
    assert_eq!(details.current_description("tools"), "read_only");
    assert_eq!(details.current_description("enable-tools"), "web_search");
}

#[test]
fn role_values_have_descriptions() {
    let item = role_value_completion("thinking-summary", "detailed");

    assert_eq!(item.value, "detailed");
    assert_eq!(item.description, "detailed thinking summaries");
}

/// Ensures `:role ... effort` completion exposes GPT-5.6 maximum effort with a
/// description distinct from `xhigh`.
#[test]
fn role_effort_completions_include_max() {
    let items = role_setting_value_completions("effort", "max");

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].value, "max");
    assert_eq!(items[0].description, "maximum reasoning effort for GPT-5.6");
}

/// Ensures the real embedded harness tool/continuation event sequence leaves
/// main, global, and watched activity idle when rendered without synthetic
/// prompt cleanup.
#[test]
fn embedded_tool_continuation_trace_renders_fully_idle() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let state_dir = temp.path().join("state");
    let outcome =
        tau_test_support::run_causal_quota_fixture(&state_dir).expect("causal quota fixture");
    assert_eq!(outcome.interaction.tool_calls.len(), 1);
    assert_eq!(outcome.interaction.tool_results.len(), 1);
    let events = outcome.events;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, tau_proto::Event::ProviderPromptSubmitted(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, tau_proto::Event::ProviderResponseFinished(_)))
            .count(),
        2
    );

    let fixture_agent = events
        .iter()
        .find_map(|event| match event {
            tau_proto::Event::ProviderResponseFinished(finished) => Some(finished.agent_id.clone()),
            _ => None,
        })
        .expect("fixture agent");
    let mut renderer = renderer_for_agent_id_tests();
    renderer.current_session_id = Some(
        "s1".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );
    renderer.switch_agent(fixture_agent.to_string());
    let mut saw_main_active = false;
    let mut saw_global_active = false;
    for event in &events {
        renderer.handle(event);
        saw_main_active |= renderer.main_agent_is_in_progress_for_test();
        saw_global_active |= renderer
            .agent_in_progress_state()
            .load(path_std_sync_atomic::Ordering::Relaxed);
    }

    let mut watched_renderer = renderer_for_agent_id_tests();
    watched_renderer.current_session_id = Some(
        "s1".parse::<tau_proto::SessionId>()
            .expect("known-safe SessionId must be valid"),
    );
    watched_renderer.current_agent_id = Some("manager".to_owned());
    watched_renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            watcher_id: agent_id("manager"),
            watched_agent_ids: vec![fixture_agent.clone()],
            changed_agent_id: Some(fixture_agent),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    let mut saw_watched_active = false;
    for event in &events {
        watched_renderer.handle(event);
        saw_watched_active |= watched_renderer.active_side_agent_count() == 1;
    }

    assert!(
        saw_main_active,
        "submitted causal prompt must activate main UI"
    );
    assert!(
        saw_global_active,
        "submitted causal prompt must activate global UI"
    );
    assert!(
        saw_watched_active,
        "the same causal prompt must activate watched-agent fallback state"
    );
    assert!(
        !renderer.main_agent_is_in_progress_for_test(),
        "final user terminal must clear effective main-turn activity"
    );
    assert!(
        !renderer
            .agent_in_progress_state()
            .load(std::sync::atomic::Ordering::Relaxed),
        "tool result and continuation terminal must clear global activity"
    );
    assert_eq!(
        watched_renderer.active_side_agent_count(),
        0,
        "the causal terminal must naturally clear watched-agent activity"
    );
}

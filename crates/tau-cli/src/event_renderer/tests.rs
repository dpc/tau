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

fn blocker_started(call_id: &str, action: &str) -> tau_proto::ToolStarted {
    tau_proto::ToolStarted {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("blocker"),
        arguments: tau_proto::CborValue::Map(vec![
            (
                tau_proto::CborValue::Text("action".to_owned()),
                tau_proto::CborValue::Text(action.to_owned()),
            ),
            (
                tau_proto::CborValue::Text("description".to_owned()),
                tau_proto::CborValue::Text("private blocker payload".to_owned()),
            ),
        ]),
        agent_id: agent_id("blocker-agent"),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn blocker_result(call_id: &str) -> tau_proto::ToolResultDisplay {
    tau_proto::ToolResultDisplay {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("blocker"),
        tool_type: tau_proto::ToolType::Function,
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "unrelated result descriptor".to_owned(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn blocker_error(call_id: &str) -> tau_proto::ToolError {
    tau_proto::ToolError {
        presentation: Default::default(),
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new("blocker"),
        tool_type: tau_proto::ToolType::Function,
        message: "private blocker failure".to_owned(),
        details: None,
        display: Some(tau_proto::ToolUseState {
            args: "unrelated error descriptor".to_owned(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn rendered_tool_header(display: &crate::tool_render::ToolCallDisplay) -> String {
    crate::tool_render::render_tool_block(&crate::tests::cli_test_theme(), display)
        .priority_line_content()
        .expect("tool header")
        .layout(120)
        .iter()
        .map(|cell| cell.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn rendered_tool_block_text(display: &crate::tool_render::ToolCallDisplay) -> String {
    let block = crate::tool_render::render_tool_block(&crate::tests::cli_test_theme(), display);
    let header: String = block
        .priority_line_content()
        .expect("tool header")
        .layout(120)
        .iter()
        .map(|cell| cell.ch)
        .collect();
    let body: String = block
        .priority_line_body_content()
        .into_iter()
        .flat_map(|body| body.spans())
        .map(|span| span.text.as_str())
        .collect();
    format!("{header}{body}")
}

/// Canonical terminal outcomes replace contradictory status hints while
/// preserving valid warnings, aligned error labels, and unrelated metadata.
#[test]
fn canonical_terminal_outcome_owns_status_matrix() {
    use tau_proto::ToolUseStatus::{Error, InProgress, Success, Warning};

    let cases = [
        (
            Success,
            "custom-success",
            super::TerminalToolOutcome::SuccessResult,
            Success,
            "ok",
        ),
        (
            Error,
            "false-error",
            super::TerminalToolOutcome::SuccessResult,
            Success,
            "ok",
        ),
        (
            InProgress,
            "still-running",
            super::TerminalToolOutcome::SuccessResult,
            Success,
            "ok",
        ),
        (
            Warning,
            "timeout",
            super::TerminalToolOutcome::SuccessResult,
            Warning,
            "timeout",
        ),
        (
            Warning,
            "",
            super::TerminalToolOutcome::SuccessResult,
            Warning,
            "warn",
        ),
        (
            Success,
            "false-ok",
            super::TerminalToolOutcome::Error {
                canonical_message: "\n canonical-failure\ntrailing",
            },
            Error,
            "canonical-failure",
        ),
        (
            Warning,
            "false-warning",
            super::TerminalToolOutcome::Error {
                canonical_message: "canonical-failure",
            },
            Error,
            "canonical-failure",
        ),
        (
            InProgress,
            "still-running",
            super::TerminalToolOutcome::Error {
                canonical_message: "canonical-failure",
            },
            Error,
            "canonical-failure",
        ),
        (
            Error,
            "custom-error",
            super::TerminalToolOutcome::Error {
                canonical_message: "canonical-failure",
            },
            Error,
            "custom-error",
        ),
        (
            Error,
            " ",
            super::TerminalToolOutcome::Error {
                canonical_message: "",
            },
            Error,
            "err",
        ),
    ];

    for (descriptor_status, descriptor_text, outcome, expected_status, expected_text) in cases {
        let descriptor = tau_proto::ToolUseState {
            args: "terminal-args".to_owned(),
            info_chips: vec!["metadata-sentinel".to_owned()],
            status: descriptor_status,
            status_text: descriptor_text.to_owned(),
            ..Default::default()
        };
        let normalized = super::normalize_terminal_tool_use_state(descriptor, outcome);

        assert_eq!(normalized.status, expected_status);
        assert_eq!(normalized.status_text, expected_text);
        assert_eq!(normalized.args, "terminal-args");
        assert_eq!(normalized.info_chips, ["metadata-sentinel"]);
    }
}

/// The foreground builders apply canonical status classes while retaining
/// terminal metadata and the existing compact status wording.
#[test]
fn terminal_builders_render_canonical_status_wording() {
    let result = tau_proto::ToolResultDisplay {
        call_id: "result-call".into(),
        tool_name: tau_proto::ToolName::new("generic"),
        tool_type: tau_proto::ToolType::Function,
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolUseState {
            args: "terminal-args".to_owned(),
            status: tau_proto::ToolUseStatus::Error,
            status_text: "false-error".to_owned(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    };
    assert_eq!(
        rendered_tool_header(&super::EventRenderer::tool_result_display(&result)),
        "generic terminal-args ok"
    );

    let error = tau_proto::ToolError {
        presentation: Default::default(),
        call_id: "error-call".into(),
        tool_name: tau_proto::ToolName::new("generic"),
        tool_type: tau_proto::ToolType::Function,
        message: "canonical-failure".to_owned(),
        details: None,
        display: Some(tau_proto::ToolUseState {
            args: "terminal-args".to_owned(),
            status: tau_proto::ToolUseStatus::Success,
            status_text: "false-ok".to_owned(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    };
    assert_eq!(
        rendered_tool_header(&super::EventRenderer::tool_error_display(&error)),
        "generic terminal-args err: canonical-failure"
    );
}

/// Delegate error fallback metadata survives normalization when `agent_start`
/// has no producer descriptor.
#[test]
fn delegate_error_fallback_retains_stats_and_canonical_wording() {
    let error = tau_proto::ToolError {
        presentation: Default::default(),
        call_id: "delegate-error".into(),
        tool_name: tau_proto::ToolName::new("agent_start"),
        tool_type: tau_proto::ToolType::Function,
        message: "canonical-delegate-failure".to_owned(),
        details: Some(tau_proto::CborValue::Text("line one\nline two".to_owned())),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };

    assert_eq!(
        rendered_tool_header(&super::EventRenderer::tool_error_display(&error)),
        "agent_start 2L, 17B err: canonical-delegate-failure"
    );
}

/// Background result and error handlers pass contradictory descriptors through
/// the same canonical terminal normalization as foreground completions.
#[test]
fn background_terminal_handlers_normalize_status() {
    let mut renderer = renderer_for_agent_id_tests();
    for call_id in ["background-result", "background-error"] {
        renderer.handle_socket_delivery(
            &tau_proto::Event::ToolStarted(tau_proto::ToolStarted {
                call_id: call_id.into(),
                tool_name: tau_proto::ToolName::new("generic"),
                arguments: tau_proto::CborValue::Map(Vec::new()),
                agent_id: agent_id("background-agent"),
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(1),
            1,
        );
        renderer.handle_socket_delivery(
            &tau_proto::Event::ToolResultDisplay(tau_proto::ToolResultDisplay {
                call_id: call_id.into(),
                tool_name: tau_proto::ToolName::new("generic"),
                tool_type: tau_proto::ToolType::Function,
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
            tau_proto::UnixMicros::new(2),
            2,
        );
    }

    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolBackgroundResultDisplay(tau_proto::ToolBackgroundResultDisplay {
            call_id: "background-result".into(),
            tool_name: tau_proto::ToolName::new("generic"),
            tool_type: tau_proto::ToolType::Function,
            display: Some(tau_proto::ToolUseState {
                args: "result-metadata".to_owned(),
                status: tau_proto::ToolUseStatus::Error,
                status_text: "false-error".to_owned(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(3),
        3,
    );
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "background-error".into(),
            tool_name: tau_proto::ToolName::new("generic"),
            tool_type: tau_proto::ToolType::Function,
            message: "background-failure".to_owned(),
            details: None,
            display: Some(tau_proto::ToolUseState {
                args: "error-metadata".to_owned(),
                status: tau_proto::ToolUseStatus::Success,
                status_text: "false-ok".to_owned(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(4),
        4,
    );

    let headers = renderer
        .tool_history
        .iter()
        .map(|entry| rendered_tool_header(&entry.display))
        .collect::<Vec<_>>();
    assert_eq!(headers[0], "generic result-metadata 0s ok");
    assert_eq!(
        headers[1],
        "generic error-metadata 0s err: background-failure"
    );
    assert!(renderer.tool_calls.is_empty());
}

/// Blocker action labels stay visible from the live start through each terminal
/// lifecycle, without exposing other structured invocation or terminal fields.
#[test]
fn blocker_actions_survive_live_and_terminal_tool_lifecycles() {
    let mut renderer = renderer_for_agent_id_tests();

    let add = blocker_started("blocker-add", "add");
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolStarted(add),
        tau_proto::UnixMicros::new(1),
        1,
    );
    assert_eq!(
        rendered_tool_header(
            renderer.tool_calls["blocker-add"]
                .live_display
                .as_ref()
                .expect("live blocker display"),
        ),
        "blocker add 0s pending"
    );
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolProgress(tau_proto::ToolProgress {
            call_id: "blocker-add".into(),
            tool_name: tau_proto::ToolName::new("blocker"),
            message: Some("private progress message".to_owned()),
            progress: None,
            display: Some(tau_proto::ToolUseState {
                args: "private progress descriptor".to_owned(),
                mode: "private mode".to_owned(),
                info_chips: vec!["private progress chip".to_owned()],
                payload: Some(tau_proto::ToolUsePayload::Text {
                    text: "private progress payload".to_owned(),
                }),
                ..Default::default()
            }),
        }),
        tau_proto::UnixMicros::new(2),
        2,
    );
    let progress_header = rendered_tool_header(
        renderer.tool_calls["blocker-add"]
            .live_display
            .as_ref()
            .expect("progress blocker display"),
    );
    assert!(progress_header.starts_with("blocker add "));
    assert!(progress_header.ends_with(" pending"));
    assert!(!progress_header.contains("private"));
    assert!(
        !rendered_tool_block_text(
            renderer.tool_calls["blocker-add"]
                .live_display
                .as_ref()
                .expect("progress blocker display"),
        )
        .contains("private")
    );
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolError(blocker_error("blocker-add")),
        tau_proto::UnixMicros::new(3),
        3,
    );

    let cancel = blocker_started("blocker-cancel", "cancel");
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolStarted(cancel),
        tau_proto::UnixMicros::new(4),
        4,
    );
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolCancelled(tau_proto::ToolCancelled {
            presentation: Default::default(),
            call_id: "blocker-cancel".into(),
            tool_name: tau_proto::ToolName::new("blocker"),
            tool_type: tau_proto::ToolType::Function,
        }),
        tau_proto::UnixMicros::new(5),
        5,
    );

    let list = blocker_started("blocker-list", "list");
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolStarted(list),
        tau_proto::UnixMicros::new(6),
        6,
    );
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolResultDisplay(blocker_result("blocker-list")),
        tau_proto::UnixMicros::new(7),
        7,
    );

    let headers = renderer
        .tool_history
        .iter()
        .map(|entry| rendered_tool_header(&entry.display))
        .collect::<Vec<_>>();
    assert!(headers[0].starts_with("blocker add 0s err: failed"));
    assert!(headers[1].starts_with("blocker cancel 0s err: cancelled"));
    assert_eq!(headers[2], "blocker list 0s ok");
    assert!(
        renderer
            .tool_history
            .iter()
            .all(|entry| !rendered_tool_block_text(&entry.display).contains("private"))
    );
}

/// An invalid blocker action fails closed through terminal rendering rather
/// than letting untrusted progress or result descriptors reach the compact
/// header.
#[test]
fn malformed_blocker_action_hides_all_descriptor_payloads() {
    let mut renderer = renderer_for_agent_id_tests();
    let started = blocker_started("malformed-blocker", "delete");
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolStarted(started),
        tau_proto::UnixMicros::new(1),
        1,
    );
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolResultDisplay(blocker_result("malformed-blocker")),
        tau_proto::UnixMicros::new(2),
        2,
    );

    let header = rendered_tool_header(
        &renderer
            .tool_history
            .last()
            .expect("completed malformed blocker")
            .display,
    );
    assert_eq!(header, "blocker 0s ok");
    assert!(!header.contains("private"));
    assert!(!header.contains("unrelated"));
}

/// Missing, non-text, duplicate, and container action values must all fail
/// closed rather than selecting one potentially valid-looking map entry.
#[test]
fn blocker_action_descriptor_rejects_ambiguous_or_invalid_arguments() {
    let invalid_arguments = [
        tau_proto::CborValue::Map(vec![]),
        tau_proto::CborValue::Map(vec![(
            tau_proto::CborValue::Text("action".to_owned()),
            tau_proto::CborValue::Integer(1.into()),
        )]),
        tau_proto::CborValue::Map(vec![
            (
                tau_proto::CborValue::Text("action".to_owned()),
                tau_proto::CborValue::Text("add".to_owned()),
            ),
            (
                tau_proto::CborValue::Text("action".to_owned()),
                tau_proto::CborValue::Text("list".to_owned()),
            ),
        ]),
        tau_proto::CborValue::Array(vec![tau_proto::CborValue::Text("add".to_owned())]),
    ];

    for arguments in invalid_arguments {
        let mut started = blocker_started("invalid-action", "add");
        started.arguments = arguments;
        assert!(super::blocker_action_descriptor(&started).is_none());
    }
}

/// Cold attachment reconstructs a pending blocker start before its replayed
/// terminal, so the safe action label remains available after completion.
#[test]
fn reconstructed_blocker_start_preserves_action_for_replayed_completion() {
    let mut renderer = renderer_for_agent_id_tests();
    let started = blocker_started("replayed-blocker", "list");
    let owner = started.agent_id.clone();
    let event = tau_proto::Event::ToolStarted(started);

    renderer.handle_reconstructed_tool_start_socket_delivery(
        &event,
        &owner,
        tau_proto::UnixMicros::new(10),
        10,
    );
    renderer.handle_socket_delivery(
        &tau_proto::Event::ToolResultDisplay(blocker_result("replayed-blocker")),
        tau_proto::UnixMicros::new(11),
        11,
    );

    assert_eq!(
        renderer
            .tool_history
            .last()
            .expect("completed replayed blocker")
            .display
            .args,
        "list"
    );
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
        (StatusElement::WorkTitle, 30),
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

/// Generic complete stats keep a watched row running across an inner
/// continuation until the outer turn becomes idle.
#[test]
fn watched_agent_stats_keep_running_until_outer_turn_is_idle() {
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
    let stats = |runtime_state| {
        tau_proto::Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
            session_id: "s1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            agent_id: agent_id("worker"),
            work_status: Default::default(),
            navigation_mode: tau_proto::AgentNavigationMode::Active,
            runtime_state,
            turn_activity: tau_proto::AgentTurnActivity::Idle,
            tools: Default::default(),
            context: Default::default(),
            estimated_api_cost: Default::default(),
            creator_subtree_estimated_api_cost: Default::default(),
        })
    };

    renderer.handle(&stats(tau_proto::AgentRuntimeState::Running));
    assert!(renderer.watched_agent_is_running("worker"));

    renderer.handle(&stats(tau_proto::AgentRuntimeState::Idle));
    assert!(!renderer.watched_agent_is_running("worker"));
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
            trusted_internal_spans: Vec::new(),
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
    assert_eq!(draft.text, None);
}

fn agent_message(sender_id: &str, recipient: &str, message: &str) -> tau_proto::Event {
    tau_proto::Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::parse(format!("msg-{sender_id}-{recipient}"))
            .expect("test message id must satisfy the identifier grammar"),
        sender_id: agent_id(sender_id),
        recipient: tau_proto::AgentMessageRecipient::Agent {
            agent_id: agent_id(recipient),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

fn received_agent_message(
    sender_id: &str,
    sender_session_id: Option<&str>,
    recipient_id: &str,
    message: &str,
) -> tau_proto::Event {
    tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse(format!(
            "received-{sender_id}-{recipient_id}"
        ))
        .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id(sender_id),
        sender_session_id: sender_session_id.map(|value| {
            value
                .parse()
                .expect("test session id must satisfy its grammar")
        }),
        recipient_id: agent_id(recipient_id),
        kind: tau_proto::AgentMessageKind::Message,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: None,
        message: message.to_owned(),
    })
}

fn block_text(block: &tau_cli_term::StyledBlock) -> String {
    block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect()
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

/// Sender projections remain owned by the sending agent instead of falling
/// back to the currently selected agent after user broadcasts are removed.
#[test]
fn agent_id_for_event_routes_sent_message_to_sender() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.current_agent_id = Some("current-agent".to_owned());

    let resolved = renderer.agent_id_for_event_for_test(&agent_message(
        "sender-agent",
        "recipient-agent",
        "visible message",
    ));

    assert_eq!(resolved, Some("sender-agent".to_owned()));
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

/// Ordinary tool starts preserve empty-screen selection semantics, while the
/// validated reconstructed presentation selects only a user-originated owner.
#[test]
fn reconstructed_tool_start_selection_is_explicit_and_user_scoped() {
    let owner = agent_id("started-agent");
    let user_start = tau_proto::Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "user-call".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: tau_proto::CborValue::Null,
        agent_id: owner.clone(),
        originator: tau_proto::PromptOriginator::User,
    });
    let mut ordinary = renderer_for_agent_id_tests();
    ordinary.handle_socket_delivery(&user_start, tau_proto::UnixMicros::new(1), 1);
    assert_eq!(ordinary.current_agent_id, None);
    assert_eq!(ordinary.displayed_agent_id, None);

    let mut reconstructed = renderer_for_agent_id_tests();
    reconstructed.handle_reconstructed_tool_start_socket_delivery(
        &user_start,
        &owner,
        tau_proto::UnixMicros::new(1),
        1,
    );
    assert_eq!(
        reconstructed.current_agent_id.as_deref(),
        Some(owner.as_str())
    );
    assert_eq!(
        reconstructed.displayed_agent_id.as_deref(),
        Some(owner.as_str())
    );

    let extension_start = tau_proto::Event::ToolStarted(tau_proto::ToolStarted {
        call_id: "extension-call".into(),
        tool_name: tau_proto::ToolName::new("read"),
        arguments: tau_proto::CborValue::Null,
        agent_id: owner.clone(),
        originator: tau_proto::PromptOriginator::Extension {
            name: tau_proto::ExtensionName::parse("fixture").expect("valid extension name"),
            query_id: "query-1".to_owned(),
        },
    });
    let mut extension = renderer_for_agent_id_tests();
    extension.handle_reconstructed_tool_start_socket_delivery(
        &extension_start,
        &owner,
        tau_proto::UnixMicros::new(1),
        1,
    );
    assert_eq!(extension.current_agent_id, None);
    assert_eq!(extension.displayed_agent_id, None);
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

/// `:set show-messages` must continue to hide, summarize, or fully render
/// agent messages after removal of the user-broadcast recipient.
#[test]
fn show_messages_modes_map_agent_messages() {
    let cases = [
        (
            path_tau_config_settings::ShowMessages::None,
            MessageRenderMode::Hidden,
        ),
        (
            path_tau_config_settings::ShowMessages::SelfSummary,
            MessageRenderMode::Hidden,
        ),
        (
            path_tau_config_settings::ShowMessages::SelfFull,
            MessageRenderMode::Hidden,
        ),
        (
            path_tau_config_settings::ShowMessages::AllSummary,
            MessageRenderMode::Summary,
        ),
        (
            path_tau_config_settings::ShowMessages::AllFull,
            MessageRenderMode::Full,
        ),
    ];

    for (mode, expected_agent) in cases {
        assert_eq!(
            super::EventRenderer::message_render_mode(mode),
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

/// Watched responses are ordinary messages, while watched prompts retain their
/// distinct lifecycle wording and both use supplemental endpoint labels.
#[test]
fn watch_content_summaries_preserve_wording_with_names() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("worker", "research task");
    renderer.remember_agent_display_name("manager", "coordination");
    let cases = [
        (
            tau_proto::AgentMessageKind::WatchResponse,
            "Message from @worker (research task) to @manager (coordination)",
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

/// Selected transcript projections omit the endpoint already established by
/// the view while retaining the remote endpoint's task label.
#[test]
fn selected_agent_messages_show_only_the_remote_endpoint() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("worker", "implementation");
    renderer.remember_agent_display_name("manager", "coordination");
    renderer.displayed_agent_id = Some("manager".to_owned());
    let inbound = received_agent_message("worker", None, "manager", "inbound body");
    assert_eq!(
        block_text(&renderer.render_agent_message_block(&inbound)),
        format!(
            "{}Message from @worker (implementation):\ninbound body",
            crate::transcript_markers::MESSAGE
        )
    );

    let outbound = agent_message("manager", "worker", "outbound body");
    assert_eq!(
        block_text(&renderer.render_agent_message_block(&outbound)),
        format!(
            "{}Message to @worker (implementation):\noutbound body",
            crate::transcript_markers::MESSAGE
        )
    );
}

/// External message facts use the message marker without changing their
/// already-escaped renderer output.
#[test]
fn external_message_facts_use_the_message_marker() {
    let renderer = renderer_for_agent_id_tests();
    assert_eq!(
        block_text(
            &renderer
                .submitted_message_fact_block("External `bridge-main` message:\nbody".to_owned())
        ),
        "■ External `bridge-main` message:\nbody"
    );
}

/// A qualified remote sender never becomes the selected local endpoint merely
/// because both agents have the same bare routing id.
#[test]
fn selected_inbound_message_matches_endpoints_with_session_scope() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.displayed_agent_id = Some("same".to_owned());
    let inbound = received_agent_message("same", Some("remote-session"), "same", "remote body");

    assert_eq!(
        block_text(&renderer.render_agent_message_block(&inbound)),
        format!(
            "{}Message from remote-session/@same:\nremote body",
            crate::transcript_markers::MESSAGE
        )
    );
}

/// The no-selection overview must retain both independently named routing
/// endpoints because no current agent supplies implicit context.
#[test]
fn overview_messages_show_both_endpoint_task_labels() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("worker", "implementation");
    renderer.remember_agent_display_name("manager", "coordination");
    renderer.displayed_agent_id = None;
    let message = agent_message("worker", "manager", "preserved body");

    assert_eq!(
        block_text(&renderer.render_agent_message_block(&message)),
        format!(
            "{}Message from @worker (implementation) to @manager (coordination):\npreserved body",
            crate::transcript_markers::MESSAGE
        )
    );
}

/// Structured work-status reports for every reportable state must render their
/// semantic phase and task instead of the empty compatibility message body.
#[test]
fn watch_work_status_renders_all_reportable_states() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.remember_agent_display_name("worker", "implementation");
    renderer.show_messages = path_tau_config_settings::ShowMessages::None;
    for (phase, label, symbol) in [
        (tau_proto::AgentWorkStatusPhase::Working, "working", "🚀"),
        (tau_proto::AgentWorkStatusPhase::Done, "done", "✅"),
        (tau_proto::AgentWorkStatusPhase::Blocked, "blocked", "⛔️"),
    ] {
        let event = tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse(format!("status-{label}"))
                .expect("test identifier must satisfy its grammar"),
            sender_id: agent_id("worker"),
            sender_session_id: None,
            recipient_id: agent_id("manager"),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: "session-1"
                    .parse::<tau_proto::SessionId>()
                    .expect("known-safe SessionId must be valid"),
                subscription_id: "subscription-1".to_owned(),
                status_epoch: 1,
                phase,
                title: Some(format!("{label} task")),
                initial: false,
            }),
            watch_long_wait: None,
            message: "must not render".to_owned(),
        });

        assert_eq!(
            block_text(&renderer.render_agent_message_block(&event)),
            format!(
                "{}Status update from @worker (implementation): {symbol} ({label} task)",
                crate::transcript_markers::STATUS_UPDATE
            )
        );
    }
}

/// Initial watch snapshots establish the status-row state without creating a
/// pointless transcript notification, while a later explicit report remains
/// visible to the watcher.
#[test]
fn initial_watch_work_status_is_cached_without_a_transcript_notification() {
    let mut renderer = renderer_for_agent_id_tests();
    renderer.current_session_id =
        Some(tau_proto::SessionId::parse("session-1").expect("valid session ID"));
    renderer.current_agent_id = Some("manager".to_owned());
    renderer.displayed_agent_id = Some("manager".to_owned());
    renderer.handle(&tau_proto::Event::AgentWatchesUpdated(
        tau_proto::AgentWatchesUpdated {
            session_id: tau_proto::SessionId::parse("session-1").expect("valid session ID"),
            watcher_id: agent_id("manager"),
            watched_agent_ids: vec![agent_id("worker")],
            changed_agent_id: Some(agent_id("worker")),
            cause: tau_proto::AgentWatchUpdateCause::AgentWatchEnable,
        },
    ));
    let status_event = |message_id: &str, phase, title, initial| {
        tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
            message_id: tau_proto::AgentMessageId::parse(message_id).expect("valid message ID"),
            sender_id: agent_id("worker"),
            sender_session_id: None,
            recipient_id: agent_id("manager"),
            kind: tau_proto::AgentMessageKind::WatchWorkStatus,
            watch_provider_status: None,
            watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                session_id: tau_proto::SessionId::parse("session-1").expect("valid session ID"),
                subscription_id: "subscription-1".to_owned(),
                status_epoch: 1,
                phase,
                title,
                initial,
            }),
            watch_long_wait: None,
            message: String::new(),
        })
    };

    renderer.handle_socket_delivery(
        &status_event(
            "initial-status",
            tau_proto::AgentWorkStatusPhase::Unreported,
            None,
            true,
        ),
        tau_proto::UnixMicros::new(1),
        1,
    );
    assert_eq!(renderer.message_history.len(), 0);
    assert_eq!(
        renderer
            .watched_agent_work_statuses
            .get("worker")
            .expect("initial snapshot cached")
            .phase,
        tau_proto::AgentWorkStatusPhase::Unreported
    );

    renderer.handle_socket_delivery(
        &status_event(
            "working-status",
            tau_proto::AgentWorkStatusPhase::Working,
            Some("implementation".to_owned()),
            false,
        ),
        tau_proto::UnixMicros::new(2),
        2,
    );
    assert_eq!(renderer.message_history.len(), 1);
    assert_eq!(
        renderer
            .watched_agent_work_statuses
            .get("worker")
            .expect("explicit transition cached")
            .phase,
        tau_proto::AgentWorkStatusPhase::Working
    );
    assert_eq!(
        block_text(&renderer.render_agent_message_block(&renderer.message_history[0].event)),
        "▤ Status update from @worker: 🚀 (implementation)"
    );
}

/// Provider-work normalizes its model-facing authenticated envelope for
/// terminal display, while a long-wait record uses its typed threshold.
#[test]
fn watch_provider_and_long_wait_statuses_use_intentional_markers() {
    let renderer = renderer_for_agent_id_tests();
    let provider = tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("provider-live")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("worker"),
        sender_session_id: None,
        recipient_id: agent_id("manager"),
        kind: tau_proto::AgentMessageKind::WatchProviderStatus,
        watch_provider_status: Some(tau_proto::AgentWatchProviderStatusNotification {
            session_id: tau_proto::SessionId::parse("session-1").expect("valid session id"),
            subscription_id: "subscription-1".to_owned(),
            turn_generation: 1,
            agent_prompt_id: tau_proto::AgentPromptId::parse("prompt-1").expect("valid prompt id"),
            state: tau_proto::AgentWatchProviderState::Blocked {
                category: tau_proto::AgentWatchProviderCategory::Account,
            },
            initial: false,
        }),
        watch_work_status: None,
        watch_long_wait: None,
        message: "<tau_internal>Watched agent worker provider status: retrying (unknown, attempt 1, next retry about 11s)&lt;/tau_internal&gt;".to_owned(),
    });
    let mut initial_provider = provider.clone();
    let tau_proto::Event::AgentMessageReceived(initial_message) = &mut initial_provider else {
        unreachable!("cloned provider event retains its variant");
    };
    initial_message
        .watch_provider_status
        .as_mut()
        .expect("provider event has typed status")
        .initial = true;
    let long_wait = tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("long-wait")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("worker"),
        sender_session_id: None,
        recipient_id: agent_id("manager"),
        kind: tau_proto::AgentMessageKind::WatchLongWait,
        watch_provider_status: None,
        watch_work_status: None,
        watch_long_wait: Some(tau_proto::AgentWatchLongWaitNotification {
            session_id: tau_proto::SessionId::parse("session-1").expect("valid session id"),
            subscription_id: "subscription-1".to_owned(),
            status_epoch: 1,
            threshold_minutes: 5,
        }),
        message: String::new(),
    });

    assert_eq!(
        block_text(&renderer.render_agent_message_block(&provider)),
        "□ [tau-internal]: Watched agent worker provider status: retrying (unknown, attempt 1, next retry about 11s)"
    );
    assert_eq!(
        block_text(&renderer.render_agent_message_block(&initial_provider)),
        "□ [tau-internal current snapshot]: Watched agent worker provider status: retrying (unknown, attempt 1, next retry about 11s)"
    );
    for (body, expected) in [
        (
            "<tau_internal>partial",
            "□ [tau-internal]: <tau_internal>partial",
        ),
        (
            "legacy &lt;/tau_internal&gt;",
            "□ [tau-internal]: legacy &lt;/tau_internal&gt;",
        ),
        (
            "<tau_internal>nested <tau_internal>body&lt;/tau_internal&gt;",
            "□ [tau-internal]: nested <tau_internal>body",
        ),
    ] {
        let mut noncanonical = provider.clone();
        let tau_proto::Event::AgentMessageReceived(message) = &mut noncanonical else {
            unreachable!("cloned provider event retains its variant");
        };
        message.message = body.to_owned();
        assert_eq!(
            block_text(&renderer.render_agent_message_block(&noncanonical)),
            expected
        );
    }
    let mut wrong_kind = provider.clone();
    let tau_proto::Event::AgentMessageReceived(message) = &mut wrong_kind else {
        unreachable!("cloned provider event retains its variant");
    };
    message.kind = tau_proto::AgentMessageKind::Message;
    assert_eq!(
        block_text(&renderer.render_agent_message_block(&wrong_kind)),
        "■ Message from @worker to @manager:\n<tau_internal>Watched agent worker provider status: retrying (unknown, attempt 1, next retry about 11s)&lt;/tau_internal&gt;"
    );
    let mut missing_status = provider.clone();
    let tau_proto::Event::AgentMessageReceived(message) = &mut missing_status else {
        unreachable!("cloned provider event retains its variant");
    };
    message.watch_provider_status = None;
    assert_eq!(
        block_text(&renderer.render_agent_message_block(&missing_status)),
        "■ Message from @worker to @manager:\n<tau_internal>Watched agent worker provider status: retrying (unknown, attempt 1, next retry about 11s)&lt;/tau_internal&gt;"
    );
    assert_eq!(
        block_text(&renderer.render_agent_message_block(&long_wait)),
        "▤ @worker has been working for 5 minutes"
    );
}

/// Work-status titles visibly escape bidi controls before entering the trusted
/// one-line status frame.
#[test]
fn watch_work_status_visibly_escapes_structural_unicode() {
    let renderer = renderer_for_agent_id_tests();
    let event = tau_proto::Event::AgentMessageReceived(tau_proto::AgentMessageReceived {
        message_id: tau_proto::AgentMessageId::parse("status-bidi")
            .expect("test identifier must satisfy its grammar"),
        sender_id: agent_id("worker"),
        sender_session_id: None,
        recipient_id: agent_id("manager"),
        kind: tau_proto::AgentMessageKind::WatchWorkStatus,
        watch_provider_status: None,
        watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
            session_id: "session-1"
                .parse::<tau_proto::SessionId>()
                .expect("known-safe SessionId must be valid"),
            subscription_id: "subscription-1".to_owned(),
            status_epoch: 1,
            phase: tau_proto::AgentWorkStatusPhase::Blocked,
            title: Some("blocked \u{202e} task".to_owned()),
            initial: false,
        }),
        watch_long_wait: None,
        message: "must not render".to_owned(),
    });

    let text = block_text(&renderer.render_agent_message_block(&event));
    assert!(text.contains(r"blocked \u{202E} task"));
    assert!(!text.contains('\u{202e}'));
    assert!(!text.contains("must not render"));
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

/// Role completion labels retain model controls but omit tool policy fragments,
/// which add noise without changing the candidate or its role configuration.
#[test]
fn role_completion_labels_omit_tool_policy() {
    let details = RoleCompletionDetails::from_description(
        "model=codex-dpcpw/gpt-5.5, effort=xhigh, verbosity=medium, thinking-summary=off, tools=read_only, enable-tools=web_search",
    );

    assert_eq!(
        details.completion_description(false),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off"
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
        details.completion_description(false),
        "codex-dpcpw/gpt-5.5 e=xhigh v=medium ts=off — Investigate deeply, no rush = thorough"
    );
}

/// Structured role metadata remains authoritative for the non-tool fields
/// displayed beside each `:role` completion candidate.
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
        details.completion_description(false),
        "provider/model e=high v=low ts=concise st=fast"
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

    assert_eq!(details.completion_description(false), "no model");
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

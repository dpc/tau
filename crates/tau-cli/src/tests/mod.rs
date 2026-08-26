use std::cell::Cell;
use std::collections::HashSet;
use std::io::BufReader;
use std::os::unix as path_std_os_unix;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{ffi as path_std_ffi, fs as path_std_fs, sync as path_std_sync, time as path_std_time};

use clap::{CommandFactory as _, Parser};
use tau_cli_term::TermHandle;
use tau_cli_term_raw::{Color, Term};
use tau_config::settings as path_tau_config_settings;
use tau_proto::{
    AgentCompacted, AgentCompactionTriggered, AgentManualCompactionRequested, AgentPromptCreated,
    AgentPromptFailed, AgentPromptQueued, AgentPromptRejected, AgentPromptSteered,
    AgentPromptSubmitted, AgentPromptTerminated, AgentPromptTerminationReason,
    AgentStandaloneCompactionFailed, AgentStandaloneCompactionStarted, CborValue, ContentPart,
    ContextItem, ContextRole, Effort, Event, ExtensionReady, HarnessContextUsageChanged,
    HarnessRoleInfo, HarnessRoleSelected, HarnessRolesAvailable, HarnessSessionDir, MessageItem,
    OpaqueProviderItem, ProviderResponseFinished, ProviderResponseUpdated, ProviderStopReason,
    ServiceTier, SessionDirStatus, SessionStartReason, SessionStarted, ThinkingSummary,
    ToolBackgroundResult, ToolCallItem, ToolCancelled, ToolError, ToolResult, UiPromptSubmitted,
    UiRoleUpdateAction, Verbosity,
};

use super::agent_navigation::AgentNavigationState;
use super::chat::cold_attach_stager::ShellStartPresentation;
use super::chat::{
    DraftSlot, UiIoMeter, UiWriter, custom_prompt_replacement,
    custom_prompt_replacement_from_snapshot, debounce_loop_with_wait, invalidate_pending_draft,
    is_known_static_command, leading_command_token, next_agent_cycle_selection,
    queue_prompt_draft_snapshot, redacted_command_echo_line, redacted_prompt_history_line,
    retarget_prompt_draft_snapshot, role_cycling_enabled, send_draft_snapshot_with_before_writer,
    should_send_draft_snapshot, terminal_options_from_settings,
};
use super::cli::{Command as CliCommand, DevCommand};
use super::event_renderer::{EventRenderer, WatchedAgentActivity, watched_agent_tool_display};
use super::tool_render::format_context_token_count;
use super::{CliError, cli as path_super_cli, transcript_markers};

/// Returns the stable inline theme shared by CLI renderer tests.
pub(crate) fn cli_test_theme() -> tau_themes::Theme {
    tau_themes::Theme::parse(
        r##"
        {
            styles: {
                "tool.mode": { fg: "yellow" },
                "watching.name": { fg: "dark_yellow" },
                "tool.status.success": { fg: "green" },
                "tool.status.error": { fg: "red" },
                "tool.status.info": { fg: "dark_cyan" },
                "system.info": { fg: "blue" },
                "system.internal_notice": { fg: "blue", italic: true },
                "system.info.important": { fg: "red" },
                "status.agents": { fg: "cyan" },
                "diff.added": { fg: "dark_green" },
                "diff.removed": { fg: "dark_red" },
                "diff.added.inline": { fg: "green", bold: true },
                "diff.removed.inline": { fg: "red", bold: true },
                "action.label": { fg: "dark_grey" },
                "action.id": { fg: "yellow", bold: true },
                "action.error": { fg: "red" },
                "agent.message.identity": { bold: true },
                "token.stats": { fg: "dark_grey" },
                "token.stats.symbol.delta": { bold: true },
                "token.stats.symbol.sigma": { bold: true },
                "token.stats.metric.cache_warn": { fg: "dark_yellow" },
                "token.stats.metric.cache_miss": { fg: "red" },
                "markdown.strong": { fg: "red", bold: true },
                "markdown.code": { fg: "green" },
            }
        }
        "##,
    )
    .expect("CLI test theme parses")
}

/// Build a renderer with the built-in lifecycle markers rather than the terse
/// legacy test markers used by [`EventRenderer::new`].
fn marker_test_renderer(handle: TermHandle) -> EventRenderer {
    EventRenderer::new_with_state(
        handle,
        tau_cli_term::CompletionData::new(),
        cli_test_theme(),
        path_tau_config_settings::CliState::default(),
        path_tau_config_settings::TauDirs::default(),
        "◯".to_owned(),
        "⬤".to_owned(),
    )
}

fn agent_id(value: &str) -> tau_proto::AgentId {
    tau_proto::AgentId::parse(value).expect("valid test agent id")
}

fn test_session_id(value: impl Into<String>) -> tau_proto::SessionId {
    tau_proto::SessionId::parse(value).expect("test session id")
}

fn test_agent_prompt_id(value: impl Into<String>) -> tau_proto::AgentPromptId {
    tau_proto::AgentPromptId::parse(value).expect("test agent prompt id")
}

/// Returns the adaptive header cells selected for a tool block at `width`.
fn priority_header_cells(
    block: &tau_cli_term::StyledBlock,
    width: usize,
) -> Vec<tau_cli_term::Cell> {
    block
        .priority_line_content()
        .expect("priority header")
        .layout(width)
}

/// Returns the plain adaptive header selected for a tool block at `width`.
fn priority_header_text(block: &tau_cli_term::StyledBlock, width: usize) -> String {
    priority_header_cells(block, width)
        .iter()
        .map(|cell| cell.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

use super::tool_render::{
    CompactionStatus, ToolLineElement, ToolStatus, build_delegate_completion_display,
    format_turn_stats_line, render_action_error_block, render_action_output_block,
    render_compaction_block, render_diff_tool_block, render_harness_notice,
    render_multi_diff_tool_block, render_shell_block, render_tool_block, render_tool_use_state,
    render_turn_stats_block, streaming_block, synthesize_fallback_display,
};

/// Writer that feeds bytes directly into a VT parser and records a screen
/// snapshot at each redraw-thread flush boundary.
#[derive(Clone)]
struct VtWriter {
    /// Parser containing the latest virtual-terminal screen.
    parser: Arc<Mutex<vt100::Parser>>,
    /// Completed flush-delimited frames and their wait notification.
    frames: Arc<(Mutex<Vec<Vec<String>>>, std::sync::Condvar)>,
}

impl VtWriter {
    fn new(parser: vt100::Parser) -> Self {
        Self {
            parser: Arc::new(Mutex::new(parser)),
            frames: Arc::new((Mutex::new(Vec::new()), path_std_sync::Condvar::new())),
        }
    }

    fn screen_text(&self, w: u16) -> Vec<String> {
        self.parser
            .lock()
            .expect("vt")
            .screen()
            .rows(0, w)
            .collect()
    }

    fn screen_contains(&self, w: u16, needle: &str) -> bool {
        self.screen_text(w).iter().any(|r| r.contains(needle))
    }

    fn scrollback_contains(&self, w: u16, rows: usize, needle: &str) -> bool {
        let mut parser = self.parser.lock().expect("vt");
        parser.screen_mut().set_scrollback(rows);
        let contains = parser.screen().rows(0, w).any(|row| row.contains(needle));
        parser.screen_mut().set_scrollback(0);
        contains
    }

    fn cell_style(&self, row: u16, col: u16) -> (vt100::Color, vt100::Color, bool) {
        let parser = self.parser.lock().expect("vt");
        let cell = parser
            .screen()
            .cell(row, col)
            .expect("visible terminal cell");
        (cell.fgcolor(), cell.bgcolor(), cell.bold())
    }

    fn frame_generation(&self) -> usize {
        self.frames.0.lock().expect("frames").len()
    }

    fn wait_for_frame_after(&self, generation: usize) -> Vec<String> {
        self.wait_for_frame_after_until(
            generation,
            Instant::now() + Duration::from_secs(2),
            "next frame",
        )
    }

    fn wait_for_frame_after_until(
        &self,
        generation: usize,
        deadline: Instant,
        context: &str,
    ) -> Vec<String> {
        let (frames, ready) = self.frames.as_ref();
        let mut frames = frames.lock().expect("frames");
        while frames.len() <= generation {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (next, timeout) = ready.wait_timeout(frames, remaining).expect("frames");
            frames = next;
            assert!(
                !timeout.timed_out() || frames.len() > generation,
                "timed out waiting for {context} after frame generation {generation}; captured frames: {frames:?}"
            );
        }
        frames[generation].clone()
    }

    fn wait_for_frame_containing_after(&self, mut generation: usize, needle: &str) -> usize {
        let starting_generation = generation;
        let deadline = Instant::now() + Duration::from_secs(2);
        let context =
            format!("frame containing {needle:?} (starting generation {starting_generation})");
        loop {
            let frame = self.wait_for_frame_after_until(generation, deadline, &context);
            generation += 1;
            if frame.iter().any(|row| row.contains(needle)) {
                return generation;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {context} after generation {}; last frame: {frame:?}",
                generation - 1
            );
        }
    }
}

impl std::io::Write for VtWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Process bytes directly into the parser. The mutex
        // ensures the test thread sees a consistent state.
        self.parser.lock().expect("vt").process(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let parser = self.parser.lock().expect("vt");
        let width = parser.screen().size().1;
        let frame = parser.screen().rows(0, width).collect();
        drop(parser);
        let (frames, ready) = self.frames.as_ref();
        frames.lock().expect("frames").push(frame);
        ready.notify_all();
        Ok(())
    }
}

fn setup(w: u16, h: u16) -> (Term, TermHandle, VtWriter) {
    let vt = VtWriter::new(vt100::Parser::new(h, w, 100));
    let (term, handle, _input) = Term::new_virtual(
        w as usize,
        h as usize,
        "> ",
        Box::new(vt.clone()),
        tau_cli_term::CursorShape::Bar,
    );
    (term, handle, vt)
}

fn sync(handle: &TermHandle) {
    handle.redraw_sync();
}

fn assert_rendered_ansi_foreground(
    vt: &VtWriter,
    width: u16,
    text: &str,
    index: u8,
) -> vt100::Color {
    let rows = vt.screen_text(width);
    let (row, column) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find(text).map(|column| (row as u16, column as u16)))
        .unwrap_or_else(|| panic!("missing submitted prompt {text:?}: {rows:?}"));
    let foreground = vt.cell_style(row, column).0;
    let expected = if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        vt100::Color::Default
    } else {
        vt100::Color::Idx(index)
    };
    assert_eq!(foreground, expected, "submitted prompt {text:?}");
    foreground
}

fn assert_rendered_bright_white(vt: &VtWriter, width: u16, text: &str) {
    let foreground = assert_rendered_ansi_foreground(vt, width, text, 15);
    if foreground == vt100::Color::Idx(15) {
        assert_ne!(
            foreground,
            vt100::Color::Idx(7),
            "submitted prompt {text:?} must not use ordinary white"
        );
    }
}

fn rendered_cell_attributes(
    vt: &VtWriter,
    width: u16,
    text: &str,
) -> (vt100::Color, vt100::Color, bool, bool, bool) {
    let rows = vt.screen_text(width);
    let (row, byte_column) = rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find(text).map(|column| (row as u16, column)))
        .unwrap_or_else(|| panic!("missing submitted prompt text {text:?}: {rows:?}"));
    let column = rows[row as usize][..byte_column].chars().count() as u16;
    let parser = vt.parser.lock().expect("vt");
    let cell = parser
        .screen()
        .cell(row, column)
        .expect("visible terminal cell");
    (
        cell.fgcolor(),
        cell.bgcolor(),
        cell.bold(),
        cell.italic(),
        cell.underline(),
    )
}

fn expected_rendered_color(color: vt100::Color) -> vt100::Color {
    if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        vt100::Color::Default
    } else {
        color
    }
}

fn agent_message(sender_id: &str, recipient: &str, message: &str) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
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

fn external_agent_message(
    sender_id: &str,
    session_id: &str,
    recipient: &str,
    message: &str,
) -> Event {
    Event::AgentMessageSent(tau_proto::AgentMessageSent {
        message_id: tau_proto::AgentMessageId::parse(format!(
            "msg-{sender_id}-{session_id}-{recipient}"
        ))
        .expect("test message id must satisfy the identifier grammar"),
        sender_id: agent_id(sender_id),
        recipient: tau_proto::AgentMessageRecipient::ExternalAgent {
            session_id: test_session_id(session_id),
            agent_id: agent_id(recipient),
        },
        kind: tau_proto::AgentMessageKind::Message,
        message: message.to_owned(),
    })
}

fn visible_lines(vt: &VtWriter, w: u16) -> Vec<String> {
    vt.screen_text(w)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn eventually_screen_contains(vt: &VtWriter, w: u16, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if vt.screen_contains(w, needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn eventually_screen_lacks(vt: &VtWriter, w: u16, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if !vt.screen_contains(w, needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn assistant_message_item(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
        responses_raw_json: None,
    })
}

fn agent_prompt_created(agent_prompt_id: &str, session_id: &str) -> AgentPromptCreated {
    AgentPromptCreated {
        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: test_session_id(session_id),
        system_prompt: String::new(),
        context: tau_proto::PromptContext::default(),
        tools: Vec::new(),
        tools_ref: None,
        model: "test/model".parse().expect("model id"),
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        compaction: None,
        operation: tau_proto::PromptOperation::Inference,
    }
}

fn agent_prompt_started(agent_prompt_id: &str, session_id: &str) -> tau_proto::AgentPromptStarted {
    tau_proto::AgentPromptStarted {
        model_params: Some(tau_proto::ModelParams::default()),
        outer_turn_id: None,

        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        session_id: test_session_id(session_id),
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::Inference,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }
}

fn standalone_compaction_started(
    transaction_id: &str,
    agent_prompt_id: &str,
) -> AgentStandaloneCompactionStarted {
    AgentStandaloneCompactionStarted {
        agent_id: agent_id("main"),
        transaction_id: tau_proto::CompactionTransactionId::parse(transaction_id)
            .expect("known-safe compaction transaction id"),
        compact_prompt_id: test_agent_prompt_id(agent_prompt_id),
        cut: tau_proto::AgentHead::Root,
        resume_through: None,
        model: "test/model".parse().expect("model id"),
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        originator: tau_proto::PromptOriginator::User,
        supersedes: None,
        trigger: tau_proto::StandaloneCompactionTrigger::Manual,
    }
}

fn standalone_compaction_prompt_started(agent_prompt_id: &str) -> tau_proto::AgentPromptStarted {
    tau_proto::AgentPromptStarted {
        operation: tau_proto::PromptOperation::StandaloneCompaction,
        ..agent_prompt_started(agent_prompt_id, "s1")
    }
}

fn self_compaction_requested(request_id: &str, call_id: &str) -> AgentManualCompactionRequested {
    AgentManualCompactionRequested {
        request_id: tau_proto::CompactionRequestId::parse(request_id)
            .expect("known-safe request id"),
        caller_agent_id: agent_id("main"),
        target_agent_id: agent_id("main"),
        initiating_agent_prompt_id: test_agent_prompt_id("ap-main-request"),
        initiating_tool_call_id: call_id.into(),
        initiating_tool_name: tau_proto::ManualCompactionTool::Compact,
        visible_tool_name: tau_proto::ToolName::new("compact"),
        requested_target_head: tau_proto::AgentHead::Root,
        target_generation: 0,
        model: "test/model".parse().expect("model id"),
        resume_inference: true,
    }
}

fn self_compaction_started(
    request_id: &str,
    call_id: &str,
    transaction_id: &str,
    agent_prompt_id: &str,
) -> AgentStandaloneCompactionStarted {
    AgentStandaloneCompactionStarted {
        trigger: tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id: tau_proto::CompactionRequestId::parse(request_id)
                .expect("known-safe request id"),
            caller_agent_id: agent_id("main"),
            initiating_tool_call_id: call_id.into(),
        },
        ..standalone_compaction_started(transaction_id, agent_prompt_id)
    }
}

/// Builds a fresh bound danger snapshot for selected-agent quota wiring tests.
fn danger_quota_event(model: &tau_proto::ModelId) -> Event {
    let now = super::event_renderer::unix_time_millis();
    let remaining = 604_800_u64 / 2;
    Event::HarnessProviderQuotaChanged(tau_proto::HarnessProviderQuotaChanged {
        provider: model.provider.clone(),
        profile_epoch: tau_proto::ProviderQuotaEpoch::parse("epoch-danger").expect("quota epoch"),
        sequence: 1,
        windows: vec![tau_proto::ProviderQuotaWindow {
            key: tau_proto::ProviderQuotaWindowKey {
                limit_id: tau_proto::ProviderQuotaLimitId::parse("codex").expect("quota pool"),
                window_id: tau_proto::ProviderQuotaWindowId::parse("secondary")
                    .expect("quota window"),
            },
            used_basis_points: 9_400,
            usage_observed_at_unix_ms: now,
            window_seconds: 604_800,
            reset_at_unix_seconds: Some(now / 1_000 + remaining),
            remaining_seconds_at_timing_anchor: Some(remaining as i64),
            timing_anchor_observed_at_unix_ms: Some(now),
            server_offset_ms: Some(0),
            server_offset_observed_at_unix_ms: Some(now),
        }],
        route_bindings: vec![tau_proto::ProviderQuotaRouteBinding {
            model: model.clone(),
            limit_ids: vec![tau_proto::ProviderQuotaLimitId::parse("codex").expect("quota pool")],
            observed_at_unix_ms: now,
            provenance: tau_proto::ProviderQuotaBindingProvenance::TurnEvent,
        }],
    })
}

fn provider_response_stats_update(
    agent_prompt_id: &str,
    agent_id: tau_proto::AgentId,
    current_bytes: u64,
    previous_bytes: u64,
    current_elapsed_micros: u64,
    previous_elapsed_micros: u64,
) -> ProviderResponseUpdated {
    ProviderResponseUpdated {
        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id,
        deltas: Vec::new(),
        compaction: None,
        status: None,
        response_stats: Some(tau_proto::ProviderResponseStats {
            current: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: current_bytes,
                elapsed_micros: current_elapsed_micros,
            },
            previous: tau_proto::ProviderResponseStatsSample {
                response_bytes_received: previous_bytes,
                elapsed_micros: previous_elapsed_micros,
            },
            first_semantic_output_elapsed_micros: None,
        }),
        originator: tau_proto::PromptOriginator::User,
    }
}

fn main_provider_response_stats_update(
    agent_prompt_id: &str,
    current_bytes: u64,
    previous_bytes: u64,
) -> ProviderResponseUpdated {
    provider_response_stats_update(
        agent_prompt_id,
        tau_proto::AgentId::parse("main").expect("agent id"),
        current_bytes,
        previous_bytes,
        2_000_000,
        1_000_000,
    )
}

fn render_submitted_prompt_projections(theme: tau_themes::Theme) -> VtWriter {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer =
        EventRenderer::new(handle.clone(), tau_cli_term::CompletionData::new(), theme);
    let ui_prompt = |text: &str| {
        Event::UiPromptSubmitted(UiPromptSubmitted {
            literal: false,
            session_id: test_session_id("s1"),
            text: text.to_owned(),
            agent_id: agent_id("main"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        })
    };

    renderer.handle(&ui_prompt("immediate submitted prompt"));
    renderer.handle(&Event::AgentPromptQueued(AgentPromptQueued {
        text: "promoted queued prompt".to_owned(),
        agent_id: agent_id("main"),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&ui_prompt("promoted queued prompt"));
    renderer.handle(&Event::AgentPromptSteered(AgentPromptSteered {
        self_compaction_terminal: None,
        inference_activation: false,
        submission_source: Default::default(),
        agent_id: agent_id("main"),
        text: "steered submitted prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        ctx_id: None,
    }));
    renderer.handle(&Event::AgentPromptSubmitted(AgentPromptSubmitted {
        inference_activation: false,
        agent_id: agent_id("main"),
        text: "replayed submitted prompt".to_owned(),
        trusted_internal_spans: Vec::new(),
        message_class: tau_proto::PromptMessageClass::User,
        internal_kind: None,
        originator: tau_proto::PromptOriginator::User,
        submission_source: Default::default(),
        display_name: None,
        ctx_id: None,
    }));
    renderer.switch_agent("other".to_owned());
    renderer.switch_agent("main".to_owned());
    sync(&handle);

    vt
}

fn tool_started(call_id: &str, tool_name: &str, arguments: CborValue) -> Event {
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments,
        agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
        originator: tau_proto::PromptOriginator::User,
    })
}

fn initial_tool_progress(call_id: &str, tool_name: &str, args: &str, mode: &str) -> Event {
    Event::ToolProgress(tau_proto::ToolProgress {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(tool_name),
        message: None,
        progress: None,
        display: Some(tau_proto::ToolUseState {
            args: args.to_owned(),
            mode: mode.to_owned(),
            status: tau_proto::ToolUseStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
            ..Default::default()
        }),
    })
}
fn provider_response_delta_update(
    agent_prompt_id: impl Into<tau_proto::AgentPromptId>,
    text: impl Into<String>,
    thinking: Option<String>,
    originator: tau_proto::PromptOriginator,
) -> ProviderResponseUpdated {
    let text = text.into();
    let mut deltas = Vec::new();
    if let Some(thinking) = thinking.filter(|thinking| !thinking.is_empty()) {
        deltas.push(tau_proto::ProviderResponseTextDelta::ReasoningText {
            output_index: 0,
            kind: tau_proto::ReasoningTextKind::Summary,
            text: thinking,
        });
    }
    if !text.is_empty() {
        deltas.push(tau_proto::ProviderResponseTextDelta::Message {
            output_index: 0,
            text,
            phase: None,
        });
    }
    ProviderResponseUpdated {
        agent_prompt_id: agent_prompt_id.into(),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        deltas,
        compaction: None,
        status: None,
        response_stats: None,
        originator,
    }
}

fn finished_response(
    agent_prompt_id: &str,
    output_items: Vec<ContextItem>,
) -> ProviderResponseFinished {
    let stop_reason = if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
    {
        ProviderStopReason::ToolCalls
    } else {
        ProviderStopReason::EndTurn
    };
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        estimated_api_cost_rates: None,
        estimated_api_cost_increment: None,

        agent_prompt_id: test_agent_prompt_id(agent_prompt_id),
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items,
        stop_reason,
        error: None,
        failure_kind: None,
        context_limit_telemetry: None,
        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
        output_length_disposition: tau_proto::OutputLengthDisposition::None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_attempt: Default::default(),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn finished_response_with_usage(
    agent_prompt_id: &str,
    agent_id_value: &str,
    prompt_sent_tokens: u64,
    prompt_cached_tokens: u64,
    response_received_tokens: u64,
    text: &str,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        automatic_compaction_decision: None,
        agent_id: agent_id(agent_id_value),
        usage: Some(tau_proto::ProviderTokenUsage {
            prompt_sent_tokens,
            prompt_cached_tokens,
            prompt_cache_read_ceiling_tokens: None,
            cache: None,
            response_received_tokens,
            stats: tau_proto::TokenUsageStats {
                total: tau_proto::TokenUsageCounts {
                    sent_tokens: prompt_sent_tokens,
                    cached_tokens: prompt_cached_tokens,
                    received_tokens: response_received_tokens,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }),
        ..finished_response(agent_prompt_id, vec![assistant_message_item(text)])
    }
}

/// Applies one complete authoritative navigation snapshot in renderer tests.
fn apply_test_navigation_mode(renderer: &mut EventRenderer, mode: tau_proto::AgentNavigationMode) {
    renderer.handle(&Event::AgentStatsUpdated(tau_proto::AgentStatsUpdated {
        session_id: test_session_id("s1"),
        agent_id: agent_id("worker-1"),
        navigation_mode: mode,
        runtime_state: tau_proto::AgentRuntimeState::Idle,
        turn_activity: tau_proto::AgentTurnActivity::Idle,
        tools: Default::default(),
        context: Default::default(),
        estimated_api_cost: Default::default(),
        creator_subtree_estimated_api_cost: Default::default(),
        work_status: Default::default(),
    }));
}

mod agent_navigation;
mod cli_parsing;
mod event_projection;
mod prompt_input;
mod renderer_update_folding;
mod tool_status_rendering;
mod transcript_rendering;

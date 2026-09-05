//! Drains the event stream from the harness socket and paints it into
//! the terminal UI. Stateful: tracks per-prompt and per-tool-call UI
//! state so streaming updates land in the right block.
//!
//! Provider delta ordering and accumulation follow
//! `SPEC-tau-cli-provider-stream-rendering`.

use std::borrow::Cow;
#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, atomic as path_std_sync_atomic};
use std::time::{Duration, Instant};
use std::{sync as path_std_sync, time as path_std_time};

use tau_cli_term::RendererDeliveryId;
use tau_config::settings as path_tau_config_settings;
use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextRole, Event, MessageItem,
    ProviderResponseCompactionStatus, ProviderResponseTextDelta, UnixMicros,
};

const MAX_SUBMITTED_PROMPT_CORRELATIONS: usize = 64;

use self::prepared_renderer_event::{DeferredRendererEvent, PreparedRendererEvent};
use self::terminal_tool_calls::TerminalToolCalls;
use crate::action_commands::ActionCommandState;
use crate::agent_activity::AgentActivity;
use crate::agent_navigation::AgentNavigation;
use crate::chat::cold_attach_stager::ShellStartPresentation;
use crate::chat::{DraftSlot, invalidate_pending_draft, retarget_prompt_draft_snapshot};
use crate::estimated_cost::AgentCostSnapshot;
use crate::markdown_render::{
    MarkdownStreamCache, MarkdownStreamUpdate, markdown_block_with_osc8,
    markdown_prefixed_block_with_osc8, markdown_prefixed_streaming_block_with_osc8,
    markdown_prompt_block_with_osc8, markdown_streaming_block_with_osc8,
};
use crate::renderer_handle::RendererHandle;
use crate::skill_commands::SkillCommandState;
use crate::tool_render::{
    CompactionStatus, ToolCallDisplay, ToolLineSegment, ToolStatus, ToolSummaryDisplay,
    agent_context_initialized_block, build_delegate_completion_display, build_tool_summary_display,
    config_profile_selection_block, diff_payload_counts, extension_status_block,
    format_context_token_count, format_token_count, pending_tool_call_display,
    render_action_output_block, render_compaction_block, render_diff_tool_block,
    render_harness_notice, render_multi_diff_tool_block, render_shell_block, render_tool_block,
    render_tool_header_block, render_tool_use_state, render_tool_use_state_payload_free,
    render_tool_use_state_without_status, render_turn_stats_projection_block, session_status_block,
    streaming_block, streaming_block_with_indicator_suffix, synthesize_fallback_display,
    tool_duration_suffix, ui_dir_block,
};
use crate::turn_stats_projection::TurnStatsPresentationProjection;
use crate::watch_activity::{VISIBLE_WATCH_EXPANSION_LIMIT, WatchGraphProjection};
use crate::{
    MUTEX_POISONED, message_fact_render as path_crate_message_fact_render,
    provider_quota as path_crate_provider_quota,
};

pub(crate) const UI_IO_MEDIUM_BYTES_PER_SEC: u64 = 10 * 1024;
const UI_IO_HIGH_BYTES_PER_SEC: u64 = 100 * 1024;

const AGENT_START_TOOL_NAME: &str = "agent_start";
const BLOCKER_TOOL_NAME: &str = "task_blocker";
const TIMER_WAKEUP_CTX_PREFIX: &str = "timer:";
static LAST_HANDLER_STALL_WARNING: std::sync::OnceLock<Mutex<Option<Instant>>> =
    path_std_sync::OnceLock::new();

#[cfg(test)]
type SubmittedPromptParserInputObserver = Box<dyn FnMut(&str)>;

#[cfg(test)]
type ToolTerminalDescriptorObserver =
    Box<dyn FnMut(Option<&tau_proto::ToolUseState>, Option<&CborValue>)>;

#[cfg(test)]
thread_local! {
    static SUBMITTED_PROMPT_PARSER_INPUT_OBSERVER: RefCell<Option<SubmittedPromptParserInputObserver>> =
        const { RefCell::new(None) };
    static TOOL_TERMINAL_DESCRIPTOR_OBSERVER: RefCell<Option<ToolTerminalDescriptorObserver>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
fn observe_submitted_prompt_parser_input(body_text: &str) {
    SUBMITTED_PROMPT_PARSER_INPUT_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow_mut().as_mut() {
            observer(body_text);
        }
    });
}

#[cfg(test)]
fn set_submitted_prompt_parser_input_observer_for_test(
    observer: Option<SubmittedPromptParserInputObserver>,
) {
    SUBMITTED_PROMPT_PARSER_INPUT_OBSERVER.with(|slot| *slot.borrow_mut() = observer);
}

#[cfg(test)]
fn observe_tool_terminal_descriptor(
    descriptor: Option<&tau_proto::ToolUseState>,
    details: Option<&CborValue>,
) {
    TOOL_TERMINAL_DESCRIPTOR_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow_mut().as_mut() {
            observer(descriptor, details);
        }
    });
}

#[cfg(test)]
fn set_tool_terminal_descriptor_observer_for_test(
    observer: Option<ToolTerminalDescriptorObserver>,
) {
    TOOL_TERMINAL_DESCRIPTOR_OBSERVER.with(|slot| *slot.borrow_mut() = observer);
}

/// Canonical outcome that owns a terminal tool row's displayed status.
#[derive(Clone, Copy)]
enum TerminalToolOutcome<'a> {
    /// The terminal event reports successful completion.
    SuccessResult,
    /// The terminal event reports failure with this canonical message.
    Error { canonical_message: &'a str },
    /// The terminal event reports cancellation.
    Cancelled,
}

/// Borrowed fields shared by foreground and background tool-error terminals.
struct BorrowedToolError<'a> {
    /// Stable call identity used to finish runtime state.
    call_id: &'a tau_proto::ToolCallId,
    /// Generic tool identity rendered in the terminal row.
    tool_name: &'a tau_proto::ToolName,
    /// Canonical terminal error message.
    message: &'a str,
    /// Optional structured details used by generic delegate fallback rendering.
    details: Option<&'a CborValue>,
    /// Optional producer-supplied generic display descriptor.
    descriptor: Option<&'a tau_proto::ToolUseState>,
    /// Whether this terminal belongs to the user-facing conversation.
    originator_is_user: bool,
}

/// Makes a producer descriptor's status agree with its canonical terminal
/// event.
///
/// The descriptor still owns all non-status presentation metadata. A successful
/// terminal may retain a completed warning, while an error descriptor may
/// retain its nonempty label only when it already described an error.
fn normalize_terminal_tool_use_state(
    mut descriptor: tau_proto::ToolUseState,
    outcome: TerminalToolOutcome<'_>,
) -> tau_proto::ToolUseState {
    match outcome {
        TerminalToolOutcome::SuccessResult => {
            if descriptor.status == tau_proto::ToolUseStatus::Warning {
                if descriptor.status_text.trim().is_empty() {
                    descriptor.status_text = "warn".to_owned();
                }
            } else {
                descriptor.status = tau_proto::ToolUseStatus::Success;
                descriptor.status_text = "ok".to_owned();
            }
        }
        TerminalToolOutcome::Error { canonical_message } => {
            let retain_producer_label = descriptor.status == tau_proto::ToolUseStatus::Error
                && !descriptor.status_text.trim().is_empty();
            descriptor.status = tau_proto::ToolUseStatus::Error;
            if !retain_producer_label {
                descriptor.status_text = {
                    if canonical_message.trim().is_empty() {
                        "err".to_owned()
                    } else {
                        let fallback =
                            synthesize_fallback_display("", Some(canonical_message)).status_text;
                        if fallback.trim().is_empty() {
                            "err".to_owned()
                        } else {
                            fallback
                        }
                    }
                };
            }
        }
        TerminalToolOutcome::Cancelled => {
            descriptor.status = tau_proto::ToolUseStatus::Warning;
            descriptor.status_text = "cancelled".to_owned();
        }
    }
    descriptor
}

/// One safe, finite action accepted by the bundled Swarm blocker tool.
#[derive(Clone, Copy)]
enum BlockerAction {
    Add,
    Cancel,
    List,
}

impl BlockerAction {
    /// Returns the stable compact label for this action.
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Cancel => "cancel",
            Self::List => "list",
        }
    }
}

/// Extracts the sole safe compact descriptor from a built-in blocker
/// invocation.
///
/// Blocker payloads carry titles, descriptions, answers, and cancellation
/// reasons. The action discriminant alone distinguishes the operation without
/// displaying any of that payload.
fn blocker_action_descriptor(started: &tau_proto::ToolStarted) -> Option<BlockerAction> {
    if !is_blocker_tool_name(started.tool_name.as_str()) {
        return None;
    }
    let CborValue::Map(entries) = &started.arguments else {
        return None;
    };
    let mut action = None;
    for (key, value) in entries {
        if !matches!(key, CborValue::Text(key) if key == "action") {
            continue;
        }
        let CborValue::Text(value) = value else {
            return None;
        };
        if action.is_some() {
            return None;
        }
        action = match value.as_str() {
            "add" => Some(BlockerAction::Add),
            "cancel" => Some(BlockerAction::Cancel),
            "list" => Some(BlockerAction::List),
            _ => return None,
        };
    }
    action
}

/// Recognizes the bundled Swarm blocker name with an optional structural
/// extension-instance prefix, but never its removed legacy alias.
fn is_blocker_tool_name(name: &str) -> bool {
    name == BLOCKER_TOOL_NAME
        || name
            .strip_suffix("_task_blocker")
            .is_some_and(|prefix| !prefix.is_empty())
}

/// Returns the effective timeout for a built-in shell invocation.
///
/// Shell providers enforce a 300-second default when the agent omits
/// `timeout`. This narrow presentation projection retains that declared limit
/// so the generic duration chip can show elapsed time against the actual
/// command budget without changing the provider display protocol.
fn effective_shell_timeout(started: &tau_proto::ToolStarted) -> Option<Duration> {
    const DEFAULT_TIMEOUT_SECS: u64 = 300;

    if !matches!(started.tool_name.as_str(), "shell" | "gpt_shell") {
        return None;
    }
    let CborValue::Map(entries) = &started.arguments else {
        return None;
    };

    let mut timeout = None;
    for (key, value) in entries {
        if !matches!(key, CborValue::Text(key) if key == "timeout") {
            continue;
        }
        let CborValue::Integer(value) = value else {
            return None;
        };
        let Ok(value) = u64::try_from(*value) else {
            return None;
        };
        if timeout.replace(value).is_some() {
            return None;
        }
    }
    Some(Duration::from_secs(timeout.unwrap_or(DEFAULT_TIMEOUT_SECS)))
}

/// Projects a blocker display through the action-only presentation boundary.
fn sanitize_blocker_display(
    display: &mut ToolCallDisplay,
    is_blocker: bool,
    action: Option<BlockerAction>,
) {
    if !is_blocker {
        return;
    }
    display.mode.clear();
    display.args = action.map_or_else(String::new, |action| action.as_str().to_owned());
    display.range = None;
    display.suffixes.retain(|suffix| {
        matches!(
            suffix.status,
            ToolStatus::Success
                | ToolStatus::Warning
                | ToolStatus::Error
                | ToolStatus::Pending
                | ToolStatus::Progress
                | ToolStatus::Time
        )
    });
    display.payload = None;
}

fn admit_handler_stall_warning(now: Instant) -> bool {
    let mut last = LAST_HANDLER_STALL_WARNING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect(MUTEX_POISONED);
    if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(5)) {
        return false;
    }
    *last = Some(now);
    true
}

/// Content-free renderer stage timing emitted on every handler exit.
struct HandlerProgress {
    /// Process-local delivery correlation when the socket supplied this event.
    delivery_id: Option<RendererDeliveryId>,
    /// Stable content-free protocol event name.
    event_name: tau_proto::EventName,
    /// Monotonic handler start time.
    started_at: Instant,
}

impl Drop for HandlerProgress {
    fn drop(&mut self) {
        let elapsed = self.started_at.elapsed();
        tracing::trace!(
            target: "tau_cli::frontend_progress",
            delivery_id = self.delivery_id.map(RendererDeliveryId::get),
            event_name = %self.event_name,
            handler_us = elapsed.as_micros(),
            "renderer handler finished"
        );
        if Duration::from_millis(500) <= elapsed && admit_handler_stall_warning(Instant::now()) {
            tracing::warn!(
                target: "tau_cli::frontend_progress",
                delivery_id = self.delivery_id.map(RendererDeliveryId::get),
                event_name = %self.event_name,
                handler_ms = elapsed.as_millis(),
                "renderer handler stalled"
            );
        }
    }
}

/// Selects canonical facts whose visible presentation can be flush-correlated.
fn presentation_fact(event: &Event) -> Option<PresentationFactClass> {
    presentation_fact_name(&event.name())
}

/// CLI-owned canonical selected-presentation fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum PresentationFactClass {
    /// A prompt entered the visible queued state.
    PromptQueued,
    /// A queued prompt reached its visible submitted state.
    PromptSubmitted,
    /// A visible prompt accepted steering content.
    PromptSteered,
    /// A streaming response visibly advanced.
    ResponseUpdated,
    /// A response visibly reached its canonical terminal presentation.
    ResponseFinished,
    /// A prompt visibly ended through cancellation or supersession.
    PromptTerminated,
}

impl PresentationFactClass {
    /// Returns the one invariant event/class label written to operational
    /// traces.
    const fn label(self) -> &'static str {
        match self {
            Self::PromptQueued => "agent.prompt_queued/prompt_queued",
            Self::PromptSubmitted => "agent.prompt_submitted/prompt_submitted",
            Self::PromptSteered => "agent.prompt_steered/prompt_steered",
            Self::ResponseUpdated => "provider.response_updated/response_updated",
            Self::ResponseFinished => "provider.response_finished/response_finished",
            Self::PromptTerminated => "agent.prompt_terminated/prompt_terminated",
        }
    }

    /// Returns this fact's opaque terminal-layer invalidation key.
    fn key(self) -> tau_cli_term::PresentationObservationKey {
        tau_cli_term::PresentationObservationKey::new(self as u8)
            .expect("finite CLI presentation class must fit the raw invalidation mask")
    }

    /// Returns the opaque predecessor-key mask superseded by this fact.
    fn invalidates(self) -> tau_cli_term::PresentationInvalidation {
        let none = tau_cli_term::PresentationInvalidation::none();
        match self {
            Self::PromptSubmitted => none.with(Self::PromptQueued.key()),
            Self::ResponseFinished => none.with(Self::ResponseUpdated.key()),
            Self::PromptTerminated => none
                .with(Self::PromptQueued.key())
                .with(Self::PromptSubmitted.key())
                .with(Self::ResponseUpdated.key()),
            _ => none,
        }
    }

    /// Builds the application-agnostic typed fact accepted by the raw layer.
    pub(super) fn opaque_fact(self) -> tau_cli_term::OpaquePresentationFact {
        tau_cli_term::OpaquePresentationFact::new(self.label(), self.key(), self.invalidates())
    }

    /// Returns whether mutation and registration require atomic capture
    /// suppression.
    const fn invalidates_pending(self) -> bool {
        matches!(
            self,
            Self::PromptSubmitted | Self::ResponseFinished | Self::PromptTerminated
        )
    }
}

/// Maps stable canonical event names to content-free presentation classes.
fn presentation_fact_name(event_name: &tau_proto::EventName) -> Option<PresentationFactClass> {
    use PresentationFactClass as Class;
    match event_name {
        name if name == &tau_proto::EventName::AGENT_PROMPT_QUEUED => Some(Class::PromptQueued),
        name if name == &tau_proto::EventName::AGENT_PROMPT_SUBMITTED => {
            Some(Class::PromptSubmitted)
        }
        name if name == &tau_proto::EventName::AGENT_PROMPT_STEERED => Some(Class::PromptSteered),
        name if name == &tau_proto::EventName::PROVIDER_RESPONSE_UPDATED => {
            Some(Class::ResponseUpdated)
        }
        name if name == &tau_proto::EventName::PROVIDER_RESPONSE_FINISHED => {
            Some(Class::ResponseFinished)
        }
        name if name == &tau_proto::EventName::AGENT_PROMPT_TERMINATED => {
            Some(Class::PromptTerminated)
        }
        _ => None,
    }
}
const COMPLETED_AGENT_RESPONSE_PREFIX: &str = "◆ ";
const STREAMING_AGENT_RESPONSE_PREFIX: &str = "◇ ";
/// Maximum rendered terminal columns for a supplemental agent message name.
const AGENT_MESSAGE_NAME_MAX_COLUMNS: usize = 48;
/// Maximum rendered UTF-8 bytes for a supplemental agent message name.
const AGENT_MESSAGE_NAME_MAX_BYTES: usize = 192;
const QUEUED_PROJECTION_WINDOW_BYTES: usize = 16 * 1024;

fn bounded_queued_line_start(text: &str) -> &str {
    let mut end = text.len().min(QUEUED_PROJECTION_WINDOW_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let window = &text[..end];
    window
        .find(['\n', '\r'])
        .map_or(window, |line_end| &window[..line_end])
}

fn bounded_queued_line_end(text: &str) -> &str {
    let mut start = text.len().saturating_sub(QUEUED_PROJECTION_WINDOW_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let window = &text[start..];
    window
        .rfind(['\n', '\r'])
        .map_or(window, |line_end| &window[line_end + 1..])
}

fn queued_prompt_projection(
    theme: &tau_themes::Theme,
    osc8_links: bool,
    prefix: tau_cli_term::StyledText,
    text: &str,
) -> tau_cli_term::TwoLineElision {
    let styled = |value| {
        markdown_block_with_osc8(
            theme,
            tau_themes::names::USER_PROMPT_QUEUED,
            value,
            osc8_links,
        )
        .content
    };
    let unabridged_text =
        (text.len() <= QUEUED_PROJECTION_WINDOW_BYTES).then(|| format!("{text} (queued)"));
    let unabridged = unabridged_text.as_deref().map(styled);
    tau_cli_term::TwoLineElision {
        prefix,
        first: styled(bounded_queued_line_start(text)),
        last: styled(bounded_queued_line_end(text)),
        first_omissions: vec![styled("   ┄"), styled("┄")],
        last_omissions: vec![styled("┄ "), styled("┄")],
        labels: vec![styled(" (queued)"), styled(" (q)"), styled("q")],
        unabridged,
    }
}

fn timer_wakeup_ctx(ctx_id: Option<&str>) -> Option<(&str, &str)> {
    let rest = ctx_id?.strip_prefix(TIMER_WAKEUP_CTX_PREFIX)?;
    rest.rsplit_once(':')
}

fn timer_wakeup_summary(timer_id: &str, text: Option<&str>) -> String {
    let Some(text) = text else {
        return format!("Timer `{timer_id}` woke this agent");
    };
    let trimmed = text.trim();
    let timer_prefix = format!("Timer `{timer_id}` fired:");
    let message = trimmed
        .strip_prefix(&timer_prefix)
        .map(str::trim)
        .unwrap_or(trimmed);
    if message.is_empty() {
        format!("Timer `{timer_id}` woke this agent")
    } else {
        format!("Timer `{timer_id}` woke this agent: {message}")
    }
}

/// Rolling UI↔harness socket throughput maxima for one terminal UI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct UiIoStats {
    /// Maximum bytes per second sent from this UI to the harness.
    pub(crate) uplink_max_bytes_per_sec: u64,
    /// Maximum bytes per second received by this UI from the harness.
    pub(crate) downlink_max_bytes_per_sec: u64,
}

/// One queued lifecycle entry and its optional visible user marker.
#[derive(Clone, Debug)]
struct QueuedUserBlock {
    /// Terminal output block for a user prompt; internal prompts own no block.
    id: Option<tau_cli_term::BlockId>,
    /// Exact prompt text used when promoting the marker into history.
    text: String,
    /// User/internal class needed to match the broadcast lifecycle FIFO.
    message_class: tau_proto::PromptMessageClass,
}

/// Shared state needed by renderer-owned selection changes to retarget prompt
/// drafts without sending protocol events directly from the renderer thread.
struct DraftRetargeter {
    /// Debounce mailbox owned by the CLI input/draft subsystem.
    handle: Arc<(Mutex<DraftSlot>, Condvar)>,
    /// Authoritative current session id shared with input routing.
    session_id: Arc<Mutex<tau_proto::SessionId>>,
}

/// Selects which parts of an agent UI state become externally visible.
enum AgentUiRestoreMode {
    /// Materialize the output model and publish its editor context.
    Visible,
    /// Restore only renderer bookkeeping around a detached hidden fold.
    DetachedBookkeeping,
}

#[derive(Default)]
struct EditorConversationContext {
    current_response: Option<String>,
    last_response: Option<String>,
}

/// UI transcript snapshot that owns UI output for an in-flight lifecycle.
#[derive(Clone)]
enum UiSnapshotOwner {
    /// The no-agent/global output snapshot owns the output.
    NoAgent,
    /// A concrete agent transcript snapshot owns the output.
    Agent(tau_proto::AgentId),
}

/// Bookkeeping for a rendered extension lifecycle block.
struct ExtensionBlockState {
    /// Terminal block id for the in-flight "starting" line.
    block_id: tau_cli_term::BlockId,
    /// Snapshot that must receive the matching ready/exited update.
    owner: UiSnapshotOwner,
}

enum EventAgentIdResolution {
    Unhandled,
    NoAgent,
    Agent(tau_proto::AgentId),
}

impl EventAgentIdResolution {
    fn from_agent_id(agent_id: Option<tau_proto::AgentId>) -> Self {
        match agent_id {
            Some(agent_id) => Self::Agent(agent_id),
            None => Self::NoAgent,
        }
    }

    fn or_else(self, f: impl FnOnce() -> Self) -> Self {
        match self {
            Self::Unhandled => f(),
            handled => handled,
        }
    }

    fn into_agent_id(
        self,
        current_agent_id: Option<&tau_proto::AgentId>,
    ) -> Option<tau_proto::AgentId> {
        match self {
            Self::Unhandled => current_agent_id.cloned(),
            Self::NoAgent => None,
            Self::Agent(agent_id) => Some(agent_id),
        }
    }
}

/// One completed file-mutation tool block. Held so `:set show-diff` can
/// re-render every diff in the chat history when the global
/// expand toggle flips.
struct DiffBlockEntry {
    block_id: tau_cli_term::BlockId,
    display: ToolCallDisplay,
    diff: tau_proto::ToolUsePayload,
}

#[derive(Clone)]
pub(crate) struct ToolTimerNotifier {
    inner: Arc<(std::sync::Mutex<ToolTimerState>, std::sync::Condvar)>,
}

pub(crate) struct ToolTimerState {
    pub(crate) active_tool_ids: HashSet<tau_proto::ToolCallId>,
    /// Whether quota pacing currently needs minute-boundary repainting.
    pub(crate) quota_active: bool,
    pub(crate) done: bool,
}

impl ToolTimerNotifier {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new((
                path_std_sync::Mutex::new(ToolTimerState {
                    active_tool_ids: HashSet::new(),
                    quota_active: false,
                    done: false,
                }),
                path_std_sync::Condvar::new(),
            )),
        }
    }

    pub(crate) fn inner(&self) -> Arc<(std::sync::Mutex<ToolTimerState>, std::sync::Condvar)> {
        self.inner.clone()
    }

    fn tool_started(&self, call_id: &tau_proto::ToolCallId) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.active_tool_ids.insert(call_id.clone());
            cv.notify_all();
        }
    }

    fn tool_finished(&self, call_id: &tau_proto::ToolCallId) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.active_tool_ids.remove(call_id);
            cv.notify_all();
        }
    }

    fn clear_active(&self) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.active_tool_ids.clear();
            cv.notify_all();
        }
    }

    fn set_quota_active(&self, active: bool) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.quota_active = active;
            cv.notify_all();
        }
    }

    pub(crate) fn stop(&self) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.done = true;
            cv.notify_all();
        }
    }
}

struct ToolBlockEntry {
    block_id: tau_cli_term::BlockId,
    display: ToolCallDisplay,
}

struct MessageBlockEntry {
    block_id: tau_cli_term::BlockId,
    event: Event,
    /// Session whose metadata may supplement this immutable message event.
    session_id: Option<tau_proto::SessionId>,
}

/// One harness notice retained for reversible transcript reprojection.
struct NoticeBlockEntry {
    /// Position-stable terminal block for the accepted notice.
    block_id: tau_cli_term::BlockId,
    /// Notice payload, including its severity, retained without protocol
    /// mutation.
    notice: tau_proto::HarnessNotice,
}

/// One typed lifecycle diagnostic retained for reversible projection.
struct DiagnosticBlockEntry {
    /// Position-stable terminal block.
    block_id: tau_cli_term::BlockId,
    /// Diagnostic verbosity threshold.
    level: tau_proto::NoticeLevel,
    /// Typed data used to render with the current theme.
    projection: DiagnosticProjection,
}

/// Typed lifecycle diagnostic data retained independently of theme.
enum DiagnosticProjection {
    /// Extension lifecycle status.
    ExtensionStatus {
        /// Stable extension name.
        extension_name: tau_proto::ExtensionName,
        /// Closed lifecycle state.
        status: ExtensionLifecycleStatus,
    },
    /// UI state directory announcement.
    UiDir {
        /// Announced directory path.
        path: std::path::PathBuf,
    },
    /// Session directory announcement.
    SessionDir {
        /// Canonical announcement payload.
        event: tau_proto::HarnessSessionDir,
    },
    /// Startup configuration profile selection.
    ConfigProfile {
        /// Display form of the selected profile.
        selection: String,
    },
    /// Per-agent context discovery summary.
    AgentContextInitialized {
        /// Canonical discovery payload.
        event: tau_proto::HarnessAgentContextInitialized,
        /// Skills omitted from the advertised subset.
        unadvertised_count: usize,
    },
    /// Extension per-agent context readiness.
    ExtensionContextReady {
        /// Agent whose context became ready.
        agent_id: tau_proto::AgentId,
    },
}

/// Closed extension lifecycle states rendered as diagnostics.
enum ExtensionLifecycleStatus {
    /// Extension process is starting.
    Starting,
    /// Extension is ready.
    Ready,
    /// Extension exited.
    Exited,
}

impl ExtensionLifecycleStatus {
    /// Returns the stable presentation word for this lifecycle state.
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Exited => "exited",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageRenderMode {
    Hidden,
    Summary,
    Full,
}

/// One finished thinking block. Held so `:set show-thinking` can swap
/// its content between the original reasoning text (visible) and
/// empty content (hidden) without losing the block's position in
/// the transcript.
struct ThinkingBlockEntry {
    block_id: tau_cli_term::BlockId,
    text: String,
}

/// One typed harness-internal prompt block retained for live reprojection.
struct InternalPromptBlockEntry {
    /// Position-stable terminal block for this canonical prompt fact.
    block_id: tau_cli_term::BlockId,
    /// Typed presentation data retained without changing the durable prompt.
    projection: InternalPromptProjection,
}

/// Typed human projection of one model-facing internal prompt.
enum InternalPromptProjection {
    /// A generic prompt whose authenticated source controls its verbose
    /// subfilter.
    SourceAware {
        /// Authenticated source stamped on the canonical prompt fact.
        submission_source: tau_proto::PromptSubmissionSource,
        /// Canonical prompt payload.
        text: String,
    },
    /// A typed context-size advisory.
    ContextSizeAlert {
        /// Canonical advisory text.
        text: String,
    },
    /// A typed timer wakeup.
    TimerWakeup {
        /// Stable timer identifier.
        timer_id: String,
        /// Optional canonical wakeup text.
        text: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RoleCompletionDetails {
    model: Option<String>,
    effort: Option<String>,
    verbosity: Option<String>,
    thinking_summary: Option<String>,
    service_tier: Option<String>,
    tools: Option<String>,
    enable_tool_groups: Option<String>,
    disable_tool_groups: Option<String>,
    enable_tools: Option<String>,
    disable_tools: Option<String>,
    inference_compaction: Option<String>,
    compactions: Option<String>,
    role_description: Option<String>,
}

impl RoleCompletionDetails {
    fn from_role_info(role: &tau_proto::HarnessRoleInfo) -> Self {
        let mut details = role
            .details
            .as_ref()
            .map(Self::from_structured_details)
            .unwrap_or_else(|| Self::from_description(&role.description));
        details.role_description = role.role_description.clone();
        details
    }

    fn from_structured_details(details: &tau_proto::HarnessRoleDetails) -> Self {
        if details.model.is_none() {
            return Self::default();
        }

        Self {
            model: details.model.as_ref().map(ToString::to_string),
            effort: Some(details.params.effort.to_string()),
            verbosity: Some(details.params.verbosity.to_string()),
            thinking_summary: Some(details.params.thinking_summary.to_string()),
            service_tier: details
                .params
                .service_tier
                .map(|tier| tier.as_str().to_owned()),
            tools: details.tools.as_ref().map(|tools| join_names(tools)),
            enable_tool_groups: (!details.enable_tool_groups.is_empty())
                .then(|| join_names(&details.enable_tool_groups)),
            disable_tool_groups: (!details.disable_tool_groups.is_empty())
                .then(|| join_names(&details.disable_tool_groups)),
            enable_tools: (!details.enable_tools.is_empty())
                .then(|| join_names(&details.enable_tools)),
            disable_tools: (!details.disable_tools.is_empty())
                .then(|| join_names(&details.disable_tools)),
            inference_compaction: details.inference_compaction.clone(),
            compactions: (!details.compactions.is_empty()).then(|| details.compactions.join(",")),
            role_description: None,
        }
    }

    fn from_description(description: &str) -> Self {
        let mut details = Self {
            model: None,
            effort: None,
            verbosity: None,
            thinking_summary: None,
            service_tier: None,
            tools: None,
            enable_tool_groups: None,
            disable_tool_groups: None,
            enable_tools: None,
            disable_tools: None,
            inference_compaction: None,
            compactions: None,
            role_description: None,
        };

        if description == "no model" {
            return details;
        }

        for part in description.split(',').map(str::trim) {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "model" => details.model = Some(value.to_owned()),
                "effort" => details.effort = Some(value.to_owned()),
                "verbosity" => details.verbosity = Some(value.to_owned()),
                "thinking-summary" => details.thinking_summary = Some(value.to_owned()),
                "service-tier" => details.service_tier = Some(value.to_owned()),
                "tools" => details.tools = Some(value.to_owned()),
                "enable-tool-groups" => details.enable_tool_groups = Some(value.to_owned()),
                "disable-tool-groups" => details.disable_tool_groups = Some(value.to_owned()),
                "enable-tools" => details.enable_tools = Some(value.to_owned()),
                "disable-tools" => details.disable_tools = Some(value.to_owned()),
                "inference-compaction" => {
                    details.inference_compaction = Some(value.to_owned());
                }
                "compactions" => details.compactions = Some(value.to_owned()),
                _ => {}
            }
        }

        details
    }

    fn completion_description(&self, include_tool_details: bool) -> String {
        let mut parts = Vec::new();
        if let Some(model) = self.model.as_deref() {
            parts.push(model.to_owned());
        }
        if let Some(effort) = self.effort.as_deref() {
            parts.push(format!("e={effort}"));
        }
        // Terminal completion rows have a strict horizontal budget; do not add
        // unrelated role metadata before this branch's compact `:role` fields.
        if include_tool_details {
            if let Some(verbosity) = self.verbosity.as_deref() {
                parts.push(format!("v={verbosity}"));
            }
            if let Some(thinking_summary) = self.thinking_summary.as_deref() {
                parts.push(format!("ts={thinking_summary}"));
            }
            if let Some(service_tier) = self.service_tier.as_deref() {
                parts.push(format!("st={service_tier}"));
            }
            if let Some(inference_compaction) = self.inference_compaction.as_deref() {
                parts.push(format!("inference-compaction={inference_compaction}"));
            }
            if let Some(compactions) = self.compactions.as_deref() {
                parts.push(format!("compactions={compactions}"));
            }
            if let Some(tools) = self.tools.as_deref() {
                parts.push(format!("tools={tools}"));
            }
            if let Some(enable_tool_groups) = self.enable_tool_groups.as_deref() {
                parts.push(format!("etg={enable_tool_groups}"));
            }
            if let Some(disable_tool_groups) = self.disable_tool_groups.as_deref() {
                parts.push(format!("dtg={disable_tool_groups}"));
            }
            if let Some(enable_tools) = self.enable_tools.as_deref() {
                parts.push(format!("et={enable_tools}"));
            }
            if let Some(disable_tools) = self.disable_tools.as_deref() {
                parts.push(format!("dt={disable_tools}"));
            }
        }
        let mut summary = if parts.is_empty() {
            "no model".to_owned()
        } else {
            parts.join(" ")
        };
        if let Some(description) = self.role_description.as_deref() {
            let description = description.trim();
            if !description.is_empty() {
                summary.push_str(" — ");
                summary.push_str(description);
            }
        }
        summary
    }

    fn current_description(&self, field: &str) -> String {
        match field {
            "model" => self.model.as_deref().unwrap_or("unset").to_owned(),
            "effort" => self.effort.as_deref().unwrap_or("unset").to_owned(),
            "verbosity" => self.verbosity.as_deref().unwrap_or("unset").to_owned(),
            "thinking-summary" => self
                .thinking_summary
                .as_deref()
                .unwrap_or("unset")
                .to_owned(),
            "service-tier" => self.service_tier.as_deref().unwrap_or("unset").to_owned(),
            "tools" => self.tools.as_deref().unwrap_or("unset").to_owned(),
            "enable-tool-groups" => self
                .enable_tool_groups
                .as_deref()
                .unwrap_or("unset")
                .to_owned(),
            "disable-tool-groups" => self
                .disable_tool_groups
                .as_deref()
                .unwrap_or("unset")
                .to_owned(),
            "enable-tools" => self.enable_tools.as_deref().unwrap_or("unset").to_owned(),
            "disable-tools" => self.disable_tools.as_deref().unwrap_or("unset").to_owned(),
            "inference-compaction" => self
                .inference_compaction
                .as_deref()
                .unwrap_or("unset")
                .to_owned(),
            "compactions" => self.compactions.as_deref().unwrap_or("unset").to_owned(),
            _ => "unset".to_owned(),
        }
    }
}

fn role_value_completion(setting: &str, value: &str) -> tau_cli_term::CompletionItem {
    let description = match (setting, value) {
        (_, "reset") => "clear this role setting",
        ("effort", "provider_default") => "omit effort and use the provider default",
        ("effort", "disabled") => "request disabled reasoning",
        ("effort", "0.0") => "minimum portable reasoning intensity",
        ("effort", "0.25") => "light portable reasoning intensity",
        ("effort", "0.5") => "medium-like portable reasoning intensity",
        ("effort", "0.75") => "strong portable reasoning intensity",
        ("effort", "1.0") => "maximum portable reasoning intensity",
        ("effort", "increase:0.25") => "increase portable intensity by 0.25",
        ("effort", "decrease:0.25") => "decrease portable intensity by 0.25",
        ("verbosity", "low") => "terse responses",
        ("verbosity", "medium") => "normal responses",
        ("verbosity", "high") => "detailed responses",
        ("thinking-summary", "off") => "hide thinking summaries",
        ("thinking-summary", "auto") => "provider default summaries",
        ("thinking-summary", "concise") => "short thinking summaries",
        ("thinking-summary", "detailed") => "detailed thinking summaries",
        ("service-tier", "fast") => "use fast service tier",
        ("service-tier", "flex") => "use flex service tier",
        _ => "",
    };
    tau_cli_term::CompletionItem::new(value, description)
}

fn join_names<T: ToString>(names: &[T]) -> String {
    names
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("|")
}
fn role_completion_matches(value: &str, needle: &str) -> bool {
    needle.is_empty() || value.starts_with(needle) || value.contains(needle)
}

fn empty_role_completion_details() -> RoleCompletionDetails {
    RoleCompletionDetails {
        model: None,
        effort: None,
        verbosity: None,
        thinking_summary: None,
        service_tier: None,
        tools: None,
        enable_tool_groups: None,
        disable_tool_groups: None,
        enable_tools: None,
        disable_tools: None,
        inference_compaction: None,
        compactions: None,
        role_description: None,
    }
}

fn role_setting_completions(
    details: &RoleCompletionDetails,
    needle: &str,
) -> Vec<tau_cli_term::CompletionItem> {
    [
        ("delete", "delete this runtime role/override".to_owned()),
        ("model", details.current_description("model")),
        ("effort", details.current_description("effort")),
        ("verbosity", details.current_description("verbosity")),
        (
            "thinking-summary",
            details.current_description("thinking-summary"),
        ),
        ("service-tier", details.current_description("service-tier")),
        ("tools", details.current_description("tools")),
        (
            "enable-tool-groups",
            details.current_description("enable-tool-groups"),
        ),
        (
            "disable-tool-groups",
            details.current_description("disable-tool-groups"),
        ),
        ("enable-tools", details.current_description("enable-tools")),
        (
            "disable-tools",
            details.current_description("disable-tools"),
        ),
    ]
    .into_iter()
    .filter(|(value, _)| role_completion_matches(value, needle))
    .map(|(value, desc)| tau_cli_term::CompletionItem::new(value, desc))
    .collect()
}

fn role_setting_value_completions(
    setting: &str,
    needle: &str,
) -> Vec<tau_cli_term::CompletionItem> {
    let values: &[&str] = match setting {
        "model"
        | "tools"
        | "enable-tool-groups"
        | "disable-tool-groups"
        | "enable-tools"
        | "disable-tools" => &["reset"],
        "effort" => &[
            "reset",
            "provider_default",
            "disabled",
            "0.0",
            "0.25",
            "0.5",
            "0.75",
            "1.0",
            "increase:0.25",
            "decrease:0.25",
        ],
        "verbosity" => &["reset", "low", "medium", "high"],
        "thinking-summary" => &["reset", "off", "auto", "concise", "detailed"],
        "service-tier" => &["reset", "fast", "flex"],
        _ => &[],
    };
    values
        .iter()
        .copied()
        .filter(|value| role_completion_matches(value, needle))
        .map(|value| role_value_completion(setting, value))
        .collect()
}

fn role_command_completions(
    role_items: &[(tau_cli_term::CompletionItem, RoleCompletionDetails)],
    args: &[&str],
) -> Vec<tau_cli_term::CompletionItem> {
    match args.len() {
        1 => role_items
            .iter()
            .filter(|(item, _)| role_completion_matches(&item.value, args[0]))
            .map(|(item, _)| item.clone())
            .collect(),
        2 => {
            let details = role_items
                .iter()
                .find(|(item, _)| item.value == args[0])
                .map(|(_, details)| details.clone())
                .unwrap_or_else(empty_role_completion_details);
            role_setting_completions(&details, args[1])
        }
        3 => role_setting_value_completions(args[1], args[2]),
        _ => Vec::new(),
    }
}

struct TurnStatsBlockEntry {
    block_id: tau_cli_term::BlockId,
    /// Allocation-free scalar state needed to re-render this block.
    projection: TurnStatsPresentationProjection,
}

/// Per-prompt UI state held by [`EventRenderer`]. Lives from the first
/// event observed for the prompt (`AgentPromptStarted`,
/// fallback `AgentPromptCreated`, or
/// `ProviderPromptSubmitted`) through `ProviderResponseFinished` or
/// `AgentPromptTerminated`; standalone compaction outcomes also retire their
/// private prompt state.
#[derive(Default)]
struct PromptState {
    /// Whether this is a standalone-compaction provider prompt whose semantic
    /// output remains private to the harness.
    is_standalone_compaction: bool,
    /// Live agent-response block. `None` until the first provider update
    /// allocates a response/progress block.
    response_block_id: Option<tau_cli_term::BlockId>,
    /// Live thinking block. Lazy-created the first time the agent emits
    /// non-empty `thinking`, so backends that don't return reasoning
    /// summaries produce no extra block.
    thinking_block_id: Option<tau_cli_term::BlockId>,
    /// Latest captured thinking text. Held so `ProviderResponseFinished`
    /// can render it into history even when the finish event doesn't
    /// carry displayable reasoning text.
    thinking_text: Option<String>,
    /// Accumulated live assistant text by provider output index.
    response_text_by_index: BTreeMap<u32, String>,
    /// Accumulated live reasoning text by provider output index.
    thinking_text_by_index: BTreeMap<u32, String>,
    /// Whether this live response started after missed earlier deltas.
    missing_response_prefix: bool,
    /// Whether this live thinking block started after missed earlier deltas.
    missing_thinking_prefix: bool,
    /// Append-aware Markdown-lite cache for the live assistant response block.
    response_markdown_cache: MarkdownStreamCache,
    /// Append-aware Markdown-lite cache for the live thinking block.
    thinking_markdown_cache: MarkdownStreamCache,
    /// Latest provider-owned response stats received directly on
    /// `provider.response_updated` for repainting the live indicator.
    provider_response_stats: Option<tau_proto::ProviderResponseStats>,
    /// Whether the live response block is the pending indicator rather than
    /// response or status text.
    live_response_is_pending_indicator: bool,
    /// Live provider-side compaction block. Created only while a provider emits
    /// an in-progress compaction item, then removed on completion/cancel.
    compaction_block_id: Option<tau_cli_term::BlockId>,
    /// Dispatch timestamp, used to compute end-to-end latency on
    /// `ProviderResponseFinished`.
    started_at: Option<Instant>,
}

/// Joins output-index buckets in ascending index order without cloning the
/// bucket strings. A missing-prefix marker is part of the checked allocation.
fn join_indexed_text(text_by_index: &BTreeMap<u32, String>, missing_prefix: bool) -> String {
    join_indexed_text_observed(text_by_index, missing_prefix, |_| {})
}

/// One exact unit of work performed while joining indexed streaming text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IndexedTextJoinStep {
    /// One bucket length included in checked capacity accounting.
    CapacityBucket,
    /// The single joined-string allocation request, with its exact capacity.
    Allocation(usize),
    /// One borrowed bucket copied into the joined string, with its byte length.
    CopyBucket(usize),
}

/// Joins indexed text while exposing deterministic allocation and copy work to
/// focused tests. A no-op observer compiles away on the production call path.
fn join_indexed_text_observed(
    text_by_index: &BTreeMap<u32, String>,
    missing_prefix: bool,
    mut observe: impl FnMut(IndexedTextJoinStep),
) -> String {
    let content_len = checked_streamed_text_capacity(
        text_by_index.values().map(|text| {
            observe(IndexedTextJoinStep::CapacityBucket);
            text.len()
        }),
        false,
    )
    .expect("streamed response length overflow");
    let prefix = missing_prefix && content_len != 0;
    let capacity = checked_streamed_text_capacity([content_len], prefix)
        .expect("streamed response length overflow");
    observe(IndexedTextJoinStep::Allocation(capacity));
    let mut joined = String::with_capacity(capacity);
    if prefix {
        joined.push('…');
    }
    for text in text_by_index.values() {
        observe(IndexedTextJoinStep::CopyBucket(text.len()));
        joined.push_str(text);
    }
    joined
}

/// Computes the exact joined byte capacity and reports arithmetic overflow
/// before asking the allocator for storage.
fn checked_streamed_text_capacity(
    lengths: impl IntoIterator<Item = usize>,
    missing_prefix: bool,
) -> Option<usize> {
    lengths
        .into_iter()
        .chain(missing_prefix.then_some('…'.len_utf8()))
        .try_fold(0usize, usize::checked_add)
}

/// Per-tool-call UI state held by [`EventRenderer`]. Created when the
/// harness publishes `ToolStarted` (or when a sub-agent's finish marks the call
/// as suppressed) and torn down on `ToolResult`/`ToolError`.
#[derive(Default)]
struct ToolCallState {
    /// Live tool-call block in the active-tools area. `None` for sub-agent
    /// tool calls whose UI is suppressed while generic watched-agent status
    /// rows summarize their owner agent's activity.
    block_id: Option<tau_cli_term::BlockId>,
    /// Empty history placeholder allocated at the tool call's logical
    /// transcript position. Final results fill this block so live progress
    /// can update the bottom active-tools area without mutating old
    /// transcript rows.
    history_block_id: Option<tau_cli_term::BlockId>,
    /// Latest live display for the block, used when `:set show-tools`
    /// flips while the call is still running.
    live_display: Option<ToolCallDisplay>,
    /// Safe action descriptor extracted from a `blocker` start. Terminal
    /// reports do not repeat invocation arguments, so this survives until the
    /// final display replaces the live row.
    blocker_action: Option<BlockerAction>,
    /// Whether this call uses the built-in blocker action-only projection.
    is_blocker: bool,
    /// Monotonic start time for live duration updates.
    started_at: Option<Instant>,
    /// Harness log timestamp for final duration chips.
    recorded_started_at: Option<UnixMicros>,
    /// Effective shell timeout retained from the tool start for duration
    /// presentation. `None` leaves non-shell tool duration chips unchanged.
    effective_shell_timeout: Option<Duration>,
    /// Summary block for the assistant tool batch this call belongs
    /// to. `None` for stray events without a preceding tool-call
    /// announcement.
    summary_block_id: Option<tau_cli_term::BlockId>,
    /// `true` for the user-facing parent `agent_start` tool call that
    /// spawned a side conversation. While it is live, side-conversation
    /// prompt lifecycle events must not hide the main tool usage chip.
    is_main_delegate: bool,
    /// `true` for tool calls in side conversations. Their lifecycle
    /// events (`ToolResult`, `ToolError`, `ToolProgress`) share the bus
    /// with the main agent's, but the UI filters them out.
    is_sub_agent: bool,
}

/// Presentation correlation retained for one self-compaction tool call.
#[derive(Clone)]
struct SelfCompactionTool {
    /// Durable accepted request that owns this exact call.
    request_id: tau_proto::CompactionRequestId,
    /// Private provider prompt once the request has started a transaction.
    compact_prompt_id: Option<tau_proto::AgentPromptId>,
    /// Durable transaction that claimed the accepted request.
    transaction_id: Option<tau_proto::CompactionTransactionId>,
    /// Latest lifecycle status to apply if the tool row arrives late on attach.
    status: Option<(CompactionStatus, String)>,
}

/// Stable transcript row retained while the first post-compaction request size
/// is still unknown.
#[derive(Clone)]
struct CompletedCompactionPresentation {
    /// History block repainted when exact continuation usage arrives.
    block_id: Option<tau_cli_term::BlockId>,
    /// Compact request input usage shown on the left side of the reduction.
    original_input_tokens: Option<tau_proto::TokenCount>,
    /// Self-compaction tool row, or `None` for an independent compaction row.
    self_tool_call_id: Option<tau_proto::ToolCallId>,
}

/// In-flight state for a user `!`/`!!` shell block.
struct ShellBlockState {
    block_id: tau_cli_term::BlockId,
    command: String,
    include_in_context: bool,
    /// Output accumulated from `ShellCommandProgress` chunks. Rendered
    /// under the header each redraw.
    output: String,
}

/// Independently hideable bottom-status elements in priority order.
///
/// `ARCH-tau-cli` defines the ten-point bands. Active side-agent activity
/// shares the tool-activity band, while model adjustments share the
/// agent-description band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusElement {
    /// Agent, role, model, or no-selection identity.
    Identity,
    /// Selected model's context usage and capacity.
    Context,
    /// Main-agent tool progress.
    Tools,
    /// Count of active side agents.
    ActiveAgents,
    /// Selected agent's human-readable description.
    Description,
    /// Selected agent's current self-reported task title.
    WorkTitle,
    /// Effective effort, verbosity, or service-tier adjustment.
    ModelAdjustment,
    /// Agents watching the selected agent.
    Watchers,
    /// Weekly provider quota pacing.
    WeeklyQuota,
    /// Runtime estimated equivalent API cost.
    EstimatedCost,
    /// Optional UI-to-harness throughput diagnostics.
    UiIoDebug,
    /// Optional full-redraw counter.
    RedrawDebug,
}

impl StatusElement {
    /// Returns the priority where zero is most important.
    const fn priority(self) -> tau_cli_term::PriorityLinePriority {
        let value = match self {
            Self::Identity => 0,
            Self::Context => 10,
            Self::Tools | Self::ActiveAgents => 20,
            Self::Description | Self::WorkTitle | Self::ModelAdjustment => 30,
            Self::Watchers => 40,
            Self::WeeklyQuota | Self::EstimatedCost => 50,
            Self::UiIoDebug => 60,
            Self::RedrawDebug => 70,
        };
        tau_cli_term::PriorityLinePriority::new(value)
    }
}

fn status_chip(
    theme: &tau_themes::Theme,
    style: &str,
    text: impl Into<String>,
) -> tau_cli_term::StyledText {
    tau_cli_term::Span::new(text, tau_cli_term::resolve::resolve(theme, style)).into()
}

pub(crate) fn unix_time_millis() -> u64 {
    path_std_time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn ui_io_status_style(stats: UiIoStats) -> &'static str {
    use tau_themes::names;

    let max_bytes_per_sec = stats
        .uplink_max_bytes_per_sec
        .max(stats.downlink_max_bytes_per_sec);
    if max_bytes_per_sec < UI_IO_MEDIUM_BYTES_PER_SEC {
        names::STATUS_UI_IO_LOW
    } else if max_bytes_per_sec < UI_IO_HIGH_BYTES_PER_SEC {
        names::STATUS_UI_IO_MEDIUM
    } else {
        names::STATUS_UI_IO_HIGH
    }
}

fn format_ui_io_rate(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 {
        return "0".to_owned();
    }
    if bytes_per_sec < 1024 {
        return format!("{bytes_per_sec}B");
    }
    if bytes_per_sec < 1024 * 1024 {
        return format_ui_io_scaled_rate(bytes_per_sec, 1024, "K");
    }
    format_ui_io_scaled_rate(bytes_per_sec, 1024 * 1024, "M")
}

fn format_ui_io_scaled_rate(bytes_per_sec: u64, divisor: u64, suffix: &str) -> String {
    let whole = bytes_per_sec / divisor;
    let tenth = bytes_per_sec % divisor * 10 / divisor;
    if whole < 10 && tenth != 0 {
        format!("{whole}.{tenth}{suffix}")
    } else {
        format!("{whole}{suffix}")
    }
}

fn response_stats_indicator_suffix(stats: &tau_proto::ProviderResponseStats) -> String {
    let current = stats.current;
    let previous = stats.previous;
    // This widget is intentionally stateless with respect to rates. Do not use
    // `Instant::now()` here. The provider owns sampling cadence; the CLI only
    // renders the latest current/previous sample it received.
    let total_bytes = current.response_bytes_received;
    let elapsed_seconds = current.elapsed_micros / 1_000_000;
    let bytes = format_progress_bytes(total_bytes);
    let delta_micros = current
        .elapsed_micros
        .saturating_sub(previous.elapsed_micros);
    let delta_bytes = current
        .response_bytes_received
        .saturating_sub(previous.response_bytes_received);
    let delta_bytes_per_sec = delta_bytes.saturating_mul(1_000_000) / delta_micros.max(1);
    let total_bytes_per_sec = total_bytes.saturating_mul(1_000_000) / current.elapsed_micros.max(1);
    let delta_rate = format!("{}/s", format_progress_bytes(delta_bytes_per_sec));
    let total_rate = format!("{}/s", format_progress_bytes(total_bytes_per_sec));
    let first_output = stats
        .first_semantic_output_elapsed_micros
        .map(path_std_time::Duration::from_micros)
        .map_or_else(String::new, |duration| {
            format!("{}, ", format_compact_duration(duration))
        });
    format!(" ({first_output}{elapsed_seconds}s, {bytes}, Δ{delta_rate}, {total_rate})")
}

fn format_compact_duration(duration: std::time::Duration) -> String {
    if duration < path_std_time::Duration::from_secs(5) {
        return format!("{}ms", duration.as_millis());
    }
    if duration < path_std_time::Duration::from_secs(5 * 60) {
        return format!("{}s", duration.as_secs());
    }
    format!("{}m", duration.as_secs() / 60)
}

fn response_stats_indicator_for_prompt(state: &PromptState, verbose_mode: bool) -> String {
    verbose_mode
        .then(|| {
            state
                .provider_response_stats
                .as_ref()
                .map(response_stats_indicator_suffix)
        })
        .flatten()
        .unwrap_or_default()
}

fn provider_response_update_has_visible_content(
    update: &tau_proto::ProviderResponseUpdated,
) -> bool {
    !update.deltas.is_empty() || update.compaction.is_some() || update.status.is_some()
}

fn format_progress_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    if bytes < 1024 * 1024 {
        return format!("{}KB", bytes / 1024);
    }
    format!("{}MB", bytes / 1024 / 1024)
}

fn update_compaction_status(
    update: &tau_proto::ProviderResponseUpdated,
) -> Option<(CompactionStatus, String)> {
    let compaction = update.compaction.as_ref()?;
    match compaction.status {
        ProviderResponseCompactionStatus::Started => Some((
            CompactionStatus::Progress,
            EventRenderer::compaction_progress_status(compaction.original_input_tokens),
        )),
        ProviderResponseCompactionStatus::Completed => Some((
            CompactionStatus::Success,
            EventRenderer::compaction_success_status(compaction.original_input_tokens),
        )),
    }
}

fn reasoning_text_from_output_items<'a>(
    output_items: &'a [ContextItem],
    #[cfg(test)] concat_allocations: &mut u64,
) -> Option<Cow<'a, str>> {
    text_projection(
        output_items.iter().filter_map(|item| match item {
            ContextItem::ReasoningText(reasoning) => Some(reasoning.text.as_str()),
            _ => None,
        }),
        #[cfg(test)]
        concat_allocations,
    )
}

fn assistant_text_from_output_items<'a>(
    output_items: &'a [ContextItem],
    #[cfg(test)] concat_allocations: &mut u64,
) -> Option<Cow<'a, str>> {
    text_projection(
        output_items
            .iter()
            .flat_map(|item| match item {
                ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content,
                    ..
                }) => content.as_slice(),
                _ => &[],
            })
            .map(content_part_text),
        #[cfg(test)]
        concat_allocations,
    )
}

/// Borrows one non-empty semantic text part and concatenates only when a second
/// non-empty part exists.
fn text_projection<'a>(
    parts: impl Iterator<Item = &'a str> + Clone,
    #[cfg(test)] concat_allocations: &mut u64,
) -> Option<Cow<'a, str>> {
    let mut non_empty = parts.filter(|part| !part.is_empty());
    let first = non_empty.next()?;
    let Some(second) = non_empty.next() else {
        return Some(Cow::Borrowed(first));
    };
    let capacity = non_empty
        .clone()
        .fold(first.len().saturating_add(second.len()), |total, part| {
            total.saturating_add(part.len())
        });
    let mut text = String::with_capacity(capacity);
    #[cfg(test)]
    {
        *concat_allocations += 1;
    }
    text.push_str(first);
    text.push_str(second);
    for part in non_empty {
        text.push_str(part);
    }
    Some(Cow::Owned(text))
}

fn content_part_text(part: &ContentPart) -> &str {
    match part {
        ContentPart::Text { text }
        | ContentPart::SyntheticCompactionSummary { text }
        | ContentPart::HarnessInternalText { text } => text,
        ContentPart::UrlCitation { .. } | ContentPart::CitationMetadataInvalid => "",
    }
}

fn assistant_text_from_message_item<'a>(
    message: &'a MessageItem,
    #[cfg(test)] concat_allocations: &mut u64,
) -> Option<Cow<'a, str>> {
    if message.role != ContextRole::Assistant {
        return None;
    }
    text_projection(
        message.content.iter().map(content_part_text),
        #[cfg(test)]
        concat_allocations,
    )
}

/// Semantic state of one visible projected watched-agent row.
pub(crate) enum WatchedAgentActivity<'a> {
    /// The selected predecessor edge is idle with no running descendant.
    Idle,
    /// The selected predecessor edge reports a running outer turn.
    Running,
    /// The selected edge is idle but its target watches an active descendant.
    Watching {
        /// Nearest directly running descendant, identified by stable id.
        witness: &'a str,
    },
}

/// Builds the generic tool-block-shaped display for a watched-agent status row.
///
/// This intentionally reuses [`tau_proto::ToolUseState`] counter formatting so
/// rows keep the compact generic layout, stable `@agent_id` identity, and
/// existing telemetry counters. The self-reported work state has result-status
/// priority, while an optional display name and task title yield first under
/// width pressure.
pub(crate) fn watched_agent_tool_display(
    display_name: Option<&str>,
    agent_id: &str,
    via: Option<&str>,
    stats: Option<&tau_proto::AgentStatsUpdated>,
    activity: WatchedAgentActivity<'_>,
    work_status: Option<&tau_proto::AgentWatchWorkStatusNotification>,
) -> ToolCallDisplay {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolUseState, ToolUseStatus};

    let mut progress_counters = Vec::new();
    if let Some(stats) = stats {
        progress_counters.push(ProgressCounter {
            label: Some("tools".to_owned()),
            unit: ProgressUnit::Count,
            complete: Some(u64::from(
                stats
                    .tools
                    .started_total
                    .saturating_sub(stats.tools.in_flight),
            )),
            total: Some(u64::from(stats.tools.started_total)),
        });

        if stats.context.input_tokens.is_some() || stats.context.context_window.is_some() {
            progress_counters.push(ProgressCounter {
                label: Some("ctx".to_owned()),
                unit: ProgressUnit::Tokens,
                complete: stats.context.input_tokens,
                total: stats.context.context_window,
            });
        } else if let Some(percent) = stats.context.percent_used {
            progress_counters.push(ProgressCounter {
                label: Some("ctx".to_owned()),
                unit: ProgressUnit::Percent,
                complete: Some(u64::from(percent)),
                total: None,
            });
        }
    }

    let display = ToolUseState {
        args: String::new(),
        progress_counters,
        status: ToolUseStatus::Success,
        status_text: String::new(),
        ..Default::default()
    };
    let primary_agent_id = via.unwrap_or(agent_id);
    let mut rendered =
        render_tool_use_state_without_status(&format!("@{primary_agent_id}"), &display);
    rendered.tool_name_style = Some(tau_themes::names::WATCHING_NAME);
    if via.is_some() {
        rendered.leading_segments.push(ToolLineSegment {
            text: format!("-> @{agent_id}"),
            status: ToolStatus::AgentContext,
            no_leading_space: false,
        });
    }
    if let Some(display_name) = display_name.map(str::trim).filter(|name| !name.is_empty()) {
        rendered.leading_segments.push(ToolLineSegment {
            text: format!("({})", tau_proto::visible_escape_metadata(display_name)),
            status: ToolStatus::Info,
            no_leading_space: false,
        });
    }
    let fallback_activity = match activity {
        WatchedAgentActivity::Running => tau_proto::AgentTurnActivity::Responding,
        WatchedAgentActivity::Idle | WatchedAgentActivity::Watching { .. } => {
            tau_proto::AgentTurnActivity::Idle
        }
    };
    let turn_activity = stats.map_or(fallback_activity, |stats| stats.turn_activity);
    rendered.status_prefix = Some((
        format!(
            "{}{}",
            watched_agent_work_status_symbol(work_status),
            crate::list_agents::turn_activity_symbol(turn_activity),
        ),
        ToolStatus::Progress,
    ));
    if let Some(title) = work_status.and_then(|status| status.title.as_deref()) {
        rendered.leading_segments.push(ToolLineSegment {
            text: tau_proto::visible_escape_metadata(title),
            status: ToolStatus::WorkTitle,
            no_leading_space: false,
        });
    }
    match activity {
        WatchedAgentActivity::Idle => {}
        WatchedAgentActivity::Running => {}
        WatchedAgentActivity::Watching { witness } => {
            rendered.leading_segments.push(ToolLineSegment {
                text: "watching".to_owned(),
                status: ToolStatus::Info,
                no_leading_space: false,
            });
            rendered.suffixes.insert(
                0,
                ToolLineSegment {
                    text: format!("-> @{witness}"),
                    status: ToolStatus::Agent,
                    no_leading_space: false,
                },
            );
        }
    }
    rendered
}

/// Returns the stable UI spelling for one self-reported agent work phase.
fn watched_agent_work_status_symbol(
    status: Option<&tau_proto::AgentWatchWorkStatusNotification>,
) -> String {
    crate::list_agents::work_status_symbol(status.map(|status| status.phase)).to_owned()
}

impl EventRenderer {
    #[cfg(test)]
    pub(crate) fn new(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
    ) -> Self {
        // Tests pass a state_dir of None so toggles never touch the
        // user's real `~/.local/state/tau/cli.json`.
        Self::new_with_state(
            handle,
            completion_data,
            theme,
            path_tau_config_settings::CliState::default(),
            tau_config::settings::TauDirs {
                config_dir: None,
                state_dir: None,
            },
            ">".to_string(),
            ">".to_string(),
        )
    }

    pub(crate) fn new_with_state(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
        state: tau_config::settings::CliState,
        state_dirs: tau_config::settings::TauDirs,
        prompt_symbol: String,
        submitted_prompt_symbol: String,
    ) -> Self {
        let cli_state_mirror = path_std_sync::Arc::new(path_std_sync::Mutex::new(state.clone()));
        handle.set_redraw_history_size(state.redraw_history_size);
        Self {
            resources: renderer_state::RendererResourcesState {
                handle: RendererHandle::new(handle),
                completion_data,
                action_state: ActionCommandState::new(std::iter::empty::<&str>()),
                skill_state: SkillCommandState::new(),
                theme,
                prompt_symbol,
                submitted_prompt_symbol,
            },
            discovery: renderer_state::AgentDiscoveryState {
                initialized_discovery_epochs: HashSet::new(),
                pending_initial_discovery: HashMap::new(),
                known_agents: Arc::new(Mutex::new(Vec::new())),
                agent_display_names: Arc::new(Mutex::new(HashMap::new())),
                agent_navigation: Arc::new(Mutex::new(AgentNavigation::default())),
                ephemeral_agents: Arc::new(Mutex::new(HashSet::new())),
            },
            selection: renderer_state::AgentSelectionState {
                current_agent_id: None,
                displayed_agent_id: None,
                awaiting_new_agent_selection: false,
                no_agent_ui_state: AgentUiState::default(),
                agents_ui_state: HashMap::new(),
                overview_message_ids: HashSet::new(),
                current_agent_state: Arc::new(Mutex::new(
                    renderer_state::SelectionIntent::default(),
                )),
                draft_retargeter: None,
            },
            event_owners: renderer_state::EventOwnershipState::default(),
            watches: renderer_state::WatchActivityState::default(),
            transcript: renderer_state::TranscriptState::default(),
            session: renderer_state::SessionPresentationState::default(),
            presentation: renderer_state::PresentationSettingsState {
                state_dirs,
                diffs_expanded: state.show_diff,
                show_thinking: state.show_thinking,
                verbose_mode: true,
                show_turn_stats: state.show_turn_stats,
                redraw_counter: state.redraw_counter,
                redraw_history_size: state.redraw_history_size,
                osc8_links: true,
                show_ui_io: state.show_ui_io,
                ui_io_stats: UiIoStats::default(),
                last_full_render_count: 0,
                last_full_render_at: None,
                show_tools: state.show_tools,
                show_messages: state.show_messages,
                show_internal_prompts: state.show_internal_prompts,
                notice_level: state.notice_level,
                show_prompt_scroll_indicator: state.show_prompt_scroll_indicator,
                cli_state_mirror,
            },
            role: renderer_state::RolePresentationState {
                current_model: None,
                quota_pacing: path_crate_provider_quota::QuotaPacingState::default(),
                last_quota_tick: None,
                current_role: None,
                role_defaults: HashMap::new(),
                baseline_params: None,
                model_params: tau_proto::ModelParams::default(),
                fast_service_tier_state: Arc::new(AtomicBool::new(false)),
                current_role_state: Arc::new(Mutex::new(None)),
                roles_available: Arc::new(Mutex::new(Vec::new())),
                custom_prompts: Arc::new(Mutex::new(Vec::new())),
                role_groups_available: Arc::new(Mutex::new(Vec::new())),
                role_group_memory: Arc::new(Mutex::new(HashMap::new())),
                verbosity_state: Arc::new(path_std_sync_atomic::AtomicU8::new(
                    tau_proto::Verbosity::default().as_u8(),
                )),
                thinking_summary_state: Arc::new(path_std_sync_atomic::AtomicU8::new(
                    tau_proto::ThinkingSummary::default().as_u8(),
                )),
            },
            editor: renderer_state::EditorPublicationState {
                editor_context: Arc::new(Mutex::new(tau_cli_term::EditorContext::default())),
                suppress_editor_context_publish: false,
                #[cfg(test)]
                response_copy_bytes: Cell::new(0),
                #[cfg(test)]
                final_semantic_projection: renderer_state::FinalSemanticProjectionCounts::default(),
            },
            activity: renderer_state::RendererActivityState {
                tool_timer: None,
                agent_in_progress: Arc::new(AtomicBool::new(false)),
            },
            staged_finished_response: None,
            staged_finished_status: None,
            final_publication_in_progress: false,
            hidden_finalization_in_progress: false,
            #[cfg(test)]
            finished_staging_hook: None,
            #[cfg(test)]
            finished_commit_hook: None,
            #[cfg(test)]
            finished_published_hook: None,
        }
    }

    /// Configures whether transcript Markdown links carry OSC 8 metadata.
    pub(crate) fn set_osc8_links(&mut self, enabled: bool) {
        self.presentation.osc8_links = enabled;
    }

    pub(crate) fn set_tool_timer(&mut self, timer: ToolTimerNotifier) {
        self.activity.tool_timer = Some(timer);
    }

    /// Installs a deterministic ordinary-final staging midpoint for frame
    /// tests.
    #[cfg(test)]
    pub(crate) fn set_finished_staging_hook(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.finished_staging_hook = Some(hook);
    }

    /// Installs a deterministic selected-final commit midpoint for frame tests.
    #[cfg(test)]
    pub(crate) fn set_finished_commit_hook(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.finished_commit_hook = Some(hook);
    }

    /// Installs a selected-final publication-complete hook for frame tests.
    #[cfg(test)]
    pub(crate) fn set_finished_published_hook(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.finished_published_hook = Some(hook);
    }

    /// Returns test-only generic tool bookkeeping without exposing a production
    /// inspection side channel.
    #[cfg(test)]
    pub(crate) fn test_active_tool_count(&self) -> usize {
        self.transcript.runtime.tool_calls.len()
    }

    /// Returns renderer-owned redraw requests for hidden-final tests.
    #[cfg(test)]
    pub(crate) fn redraw_request_count_for_test(&self) -> u64 {
        self.resources.handle.redraw_request_count()
    }

    /// Returns exact output-block replacements for focused rendering tests.
    #[cfg(test)]
    pub(crate) fn block_replacement_count_for_test(&self) -> u64 {
        self.resources.handle.block_replacement_count()
    }

    /// Returns the exact raw text retained for the latest submitted user
    /// prompt.
    #[cfg(test)]
    pub(crate) fn last_submitted_user_prompt_text_for_test(&self) -> Option<&str> {
        self.transcript
            .runtime
            .last_user_block
            .as_ref()
            .map(|(_, text)| text.as_str())
    }

    pub(crate) fn set_draft_retargeter(
        &mut self,
        handle: Arc<(Mutex<DraftSlot>, Condvar)>,
        session_id: Arc<Mutex<tau_proto::SessionId>>,
    ) {
        self.selection.draft_retargeter = Some(DraftRetargeter { handle, session_id });
    }

    /// Sets the ordered profile selection resolved for the daemon this UI
    /// starts.
    pub(crate) fn set_startup_profile_selection(
        &mut self,
        selection: Option<tau_config::settings::ProfileSelection>,
    ) {
        self.session.startup_profile_selection = selection;
    }

    /// Configures the filesystem context rendered beside the current session.
    pub(crate) fn set_right_prompt_paths(
        &mut self,
        cwd: std::path::PathBuf,
        home: Option<std::path::PathBuf>,
    ) {
        self.session.right_prompt_paths = Some((cwd, home));
    }

    pub(crate) fn known_agents(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        self.discovery.known_agents.clone()
    }

    pub(crate) fn agent_display_names(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<tau_proto::AgentId, String>>> {
        self.discovery.agent_display_names.clone()
    }

    /// Returns the canonical cumulative per-agent costs shared with input
    /// actions.
    pub(crate) fn agent_estimated_api_costs(&self) -> crate::estimated_cost::AgentCostProjection {
        self.watches.agent_estimated_api_costs.clone()
    }

    pub(crate) fn ephemeral_agents(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<HashSet<tau_proto::AgentId>>> {
        self.discovery.ephemeral_agents.clone()
    }

    pub(crate) fn agent_navigation(&self) -> Arc<Mutex<AgentNavigation>> {
        self.discovery.agent_navigation.clone()
    }

    pub(crate) fn current_agent_state(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<renderer_state::SelectionIntent>> {
        self.selection.current_agent_state.clone()
    }

    #[cfg(test)]
    pub(crate) fn displayed_agent_id_for_test(&self) -> Option<&tau_proto::AgentId> {
        self.selection.displayed_agent_id.as_ref()
    }

    #[cfg(test)]
    /// Removes the status block so tests can exercise placeholder-only redraws.
    pub(crate) fn clear_model_status_for_test(&mut self) {
        self.transcript.runtime.model_status_block = None;
    }

    #[cfg(test)]
    pub(crate) fn agent_id_for_event_for_test(&self, event: &Event) -> Option<tau_proto::AgentId> {
        self.agent_id_for_event(event)
    }

    #[cfg(test)]
    pub(crate) fn switch_agent(&mut self, agent_id: tau_proto::AgentId) {
        self.switch_agent_after_display_update(agent_id, || {});
    }

    /// Applies a target already claimed by the input or attach CAS boundary
    /// only while the exact intent epoch and target remain current.
    pub(crate) fn apply_claimed_agent(&mut self, agent_id: tau_proto::AgentId, intent_epoch: u64) {
        if !self.selection_intent_matches(intent_epoch, Some(&agent_id)) {
            return;
        }
        self.switch_agent_after_display_update_inner(agent_id, || {}, false);
    }

    #[cfg(test)]
    /// Invokes `after_display_update` after restoring the destination
    /// transcript but before updating the selected target, status, or
    /// placeholder.
    pub(crate) fn switch_agent_after_display_update_for_test(
        &mut self,
        agent_id: tau_proto::AgentId,
        after_display_update: impl FnOnce(),
    ) {
        self.switch_agent_after_display_update(agent_id, after_display_update);
    }

    #[cfg(test)]
    fn switch_agent_after_display_update(
        &mut self,
        agent_id: tau_proto::AgentId,
        after_display_update: impl FnOnce(),
    ) {
        self.switch_agent_after_display_update_inner(agent_id, after_display_update, true);
    }

    fn switch_agent_after_display_update_inner(
        &mut self,
        agent_id: tau_proto::AgentId,
        after_display_update: impl FnOnce(),
        update_intent: bool,
    ) {
        let handle = self.resources.handle.terminal_handle();
        // A selection transition publishes one coherent transcript, target,
        // status, and placeholder frame. Input routing is mirrored earlier by
        // the input thread and is intentionally outside this renderer batch.
        handle.with_redraw_suppressed(|| {
            self.remember_agent(agent_id.clone());
            let target_changed = self.selection.current_agent_id.as_ref() != Some(&agent_id);
            let display_changed = self.selection.displayed_agent_id.as_ref() != Some(&agent_id);

            if display_changed {
                // Let transcript switching see the previous awaiting flag so it can
                // distinguish initial no-agent adoption from explicit `:agent new`.
                self.show_agent_transcript(agent_id.clone());
            }
            after_display_update();
            self.selection.awaiting_new_agent_selection = false;

            if target_changed {
                self.set_current_agent_id(Some(agent_id), false, update_intent);
                self.render_model_status();
                self.refresh_prompt_placeholder();
                handle.redraw();
            }
        });
        self.flush_pending_initial_discovery();
    }

    #[cfg(test)]
    pub(crate) fn clear_selected_agent(&mut self) {
        self.clear_selected_agent_after_display_update(|| {});
    }

    /// Applies an empty target already claimed by the input boundary only while
    /// the exact intent epoch remains current.
    pub(crate) fn apply_claimed_clear(&mut self, intent_epoch: u64) {
        if !self.selection_intent_matches(intent_epoch, None) {
            return;
        }
        self.clear_selected_agent_after_display_update_inner(|| {}, false);
    }

    fn selection_intent_matches(
        &self,
        intent_epoch: u64,
        agent_id: Option<&tau_proto::AgentId>,
    ) -> bool {
        self.selection
            .current_agent_state
            .lock()
            .is_ok_and(|intent| {
                intent.epoch == intent_epoch && intent.selected_agent_id.as_ref() == agent_id
            })
    }

    #[cfg(test)]
    /// Invokes `after_display_update` after restoring the no-agent transcript
    /// but before clearing the selected target, status, or placeholder.
    pub(crate) fn clear_selected_agent_after_display_update_for_test(
        &mut self,
        after_display_update: impl FnOnce(),
    ) {
        self.clear_selected_agent_after_display_update(after_display_update);
    }

    #[cfg(test)]
    fn clear_selected_agent_after_display_update(&mut self, after_display_update: impl FnOnce()) {
        self.clear_selected_agent_after_display_update_inner(after_display_update, true);
    }

    fn clear_selected_agent_after_display_update_inner(
        &mut self,
        after_display_update: impl FnOnce(),
        update_intent: bool,
    ) {
        let handle = self.resources.handle.terminal_handle();
        handle.with_redraw_suppressed(|| {
            handle.with_output_transaction(|| {
                let target_changed = self.selection.current_agent_id.is_some();
                let display_changed = self.selection.displayed_agent_id.is_some();
                if target_changed || display_changed {
                    // Only a clear that actually leaves an agent creates the explicit
                    // no-agent boundary. A delayed clear command that arrives while
                    // the UI is already on the no-agent screen must stay a no-op,
                    // otherwise the next agent would incorrectly clear startup
                    // history instead of adopting it.
                    self.selection.awaiting_new_agent_selection = true;
                }

                if display_changed {
                    self.store_visible_agent_state();
                    let state = std::mem::take(&mut self.selection.no_agent_ui_state);
                    self.restore_visible_agent_state(state);
                    self.rerender_visible_for_current_settings();
                    self.selection.displayed_agent_id = None;
                }
                after_display_update();

                if target_changed {
                    self.set_current_agent_id(None, false, update_intent);
                    self.render_model_status();
                    self.refresh_prompt_placeholder();
                    handle.redraw();
                }
            });
        });
    }

    fn store_visible_agent_state(&mut self) {
        let state = self.take_visible_agent_state();
        if let Some(displayed) = self.selection.displayed_agent_id.clone() {
            self.selection.agents_ui_state.insert(displayed, state);
        } else {
            self.selection.no_agent_ui_state = state;
        }
    }

    fn show_agent_transcript(&mut self, agent_id: tau_proto::AgentId) {
        let handle = self.resources.handle.terminal_handle();
        handle.with_output_transaction(|| self.show_agent_transcript_inner(agent_id));
    }

    /// Swaps the visible transcript under the caller's output transaction.
    fn show_agent_transcript_inner(&mut self, agent_id: tau_proto::AgentId) {
        let needs_snapshot_swap = self.selection.displayed_agent_id.is_some()
            || self.selection.agents_ui_state.contains_key(&agent_id)
            || self.visible_no_agent_snapshot_needs_preservation();
        if needs_snapshot_swap {
            self.store_visible_agent_state();
            let state = self
                .selection
                .agents_ui_state
                .remove(&agent_id)
                .unwrap_or_default();
            self.restore_visible_agent_state(state);
            self.rerender_visible_for_current_settings();
        }
        if !needs_snapshot_swap && self.selection.displayed_agent_id.is_none() {
            self.adopt_visible_no_agent_owners(&agent_id);
        }
        self.selection.displayed_agent_id = Some(agent_id);
    }

    /// Render initialization summaries deferred while no first agent was
    /// selected.
    fn flush_pending_initial_discovery(&mut self) {
        let Some(agent_id) = self.selection.displayed_agent_id.as_ref() else {
            return;
        };
        let Some(events) = self.discovery.pending_initial_discovery.remove(agent_id) else {
            return;
        };
        for deferred in events {
            deferred.with_prepared(|prepared, recorded_at| {
                if let Event::HarnessAgentContextInitialized(initialized) = prepared.event() {
                    self.print_agent_context_initialized(initialized);
                } else if prepared.finished().is_some() {
                    self.handle_deferred_provider_finished(&prepared, recorded_at);
                } else {
                    self.learn_agent_metadata(&prepared);
                    self.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
                }
            });
        }
        self.update_agent_in_progress();
    }

    /// Publishes one provider terminal with its original metadata projection.
    fn handle_deferred_provider_finished(
        &mut self,
        prepared: &PreparedRendererEvent<'_>,
        recorded_at: UnixMicros,
    ) {
        let (finished, terminal_tool_calls) = prepared
            .finished()
            .expect("deferred provider terminal preserves projection");
        let is_standalone = self
            .transcript
            .runtime
            .prompts
            .get(&finished.agent_prompt_id)
            .is_some_and(|state| state.is_standalone_compaction);
        if is_standalone {
            self.staged_finished_status =
                Some(self.stage_standalone_finished_status_block(finished, terminal_tool_calls));
        } else {
            self.staged_finished_response =
                Some(self.stage_finished_response(finished, terminal_tool_calls));
        }
        let handle = self.resources.handle.terminal_handle();
        self.final_publication_in_progress = true;
        handle.with_redraw_suppressed(|| {
            self.learn_agent_metadata(prepared);
            self.handle_recorded_at_for_visible_agent(prepared, recorded_at);
        });
        self.final_publication_in_progress = false;
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(self.staged_finished_response.is_none());
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(self.staged_finished_status.is_none());
    }

    fn adopt_visible_no_agent_owners(&mut self, agent_id: &tau_proto::AgentId) {
        let owner = UiSnapshotOwner::Agent(agent_id.clone());
        for state in self.session.extension_blocks.values_mut() {
            if matches!(state.owner, UiSnapshotOwner::NoAgent) {
                state.owner = owner.clone();
            }
        }
        for invocation_owner in self.event_owners.action_invocation_owners.values_mut() {
            if matches!(invocation_owner, UiSnapshotOwner::NoAgent) {
                *invocation_owner = owner.clone();
            }
        }
    }

    fn visible_no_agent_snapshot_needs_preservation(&self) -> bool {
        // Ordinary startup/status history on the initial start-new-agent screen
        // begins the first user-created conversation and remains adoptable.
        // Globally owned message facts are the exception: they never belong to an
        // agent transcript, so their snapshot is preserved even on that initial
        // screen. Other preservation state applies only after explicit
        // `:agent none` or `:agent new`, when the user has deliberately left a
        // previous transcript and the no-agent output must remain available.
        self.selection.displayed_agent_id.is_none()
            && (self.transcript.ownership.contains_global_message_fact
                || self.transcript.ownership.contains_overview_message
                || (self.selection.awaiting_new_agent_selection
                    && (self.transcript.ownership.preserve_on_fresh_agent_switch
                        || self.has_pending_no_agent_owner())))
    }

    fn has_pending_no_agent_owner(&self) -> bool {
        self.event_owners
            .action_invocation_owners
            .values()
            .any(|owner| matches!(owner, UiSnapshotOwner::NoAgent))
            || self
                .session
                .extension_blocks
                .values()
                .any(|state| matches!(state.owner, UiSnapshotOwner::NoAgent))
    }

    fn set_current_agent_id(
        &mut self,
        agent_id: Option<tau_proto::AgentId>,
        retarget_draft: bool,
        update_intent: bool,
    ) {
        self.selection.current_agent_id = agent_id.clone();
        if update_intent && let Ok(mut current) = self.selection.current_agent_state.lock() {
            current.epoch = current.epoch.saturating_add(1);
            current.selected_agent_id = agent_id;
        }
        if retarget_draft {
            self.retarget_prompt_draft();
        }
        self.refresh_watched_agent_blocks();
    }

    fn handle_agent_watches_updated(&mut self, updated: &tau_proto::AgentWatchesUpdated) {
        if self
            .session
            .current_session_id
            .as_ref()
            .is_some_and(|session_id| session_id != &updated.session_id)
        {
            return;
        }
        let watcher_id = updated.watcher_id.clone();
        if let Some(previous) = self.watches.watched_agents.get(&watcher_id) {
            for watched_agent_id in previous {
                if let Some(watchers) = self.watches.agent_watchers.get_mut(watched_agent_id) {
                    watchers.retain(|candidate| candidate != &watcher_id);
                    if watchers.is_empty() {
                        self.watches.agent_watchers.remove(watched_agent_id);
                    }
                }
            }
        }
        for watched_agent_id in &updated.watched_agent_ids {
            let watchers = self
                .watches
                .agent_watchers
                .entry(watched_agent_id.clone())
                .or_default();
            if !watchers.iter().any(|candidate| candidate == &watcher_id) {
                watchers.push(watcher_id.clone());
                watchers.sort();
            }
        }
        self.watches
            .watched_agents
            .insert(watcher_id, updated.watched_agent_ids.to_vec());
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn handle_agent_stats_updated(&mut self, updated: &tau_proto::AgentStatsUpdated) {
        if self
            .session
            .current_session_id
            .as_ref()
            .is_some_and(|session_id| session_id != &updated.session_id)
        {
            return;
        }
        if let Ok(mut navigation) = self.discovery.agent_navigation.lock() {
            navigation.apply_stats(
                &updated.agent_id,
                updated.navigation_mode,
                updated.runtime_state,
            );
        }
        self.watches
            .agent_stats
            .insert(updated.agent_id.clone(), updated.clone());
        self.watches.agent_estimated_api_costs.record(
            updated.agent_id.clone(),
            AgentCostSnapshot::new(
                updated.estimated_api_cost,
                updated.creator_subtree_estimated_api_cost,
            ),
        );
        self.render_model_status_if_present();
        if self.selection.current_agent_id.as_ref() == Some(&updated.agent_id) {
            self.refresh_prompt_placeholder();
            self.resources.handle.redraw();
        }
        self.refresh_watched_agent_blocks();
    }

    fn refresh_watched_agent_blocks(&mut self) {
        let Some(current) = self.selection.current_agent_id.clone() else {
            self.clear_watched_agent_blocks();
            return;
        };
        let projection = self.watch_activity_projection();
        let visible = WatchGraphProjection::visible_rows(
            &current,
            &self.watches.watched_agents,
            |agent_id| self.watched_agent_is_visible(agent_id),
            VISIBLE_WATCH_EXPANSION_LIMIT,
        );
        let visible_set: HashSet<_> = visible.iter().map(|row| row.agent_id.clone()).collect();
        let stale: Vec<_> = self
            .transcript
            .runtime
            .watched_agent_blocks
            .keys()
            .filter(|agent_id| !visible_set.contains(*agent_id))
            .cloned()
            .collect();
        for agent_id in stale {
            if let Some(block_id) = self
                .transcript
                .runtime
                .watched_agent_blocks
                .remove(&agent_id)
            {
                self.resources.handle.remove_block(block_id);
            }
        }
        for (index, row) in visible.iter().enumerate() {
            let edge_watcher = row.via.as_ref().unwrap_or(&current);
            let block = self.watched_agent_block(
                edge_watcher,
                &row.agent_id,
                row.via.as_ref(),
                &projection,
            );
            let block_id = if let Some(block_id) = self
                .transcript
                .runtime
                .watched_agent_blocks
                .get(&row.agent_id)
                .copied()
            {
                self.resources.handle.set_block(block_id, block);
                block_id
            } else {
                let block_id = self
                    .resources
                    .handle
                    .new_block(format!("watched-agent:{}", row.agent_id), block);
                self.transcript
                    .runtime
                    .watched_agent_blocks
                    .insert(row.agent_id.clone(), block_id);
                block_id
            };
            let later_blocks = visible[index + 1..].iter().filter_map(|later| {
                self.transcript
                    .runtime
                    .watched_agent_blocks
                    .get(&later.agent_id)
                    .copied()
            });
            self.resources
                .handle
                .push_above_active_before_any(block_id, later_blocks);
        }
        if !self.hidden_finalization_in_progress {
            self.resources.handle.redraw();
        }
    }

    /// Returns queued-prompt blocks in their existing queue order.
    fn queued_prompt_anchor_ids(&self) -> Vec<tau_cli_term::BlockId> {
        self.transcript
            .runtime
            .queued_user_blocks
            .iter()
            .filter_map(|queued| queued.id)
            .collect()
    }

    /// Returns watched-agent blocks that belong below queued prompts.
    fn watched_agent_anchor_ids(&self) -> Vec<tau_cli_term::BlockId> {
        self.transcript
            .runtime
            .watched_agent_blocks
            .values()
            .copied()
            .collect()
    }

    /// Returns the non-tool live activity blocks that follow active tool calls.
    fn non_tool_activity_anchor_ids(&self) -> Vec<tau_cli_term::BlockId> {
        let mut anchors = self.queued_prompt_anchor_ids();
        anchors.extend(self.watched_agent_anchor_ids());
        anchors
    }

    /// Returns whether a selected watched target remains visible for its
    /// current self-reported task status.
    ///
    /// A missing snapshot is the canonical unreported state. Work status,
    /// rather than transient turn activity, owns row lifetime so running
    /// and idle transitions can only redraw the existing row. A `Done`
    /// report is the one terminal status that removes the row.
    fn watched_agent_is_visible(&self, agent_id: &tau_proto::AgentId) -> bool {
        !matches!(
            self.watches
                .watched_agent_work_statuses
                .get(agent_id)
                .map(|status| status.phase),
            Some(tau_proto::AgentWorkStatusPhase::Done)
        )
    }

    fn agent_has_active_prompt(&self, agent_id: &tau_proto::AgentId) -> bool {
        self.watches
            .active_agent_prompts
            .get(agent_id)
            .is_some_and(|prompts| !prompts.is_empty())
    }

    /// Returns the current runtime state for one watched agent.
    ///
    /// Prompt activity is retained as a compatibility/catch-up fallback until
    /// the first complete runtime snapshot for the watched agent is observed.
    fn watched_agent_is_running(&self, watched_agent_id: &tau_proto::AgentId) -> bool {
        self.watches.agent_stats.get(watched_agent_id).map_or_else(
            || self.agent_has_active_prompt(watched_agent_id),
            |stats| stats.runtime_state == tau_proto::AgentRuntimeState::Running,
        )
    }

    /// Derives exact recursive watch activity from current live topology and
    /// current runtime state.
    fn watch_activity_projection(&self) -> WatchGraphProjection {
        let direct_edges = self
            .watches
            .watched_agents
            .iter()
            .flat_map(|(watcher, watched)| {
                watched
                    .iter()
                    .filter(|target| self.watched_agent_is_running(target))
                    .map(|target| (watcher.clone(), target.clone()))
            })
            .collect();
        WatchGraphProjection::new(
            &self.watches.watched_agents,
            &self.watches.agent_watchers,
            direct_edges,
        )
    }

    /// Records a current-session self-reported work-status snapshot for a
    /// watched agent and redraws any visible row using that presentation
    /// metadata.
    fn handle_watched_agent_work_status(
        &mut self,
        message: &tau_proto::AgentMessageReceived,
        status: &tau_proto::AgentWatchWorkStatusNotification,
    ) {
        if self
            .session
            .current_session_id
            .as_ref()
            .is_none_or(|session_id| session_id != &status.session_id)
        {
            return;
        }
        self.watches
            .watched_agent_work_statuses
            .insert(message.sender_id.clone(), status.clone());
        self.refresh_watched_agent_blocks();
    }

    fn mark_agent_prompt_active(
        &mut self,
        agent_id: &tau_proto::AgentId,
        agent_prompt_id: &tau_proto::AgentPromptId,
    ) {
        if self
            .watches
            .terminal_agent_prompts
            .contains(agent_prompt_id)
        {
            return;
        }
        // Prompt membership is exclusive: every transition below removes this
        // id from all previous owners before adding this owner. This direct
        // lookup avoids rebuilding status and watch rows for sampled updates
        // that already retain that exact association.
        if self
            .watches
            .active_agent_prompts
            .get(agent_id)
            .is_some_and(|prompts| prompts.contains(agent_prompt_id))
        {
            return;
        }
        self.remove_active_agent_prompt(agent_prompt_id);
        self.watches
            .active_agent_prompts
            .entry(agent_id.clone())
            .or_default()
            .insert(agent_prompt_id.clone());
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn mark_known_agent_prompt_active(
        &mut self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        originator: &tau_proto::PromptOriginator,
    ) {
        if let Some(agent_id) = self
            .event_owners
            .prompt_agents
            .get(agent_prompt_id)
            .cloned()
            .or_else(|| self.agent_id_for_originator(originator))
        {
            self.mark_agent_prompt_active(&agent_id, agent_prompt_id);
        }
    }

    fn mark_agent_prompt_inactive(&mut self, agent_prompt_id: &tau_proto::AgentPromptId) {
        self.watches
            .terminal_agent_prompts
            .insert(agent_prompt_id.clone());
        self.remove_active_agent_prompt(agent_prompt_id);
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn remove_active_agent_prompt(&mut self, agent_prompt_id: &tau_proto::AgentPromptId) {
        self.watches.active_agent_prompts.retain(|_, prompts| {
            prompts.remove(agent_prompt_id);
            !prompts.is_empty()
        });
    }

    fn clear_watched_agent_blocks(&mut self) {
        for (_, block_id) in self.transcript.runtime.watched_agent_blocks.drain() {
            self.resources.handle.remove_block(block_id);
        }
        self.resources.handle.redraw();
    }

    /// Retires all cached watch topology and state involving an unloaded agent.
    fn remove_agent_watch_endpoint(&mut self, agent_id: &tau_proto::AgentId) {
        self.watches.watched_agents.remove(agent_id);
        for watched in self.watches.watched_agents.values_mut() {
            watched.retain(|watched_id| watched_id != agent_id);
        }
        self.watches.agent_watchers.remove(agent_id);
        for watchers in self.watches.agent_watchers.values_mut() {
            watchers.retain(|watcher_id| watcher_id != agent_id);
        }
        self.watches
            .agent_watchers
            .retain(|_, watchers| !watchers.is_empty());
        self.watches.watched_agent_work_statuses.remove(agent_id);
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn watched_agent_block(
        &self,
        watcher_id: &tau_proto::AgentId,
        agent_id: &tau_proto::AgentId,
        via: Option<&tau_proto::AgentId>,
        projection: &WatchGraphProjection,
    ) -> tau_cli_term::StyledBlock {
        let display_name = self
            .discovery
            .agent_display_names
            .lock()
            .ok()
            .and_then(|names| names.get(agent_id).cloned());
        let stats = self.watches.agent_stats.get(agent_id);
        let work_status = self.watches.watched_agent_work_statuses.get(agent_id);
        let directly_running = projection.edge_is_directly_running(watcher_id, agent_id);
        let witness = (!directly_running)
            .then(|| projection.witness_for(agent_id, &self.watches.watched_agents))
            .flatten();
        let activity = if directly_running {
            WatchedAgentActivity::Running
        } else if let Some(witness) = witness.as_ref() {
            WatchedAgentActivity::Watching {
                witness: witness.as_str(),
            }
        } else {
            WatchedAgentActivity::Idle
        };
        let display = watched_agent_tool_display(
            display_name.as_deref(),
            agent_id.as_str(),
            via.map(tau_proto::AgentId::as_str),
            stats,
            activity,
            work_status,
        );
        render_tool_block(&self.resources.theme, &display)
    }

    fn retarget_prompt_draft(&self) {
        let Some(retargeter) = &self.selection.draft_retargeter else {
            return;
        };
        let Ok(session_id) = retargeter.session_id.lock().map(|id| id.clone()) else {
            return;
        };
        let target_agent_id = self.selection.current_agent_id.clone();
        retarget_prompt_draft_snapshot(
            retargeter.handle.as_ref(),
            session_id,
            target_agent_id,
            self.resources.handle.get_buffer(),
        );
    }

    fn refresh_prompt_placeholder(&mut self) {
        let current_agent_navigation =
            self.selection
                .current_agent_id
                .as_deref()
                .and_then(|agent_id| {
                    self.discovery
                        .agent_navigation
                        .lock()
                        .ok()
                        .map(|navigation| {
                            navigation.live_agents().get(agent_id).map_or(
                                (tau_proto::AgentNavigationMode::default(), false),
                                |agent_id| {
                                    (navigation.mode(agent_id), navigation.is_active(agent_id))
                                },
                            )
                        })
                });
        self.resources
            .handle
            .set_input_placeholder(crate::theme::prompt_input_placeholder(
                &self.resources.theme,
                self.role.current_role.as_deref(),
                self.selection.current_agent_id.as_deref(),
                current_agent_navigation,
            ));
    }

    pub(crate) fn set_action_state(&mut self, action_state: ActionCommandState) {
        self.resources.action_state = action_state;
        self.refresh_action_completions();
    }

    /// Remember which transcript was viewed when a dynamic action was invoked.
    pub(crate) fn record_action_invocation(
        &mut self,
        invocation_id: tau_proto::ActionInvocationId,
        owner_agent_id: Option<tau_proto::AgentId>,
    ) {
        let owner = owner_agent_id
            .map(UiSnapshotOwner::Agent)
            .unwrap_or(UiSnapshotOwner::NoAgent);
        self.event_owners
            .action_invocation_owners
            .insert(invocation_id, owner);
    }

    pub(crate) fn skill_arg_completer(&self) -> tau_cli_term::ArgCompleter {
        self.resources.skill_state.arg_completer()
    }

    fn take_visible_agent_state(&mut self) -> AgentUiState {
        let output = self.resources.handle.take_output_snapshot();
        self.take_visible_agent_state_with_output(output)
    }

    /// Moves all renderer bookkeeping into a state paired with the supplied
    /// model.
    fn take_visible_agent_state_with_output(
        &mut self,
        output: tau_cli_term::OutputSnapshot,
    ) -> AgentUiState {
        AgentUiState {
            output,
            transcript: std::mem::take(&mut self.transcript),
        }
    }

    fn restore_visible_agent_state(&mut self, state: AgentUiState) {
        self.restore_agent_ui_state(state, AgentUiRestoreMode::Visible);
    }

    fn restore_renderer_bookkeeping(&mut self, state: AgentUiState) {
        self.restore_agent_ui_state(state, AgentUiRestoreMode::DetachedBookkeeping);
    }

    fn restore_agent_ui_state(&mut self, state: AgentUiState, mode: AgentUiRestoreMode) {
        if matches!(mode, AgentUiRestoreMode::Visible) {
            self.resources.handle.replace_output_snapshot(state.output);
        }
        let mut transcript = state.transcript;
        transcript.status.main_agent_turn_active = transcript.status.main_agent_turn_active
            && transcript.status.agent_activity.is_in_progress();
        transcript.status.main_tools_visible =
            transcript.status.main_tools_visible && transcript.status.main_agent_turn_active;
        self.transcript = transcript;
        if matches!(mode, AgentUiRestoreMode::Visible) {
            self.publish_editor_conversation_context();
        }
    }

    fn agent_status_description(&self, agent_id: &tau_proto::AgentId) -> Option<String> {
        self.discovery
            .agent_display_names
            .lock()
            .ok()
            .and_then(|names| names.get(agent_id).cloned())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty() && name != agent_id.as_str())
            .map(|name| format!("({name})"))
    }

    /// Builds a message-safe label from current local presentation metadata.
    fn message_agent_display_label(
        &self,
        agent_id: &tau_proto::AgentId,
        use_local_names: bool,
    ) -> String {
        if !use_local_names {
            return format!("@{agent_id}");
        }
        let display_name = self
            .discovery
            .agent_display_names
            .lock()
            .ok()
            .and_then(|names| names.get(agent_id).cloned());
        Self::agent_identity_with_name(
            &format!("@{agent_id}"),
            display_name.as_deref(),
            agent_id.as_str(),
        )
    }

    /// Combines an unambiguous routing identity with bounded presentation-only
    /// metadata, suppressing names that contain the agent id.
    fn agent_identity_with_name(
        identity: &str,
        display_name: Option<&str>,
        agent_id: &str,
    ) -> String {
        display_name
            .map(str::trim)
            .filter(|name| !name.is_empty() && !name.contains(agent_id))
            .map(Self::bounded_agent_message_name)
            .filter(|name| !name.is_empty())
            .map(|name| format!("{identity} ({name})"))
            .unwrap_or_else(|| identity.to_owned())
    }

    fn watched_by_status(&self, agent_id: &tau_proto::AgentId) -> Option<String> {
        let watchers = self.watches.agent_watchers.get(agent_id)?;
        let first = watchers.first()?;
        match watchers.len() {
            0 => None,
            1 => Some(first.to_string()),
            count => Some(format!("{first}, +{} more agents", count.saturating_sub(1))),
        }
    }

    fn remember_agent(&mut self, agent_id: tau_proto::AgentId) {
        if let Ok(mut agents) = self.discovery.known_agents.lock()
            && !agents.iter().any(|known| known == agent_id.as_str())
        {
            agents.push(agent_id.into_string());
            agents.sort();
        }
    }

    fn remember_agent_display_name(&mut self, agent_id: &tau_proto::AgentId, display_name: &str) {
        let mut changed = false;
        if let Ok(mut names) = self.discovery.agent_display_names.lock() {
            let display_name = display_name.trim();
            if !display_name.is_empty() {
                changed = names
                    .get(agent_id)
                    .is_none_or(|known| known != display_name);
                names.insert(agent_id.clone(), display_name.to_owned());
            }
        }
        if changed {
            self.rerender_message_history();
        }
    }

    /// Clears presentation metadata whose authority is limited to one session.
    fn clear_agent_display_names(&self) {
        if let Ok(mut names) = self.discovery.agent_display_names.lock() {
            names.clear();
        }
    }

    /// Reprojects visible message blocks from semantic events and current UI
    /// settings, including the latest session-scoped agent metadata.
    fn rerender_message_history(&mut self) {
        for entry in &self.transcript.history.message_history {
            let use_local_names = entry.session_id == self.session.current_session_id;
            self.resources.handle.set_block(
                entry.block_id,
                self.render_agent_message_block_with_local_names(&entry.event, use_local_names),
            );
        }
        if !self.transcript.history.message_history.is_empty() {
            self.resources.handle.redraw();
        }
    }

    fn mark_agent_live(&mut self, agent_id: tau_proto::AgentId) {
        self.remember_agent(agent_id.clone());
        if let Ok(mut navigation) = self.discovery.agent_navigation.lock() {
            navigation.mark_live(agent_id);
        }
        self.render_model_status_if_present();
    }

    fn remember_agent_ephemeral(&mut self, agent_id: &tau_proto::AgentId) {
        if let Ok(mut agents) = self.discovery.ephemeral_agents.lock() {
            agents.insert(agent_id.clone());
        }
    }

    fn render_model_status_if_present(&mut self) {
        if !self.final_publication_in_progress
            && self.transcript.runtime.model_status_block.is_some()
        {
            self.render_model_status();
        }
    }

    fn save_cli_state(&self) {
        let state = tau_config::settings::CliState {
            show_diff: self.presentation.diffs_expanded,
            show_thinking: self.presentation.show_thinking,
            show_turn_stats: self.presentation.show_turn_stats,
            redraw_counter: self.presentation.redraw_counter,
            redraw_history_size: self.presentation.redraw_history_size,
            show_ui_io: self.presentation.show_ui_io,
            show_tools: self.presentation.show_tools,
            show_messages: self.presentation.show_messages,
            show_internal_prompts: self.presentation.show_internal_prompts,
            notice_level: self.presentation.notice_level,
            show_status: path_tau_config_settings::ShowStatus::All,
            show_prompt_scroll_indicator: self.presentation.show_prompt_scroll_indicator,
        };
        if let Ok(mut mirror) = self.presentation.cli_state_mirror.lock() {
            *mirror = state.clone();
        }
        state.save(&self.presentation.state_dirs);
    }

    /// Shared snapshot of the persisted CLI settings, updated in sync
    /// with every successful `:set` (i.e. on every
    /// [`Self::save_cli_state`] call). Cloned by the input loop so the
    /// `:set` name-completion menu can show each setting's current
    /// value without touching renderer-thread fields directly.
    pub(crate) fn cli_state_mirror(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_config::settings::CliState>> {
        self.presentation.cli_state_mirror.clone()
    }

    pub(crate) fn editor_context(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>> {
        self.editor.editor_context.clone()
    }

    fn publish_editor_conversation_context(&self) {
        if self.editor.suppress_editor_context_publish {
            return;
        }
        if let Ok(mut context) = self.editor.editor_context.lock() {
            let responses = &self.transcript.runtime.editor_conversation_context;
            if context.current_response != responses.current_response {
                #[cfg(test)]
                self.editor.response_copy_bytes.set(
                    self.editor.response_copy_bytes.get()
                        + responses
                            .current_response
                            .as_ref()
                            .map_or(0, |text| text.len() as u64),
                );
                context.current_response = responses.current_response.clone();
            }
            if context.last_response != responses.last_response {
                #[cfg(test)]
                self.editor.response_copy_bytes.set(
                    self.editor.response_copy_bytes.get()
                        + responses
                            .last_response
                            .as_ref()
                            .map_or(0, |text| text.len() as u64),
                );
                context.last_response = responses.last_response.clone();
            }
        }
    }

    fn set_editor_current_response(&mut self, text: Option<String>) {
        self.transcript
            .runtime
            .editor_conversation_context
            .current_response = text;
        self.publish_editor_conversation_context();
    }

    fn with_editor_context_publish_suppressed<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let previous = self.editor.suppress_editor_context_publish;
        self.editor.suppress_editor_context_publish = true;
        let result = f(self);
        self.editor.suppress_editor_context_publish = previous;
        result
    }

    /// Returns a shared flag that is true while any agent/session work
    /// is in flight. The input loop uses it to keep Ctrl-D from
    /// terminating an active session accidentally.
    pub(crate) fn agent_in_progress_state(&self) -> Arc<AtomicBool> {
        self.activity.agent_in_progress.clone()
    }

    #[cfg(test)]
    pub(crate) fn main_agent_turn_active_for_test(&self) -> bool {
        self.transcript.status.main_agent_turn_active
    }

    /// Returns response bytes copied into the shared editor context by test
    /// builds.
    #[cfg(test)]
    pub(crate) fn editor_response_copy_bytes_for_test(&self) -> u64 {
        self.editor.response_copy_bytes.get()
    }

    /// Returns exact final semantic projection work performed by production
    /// renderer paths.
    #[cfg(test)]
    pub(crate) fn final_semantic_projection_counts_for_test(
        &self,
    ) -> renderer_state::FinalSemanticProjectionCounts {
        self.editor.final_semantic_projection
    }

    /// Reports whether generic prompt fallback still marks an agent active.
    #[cfg(test)]
    pub(crate) fn agent_has_active_prompt_for_test(&self, agent_id: &tau_proto::AgentId) -> bool {
        self.watches.active_agent_prompts.contains_key(agent_id)
    }

    /// Reports the generic active side-agent count without reading terminal UI.
    #[cfg(test)]
    pub(crate) fn active_side_agent_count_for_test(&self) -> usize {
        self.active_side_agent_count()
    }

    /// Reports whether the selected main agent has effective live activity.
    #[cfg(test)]
    fn main_agent_is_in_progress_for_test(&self) -> bool {
        self.transcript.status.main_agent_turn_active
            && self.transcript.status.agent_activity.is_in_progress()
    }

    /// Returns a clone of the shared Fast-mode mirror, used by configurable
    /// bindings.
    pub(crate) fn fast_service_tier_state(
        &self,
    ) -> std::sync::Arc<path_std_sync::atomic::AtomicBool> {
        self.role.fast_service_tier_state.clone()
    }

    /// Returns a clone of the shared active-role mirror used by role cycling.
    pub(crate) fn current_role_state(&self) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
        self.role.current_role_state.clone()
    }

    /// Returns a clone of the shared ordered role list used by role cycling.
    pub(crate) fn roles_available(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        self.role.roles_available.clone()
    }

    /// Returns a clone of the shared custom prompts announced by the harness.
    pub(crate) fn custom_prompts(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessCustomPrompt>>> {
        self.role.custom_prompts.clone()
    }

    /// Returns a clone of the shared ordered role groups used by role cycling.
    pub(crate) fn role_groups_available(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessRoleGroup>>> {
        self.role.role_groups_available.clone()
    }

    /// Returns a clone of the per-group runtime role memory used by role
    /// cycling.
    pub(crate) fn role_group_memory(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<String, String>>> {
        self.role.role_group_memory.clone()
    }

    /// Applies a runtime `:theme` change to this renderer-only UI process.
    pub(crate) fn apply_theme(&mut self, theme: tau_themes::Theme) {
        self.resources.theme = theme;
        self.resources
            .handle
            .set_left_prompt(crate::theme::active_prompt_marker(
                &self.resources.theme,
                &self.resources.prompt_symbol,
                self.role.current_role.as_deref(),
            ));
        self.refresh_prompt_placeholder();
        let effective_session_id = self
            .selection
            .draft_retargeter
            .as_ref()
            .and_then(|retargeter| {
                retargeter
                    .session_id
                    .lock()
                    .ok()
                    .map(|session_id| session_id.clone())
            })
            .or_else(|| self.session.current_session_id.clone());
        if let Some(session_id) = effective_session_id {
            self.render_right_prompt_context(&session_id);
        }
        self.render_model_status_if_present();
        self.rerender_visible_for_current_settings();
        self.resources.handle.invalidate_screen();
    }

    /// Apply a `:set <name> <value>` change. The caller (input loop)
    /// has already validated `name` and `value` against the
    /// [`crate::settings_registry`] table.
    pub(crate) fn apply_setting(&mut self, name: &str, value: &str) {
        let on = value == "true";
        match name {
            "show-diff" => self.set_diffs_expanded(on),
            "show-thinking" => self.set_show_thinking(on),
            "show-turn-stats" => self.set_show_turn_stats(on),
            "redraw-counter" => self.set_redraw_counter(on),
            "redraw-history-size" => {
                if let Ok(redraw_history_size) = value.parse::<usize>() {
                    self.set_redraw_history_size(redraw_history_size);
                }
            }
            "show-ui-io" => self.set_show_ui_io(on),
            "show-tools" => {
                if let Some(show_tools) = path_tau_config_settings::ShowTools::parse(value) {
                    self.set_show_tools(show_tools);
                }
            }
            "show-messages" => {
                if let Some(show_messages) = path_tau_config_settings::ShowMessages::parse(value) {
                    self.set_show_messages(show_messages);
                }
            }
            "show-internal-prompts" => self.set_show_internal_prompts(value == "on"),
            "notice-level" => {
                if let Some(level) = tau_proto::NoticeLevel::parse(value) {
                    self.set_notice_level(level);
                }
            }
            "show-prompt-scroll-indicator" => self.set_show_prompt_scroll_indicator(on),
            _ => {}
        }
    }

    /// Set the global expand-diffs flag and re-render every diff
    /// block in the chat history so the entire transcript switches
    /// mode at once. No-op if already in the requested state.
    fn set_diffs_expanded(&mut self, on: bool) {
        if self.presentation.diffs_expanded == on {
            return;
        }
        self.presentation.diffs_expanded = on;
        for entry in &self.transcript.history.diff_blocks {
            let block = self.render_diff_history_block(&entry.display, &entry.diff);
            self.resources.handle.set_block(entry.block_id, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Set the global show-thinking flag and re-render every prior
    /// thinking block in the transcript so the change takes effect
    /// retroactively (full text when on, empty content when off).
    /// Live in-flight thinking blocks are also flipped. New turns
    /// continue to be gated by the same flag.
    ///
    /// Empty content is used instead of `remove_block` so the
    /// block's position in the transcript is preserved; turning
    /// back on restores the original reasoning text in place.
    fn set_show_thinking(&mut self, on: bool) {
        use tau_themes::names;
        if self.presentation.show_thinking == on {
            return;
        }
        self.presentation.show_thinking = on;
        for entry in &self.transcript.history.thinking_history {
            let display = if self.presentation.verbose_mode && self.presentation.show_thinking {
                entry.text.as_str()
            } else {
                ""
            };
            self.resources.handle.set_block(
                entry.block_id,
                markdown_block_with_osc8(
                    &self.resources.theme,
                    names::AGENT_THINKING,
                    display,
                    self.presentation.osc8_links,
                ),
            );
        }
        for state in self.transcript.runtime.prompts.values_mut() {
            let Some(bid) = state.thinking_block_id else {
                continue;
            };
            let block = if self.presentation.verbose_mode && self.presentation.show_thinking {
                markdown_streaming_block_with_osc8(
                    &self.resources.theme,
                    names::AGENT_THINKING,
                    state.thinking_text.as_deref().unwrap_or_default(),
                    &mut state.thinking_markdown_cache,
                    MarkdownStreamUpdate::Append,
                    self.presentation.osc8_links,
                )
            } else {
                markdown_block_with_osc8(
                    &self.resources.theme,
                    names::AGENT_THINKING,
                    "",
                    self.presentation.osc8_links,
                )
            };
            self.resources.handle.set_block(bid, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Force a full repaint after a `:set show-*` change. Edited blocks
    /// from earlier in the transcript may already have scrolled out of
    /// the visible window, so the renderer needs to redraw from scratch
    /// for the change to take effect retroactively across scrollback.
    fn invalidate_for_retroactive_toggle(&mut self) {
        self.resources.handle.invalidate_screen();
    }

    fn set_redraw_counter(&mut self, on: bool) {
        if self.presentation.redraw_counter == on {
            return;
        }
        self.presentation.redraw_counter = on;
        self.render_model_status();
        self.save_cli_state();
    }

    fn set_redraw_history_size(&mut self, redraw_history_size: usize) {
        if self.presentation.redraw_history_size == redraw_history_size {
            return;
        }
        let previous = self.presentation.redraw_history_size;
        self.presentation.redraw_history_size = redraw_history_size;
        self.resources
            .handle
            .set_redraw_history_size(redraw_history_size);
        if previous < redraw_history_size {
            self.resources.handle.invalidate_screen();
        }
        self.save_cli_state();
    }

    fn set_show_ui_io(&mut self, on: bool) {
        if self.presentation.show_ui_io == on {
            return;
        }
        self.presentation.show_ui_io = on;
        self.render_model_status();
        self.save_cli_state();
    }

    pub(crate) fn handle_ui_io_sample(&mut self, stats: UiIoStats) {
        if self.presentation.ui_io_stats == stats {
            return;
        }
        self.presentation.ui_io_stats = stats;
        if self.presentation.show_ui_io {
            self.render_model_status();
        }
    }

    fn set_show_turn_stats(&mut self, on: bool) {
        if self.presentation.show_turn_stats == on {
            return;
        }
        self.presentation.show_turn_stats = on;
        for entry in &self.transcript.history.turn_stats_history {
            let block = self.render_turn_stats_entry(entry);
            self.resources.handle.set_block(entry.block_id, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }
    fn render_turn_stats_entry(&self, entry: &TurnStatsBlockEntry) -> tau_cli_term::StyledBlock {
        if self.presentation.verbose_mode && self.presentation.show_turn_stats {
            render_turn_stats_projection_block(&self.resources.theme, &entry.projection)
        } else {
            Self::empty_block()
        }
    }

    fn empty_block() -> tau_cli_term::StyledBlock {
        tau_cli_term::StyledBlock::new(tau_cli_term::StyledText::from(String::new()))
    }

    fn compaction_token_chip(tokens: u64) -> String {
        format!("#{}", format_token_count(tokens))
    }

    fn compaction_progress_status(original_input_tokens: Option<u64>) -> String {
        original_input_tokens
            .map(|tokens| {
                format!(
                    "{} {}",
                    Self::compaction_token_chip(tokens),
                    tau_proto::PROGRESS_INDICATOR_TEXT
                )
            })
            .unwrap_or_else(|| tau_proto::PROGRESS_INDICATOR_TEXT.to_owned())
    }

    fn compaction_success_status(original_input_tokens: Option<u64>) -> String {
        original_input_tokens.map_or_else(
            || "ok".to_owned(),
            |original| format!("{} → ? ok", Self::compaction_token_chip(original)),
        )
    }

    /// Formats one successful standalone compaction from its request input and
    /// the exact first transaction-owned continuation input, when available.
    pub(crate) fn standalone_compaction_success_status(
        original: Option<tau_proto::TokenCount>,
        after: Option<tau_proto::TokenCount>,
    ) -> String {
        match (original, after) {
            (Some(original), Some(after)) if original.get() != 0 => {
                let retained = (u128::from(after.get()) * 100 + u128::from(original.get()) / 2)
                    / u128::from(original.get());
                format!(
                    "{} → {} ({retained}%) ok",
                    Self::compaction_token_chip(original.get()),
                    Self::compaction_token_chip(after.get()),
                )
            }
            (Some(original), Some(after)) => format!(
                "{} → {} ok",
                Self::compaction_token_chip(original.get()),
                Self::compaction_token_chip(after.get()),
            ),
            (Some(original), None) => {
                format!("{} → ? ok", Self::compaction_token_chip(original.get()))
            }
            (None, Some(after)) => {
                format!("? → {} ok", Self::compaction_token_chip(after.get()))
            }
            (None, None) => "ok".to_owned(),
        }
    }

    fn render_tool_history_block(&self, display: &ToolCallDisplay) -> tau_cli_term::StyledBlock {
        if !self.presentation.verbose_mode {
            return Self::empty_block();
        }
        match self.presentation.show_tools {
            path_tau_config_settings::ShowTools::Full => {
                render_tool_block(&self.resources.theme, display)
            }
            path_tau_config_settings::ShowTools::Compact => self.render_compact_tool_block(display),
            path_tau_config_settings::ShowTools::Off
            | path_tau_config_settings::ShowTools::SummarizeTurn
            | path_tau_config_settings::ShowTools::SummarizePrompt => Self::empty_block(),
        }
    }

    /// Renders one still-running tool according to the top-level display mode.
    fn render_live_tool_block(&self, display: &ToolCallDisplay) -> tau_cli_term::StyledBlock {
        if self.presentation.verbose_mode {
            return self.render_tool_history_block(display);
        }
        self.render_compact_tool_block(display)
    }

    fn render_compact_tool_block(&self, display: &ToolCallDisplay) -> tau_cli_term::StyledBlock {
        render_tool_header_block(&self.resources.theme, display)
    }

    fn render_diff_history_block(
        &self,
        display: &ToolCallDisplay,
        diff: &tau_proto::ToolUsePayload,
    ) -> tau_cli_term::StyledBlock {
        if !self.presentation.verbose_mode {
            return Self::empty_block();
        }
        match self.presentation.show_tools {
            path_tau_config_settings::ShowTools::Full => match diff {
                tau_proto::ToolUsePayload::Diff(summary) => render_diff_tool_block(
                    &self.resources.theme,
                    display,
                    summary,
                    self.presentation.diffs_expanded,
                ),
                tau_proto::ToolUsePayload::Diffs { files } => render_multi_diff_tool_block(
                    &self.resources.theme,
                    display,
                    files,
                    self.presentation.diffs_expanded,
                ),
                tau_proto::ToolUsePayload::Text { .. } => {
                    render_tool_block(&self.resources.theme, display)
                }
            },
            path_tau_config_settings::ShowTools::Compact => self.render_compact_tool_block(display),
            path_tau_config_settings::ShowTools::Off
            | path_tau_config_settings::ShowTools::SummarizeTurn
            | path_tau_config_settings::ShowTools::SummarizePrompt => Self::empty_block(),
        }
    }

    fn render_summary_block(&self, summary: &ToolSummaryDisplay) -> tau_cli_term::StyledBlock {
        if self.presentation.verbose_mode
            && matches!(
                self.presentation.show_tools,
                tau_config::settings::ShowTools::SummarizeTurn
                    | tau_config::settings::ShowTools::SummarizePrompt
            )
        {
            render_tool_block(&self.resources.theme, &build_tool_summary_display(summary))
        } else {
            Self::empty_block()
        }
    }

    fn update_tool_summary_block(&mut self, block_id: tau_cli_term::BlockId) {
        let Some(summary) = self.transcript.status.tool_summaries.get(&block_id) else {
            return;
        };
        self.resources
            .handle
            .set_block(block_id, self.render_summary_block(summary));
    }

    fn record_tool_summary_result(
        &mut self,
        block_id: Option<tau_cli_term::BlockId>,
        display: Option<&tau_proto::ToolUseState>,
        diff: Option<&tau_proto::ToolUsePayload>,
        is_error: bool,
    ) {
        let Some(block_id) = block_id else {
            return;
        };
        if let Some(summary) = self.transcript.status.tool_summaries.get_mut(&block_id) {
            summary.completed += 1;
            if is_error {
                summary.err += 1;
            } else {
                summary.ok += 1;
            }
            if let Some(display) = display {
                summary.matches += display.stats.matches.unwrap_or(0);
                summary.lines += display.stats.lines.unwrap_or(0);
                summary.bytes += display.stats.bytes.unwrap_or(0);
            }
            if let Some(diff) = diff {
                let (added, removed) = diff_payload_counts(diff);
                summary.added += u64::from(added);
                summary.removed += u64::from(removed);
            }
        }
        let finished = self
            .transcript
            .status
            .tool_summaries
            .get(&block_id)
            .is_some_and(|summary| summary.completed == summary.total);
        if finished {
            if self.transcript.status.prompt_tool_summary == Some(block_id)
                && self.transcript.status.prompt_tool_summary_active
            {
                self.update_tool_summary_block(block_id);
                return;
            }
            let Some(summary) = self.transcript.status.tool_summaries.remove(&block_id) else {
                return;
            };
            self.resources.handle.remove_block(block_id);
            let new_block_id = self
                .resources
                .handle
                .print_output("tool-summary", self.render_summary_block(&summary));
            self.transcript
                .status
                .tool_summaries
                .insert(new_block_id, summary);
        } else {
            self.update_tool_summary_block(block_id);
        }
    }

    fn rerender_visible_for_current_settings(&mut self) {
        use tau_themes::names;
        self.rerender_message_history();
        for entry in &self.transcript.history.tool_history {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_tool_history_block(&entry.display),
            );
        }
        for entry in &self.transcript.history.diff_blocks {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_diff_history_block(&entry.display, &entry.diff),
            );
        }
        for (block_id, summary) in &self.transcript.status.tool_summaries {
            self.resources
                .handle
                .set_block(*block_id, self.render_summary_block(summary));
        }
        for entry in &self.transcript.history.thinking_history {
            let display = if self.presentation.verbose_mode && self.presentation.show_thinking {
                entry.text.as_str()
            } else {
                ""
            };
            self.resources.handle.set_block(
                entry.block_id,
                markdown_block_with_osc8(
                    &self.resources.theme,
                    names::AGENT_THINKING,
                    display,
                    self.presentation.osc8_links,
                ),
            );
        }
        for entry in &self.transcript.history.turn_stats_history {
            let block = self.render_turn_stats_entry(entry);
            self.resources.handle.set_block(entry.block_id, block);
        }
        for entry in &self.transcript.history.notice_history {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_harness_notice_block(&entry.notice),
            );
        }
        for entry in &self.transcript.history.diagnostic_history {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_diagnostic_projection(entry.level, &entry.projection),
            );
        }
        for entry in &self.transcript.history.internal_prompt_history {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_internal_prompt_projection(&entry.projection),
            );
        }
        self.rerender_live_thinking();
        self.rerender_live_response_stat_indicators();
        let live_tool_blocks = self
            .transcript
            .runtime
            .tool_calls
            .values()
            .filter_map(|state| {
                Some((
                    state.block_id?,
                    state
                        .live_display
                        .as_ref()
                        .map(|display| self.render_live_tool_block(display)),
                ))
            })
            .collect::<Vec<_>>();
        for (block_id, block) in live_tool_blocks {
            self.resources
                .handle
                .set_block(block_id, block.unwrap_or_else(Self::empty_block));
        }
    }

    /// Reprojects retained in-flight reasoning in either top-level mode.
    fn rerender_live_thinking(&mut self) {
        use tau_themes::names;

        let existing = self
            .transcript
            .runtime
            .prompts
            .values_mut()
            .filter_map(|state| {
                let block_id = state.thinking_block_id?;
                let block = if self.presentation.verbose_mode && self.presentation.show_thinking {
                    markdown_streaming_block_with_osc8(
                        &self.resources.theme,
                        names::AGENT_THINKING,
                        state.thinking_text.as_deref().unwrap_or_default(),
                        &mut state.thinking_markdown_cache,
                        MarkdownStreamUpdate::Append,
                        self.presentation.osc8_links,
                    )
                } else {
                    markdown_block_with_osc8(
                        &self.resources.theme,
                        names::AGENT_THINKING,
                        "",
                        self.presentation.osc8_links,
                    )
                };
                Some((block_id, block))
            })
            .collect::<Vec<_>>();
        for (block_id, block) in existing {
            self.resources.handle.set_block(block_id, block);
        }

        if !self.presentation.verbose_mode || !self.presentation.show_thinking {
            return;
        }
        let missing = self
            .transcript
            .runtime
            .prompts
            .iter()
            .filter(|(_, state)| state.thinking_block_id.is_none())
            .filter_map(|(prompt_id, state)| {
                state
                    .thinking_text
                    .as_deref()
                    .filter(|text| !text.is_empty())
                    .map(|_| prompt_id.clone())
            })
            .collect::<Vec<_>>();
        for prompt_id in missing {
            self.update_live_thinking_block(&prompt_id, MarkdownStreamUpdate::Replace);
        }
    }

    /// Reprojects in-progress response-stat suffixes for the current transcript
    /// mode without disturbing live assistant response or status text.
    fn rerender_live_response_stat_indicators(&mut self) {
        use tau_themes::names;

        let verbose_mode = self.presentation.verbose_mode;
        let updates = self
            .transcript
            .runtime
            .prompts
            .values()
            .filter(|state| state.live_response_is_pending_indicator)
            .filter_map(|state| {
                Some((
                    state.response_block_id?,
                    response_stats_indicator_for_prompt(state, verbose_mode),
                ))
            })
            .collect::<Vec<_>>();
        for (block_id, suffix) in updates {
            self.resources.handle.set_block(
                block_id,
                streaming_block_with_indicator_suffix(
                    &self.resources.theme,
                    names::AGENT_PENDING,
                    STREAMING_AGENT_RESPONSE_PREFIX.trim_end(),
                    suffix,
                ),
            );
        }
    }

    fn set_show_messages(&mut self, show_messages: tau_config::settings::ShowMessages) {
        if self.presentation.show_messages == show_messages {
            return;
        }
        self.presentation.show_messages = show_messages;
        self.rerender_message_history();
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Reproject typed harness-internal prompt facts without changing model
    /// state.
    fn set_show_internal_prompts(&mut self, enabled: bool) {
        if self.presentation.show_internal_prompts == enabled {
            return;
        }
        self.presentation.show_internal_prompts = enabled;
        for entry in &self.transcript.history.internal_prompt_history {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_internal_prompt_projection(&entry.projection),
            );
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    fn set_notice_level(&mut self, notice_level: tau_proto::NoticeLevel) {
        if self.presentation.notice_level == notice_level {
            return;
        }
        self.presentation.notice_level = notice_level;
        self.rerender_visible_for_current_settings();
        self.save_cli_state();
    }

    fn set_show_prompt_scroll_indicator(&mut self, enabled: bool) {
        if self.presentation.show_prompt_scroll_indicator == enabled {
            return;
        }
        self.presentation.show_prompt_scroll_indicator = enabled;
        self.resources.handle.set_prompt_scroll_indicator(enabled);
        self.resources.handle.redraw();
        self.save_cli_state();
    }

    /// Applies the UI-owned threshold and override policy from
    /// `SPEC-tau-cli-notice-filtering`.
    fn notice_visible(
        &self,
        purpose: tau_proto::NoticePurpose,
        level: tau_proto::NoticeLevel,
    ) -> bool {
        level == tau_proto::NoticeLevel::Critical
            || matches!(
                purpose,
                tau_proto::NoticePurpose::Response | tau_proto::NoticePurpose::Alert
            )
            || (self.presentation.verbose_mode && level.visible_at(self.presentation.notice_level))
    }

    /// Renders retained harness notices according to the local transcript mode.
    ///
    /// Compact mode shows responses, alerts, and critical notices while hiding
    /// other diagnostics. The original notice stays in
    /// [`Self::transcript.history.notice_history`] so verbose mode can restore
    /// it in place.
    fn render_harness_notice_block(
        &self,
        notice: &tau_proto::HarnessNotice,
    ) -> tau_cli_term::StyledBlock {
        if self.notice_visible(notice.purpose, notice.level) {
            render_harness_notice(&self.resources.theme, notice)
        } else {
            Self::empty_block()
        }
    }

    /// Adds one harness notice to the retained transcript.
    fn retain_harness_notice(&mut self, label: &'static str, notice: tau_proto::HarnessNotice) {
        let block_id = self
            .resources
            .handle
            .print_output(label, self.render_harness_notice_block(&notice));
        self.transcript
            .history
            .notice_history
            .push(NoticeBlockEntry { block_id, notice });
    }

    /// Retains one typed lifecycle diagnostic for compact/verbose reprojection.
    fn retain_diagnostic_block(
        &mut self,
        label: &'static str,
        level: tau_proto::NoticeLevel,
        projection: DiagnosticProjection,
    ) {
        let rendered = self.render_diagnostic_projection(level, &projection);
        let block_id = self.resources.handle.print_output(label, rendered);
        self.transcript
            .history
            .diagnostic_history
            .push(DiagnosticBlockEntry {
                block_id,
                level,
                projection,
            });
    }

    /// Renders one typed lifecycle diagnostic with current theme and
    /// visibility.
    fn render_diagnostic_projection(
        &self,
        level: tau_proto::NoticeLevel,
        projection: &DiagnosticProjection,
    ) -> tau_cli_term::StyledBlock {
        if !self.notice_visible(tau_proto::NoticePurpose::Diagnostic, level) {
            return Self::empty_block();
        }
        match projection {
            DiagnosticProjection::ExtensionStatus {
                extension_name,
                status,
            } => extension_status_block(
                &self.resources.theme,
                extension_name.as_str(),
                status.as_str(),
            ),
            DiagnosticProjection::UiDir { path } => ui_dir_block(&self.resources.theme, path),
            DiagnosticProjection::SessionDir { event } => session_status_block(
                &self.resources.theme,
                &event.path,
                "/",
                event.status.as_str(),
            ),
            DiagnosticProjection::ConfigProfile { selection } => {
                config_profile_selection_block(&self.resources.theme, selection)
            }
            DiagnosticProjection::AgentContextInitialized {
                event,
                unadvertised_count,
            } => agent_context_initialized_block(&self.resources.theme, event, *unadvertised_count),
            DiagnosticProjection::ExtensionContextReady { agent_id } => {
                crate::tool_render::agent_context_ready_block(&self.resources.theme, agent_id)
            }
        }
    }

    fn set_show_tools(&mut self, show_tools: tau_config::settings::ShowTools) {
        if self.presentation.show_tools == show_tools {
            return;
        }
        self.presentation.show_tools = show_tools;
        for entry in &self.transcript.history.tool_history {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_tool_history_block(&entry.display),
            );
        }
        for entry in &self.transcript.history.diff_blocks {
            self.resources.handle.set_block(
                entry.block_id,
                self.render_diff_history_block(&entry.display, &entry.diff),
            );
        }
        for (block_id, summary) in &self.transcript.status.tool_summaries {
            self.resources
                .handle
                .set_block(*block_id, self.render_summary_block(summary));
        }
        let freeze_multiline_payloads = self.freeze_multiline_live_payloads();
        let mut live_updates = Vec::new();
        for state in self.transcript.runtime.tool_calls.values_mut() {
            if let Some(block_id) = state.block_id {
                let display = if let Some(display) = state.live_display.as_ref() {
                    let mut display = display.clone();
                    let duration = Self::live_tool_duration(state);
                    Self::normalize_live_tool_duration(
                        freeze_multiline_payloads,
                        &mut display,
                        duration,
                        state.effective_shell_timeout,
                    );
                    state.live_display = Some(display.clone());
                    Some(display)
                } else {
                    None
                };
                live_updates.push((block_id, display));
            }
        }
        for (block_id, display) in live_updates {
            let block = display
                .as_ref()
                .map(|display| self.render_live_tool_block(display))
                .unwrap_or_else(Self::empty_block);
            self.resources.handle.set_block(block_id, block);
        }
        for state in self.transcript.runtime.tool_calls.values() {
            if let Some(block_id) = state.summary_block_id
                && let Some(summary) = self.transcript.status.tool_summaries.get(&block_id)
            {
                self.resources
                    .handle
                    .set_block(block_id, self.render_summary_block(summary));
            }
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Toggles the reversible process-local transcript presentation projection.
    pub(crate) fn toggle_verbose_mode(&mut self) {
        self.presentation.verbose_mode = !self.presentation.verbose_mode;
        self.rerender_visible_for_current_settings();
        self.invalidate_for_retroactive_toggle();
    }

    fn build_model_status_block(&mut self) -> tau_cli_term::StyledBlock {
        use tau_cli_term::resolve::convert_color;
        use tau_cli_term::{PriorityLine, PriorityLineAlignment, StyledBlock};
        use tau_themes::{StyleName, names};

        let mut line = PriorityLine::new();
        let left = PriorityLineAlignment::Left;
        let right = PriorityLineAlignment::Right;

        match (
            self.selection.current_agent_id.as_ref(),
            self.role.current_role.as_deref(),
            self.role.current_model.as_ref(),
        ) {
            (Some(agent_id), _, _) => {
                let (phase, title) = self.selected_agent_work_status(agent_id);
                line.push(
                    StatusElement::Identity.priority(),
                    left,
                    status_chip(
                        &self.resources.theme,
                        names::STATUS_ROLE,
                        format!(
                            "{phase}{} @{agent_id}",
                            crate::list_agents::turn_activity_symbol(
                                self.selected_agent_turn_activity(agent_id),
                            ),
                        ),
                    ),
                );
                if let Some(title) = title {
                    line.push(
                        StatusElement::WorkTitle.priority(),
                        left,
                        status_chip(&self.resources.theme, names::STATUS_ROLE, title),
                    );
                }
                if let Some(description) = self.agent_status_description(agent_id) {
                    line.push(
                        StatusElement::Description.priority(),
                        left,
                        status_chip(&self.resources.theme, names::STATUS_ROLE, description),
                    );
                }
                if let Some(watched_by) = self.watched_by_status(agent_id) {
                    line.push(
                        StatusElement::Watchers.priority(),
                        left,
                        status_chip(&self.resources.theme, names::MODEL_STATUS, watched_by),
                    );
                }
            }
            (None, Some(role), _) => line.push(
                StatusElement::Identity.priority(),
                left,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_ROLE,
                    format!("+{role}"),
                ),
            ),
            (None, None, Some(model)) => line.push(
                StatusElement::Identity.priority(),
                left,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_MODEL,
                    format!("={model}"),
                ),
            ),
            (None, None, None) if self.session.current_session_id.is_none() => line.push(
                StatusElement::Identity.priority(),
                left,
                status_chip(
                    &self.resources.theme,
                    names::MODEL_STATUS,
                    "no role selected",
                ),
            ),
            (None, None, None) => {}
        }
        let show_effort = self.role.baseline_params.map_or_else(
            || {
                self.role_default_effort().map_or(
                    self.role.model_params.effort != Default::default(),
                    |default| self.role.model_params.effort != default,
                )
            },
            |default| self.role.model_params.effort != default.effort,
        );
        if show_effort {
            line.push(
                StatusElement::ModelAdjustment.priority(),
                left,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_EFFORT,
                    format!("^{}", self.role.model_params.effort),
                ),
            );
        }
        let show_verbosity = self.role.baseline_params.map_or_else(
            || {
                self.role_default_verbosity()
                    .map_or(!self.role.model_params.verbosity.is_default(), |default| {
                        self.role.model_params.verbosity != default
                    })
            },
            |default| self.role.model_params.verbosity != default.verbosity,
        );
        if show_verbosity {
            line.push(
                StatusElement::ModelAdjustment.priority(),
                left,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_VERBOSITY,
                    format!("~{}", self.role.model_params.verbosity.as_str()),
                ),
            );
        }
        let show_service_tier = self
            .role
            .baseline_params
            .map_or(self.role.model_params.service_tier.is_some(), |default| {
                self.role.model_params.service_tier != default.service_tier
            });
        if show_service_tier {
            let service_tier = self
                .role
                .model_params
                .service_tier
                .map(|tier| tier.as_str())
                .unwrap_or("off");
            line.push(
                StatusElement::ModelAdjustment.priority(),
                left,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_SERVICE_TIER,
                    format!("!{service_tier}"),
                ),
            );
        }
        if let Some(tools) = self.main_tools_status_chip() {
            line.push(
                StatusElement::Tools.priority(),
                right,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_TOOLS,
                    format!("%{tools}"),
                ),
            );
        }
        let active_agents = self.active_side_agent_count();
        if 0 < active_agents {
            line.push(
                StatusElement::ActiveAgents.priority(),
                right,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_AGENTS,
                    format!("@{active_agents}"),
                ),
            );
        }
        if let Some(context) = self.context_status_chip() {
            line.push(
                StatusElement::Context.priority(),
                right,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_CONTEXT,
                    format!("#{context}"),
                ),
            );
        }
        if let Some(agent_id) = self.selection.current_agent_id.as_ref() {
            let costs = self.watches.agent_stats.get(agent_id).map(|stats| {
                AgentCostSnapshot::new(
                    stats.estimated_api_cost,
                    stats.creator_subtree_estimated_api_cost,
                )
            });
            line.push(
                StatusElement::EstimatedCost.priority(),
                right,
                status_chip(
                    &self.resources.theme,
                    names::STATUS_CONTEXT,
                    crate::estimated_cost::format_snapshot(costs),
                ),
            );
        }
        if self.presentation.show_ui_io {
            line.push(
                StatusElement::UiIoDebug.priority(),
                right,
                status_chip(
                    &self.resources.theme,
                    ui_io_status_style(self.presentation.ui_io_stats),
                    format!(
                        "io ↑{} ↓{}",
                        format_ui_io_rate(self.presentation.ui_io_stats.uplink_max_bytes_per_sec),
                        format_ui_io_rate(self.presentation.ui_io_stats.downlink_max_bytes_per_sec)
                    ),
                ),
            );
        }

        let full_render_count = self.resources.handle.full_render_count();
        if self.presentation.last_full_render_count < full_render_count {
            self.presentation.last_full_render_count = full_render_count;
            self.presentation.last_full_render_at = Some(Instant::now());
        }
        let show_redraw_counter = self.presentation.redraw_counter
            && self
                .presentation
                .last_full_render_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(5 * 60));
        if show_redraw_counter {
            line.push(
                StatusElement::RedrawDebug.priority(),
                right,
                status_chip(
                    &self.resources.theme,
                    names::REDRAW_COUNTER,
                    full_render_count.to_string(),
                ),
            );
        }

        let quota_model = self.selection.current_agent_id.as_ref().map_or_else(
            || self.role.current_model.clone(),
            |agent_id| self.watches.agent_models.get(agent_id).cloned(),
        );
        let quota = quota_model.and_then(|model| {
            self.role
                .quota_pacing
                .classify(&model, tau_proto::UnixMillis::new(unix_time_millis()))
        });
        if let Some(quota) = quota {
            let style = match quota {
                path_crate_provider_quota::QuotaPacing::FarUnder => names::STATUS_QUOTA_UNDER,
                path_crate_provider_quota::QuotaPacing::Aligned => names::STATUS_QUOTA_ALIGNED,
                path_crate_provider_quota::QuotaPacing::Over => names::STATUS_QUOTA_OVER,
                path_crate_provider_quota::QuotaPacing::Danger => names::STATUS_QUOTA_DANGER,
                path_crate_provider_quota::QuotaPacing::Unknown => names::STATUS_QUOTA_UNKNOWN,
            };
            line.push(
                StatusElement::WeeklyQuota.priority(),
                right,
                status_chip(&self.resources.theme, style, quota.chip()),
            );
        }
        if let Some(timer) = &self.activity.tool_timer {
            timer.set_quota_active(quota.is_some());
        }

        let bg = self
            .resources
            .theme
            .resolve_style(&StyleName::new(names::MODEL_STATUS))
            .bg;
        let mut block = StyledBlock::new("").priority_line(line);
        if let Some(bg) = bg {
            block = block.bg(convert_color(bg));
        }
        block
    }

    /// Builds and publishes the current terminal status projection.
    fn render_model_status(&mut self) {
        let block = self.build_model_status_block();
        self.publish_model_status_block(block);
    }

    /// Publishes one already-built status projection.
    fn publish_model_status_block(&mut self, block: tau_cli_term::StyledBlock) {
        match self.transcript.runtime.model_status_block {
            Some(bid) => self.resources.handle.set_block(bid, block),
            None => {
                let bid = self.resources.handle.new_block("model-status", block);
                self.resources.handle.push_below(bid);
                self.transcript.runtime.model_status_block = Some(bid);
            }
        }
        self.resources.handle.redraw();
    }

    /// Returns detailed turn activity with a live-prompt fallback before stats.
    fn selected_agent_turn_activity(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> tau_proto::AgentTurnActivity {
        self.watches.agent_stats.get(agent_id).map_or_else(
            || {
                if self.transcript.status.main_agent_turn_active
                    || self.watches.active_agent_prompts.contains_key(agent_id)
                {
                    tau_proto::AgentTurnActivity::Responding
                } else {
                    tau_proto::AgentTurnActivity::Idle
                }
            },
            |stats| stats.turn_activity,
        )
    }

    /// Returns the selected-agent work phase and escaped task title.
    fn selected_agent_work_status(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> (&'static str, Option<String>) {
        let Some(status) = self
            .watches
            .agent_stats
            .get(agent_id)
            .map(|stats| &stats.work_status)
        else {
            return (crate::list_agents::work_status_symbol(None), None);
        };
        (
            crate::list_agents::work_status_symbol(Some(status.phase())),
            status.title().map(tau_proto::visible_escape_metadata),
        )
    }

    fn role_default_effort(&self) -> Option<tau_proto::ReasoningSelection> {
        let role = self.role.current_role.as_deref()?;
        self.role
            .role_defaults
            .get(role)?
            .effort
            .as_deref()?
            .parse::<tau_proto::ReasoningIntent>()
            .ok()
            .map(|requested| tau_proto::ReasoningSelection {
                requested,
                effective: tau_proto::EffectiveReasoningEffort::ProviderDefault(None),
            })
    }

    fn role_default_verbosity(&self) -> Option<tau_proto::Verbosity> {
        let role = self.role.current_role.as_deref()?;
        self.role
            .role_defaults
            .get(role)?
            .verbosity
            .as_deref()?
            .parse()
            .ok()
    }

    /// Returns the session-wide number of active watched side agents.
    fn active_side_agent_count(&self) -> usize {
        let mut watched = HashSet::new();
        for watched_agent_ids in self.watches.watched_agents.values() {
            for agent_id in watched_agent_ids {
                watched.insert(agent_id);
            }
        }
        let projection = self.watch_activity_projection();
        let prompt_only = self
            .watches
            .active_agent_prompts
            .iter()
            .filter(|(agent_id, prompts)| {
                !prompts.is_empty()
                    && !watched.contains(agent_id)
                    && self.selection.current_agent_id.as_ref() != Some(agent_id)
            });
        projection
            .effective_targets()
            .iter()
            .filter(|agent_id| self.selection.current_agent_id.as_ref() != Some(agent_id))
            .count()
            + prompt_only.count()
    }

    fn main_tools_status_chip(&self) -> Option<String> {
        ((self.transcript.status.main_tools_visible
            || !self.transcript.status.main_backgrounded_tools.is_empty())
            && self.transcript.status.main_tools_total != 0)
            .then(|| {
                format!(
                    "{}/{}",
                    self.transcript.status.main_tools_completed,
                    self.transcript.status.main_tools_total
                )
            })
    }

    fn record_main_tool_completed(&mut self) {
        if self.transcript.status.main_tools_completed < self.transcript.status.main_tools_total {
            self.transcript.status.main_tools_completed += 1;
        }
    }

    fn set_main_tools_visible(&mut self, visible: bool) {
        if self.transcript.status.main_tools_visible == visible {
            return;
        }
        self.transcript.status.main_tools_visible = visible;
        if !self.final_publication_in_progress
            && self.transcript.runtime.model_status_block.is_some()
        {
            self.render_model_status();
        }
    }

    fn set_main_agent_turn_active(&mut self, active: bool) {
        self.transcript.status.main_agent_turn_active = active;
        self.set_main_tools_visible(active && self.transcript.status.main_tools_total != 0);
    }

    fn clear_main_agent_turn_active_everywhere(&mut self) {
        self.set_main_agent_turn_active(false);
        for state in self.selection.agents_ui_state.values_mut() {
            state.transcript.status.main_agent_turn_active = false;
            state.transcript.status.main_tools_visible = false;
        }
    }

    fn has_live_main_delegate_tool_call(&self) -> bool {
        self.transcript
            .runtime
            .tool_calls
            .values()
            .any(|state| state.is_main_delegate && !state.is_sub_agent)
    }

    fn sync_agent_activity_for_lifecycle(&mut self, prepared: &PreparedRendererEvent<'_>) {
        let event = prepared.event();
        match event {
            Event::UiPromptSubmitted(_) => self
                .transcript
                .status
                .agent_activity
                .mark_optimistic_submission(),
            Event::AgentCompactionTriggered(triggered) => {
                if triggered.originator.is_user() {
                    self.mark_agent_live(triggered.agent_id.clone());
                } else {
                    self.remember_agent(triggered.agent_id.clone());
                }
            }
            Event::AgentPromptCreated(prompt) => {
                self.transcript
                    .status
                    .agent_activity
                    .start_prompt(&prompt.agent_prompt_id);
            }
            Event::AgentPromptStarted(prompt) => {
                self.transcript
                    .status
                    .agent_activity
                    .start_prompt(&prompt.agent_prompt_id);
            }
            Event::ProviderPromptSubmitted(submitted) => {
                self.transcript
                    .status
                    .agent_activity
                    .start_prompt(&submitted.agent_prompt_id);
            }
            Event::ProviderResponseUpdated(update) => {
                if !self.is_stale_terminal_stats_only_update(update) {
                    self.transcript
                        .status
                        .agent_activity
                        .start_prompt(&update.agent_prompt_id);
                }
            }
            Event::ProviderResponseFinished(finished) => {
                let (_, calls) = prepared
                    .finished()
                    .expect("provider terminal preserves projection");
                self.transcript
                    .status
                    .agent_activity
                    .finish_prompt_if_active_with_tool_call_ids(
                        &finished.agent_prompt_id,
                        calls.call_ids(),
                    );
            }
            Event::AgentCompacted(compacted) => {
                if let Some(prompt_id) = &compacted.compact_prompt_id {
                    self.transcript
                        .status
                        .agent_activity
                        .finish_prompt(prompt_id, &[]);
                }
            }
            Event::AgentPromptTerminated(terminated) => {
                self.transcript
                    .status
                    .agent_activity
                    .finish_prompt(&terminated.agent_prompt_id, &[]);
            }
            Event::ToolRequest(_) => {}
            Event::ToolStarted(invoke) => self
                .transcript
                .status
                .agent_activity
                .start_tool(&invoke.call_id),
            Event::ToolRejected(rejected) => {
                self.transcript
                    .status
                    .agent_activity
                    .finish_tool(&rejected.call_id);
            }
            Event::ToolResultDisplay(result) => {
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                    self.transcript
                        .status
                        .agent_activity
                        .background_tool(&result.call_id);
                } else {
                    self.transcript
                        .status
                        .agent_activity
                        .finish_tool(&result.call_id);
                }
            }
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                    self.transcript
                        .status
                        .agent_activity
                        .background_tool(&result.call_id);
                } else {
                    self.transcript
                        .status
                        .agent_activity
                        .finish_tool(&result.call_id);
                }
            }
            Event::ToolError(error) => {
                self.transcript
                    .status
                    .agent_activity
                    .finish_tool(&error.call_id);
            }
            Event::ToolBackgroundResultDisplay(result) => {
                self.transcript
                    .status
                    .agent_activity
                    .finish_background_tool(&result.call_id);
            }
            Event::ToolBackgroundResult(result) => {
                self.transcript
                    .status
                    .agent_activity
                    .finish_background_tool(&result.call_id);
            }
            Event::ToolBackgroundError(error) => {
                self.transcript
                    .status
                    .agent_activity
                    .finish_background_tool(&error.call_id);
            }
            Event::ToolCancelled(cancelled) => {
                self.transcript
                    .status
                    .agent_activity
                    .finish_background_tool(&cancelled.call_id);
            }
            Event::UiCancelPrompt(_) => self
                .transcript
                .status
                .agent_activity
                .clear_optimistic_submissions(),
            Event::SessionShutdown(_) => {
                self.clear_accepted_submission_indicators_everywhere();
                self.clear_main_agent_turn_active_everywhere();
                self.transcript.status.agent_activity.clear();
            }
            _ => {}
        }
    }

    fn sync_main_tools_visibility_for_prompt_lifecycle(
        &mut self,
        prepared: &PreparedRendererEvent<'_>,
    ) {
        let event = prepared.event();
        match event {
            Event::AgentPromptSubmitted(prompt)
                if prompt.inference_activation && prompt.originator.is_user() =>
            {
                self.set_main_agent_turn_active(true);
                self.show_accepted_submission_indicator();
            }
            Event::AgentPromptCreated(prompt) => {
                if prompt.originator.is_user() || !self.has_live_main_delegate_tool_call() {
                    self.set_main_agent_turn_active(prompt.originator.is_user());
                }
            }
            Event::AgentPromptStarted(prompt) => {
                if prompt.originator.is_user() || !self.has_live_main_delegate_tool_call() {
                    self.set_main_agent_turn_active(prompt.originator.is_user());
                }
            }
            Event::ProviderPromptSubmitted(submitted) => {
                if submitted.originator.is_user() || !self.has_live_main_delegate_tool_call() {
                    self.set_main_agent_turn_active(submitted.originator.is_user());
                }
            }
            Event::ProviderResponseUpdated(update) => {
                if !self.is_stale_terminal_stats_only_update(update)
                    && (update.originator.is_user() || !self.has_live_main_delegate_tool_call())
                {
                    self.set_main_agent_turn_active(update.originator.is_user());
                }
            }
            Event::ProviderResponseFinished(finished) if finished.originator.is_user() => {
                self.clear_accepted_submission_indicator();
                if prepared
                    .finished()
                    .expect("provider terminal preserves projection")
                    .1
                    .is_empty()
                {
                    self.clear_main_agent_turn_active_everywhere();
                }
            }
            Event::ProviderResponseFinished(finished)
                if !finished.originator.is_user() && !self.has_live_main_delegate_tool_call() =>
            {
                self.set_main_agent_turn_active(false);
            }
            Event::AgentPromptTerminated(terminated) if terminated.originator.is_user() => {
                self.clear_accepted_submission_indicator();
                if !self.transcript.status.agent_activity.has_active_prompts() {
                    self.set_main_agent_turn_active(false);
                }
            }
            Event::AgentPromptTerminated(terminated)
                if !terminated.originator.is_user() && !self.has_live_main_delegate_tool_call() =>
            {
                self.set_main_agent_turn_active(false);
            }
            Event::UiCancelPrompt(_) => {
                self.clear_accepted_submission_indicator();
                self.set_main_agent_turn_active(false);
            }
            _ => {}
        }
    }

    fn show_accepted_submission_indicator(&mut self) {
        if self.transcript.runtime.accepted_submission_block.is_some() {
            return;
        }
        let block = streaming_block(
            &self.resources.theme,
            tau_themes::names::AGENT_PENDING,
            STREAMING_AGENT_RESPONSE_PREFIX.trim_end(),
        );
        let block_id = self
            .resources
            .handle
            .new_block("agent-turn-accepted".to_owned(), block);
        self.resources.handle.push_above_active(block_id);
        self.transcript.runtime.accepted_submission_block = Some(block_id);
        self.resources.handle.redraw();
    }

    fn clear_accepted_submission_indicator(&mut self) {
        if let Some(block_id) = self.transcript.runtime.accepted_submission_block.take() {
            self.resources.handle.remove_block(block_id);
            self.resources.handle.redraw();
        }
    }

    fn clear_accepted_submission_indicators_everywhere(&mut self) {
        self.clear_accepted_submission_indicator();
        for state in self.selection.agents_ui_state.values_mut() {
            if let Some(block_id) = state.transcript.runtime.accepted_submission_block.take() {
                state.output.remove_block(block_id);
            }
            state.transcript.status.main_agent_turn_active = false;
            state.transcript.status.agent_activity.clear();
        }
    }

    fn reset_main_tool_usage(&mut self) {
        if self.transcript.status.main_tools_completed == 0
            && self.transcript.status.main_tools_total == 0
            && !self.transcript.status.main_tools_visible
            && self.transcript.status.main_backgrounded_tools.is_empty()
        {
            return;
        }
        if self.transcript.status.main_backgrounded_tools.is_empty() {
            self.transcript.status.main_tools_completed = 0;
            self.transcript.status.main_tools_total = 0;
            self.transcript.status.main_tools_visible = false;
        } else {
            self.transcript.status.main_tools_visible = true;
        }
        if self.transcript.runtime.model_status_block.is_some() {
            self.render_model_status();
        }
    }

    fn context_status_chip(&self) -> Option<String> {
        match (
            self.transcript.status.current_context_percent,
            self.transcript.status.current_context_input_tokens,
            self.transcript.status.current_context_window,
        ) {
            (_, Some(input), Some(window)) => Some(format!(
                "{}/{}",
                format_context_token_count(input),
                format_context_token_count(window)
            )),
            (Some(percent), _, Some(window)) => {
                Some(format!("{percent}%/{}", format_context_token_count(window)))
            }
            (Some(percent), _, None) => Some(format!("{percent}%")),
            (None, Some(input), None) => Some(format_context_token_count(input)),
            (None, None, Some(window)) => Some(format!("-/{}", format_context_token_count(window))),
            (None, None, None) => None,
        }
    }

    /// Parses a submitted prompt while borrowing its raw text.
    ///
    /// The caller retains or moves the raw string only when later queue
    /// matching requires it; the Markdown renderer owns the resulting
    /// styled text.
    fn submitted_prompt_block(
        &self,
        body_name: &str,
        body_text: &str,
    ) -> tau_cli_term::StyledBlock {
        #[cfg(test)]
        observe_submitted_prompt_parser_input(body_text);
        markdown_prompt_block_with_osc8(
            &self.resources.theme,
            body_name,
            format!("{} ", self.resources.submitted_prompt_symbol),
            body_text,
            self.presentation.osc8_links,
        )
    }

    /// Renders a non-Markdown semantic transcript row with its fixed marker.
    fn marked_plain_block(
        &self,
        body_name: &str,
        marker: &str,
        body_text: impl Into<String>,
    ) -> tau_cli_term::StyledBlock {
        use tau_cli_term::resolve::{convert_color, themed_text};
        use tau_themes::{SpanTree, StyleName, ThemedText, names};

        let mut themed = ThemedText::new();
        let body_style = themed.add_style(body_name);
        let marker_style = themed.add_style(names::PROMPT_MARKER_SUBMITTED);
        themed.push_tree(SpanTree::span(
            body_style,
            vec![
                SpanTree::span(marker_style, vec![SpanTree::text(marker)]),
                SpanTree::text(body_text.into()),
            ],
        ));

        let body_ts = self
            .resources
            .theme
            .resolve_style(&StyleName::new(body_name));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.resources.theme, &themed));
        if let Some(bg) = body_ts.bg {
            block = block.bg(convert_color(bg));
        }
        block
    }

    /// Renders one typed harness-internal notice through its dedicated theme
    /// classification.
    fn internal_notice_block(&self, text: impl Into<String>) -> tau_cli_term::StyledBlock {
        self.marked_plain_block(
            tau_themes::names::SYSTEM_INTERNAL_NOTICE,
            crate::transcript_markers::NOTICE,
            text,
        )
    }

    /// Renders one typed context-size alert.
    fn context_size_alert_block(&self, text: &str) -> tau_cli_term::StyledBlock {
        self.internal_notice_block(text)
    }

    /// Renders one typed timer wakeup from its semantic payload fields.
    fn timer_wakeup_block(&self, timer_id: &str, text: Option<&str>) -> tau_cli_term::StyledBlock {
        self.internal_notice_block(timer_wakeup_summary(timer_id, text))
    }

    /// Render a message-fact block while styling only its authenticated
    /// publisher code span, never interpreting untrusted heading metadata
    /// or body text.
    fn submitted_message_fact_block(&self, body_text: String) -> tau_cli_term::StyledBlock {
        use tau_cli_term::resolve::{convert_color, themed_text};
        use tau_themes::{SpanTree, StyleName, ThemedText, names};

        let mut themed = ThemedText::new();
        let body_name = names::SYSTEM_INFO;
        let body_style = themed.add_style(body_name);
        let marker_style = themed.add_style(names::PROMPT_MARKER_SUBMITTED);
        let code_style = themed.add_style(names::MARKDOWN_CODE);
        let body = if let Some(code_start) = body_text.find('`')
            && let Some(relative_code_end) = body_text[code_start + 1..].find('`')
        {
            let code_end = code_start + 1 + relative_code_end + 1;
            vec![
                SpanTree::text(&body_text[..code_start]),
                SpanTree::span(
                    code_style,
                    vec![SpanTree::text(&body_text[code_start..code_end])],
                ),
                SpanTree::text(&body_text[code_end..]),
            ]
        } else {
            vec![SpanTree::text(body_text)]
        };
        themed.push_tree(SpanTree::span(
            body_style,
            vec![
                SpanTree::span(
                    marker_style,
                    vec![SpanTree::text(crate::transcript_markers::MESSAGE)],
                ),
                SpanTree::span(body_style, body),
            ],
        ));

        let body_theme_style = self
            .resources
            .theme
            .resolve_style(&StyleName::new(body_name));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.resources.theme, &themed));
        if let Some(background) = body_theme_style.bg {
            block = block.bg(convert_color(background));
        }
        block
    }

    pub(crate) fn handle_disconnect(&mut self, reason: Option<String>) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        self.clear_accepted_submission_indicators_everywhere();
        self.clear_main_agent_turn_active_everywhere();
        self.transcript.status.agent_activity.clear();
        self.activity
            .agent_in_progress
            .store(false, Ordering::Relaxed);
        let mut summary_blocks = HashSet::new();
        for state in self.transcript.runtime.tool_calls.values() {
            if let Some(block_id) = state.block_id {
                self.resources.handle.remove_block(block_id);
            }
            if let Some(block_id) = state.summary_block_id {
                summary_blocks.insert(block_id);
            }
        }
        for block_id in summary_blocks {
            self.resources.handle.remove_block(block_id);
            self.transcript.status.tool_summaries.remove(&block_id);
            if self.transcript.status.prompt_tool_summary == Some(block_id) {
                self.transcript.status.prompt_tool_summary = None;
                self.transcript.status.prompt_tool_summary_active = false;
            }
        }
        if self.transcript.status.prompt_tool_summary_active {
            self.finish_prompt_tool_summary();
        }
        self.transcript.runtime.tool_calls.clear();
        if let Some(timer) = &self.activity.tool_timer {
            timer.clear_active();
        }
        let reason = reason.as_deref().unwrap_or("disconnected");
        self.resources.handle.print_output(
            "system-disconnect",
            themed_block(
                &self.resources.theme,
                names::SYSTEM_DISCONNECT,
                format!("{}{}", crate::transcript_markers::NOTICE, reason),
            ),
        );
    }

    #[cfg(test)]
    pub(crate) fn handle(&mut self, event: &Event) {
        self.handle_recorded_at(event, UnixMicros::now());
    }

    #[cfg(test)]
    pub(crate) fn handle_recorded_at(&mut self, event: &Event, recorded_at: UnixMicros) {
        self.handle_recorded_delivery(event, recorded_at, None, false, false);
    }

    /// Handles one socket delivery with its content-free frontend correlation.
    pub(crate) fn handle_socket_delivery(
        &mut self,
        event: &Event,
        recorded_at: UnixMicros,
        delivery_id: RendererDeliveryId,
    ) {
        self.handle_recorded_delivery(event, recorded_at, Some(delivery_id), false, false);
    }

    /// Handles one replay delivery without letting replay-derived selection
    /// replace newer explicit input intent.
    pub(crate) fn handle_replay_socket_delivery(
        &mut self,
        event: &Event,
        recorded_at: UnixMicros,
        delivery_id: RendererDeliveryId,
    ) {
        self.handle_recorded_delivery(event, recorded_at, Some(delivery_id), true, false);
    }

    /// Handles replay retained by an explicit cold attach. These rows can
    /// populate agent snapshots, but only the replay boundary may select one.
    pub(crate) fn handle_cold_attach_replay_socket_delivery(
        &mut self,
        event: &Event,
        recorded_at: UnixMicros,
        delivery_id: RendererDeliveryId,
    ) {
        self.handle_recorded_delivery(event, recorded_at, Some(delivery_id), false, true);
    }

    /// Selects the validated owner of a reconstructed pending start, then
    /// processes the start through the ordinary visible-agent path.
    #[cfg(test)]
    pub(crate) fn handle_reconstructed_tool_start_socket_delivery(
        &mut self,
        event: &Event,
        target_agent_id: &tau_proto::AgentId,
        recorded_at: UnixMicros,
        delivery_id: RendererDeliveryId,
    ) {
        let user_originated =
            matches!(event, Event::ToolStarted(started) if started.originator.is_user());
        let can_claim = user_originated
            && self.selection.current_agent_id.is_none()
            && self.selection.displayed_agent_id.is_none()
            && !self.selection.awaiting_new_agent_selection
            && self.can_select_target_from_empty(target_agent_id);
        if can_claim
            && let Some(intent_epoch) = self.claim_initial_selection_intent(target_agent_id)
        {
            self.apply_claimed_agent(target_agent_id.clone(), intent_epoch);
        }
        self.handle_replay_socket_delivery(event, recorded_at, delivery_id);
    }

    /// Selects an attach-time replay target only while the UI still has no
    /// explicit agent choice, then processes the replay boundary normally.
    pub(crate) fn handle_attach_agent_selection_socket_delivery(
        &mut self,
        event: &Event,
        target_agent_id: &tau_proto::AgentId,
        recorded_at: UnixMicros,
        delivery_id: RendererDeliveryId,
    ) {
        let claimed_epoch = self.claim_initial_selection_intent(target_agent_id);
        if let Some(intent_epoch) = claimed_epoch {
            self.apply_claimed_agent(target_agent_id.clone(), intent_epoch);
        }
        self.handle_socket_delivery(event, recorded_at, delivery_id);
    }

    fn claim_initial_selection_intent(&self, target_agent_id: &tau_proto::AgentId) -> Option<u64> {
        self.selection
            .current_agent_state
            .lock()
            .ok()
            .and_then(|mut intent| {
                if intent.epoch == 0 && intent.selected_agent_id.is_none() {
                    intent.epoch = 1;
                    intent.selected_agent_id = Some(target_agent_id.clone());
                    Some(intent.epoch)
                } else {
                    None
                }
            })
    }

    fn handle_recorded_delivery(
        &mut self,
        event: &Event,
        recorded_at: UnixMicros,
        delivery_id: Option<RendererDeliveryId>,
        replay_selection_guard: bool,
        suppress_auto_selection: bool,
    ) {
        if self.session.session_binding_failed {
            return;
        }
        match event {
            Event::SessionStarted(started) => match self.session.current_session_id.as_ref() {
                Some(bound) if bound == &started.session_id => return,
                Some(bound) => {
                    tracing::error!(
                        expected_session_id = %bound,
                        received_session_id = %started.session_id,
                        "terminal renderer rejected conflicting immutable session"
                    );
                    self.session.session_binding_failed = true;
                    self.watches.agent_estimated_api_costs.clear();
                    self.session.session_token_usage = tau_proto::TokenUsageCounts::default();
                    self.clear_agent_display_names();
                    self.clear_accepted_submission_indicators_everywhere();
                    self.clear_main_agent_turn_active_everywhere();
                    self.transcript.status.agent_activity.clear();
                    return;
                }
                None => {}
            },
            Event::SessionShutdown(shutdown)
                if self.session.current_session_id.as_ref() != Some(&shutdown.session_id) =>
            {
                tracing::error!(
                    received_session_id = %shutdown.session_id,
                    "terminal renderer rejected shutdown outside immutable session"
                );
                self.session.session_binding_failed = true;
                return;
            }
            _ => {}
        }
        let prepared = PreparedRendererEvent::new(event);
        if let Some((_, calls)) = prepared.finished() {
            let work = calls.work();
            tracing::trace!(
                target: "tau_cli::frontend_progress",
                output_items_visited = work.output_items_visited,
                metadata_buffers_allocated = work.metadata_buffers_allocated,
                metadata_slots_reserved = work.metadata_slots_reserved,
                metadata_fields_cloned = work.metadata_fields_cloned,
                "projected terminal tool-call metadata"
            );
        }
        let observation = delivery_id
            .zip(presentation_fact(event))
            .filter(|_| self.resources.handle.presentation_observation_interest());
        if let Event::ProviderResponseFinished(finished) = event {
            let (_, calls) = prepared
                .finished()
                .expect("prepared provider terminal preserves projection");
            let selected = self.selection.displayed_agent_id.as_ref() == Some(&finished.agent_id)
                || self.selection.current_agent_id.as_ref() == Some(&finished.agent_id);
            if selected {
                let is_standalone = self
                    .transcript
                    .runtime
                    .prompts
                    .get(&finished.agent_prompt_id)
                    .is_some_and(|state| state.is_standalone_compaction);
                if is_standalone {
                    self.staged_finished_status =
                        Some(self.stage_standalone_finished_status_block(finished, calls));
                }
                self.staged_finished_response =
                    (!is_standalone).then(|| self.stage_finished_response(finished, calls));
                let handle = self.resources.handle.terminal_handle();
                self.final_publication_in_progress = true;
                handle.with_redraw_suppressed(|| {
                    self.handle_recorded_delivery_inner(
                        prepared,
                        recorded_at,
                        observation,
                        replay_selection_guard,
                        suppress_auto_selection,
                    );
                });
                self.final_publication_in_progress = false;
                // ast-grep-ignore: debug-assert-expression-must-not-mutate
                debug_assert!(self.staged_finished_response.is_none());
                // ast-grep-ignore: debug-assert-expression-must-not-mutate
                debug_assert!(self.staged_finished_status.is_none());
                return;
            }
            self.final_publication_in_progress = true;
            self.hidden_finalization_in_progress = true;
            self.handle_recorded_delivery_inner(
                prepared,
                recorded_at,
                observation,
                replay_selection_guard,
                suppress_auto_selection,
            );
            self.hidden_finalization_in_progress = false;
            self.final_publication_in_progress = false;
            return;
        }
        let invalidates_pending = observation.is_some_and(|(_, class)| class.invalidates_pending());
        if invalidates_pending {
            let handle = self.resources.handle.terminal_handle();
            handle.with_redraw_suppressed(|| {
                self.handle_recorded_delivery_inner(
                    prepared,
                    recorded_at,
                    observation,
                    replay_selection_guard,
                    suppress_auto_selection,
                );
            });
        } else {
            self.handle_recorded_delivery_inner(
                prepared,
                recorded_at,
                observation,
                replay_selection_guard,
                suppress_auto_selection,
            );
        }
    }

    /// Routes one delivery after any selected-final redraw cut has been
    /// entered.
    fn handle_recorded_delivery_inner(
        &mut self,
        prepared: PreparedRendererEvent<'_>,
        recorded_at: UnixMicros,
        observation: Option<(RendererDeliveryId, PresentationFactClass)>,
        replay_selection_guard: bool,
        suppress_auto_selection: bool,
    ) {
        self.resources
            .handle
            .begin_selected_delivery(observation.is_some());
        self.handle_recorded_delivery_routed(
            prepared,
            recorded_at,
            observation.map(|(delivery_id, _)| delivery_id),
            replay_selection_guard,
            suppress_auto_selection,
        );
        let Some((delivery_id, class)) = observation else {
            return;
        };
        if self.resources.handle.selected_delivery_mutated() {
            self.resources
                .handle
                .observe_presentation_mutation(delivery_id, class);
        }
    }

    /// Routes one delivery while retaining its process-local timing context.
    fn handle_recorded_delivery_routed(
        &mut self,
        prepared: PreparedRendererEvent<'_>,
        recorded_at: UnixMicros,
        delivery_id: Option<RendererDeliveryId>,
        replay_selection_guard: bool,
        suppress_auto_selection: bool,
    ) {
        let event = prepared.event();
        let event_name = event.name();
        tracing::trace!(
            target: "tau_cli::frontend_progress",
            delivery_id = delivery_id.map(RendererDeliveryId::get),
            %event_name,
            "renderer handler started"
        );
        let _progress = HandlerProgress {
            delivery_id,
            event_name,
            started_at: Instant::now(),
        };
        self.record_session_token_usage(event);
        let deferred_metadata_target = (!matches!(event, Event::HarnessAgentContextInitialized(_)))
            .then(|| self.agent_id_for_event(event))
            .flatten()
            .filter(|target_agent_id| {
                self.selection.current_agent_id.is_none()
                    && self.selection.displayed_agent_id.is_none()
                    && !self.selection.awaiting_new_agent_selection
                    && !self.event_selects_agent_from_empty(event, target_agent_id)
                    && self
                        .discovery
                        .pending_initial_discovery
                        .contains_key(target_agent_id)
            });
        if deferred_metadata_target.is_none() {
            self.learn_agent_metadata(&prepared);
        } else {
            self.learn_deferred_routing_metadata(&prepared);
        }
        if let Event::HarnessAgentContextInitialized(initialized) = event
            && self.selection.current_agent_id.is_none()
            && self.selection.displayed_agent_id.is_none()
            && !self.selection.awaiting_new_agent_selection
        {
            let key = (
                initialized.agent_id.clone(),
                initialized.agent_initialization_id.clone(),
            );
            if self.discovery.initialized_discovery_epochs.insert(key) {
                self.discovery
                    .pending_initial_discovery
                    .entry(initialized.agent_id.clone())
                    .or_default()
                    .push(prepared.deferred(recorded_at));
            }
            return;
        }
        let inter_agent_message = Self::is_inter_agent_message(event);
        self.project_agent_message_to_overview(event);
        if let Some(owner) = self.extension_lifecycle_owner(event) {
            self.handle_recorded_at_for_snapshot_owner(&prepared, recorded_at, owner);
            self.update_agent_in_progress();
            return;
        }
        if let Some(owner) = self.take_action_completion_owner(event) {
            self.handle_recorded_at_for_snapshot_owner(&prepared, recorded_at, owner);
            self.update_agent_in_progress();
            return;
        }
        if let Some(owner) = self.message_fact_snapshot_owner(event) {
            self.handle_recorded_at_for_snapshot_owner(&prepared, recorded_at, owner);
            self.update_agent_in_progress();
            return;
        }
        let target_agent_id = self.agent_id_for_event(event);
        if let Some(target_agent_id) = target_agent_id.as_ref() {
            tracing::trace!(
                target: "tau_cli::frontend_progress",
                agent_id = %target_agent_id,
                selected = self.selection.displayed_agent_id.as_ref() == Some(target_agent_id),
                "renderer target resolved"
            );
        }
        let Some(target_agent_id) = target_agent_id else {
            self.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
            self.update_agent_in_progress();
            return;
        };
        if deferred_metadata_target.as_ref() == Some(&target_agent_id)
            && let Some(events) = self
                .discovery
                .pending_initial_discovery
                .get_mut(&target_agent_id)
        {
            events.push(prepared.deferred(recorded_at));
            return;
        }
        if self.selection.current_agent_id.is_none() {
            if self.event_selects_agent_from_empty(event, &target_agent_id) {
                if suppress_auto_selection {
                    self.handle_recorded_at_for_hidden_agent(
                        &prepared,
                        recorded_at,
                        target_agent_id,
                    );
                    self.update_agent_in_progress();
                    return;
                }
                let claimed_epoch = replay_selection_guard
                    .then(|| self.claim_initial_selection_intent(&target_agent_id))
                    .flatten();
                if replay_selection_guard && claimed_epoch.is_none() {
                    self.handle_recorded_at_for_hidden_agent(
                        &prepared,
                        recorded_at,
                        target_agent_id,
                    );
                    self.update_agent_in_progress();
                    return;
                }
                if self.selection.displayed_agent_id.as_ref() != Some(&target_agent_id) {
                    self.show_agent_transcript(target_agent_id.clone());
                }
                self.selection.awaiting_new_agent_selection = false;
                self.set_current_agent_id(
                    Some(target_agent_id.clone()),
                    true,
                    !replay_selection_guard,
                );
                self.refresh_prompt_placeholder();
                self.render_model_status();
                self.flush_pending_initial_discovery();
                self.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
                self.update_agent_in_progress();
                return;
            }
            if matches!(
                event,
                Event::AgentMessageSent(_) | Event::AgentMessageReceived(_)
            ) && !inter_agent_message
                && self.agent_message_visible_on_empty_screen(&target_agent_id)
            {
                self.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
                self.update_agent_in_progress();
                return;
            }
            if !inter_agent_message
                && !Self::event_originator_is_extension(event)
                && !Self::event_has_explicit_ui_target(event)
                && !matches!(event, Event::HarnessAgentContextInitialized(_))
                && !self
                    .selection
                    .agents_ui_state
                    .contains_key(&target_agent_id)
            {
                self.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
                self.update_agent_in_progress();
                return;
            }
            self.handle_recorded_at_for_hidden_agent(&prepared, recorded_at, target_agent_id);
            self.update_agent_in_progress();
            return;
        }
        if self.selection.displayed_agent_id.as_ref() == Some(&target_agent_id) {
            self.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
            self.update_agent_in_progress();
            return;
        }

        self.handle_recorded_at_for_hidden_agent(&prepared, recorded_at, target_agent_id);
        self.update_agent_in_progress();
    }

    /// Folds a terminal provider response into the session-wide command total.
    ///
    /// This runs before transcript routing so an owning agent's hidden-state
    /// projection cannot cause the same durable occurrence to be counted twice.
    fn record_session_token_usage(&mut self, event: &Event) {
        let Event::ProviderResponseFinished(finished) = event else {
            return;
        };
        let Some(usage) = finished.usage.as_ref() else {
            return;
        };
        Self::add_finished_token_usage(&mut self.session.session_token_usage, usage);
    }

    /// Returns the flat token total exposed by `:session-stats`.
    pub(crate) fn session_token_stats_text(&self) -> String {
        let usage = &self.session.session_token_usage;
        format!(
            "session token totals: ↑{}/{} ↓{}",
            format_token_count(usage.cached_tokens),
            format_token_count(usage.sent_tokens),
            format_token_count(usage.received_tokens),
        )
    }

    /// Prints the flat session-wide provider token total.
    pub(crate) fn show_session_token_stats(&mut self) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        self.resources.handle.print_output(
            "session-stats",
            themed_block(
                &self.resources.theme,
                names::SYSTEM_INFO,
                format!(
                    "{}{}",
                    crate::transcript_markers::NOTICE,
                    self.session_token_stats_text()
                ),
            ),
        );
    }

    /// Copy one semantic inter-agent message into the no-agent overview.
    ///
    /// Sender and recipient projections retain their existing transcript
    /// routing. The overview deduplicates those projections by originating
    /// session and message id.
    fn project_agent_message_to_overview(&mut self, event: &Event) {
        let Some(message_id) = Self::overview_agent_message_id(event) else {
            return;
        };
        if !self
            .selection
            .overview_message_ids
            .insert((self.session.current_session_id.clone(), message_id.clone()))
        {
            return;
        }
        if self.selection.displayed_agent_id.is_none() {
            self.transcript.ownership.contains_overview_message = true;
            self.handle_agent_message_event(event);
            return;
        }

        self.update_hidden_no_agent_state(|this| {
            this.transcript.ownership.contains_overview_message = true;
            this.handle_agent_message_event(event);
        });
    }

    /// Return the stable id for a genuine message between agent endpoints.
    fn overview_agent_message_id(event: &Event) -> Option<&tau_proto::AgentMessageId> {
        match event {
            Event::AgentMessageSent(message)
                if matches!(
                    message.kind,
                    tau_proto::AgentMessageKind::Message
                        | tau_proto::AgentMessageKind::WatchResponse
                        | tau_proto::AgentMessageKind::WatchPrompt
                ) =>
            {
                Some(&message.message_id)
            }
            Event::AgentMessageReceived(message)
                if matches!(
                    message.kind,
                    tau_proto::AgentMessageKind::Message
                        | tau_proto::AgentMessageKind::WatchResponse
                        | tau_proto::AgentMessageKind::WatchPrompt
                ) =>
            {
                Some(&message.message_id)
            }
            _ => None,
        }
    }

    /// Return whether an event is an agent-to-agent projection that must route
    /// to its owning agent transcript instead of the no-agent fallback.
    fn is_inter_agent_message(event: &Event) -> bool {
        matches!(
            event,
            Event::AgentMessageSent(_) | Event::AgentMessageReceived(_)
        )
    }

    fn extension_lifecycle_owner(&self, event: &Event) -> Option<UiSnapshotOwner> {
        let instance_id = match event {
            Event::ExtensionReady(ready) => ready.instance_id,
            Event::ExtensionExited(exited) => exited.instance_id,
            _ => return None,
        };
        self.session
            .extension_blocks
            .get(&instance_id)
            .map(|state| state.owner.clone())
    }

    fn current_extension_block_owner(&self) -> UiSnapshotOwner {
        self.selection
            .displayed_agent_id
            .clone()
            .map(UiSnapshotOwner::Agent)
            .unwrap_or(UiSnapshotOwner::NoAgent)
    }

    fn take_action_completion_owner(&mut self, event: &Event) -> Option<UiSnapshotOwner> {
        let invocation_id = match event {
            Event::ActionResult(result) => &result.invocation_id,
            Event::ActionError(error) => &error.invocation_id,
            _ => return None,
        };
        self.event_owners
            .action_invocation_owners
            .remove(invocation_id)
    }

    fn handle_recorded_at_for_snapshot_owner(
        &mut self,
        prepared: &PreparedRendererEvent<'_>,
        recorded_at: UnixMicros,
        owner: UiSnapshotOwner,
    ) {
        let event = prepared.event();
        let is_global_message_fact = crate::message_fact_render::target_agent_id(event).is_some();
        match owner {
            UiSnapshotOwner::Agent(agent_id)
                if self.selection.displayed_agent_id.as_ref() == Some(&agent_id) =>
            {
                self.handle_recorded_at_for_visible_agent(prepared, recorded_at);
            }
            UiSnapshotOwner::NoAgent if self.selection.displayed_agent_id.is_none() => {
                self.transcript.ownership.contains_global_message_fact |= is_global_message_fact;
                self.handle_recorded_at_for_visible_agent(prepared, recorded_at);
            }
            UiSnapshotOwner::Agent(agent_id) => {
                self.handle_recorded_at_for_hidden_agent(prepared, recorded_at, agent_id);
            }
            UiSnapshotOwner::NoAgent => {
                self.handle_recorded_at_for_hidden_no_agent(
                    event,
                    recorded_at,
                    is_global_message_fact,
                );
            }
        }
    }

    fn handle_recorded_at_for_hidden_agent(
        &mut self,
        prepared: &PreparedRendererEvent<'_>,
        recorded_at: UnixMicros,
        target_agent_id: tau_proto::AgentId,
    ) {
        let event = prepared.event();
        self.resources
            .handle
            .suppress_selected_delivery_observation();
        let refresh_visible_watch_activity = matches!(
            event,
            Event::AgentCompacted(_) | Event::AgentStandaloneCompactionFailed(_)
        );
        let visible_agent_id = self.selection.displayed_agent_id.clone();
        let visible_state =
            self.take_visible_agent_state_with_output(tau_cli_term::OutputSnapshot::default());
        let mut target_state = self
            .selection
            .agents_ui_state
            .remove(&target_agent_id)
            .unwrap_or_default();
        self.resources
            .handle
            .select_detached(std::mem::take(&mut target_state.output));
        self.with_editor_context_publish_suppressed(|this| {
            this.restore_renderer_bookkeeping(target_state);
            this.selection.displayed_agent_id = Some(target_agent_id.clone());
            this.handle_recorded_at_for_visible_agent(prepared, recorded_at);
        });
        let target_output = self.resources.handle.take_detached();
        tracing::trace!(target: "tau_cli::frontend_progress", agent_id = %target_agent_id, blocks = target_output.block_count(), "hidden presentation updated");
        let target_state = self.take_visible_agent_state_with_output(target_output);
        self.selection
            .agents_ui_state
            .insert(target_agent_id, target_state);
        self.restore_renderer_bookkeeping(visible_state);
        self.selection.displayed_agent_id = visible_agent_id;
        if refresh_visible_watch_activity {
            self.refresh_watched_agent_blocks();
            self.render_model_status_if_present();
        }
        self.publish_editor_conversation_context();
    }

    fn handle_recorded_at_for_hidden_no_agent(
        &mut self,
        event: &Event,
        recorded_at: UnixMicros,
        is_global_message_fact: bool,
    ) {
        self.resources
            .handle
            .suppress_selected_delivery_observation();
        self.update_hidden_no_agent_state(|this| {
            this.transcript.ownership.contains_global_message_fact |= is_global_message_fact;
            let prepared = PreparedRendererEvent::new(event);
            this.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
        });
    }

    /// Temporarily restore and update the hidden no-agent snapshot without
    /// publishing its editor context or disturbing the visible agent
    /// transcript.
    fn update_hidden_no_agent_state(&mut self, update: impl FnOnce(&mut Self)) {
        let visible_agent_id = self.selection.displayed_agent_id.clone();
        let visible_state =
            self.take_visible_agent_state_with_output(tau_cli_term::OutputSnapshot::default());
        let mut no_agent_state = std::mem::take(&mut self.selection.no_agent_ui_state);
        self.resources
            .handle
            .select_detached(std::mem::take(&mut no_agent_state.output));
        self.with_editor_context_publish_suppressed(|this| {
            this.restore_renderer_bookkeeping(no_agent_state);
            this.selection.displayed_agent_id = None;
            update(this);
        });
        let no_agent_output = self.resources.handle.take_detached();
        self.selection.no_agent_ui_state =
            self.take_visible_agent_state_with_output(no_agent_output);
        self.restore_renderer_bookkeeping(visible_state);
        self.selection.displayed_agent_id = visible_agent_id;
        self.publish_editor_conversation_context();
    }

    fn agent_message_visible_on_empty_screen(&self, target_agent_id: &tau_proto::AgentId) -> bool {
        !self.selection.awaiting_new_agent_selection
            || !self.selection.agents_ui_state.contains_key(target_agent_id)
    }

    fn event_selects_agent_from_empty(
        &self,
        event: &Event,
        target_agent_id: &tau_proto::AgentId,
    ) -> bool {
        match event {
            Event::AgentPromptCreated(prompt) => {
                prompt.originator.is_user() && self.can_select_target_from_empty(target_agent_id)
            }
            Event::AgentPromptStarted(prompt) => {
                prompt.originator.is_user() && self.can_select_target_from_empty(target_agent_id)
            }
            Event::AgentCompactionTriggered(triggered) => {
                triggered.originator.is_user() && self.can_select_target_from_empty(target_agent_id)
            }
            Event::AgentPromptQueued(queued) => {
                !queued.message_class.is_internal()
                    && self.can_select_target_from_empty(target_agent_id)
            }
            Event::AgentPromptSubmitted(prompt) => {
                prompt.originator.is_user()
                    && !prompt.message_class.is_internal()
                    && self.can_select_target_from_empty(target_agent_id)
            }
            Event::UiPromptSubmitted(prompt) => {
                prompt.originator.is_user() && self.can_select_target_from_empty(target_agent_id)
            }
            _ => false,
        }
    }

    fn can_select_target_from_empty(&self, target_agent_id: &tau_proto::AgentId) -> bool {
        // When the UI is in the explicit start-new-agent state (`:agent new` or
        // `:agent switch none`), background activity from the previously visible
        // agent must not steal selection while the user is typing the prompt
        // meant to create a fresh agent. An event for an agent whose transcript
        // is already hidden here is therefore treated as background work, not as
        // the new agent.
        !self.selection.awaiting_new_agent_selection
            || !self.selection.agents_ui_state.contains_key(target_agent_id)
    }

    fn event_originator_is_extension(event: &Event) -> bool {
        match event {
            Event::UiPromptSubmitted(prompt) => !prompt.originator.is_user(),
            Event::AgentPromptSubmitted(prompt) => !prompt.originator.is_user(),
            Event::AgentCompactionTriggered(triggered) => !triggered.originator.is_user(),
            Event::AgentPromptCreated(prompt) => !prompt.originator.is_user(),
            Event::AgentPromptStarted(prompt) => !prompt.originator.is_user(),
            Event::AgentPromptTerminated(terminated) => !terminated.originator.is_user(),
            Event::ProviderPromptSubmitted(submitted) => !submitted.originator.is_user(),
            Event::ProviderResponseUpdated(update) => !update.originator.is_user(),
            Event::ProviderResponseFinished(finished) => !finished.originator.is_user(),
            Event::ToolStarted(started) => !started.originator.is_user(),
            Event::ToolResultDisplay(result) => !result.originator.is_user(),
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                !result.originator.is_user()
            }
            Event::ToolError(error) => !error.originator.is_user(),
            Event::ExtensionContextReady(_) => true,
            _ => false,
        }
    }

    fn event_has_explicit_ui_target(event: &Event) -> bool {
        match event {
            Event::UiShellCommand(command) => command.target_agent_id.is_some(),
            Event::ShellCommandProgress(progress) => progress.target_agent_id.is_some(),
            Event::ShellCommandFinished(finished) => finished.target_agent_id.is_some(),
            Event::UiCancelPrompt(cancel) => cancel.target_agent_id.is_some(),
            Event::UiRecallQueuedPrompt(recall) => recall.target_agent_id.is_some(),
            Event::AgentManualCompactionRequested(_)
            | Event::AgentStandaloneCompactionStarted(_) => true,
            _ => false,
        }
    }

    fn update_agent_in_progress(&self) {
        let hidden_in_progress = self
            .selection
            .agents_ui_state
            .values()
            .any(|state| state.transcript.status.agent_activity.is_in_progress());
        self.activity.agent_in_progress.store(
            self.transcript.status.agent_activity.is_in_progress() || hidden_in_progress,
            Ordering::Relaxed,
        );
    }

    fn learn_agent_metadata(&mut self, prepared: &PreparedRendererEvent<'_>) {
        let event = prepared.event();
        if self.learn_agent_lifecycle_metadata(event) {
            return;
        }
        if self.learn_agent_prompt_metadata(event) {
            return;
        }
        if self.learn_provider_tool_metadata(prepared) {
            return;
        }
        if self.learn_shell_metadata(event) {
            return;
        }
        self.learn_agent_message_metadata(event);
    }

    /// Retains only ownership correlations needed to route later events while
    /// transcript-visible metadata waits for deferred discovery replay.
    fn learn_deferred_routing_metadata(&mut self, prepared: &PreparedRendererEvent<'_>) {
        let event = prepared.event();
        match event {
            Event::ProviderResponseUpdated(update) => {
                self.event_owners
                    .prompt_agents
                    .insert(update.agent_prompt_id.clone(), update.agent_id.clone());
            }
            Event::ProviderResponseFinished(finished) => {
                self.event_owners
                    .prompt_agents
                    .insert(finished.agent_prompt_id.clone(), finished.agent_id.clone());
                let (_, calls) = prepared
                    .finished()
                    .expect("provider terminal preserves projection");
                for call in calls.iter() {
                    self.event_owners
                        .tool_agents
                        .insert(call.call_id.clone(), finished.agent_id.clone());
                }
            }
            Event::AgentPromptCreated(prompt) => {
                self.event_owners
                    .prompt_agents
                    .insert(prompt.agent_prompt_id.clone(), prompt.agent_id.clone());
            }
            Event::AgentPromptStarted(prompt) => {
                self.event_owners
                    .prompt_agents
                    .insert(prompt.agent_prompt_id.clone(), prompt.agent_id.clone());
            }
            Event::ToolStarted(started) => {
                self.event_owners
                    .tool_agents
                    .insert(started.call_id.clone(), started.agent_id.clone());
            }
            _ => {}
        }
    }

    fn learn_agent_lifecycle_metadata(&mut self, event: &Event) -> bool {
        match event {
            Event::StartAgentRequest(_) => true,
            Event::StartAgentAccepted(accepted) => {
                let agent_id = accepted.agent_id.clone();
                self.event_owners
                    .query_agents
                    .insert(accepted.query_id.clone(), agent_id.clone());
                self.remember_agent(agent_id);
                true
            }
            Event::AgentStarted(started) => {
                let agent_id = started.agent_id.clone();
                self.mark_agent_live(started.agent_id.clone());
                if started.ephemeral {
                    self.remember_agent_ephemeral(&agent_id);
                }
                if let Some(display_name) = started.display_name.as_ref() {
                    self.remember_agent_display_name(&agent_id, display_name);
                }
                true
            }
            Event::AgentStartFailed(failed) => {
                if let Ok(mut navigation) = self.discovery.agent_navigation.lock() {
                    navigation.unload(&failed.agent_id);
                }
                if let Ok(mut agents) = self.discovery.known_agents.lock() {
                    agents.retain(|agent_id| agent_id != failed.agent_id.as_str());
                }
                self.event_owners
                    .query_agents
                    .retain(|_, agent_id| agent_id != &failed.agent_id);
                true
            }
            Event::AgentDisplayNameSet(name) => {
                let agent_id = name.agent_id.clone();
                self.remember_agent(agent_id.clone());
                self.remember_agent_display_name(&agent_id, &name.display_name);
                if self.selection.current_agent_id.as_ref() == Some(&agent_id) {
                    self.render_model_status_if_present();
                }
                true
            }
            Event::StartAgentResult(_) => true,
            Event::AgentWatchesUpdated(updated) => {
                self.handle_agent_watches_updated(updated);
                true
            }
            Event::AgentStatsUpdated(updated) => {
                self.remember_agent(updated.agent_id.clone());
                self.handle_agent_stats_updated(updated);
                true
            }
            Event::SessionAgentUnloaded(unloaded) => {
                if let Ok(mut navigation) = self.discovery.agent_navigation.lock() {
                    navigation.unload(&unloaded.agent_id);
                }
                self.remove_agent_watch_endpoint(&unloaded.agent_id);
                self.watches.agent_models.remove(&unloaded.agent_id);
                true
            }
            Event::HarnessAgentContextUsageChanged(changed) => {
                self.remember_agent(changed.agent_id.clone());
                true
            }
            _ => false,
        }
    }

    fn learn_agent_prompt_metadata(&mut self, event: &Event) -> bool {
        match event {
            Event::UiPromptSubmitted(prompt) => {
                let agent_id = prompt.agent_id.clone();
                // This is only a transient UI request. Activation waits for an
                // accepted queue or committed submission event from the harness.
                self.remember_agent(agent_id.clone());
                if let tau_proto::PromptOriginator::Extension { query_id, .. } = &prompt.originator
                {
                    self.event_owners
                        .query_agents
                        .insert(query_id.clone(), agent_id);
                }
                true
            }
            Event::AgentPromptQueued(queued) => {
                self.mark_agent_live(queued.agent_id.clone());
                true
            }
            Event::AgentPromptSubmitted(prompt) => {
                let agent_id = prompt.agent_id.clone();
                if let tau_proto::PromptOriginator::Extension { query_id, .. } = &prompt.originator
                {
                    self.event_owners
                        .query_agents
                        .insert(query_id.clone(), agent_id.clone());
                    self.mark_agent_live(prompt.agent_id.clone());
                } else {
                    self.mark_agent_live(prompt.agent_id.clone());
                }
                true
            }
            Event::AgentCompactionTriggered(triggered) => {
                if triggered.originator.is_user() {
                    self.mark_agent_live(triggered.agent_id.clone());
                } else {
                    self.remember_agent(triggered.agent_id.clone());
                }
                true
            }
            Event::AgentPromptCreated(prompt) => {
                self.learn_agent_prompt_start(
                    &prompt.agent_id,
                    &prompt.agent_prompt_id,
                    &prompt.model,
                );
                true
            }
            Event::AgentPromptStarted(prompt) => {
                self.learn_agent_prompt_start(
                    &prompt.agent_id,
                    &prompt.agent_prompt_id,
                    &prompt.model,
                );
                true
            }
            Event::AgentPromptTerminated(terminated) => {
                self.mark_agent_live(terminated.agent_id.clone());
                self.mark_agent_prompt_inactive(&terminated.agent_prompt_id);
                true
            }
            Event::ProviderPromptSubmitted(submitted) => {
                self.mark_known_agent_prompt_active(
                    &submitted.agent_prompt_id,
                    &submitted.originator,
                );
                true
            }
            Event::ProviderResponseUpdated(update) => {
                let agent_id = update.agent_id.clone();
                if self.is_stale_terminal_stats_only_update(update) {
                    self.clear_main_agent_turn_active_everywhere();
                    return true;
                }
                if provider_response_update_has_visible_content(update)
                    || !self
                        .event_owners
                        .prompt_agents
                        .contains_key(&update.agent_prompt_id)
                {
                    if self.event_owners.prompt_agents.get(&update.agent_prompt_id)
                        != Some(&update.agent_id)
                    {
                        self.event_owners
                            .prompt_agents
                            .insert(update.agent_prompt_id.clone(), update.agent_id.clone());
                    }
                    self.mark_agent_prompt_active(&agent_id, &update.agent_prompt_id);
                }
                true
            }
            _ => false,
        }
    }

    /// Folds the prompt-start metadata shared by direct/replay prompt creation
    /// and the lightweight production UI lifecycle event.
    fn learn_agent_prompt_start(
        &mut self,
        agent_id: &tau_proto::AgentId,
        agent_prompt_id: &tau_proto::AgentPromptId,
        model: &tau_proto::ModelId,
    ) {
        let agent_id = agent_id.clone();
        self.watches
            .agent_models
            .insert(agent_id.clone(), model.clone());
        self.mark_agent_live(agent_id.clone());
        self.event_owners
            .prompt_agents
            .insert(agent_prompt_id.clone(), agent_id.clone());
        self.mark_agent_prompt_active(&agent_id, agent_prompt_id);
    }

    fn learn_provider_tool_metadata(&mut self, prepared: &PreparedRendererEvent<'_>) -> bool {
        let event = prepared.event();
        match event {
            Event::ProviderResponseFinished(finished) => {
                let (_, calls) = prepared
                    .finished()
                    .expect("provider terminal preserves projection");
                self.transcript
                    .status
                    .agent_activity
                    .finish_prompt_with_tool_call_ids(&finished.agent_prompt_id, calls.call_ids());
                if finished.originator.is_user() && calls.is_empty() {
                    self.clear_main_agent_turn_active_everywhere();
                }
                self.mark_agent_prompt_inactive(&finished.agent_prompt_id);
                self.mark_agent_live(finished.agent_id.clone());
                self.event_owners
                    .prompt_agents
                    .insert(finished.agent_prompt_id.clone(), finished.agent_id.clone());
                for call in calls.iter() {
                    self.event_owners
                        .tool_agents
                        .insert(call.call_id.clone(), finished.agent_id.clone());
                }
                true
            }
            Event::ToolStarted(started) => {
                let agent_id = started.agent_id.clone();
                self.remember_agent(agent_id.clone());
                self.event_owners
                    .tool_agents
                    .entry(started.call_id.clone())
                    .or_insert_with(|| started.agent_id.clone());
                true
            }
            _ => false,
        }
    }

    fn learn_shell_metadata(&mut self, event: &Event) -> bool {
        match event {
            Event::UiShellCommand(command) => {
                if let Some(agent_id) = command.target_agent_id.as_ref() {
                    self.remember_agent(agent_id.clone());
                    self.event_owners
                        .shell_agents
                        .insert(command.command_id.clone(), agent_id.clone());
                }
                true
            }
            Event::ShellCommandProgress(progress) => {
                if let Some(agent_id) = progress.target_agent_id.as_ref() {
                    self.remember_agent(agent_id.clone());
                    self.event_owners
                        .shell_agents
                        .insert(progress.command_id.clone(), agent_id.clone());
                }
                true
            }
            Event::ShellCommandFinished(finished) => {
                if let Some(agent_id) = finished.target_agent_id.as_ref() {
                    self.remember_agent(agent_id.clone());
                    self.event_owners
                        .shell_agents
                        .insert(finished.command_id.clone(), agent_id.clone());
                }
                true
            }
            _ => false,
        }
    }

    fn learn_agent_message_metadata(&mut self, event: &Event) {
        match event {
            Event::AgentMessageSent(message) => {
                self.remember_agent(message.sender_id.clone());
                if let Some(agent_id) = Self::agent_message_sent_recipient_agent_id(message) {
                    self.remember_agent(agent_id.clone());
                }
            }
            Event::AgentMessageReceived(message) => {
                self.remember_agent(message.sender_id.clone());
                self.mark_agent_live(message.recipient_id.clone());
                if let Some(status) = &message.watch_work_status {
                    self.handle_watched_agent_work_status(message, status);
                }
            }
            _ => {}
        }
    }

    fn agent_id_for_event(&self, event: &Event) -> Option<tau_proto::AgentId> {
        self.tool_event_agent_id(event)
            .or_else(|| Self::agent_message_event_agent_id(event))
            .or_else(|| Self::direct_agent_event_agent_id(event))
            .or_else(|| self.shell_event_agent_id(event))
            .or_else(|| self.prompt_event_agent_id(event))
            .into_agent_id(self.selection.current_agent_id.as_ref())
    }

    /// Resolve every message fact to its loaded transcript or the no-agent
    /// snapshot, preserving unavailable and invalid facts as global output.
    fn message_fact_snapshot_owner(&self, event: &Event) -> Option<UiSnapshotOwner> {
        match crate::message_fact_render::target_agent_id(event)? {
            path_crate_message_fact_render::MessageFactTarget::Valid(agent_id)
                if self
                    .discovery
                    .agent_navigation
                    .lock()
                    .expect("agent navigation lock")
                    .is_live(&agent_id) =>
            {
                Some(UiSnapshotOwner::Agent(agent_id.clone()))
            }
            path_crate_message_fact_render::MessageFactTarget::Valid(_)
            | path_crate_message_fact_render::MessageFactTarget::Invalid => {
                Some(UiSnapshotOwner::NoAgent)
            }
        }
    }

    fn tool_event_agent_id(&self, event: &Event) -> EventAgentIdResolution {
        match event {
            Event::ToolRequest(request) => EventAgentIdResolution::from_agent_id(
                self.event_owners.tool_agents.get(&request.call_id).cloned(),
            ),
            Event::ToolStarted(started) => EventAgentIdResolution::from_agent_id(
                self.event_owners
                    .tool_agents
                    .get(&started.call_id)
                    .cloned()
                    .or_else(|| Some(started.agent_id.clone())),
            ),
            Event::ToolProgress(progress) => EventAgentIdResolution::from_agent_id(
                self.event_owners
                    .tool_agents
                    .get(&progress.call_id)
                    .cloned(),
            ),
            Event::ToolResultDisplay(result) => EventAgentIdResolution::from_agent_id(
                self.event_owners
                    .tool_agents
                    .get(&result.call_id)
                    .cloned()
                    .or_else(|| self.agent_id_for_originator(&result.originator)),
            ),
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                EventAgentIdResolution::from_agent_id(
                    self.event_owners
                        .tool_agents
                        .get(&result.call_id)
                        .cloned()
                        .or_else(|| self.agent_id_for_originator(&result.originator)),
                )
            }
            Event::ToolError(error) => EventAgentIdResolution::from_agent_id(
                self.event_owners
                    .tool_agents
                    .get(&error.call_id)
                    .cloned()
                    .or_else(|| self.agent_id_for_originator(&error.originator)),
            ),
            Event::ToolBackgroundResultDisplay(result) => EventAgentIdResolution::from_agent_id(
                self.event_owners.tool_agents.get(&result.call_id).cloned(),
            ),
            Event::ToolBackgroundResult(result) => EventAgentIdResolution::from_agent_id(
                self.event_owners.tool_agents.get(&result.call_id).cloned(),
            ),
            Event::ToolBackgroundError(error) => EventAgentIdResolution::from_agent_id(
                self.event_owners.tool_agents.get(&error.call_id).cloned(),
            ),
            Event::ToolCancelled(cancelled) => EventAgentIdResolution::from_agent_id(
                self.event_owners
                    .tool_agents
                    .get(&cancelled.call_id)
                    .cloned(),
            ),
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn agent_message_event_agent_id(event: &Event) -> EventAgentIdResolution {
        match event {
            Event::AgentMessageSent(message) => {
                EventAgentIdResolution::Agent(message.sender_id.clone())
            }
            Event::AgentMessageReceived(message) => {
                EventAgentIdResolution::Agent(message.recipient_id.clone())
            }
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn direct_agent_event_agent_id(event: &Event) -> EventAgentIdResolution {
        match event {
            Event::AgentStarted(started) => EventAgentIdResolution::Agent(started.agent_id.clone()),
            Event::AgentDisplayNameSet(name) => {
                EventAgentIdResolution::Agent(name.agent_id.clone())
            }
            Event::UiPromptSubmitted(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.clone())
            }
            Event::AgentPromptSubmitted(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.clone())
            }
            Event::AgentPromptQueued(queued) => {
                EventAgentIdResolution::Agent(queued.agent_id.clone())
            }
            Event::AgentPromptRecalled(recalled) => {
                EventAgentIdResolution::Agent(recalled.agent_id.clone())
            }
            Event::AgentPromptRejected(rejected) => {
                EventAgentIdResolution::Agent(rejected.agent_id.clone())
            }
            Event::AgentPromptFailed(failed) => {
                EventAgentIdResolution::Agent(failed.agent_id.clone())
            }
            Event::AgentPromptSteered(steered) => {
                EventAgentIdResolution::Agent(steered.agent_id.clone())
            }
            Event::AgentCompactionTriggered(triggered) => {
                EventAgentIdResolution::Agent(triggered.agent_id.clone())
            }
            Event::AgentManualCompactionRequested(requested) => {
                EventAgentIdResolution::Agent(requested.target_agent_id.clone())
            }
            Event::AgentManualCompactionRequestFailed(failed) => {
                EventAgentIdResolution::Agent(failed.target_agent_id.clone())
            }
            Event::AgentStandaloneCompactionStarted(started) => {
                EventAgentIdResolution::Agent(started.agent_id.clone())
            }
            Event::AgentStandaloneCompactionFailed(failed) => {
                EventAgentIdResolution::Agent(failed.agent_id.clone())
            }
            Event::AgentCompacted(compacted) => {
                EventAgentIdResolution::Agent(compacted.agent_id.clone())
            }
            Event::AgentInferenceDispatchStarted(started) => {
                EventAgentIdResolution::Agent(started.agent_id.clone())
            }
            Event::HarnessAgentContextUsageChanged(changed) => {
                EventAgentIdResolution::Agent(changed.agent_id.clone())
            }
            Event::HarnessAgentContextInitialized(initialized) => {
                EventAgentIdResolution::Agent(initialized.agent_id.clone())
            }
            Event::ExtensionContextReady(ready) => {
                EventAgentIdResolution::Agent(ready.agent_id.clone())
            }
            Event::UiCancelPrompt(cancel) => {
                EventAgentIdResolution::from_agent_id(cancel.target_agent_id.as_ref().cloned())
            }
            Event::UiRecallQueuedPrompt(recall) => {
                EventAgentIdResolution::from_agent_id(recall.target_agent_id.as_ref().cloned())
            }
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn shell_event_agent_id(&self, event: &Event) -> EventAgentIdResolution {
        match event {
            Event::UiShellCommand(command) => {
                EventAgentIdResolution::from_agent_id(command.target_agent_id.as_ref().cloned())
            }
            Event::ShellCommandProgress(progress) => EventAgentIdResolution::from_agent_id(
                progress.target_agent_id.as_ref().cloned().or_else(|| {
                    self.event_owners
                        .shell_agents
                        .get(&progress.command_id)
                        .cloned()
                }),
            ),
            Event::ShellCommandFinished(finished) => EventAgentIdResolution::from_agent_id(
                finished.target_agent_id.as_ref().cloned().or_else(|| {
                    self.event_owners
                        .shell_agents
                        .get(&finished.command_id)
                        .cloned()
                }),
            ),
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn prompt_event_agent_id(&self, event: &Event) -> EventAgentIdResolution {
        match event {
            Event::AgentPromptCreated(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.clone())
            }
            Event::AgentPromptStarted(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.clone())
            }
            Event::AgentPromptTerminated(terminated) => EventAgentIdResolution::from_agent_id(
                self.agent_id_for_prompt(&terminated.agent_prompt_id, &terminated.originator),
            ),
            Event::ProviderPromptSubmitted(submitted) => EventAgentIdResolution::from_agent_id(
                self.event_owners
                    .prompt_agents
                    .get(&submitted.agent_prompt_id)
                    .cloned()
                    .or_else(|| self.agent_id_for_originator(&submitted.originator)),
            ),
            Event::ProviderResponseUpdated(update) => EventAgentIdResolution::from_agent_id(
                self.event_owners
                    .prompt_agents
                    .get(&update.agent_prompt_id)
                    .cloned()
                    .or_else(|| Some(update.agent_id.clone())),
            ),
            Event::ProviderResponseFinished(finished) => {
                EventAgentIdResolution::Agent(finished.agent_id.clone())
            }
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn agent_id_for_prompt(
        &self,
        agent_prompt_id: &tau_proto::AgentPromptId,
        originator: &tau_proto::PromptOriginator,
    ) -> Option<tau_proto::AgentId> {
        self.event_owners
            .prompt_agents
            .get(agent_prompt_id)
            .cloned()
            .or_else(|| self.agent_id_for_originator(originator))
    }

    fn agent_id_for_originator(
        &self,
        originator: &tau_proto::PromptOriginator,
    ) -> Option<tau_proto::AgentId> {
        match originator {
            tau_proto::PromptOriginator::User => self.selection.current_agent_id.clone(),
            tau_proto::PromptOriginator::Extension { query_id, .. } => {
                self.event_owners.query_agents.get(query_id).cloned()
            }
        }
    }

    fn handle_recorded_at_for_visible_agent(
        &mut self,
        prepared: &PreparedRendererEvent<'_>,
        recorded_at: UnixMicros,
    ) {
        let event = prepared.event();
        self.sync_agent_activity_for_lifecycle(prepared);

        self.sync_main_tools_visibility_for_prompt_lifecycle(prepared);

        if self.handle_agent_message_event(event) {
            return;
        }
        if self.handle_message_fact_event(event) {
            return;
        }

        // Events are routed to the owning agent transcript before reaching this
        // point, so side-conversation events are rendered into their own hidden
        // or visible state instead of being dropped.

        if self.handle_session_events(event)
            || self.handle_prompt_events(event)
            || self.handle_provider_response_events(prepared)
            || self.handle_tool_events(event, recorded_at)
            || self.handle_shell_events(event)
            || self.handle_action_events(event)
            || self.handle_extension_events(event)
            || self.handle_harness_status_events(event)
            || self.handle_harness_role_events(event)
            || self.handle_harness_available_events(event)
            || self.handle_terminal_events(event)
        {
            return;
        }

        Self::trace_unhandled_event(event);
    }

    /// Render one committed message fact in the current containing view.
    fn handle_message_fact_event(&mut self, event: &Event) -> bool {
        let target_context =
            crate::message_fact_render::target_agent_id(event).is_some_and(|target| {
                let path_crate_message_fact_render::MessageFactTarget::Valid(agent_id) = target
                else {
                    return false;
                };
                self.selection.displayed_agent_id.as_ref() == Some(&agent_id)
            });
        let target_context = if target_context {
            path_crate_message_fact_render::MessageFactTargetContext::Implied
        } else {
            path_crate_message_fact_render::MessageFactTargetContext::Explicit
        };
        let Some(rendered) = crate::message_fact_render::render(event, target_context) else {
            return false;
        };
        self.resources
            .handle
            .print_output("message-fact", self.submitted_message_fact_block(rendered));
        true
    }

    fn trace_unhandled_event(event: &Event) {
        tracing::trace!(
            target: "tau_cli::ui",
            event = ?std::mem::discriminant(event),
            "unhandled event variant"
        );
    }

    fn handle_agent_message_event(&mut self, event: &Event) -> bool {
        if !matches!(
            event,
            Event::AgentMessageSent(_) | Event::AgentMessageReceived(_)
        ) {
            return false;
        }
        if matches!(
            event,
            Event::AgentMessageReceived(message)
                if message.watch_work_status.as_ref().is_some_and(|status| status.initial)
        ) {
            // `learn_agent_metadata` already folded this initial snapshot into
            // the watched-agent row. It establishes current state rather than
            // reporting an actionable transition, so it must not create a
            // transcript notification.
            return true;
        }
        let block = self.render_agent_message_block(event);
        let block_id = self.resources.handle.print_output("agent-message", block);
        self.transcript
            .history
            .message_history
            .push(MessageBlockEntry {
                block_id,
                event: event.clone(),
                session_id: self.session.current_session_id.clone(),
            });
        true
    }

    fn render_agent_message_block(&self, event: &Event) -> tau_cli_term::StyledBlock {
        self.render_agent_message_block_with_local_names(event, true)
    }

    fn render_agent_message_block_with_local_names(
        &self,
        event: &Event,
        use_local_names: bool,
    ) -> tau_cli_term::StyledBlock {
        if let Some(summary) = Self::watch_provider_status_summary(event) {
            return self.internal_notice_block(summary);
        }
        if let Some(summary) = self.watch_long_wait_summary_with_local_names(event, use_local_names)
        {
            return self.marked_plain_block(
                tau_themes::names::SYSTEM_INFO,
                crate::transcript_markers::STATUS_UPDATE,
                summary,
            );
        }
        if let Some(summary) =
            self.watch_work_status_summary_with_local_names(event, use_local_names)
        {
            if !self.presentation.verbose_mode {
                return Self::empty_block();
            }
            return self.marked_plain_block(
                tau_themes::names::SYSTEM_INFO,
                crate::transcript_markers::STATUS_UPDATE,
                summary,
            );
        }
        match Self::message_render_mode(self.presentation.show_messages) {
            MessageRenderMode::Hidden => Self::empty_block(),
            MessageRenderMode::Summary => {
                self.submitted_agent_message_block(event, use_local_names, false)
            }
            MessageRenderMode::Full => self.submitted_agent_message_block(
                event,
                use_local_names,
                self.presentation.verbose_mode
                    || !Self::compact_suppresses_agent_message_body(event),
            ),
        }
    }

    /// Returns whether compact mode removes the body of this ordinary
    /// agent-to-agent message while retaining its existing header.
    fn compact_suppresses_agent_message_body(event: &Event) -> bool {
        let kind = match event {
            Event::AgentMessageSent(message) => message.kind,
            Event::AgentMessageReceived(message) => message.kind,
            _ => return false,
        };
        matches!(
            kind,
            tau_proto::AgentMessageKind::Message | tau_proto::AgentMessageKind::WatchResponse
        )
    }

    /// Renders a harness-authored provider-work record as a normalized internal
    /// notice.
    fn watch_provider_status_summary(event: &Event) -> Option<String> {
        let Event::AgentMessageReceived(message) = event else {
            return None;
        };
        if message.kind != tau_proto::AgentMessageKind::WatchProviderStatus {
            return None;
        }
        if message.watch_provider_status.is_some() {
            let body = message
                .message
                .strip_prefix(tau_proto::TAU_INTERNAL_OPEN)
                .and_then(|text| text.strip_suffix(tau_proto::TAU_INTERNAL_CLOSE))
                .filter(|text| {
                    !text.contains(tau_proto::TAU_INTERNAL_OPEN)
                        && !text.contains(tau_proto::TAU_INTERNAL_CLOSE)
                })
                .unwrap_or(&message.message);
            return Some(body.to_owned());
        }
        None
    }

    /// Renders a harness-authored long-wait record as a typed status summary.
    fn watch_long_wait_summary_with_local_names(
        &self,
        event: &Event,
        use_local_names: bool,
    ) -> Option<String> {
        let Event::AgentMessageReceived(message) = event else {
            return None;
        };
        let long_wait = message.watch_long_wait.as_ref()?;
        let sender = self.agent_message_received_sender_label(message, use_local_names);
        let minute_label = if long_wait.threshold_minutes == 1 {
            "minute"
        } else {
            "minutes"
        };
        Some(format!(
            "{sender} has been working for {} {minute_label}",
            long_wait.threshold_minutes
        ))
    }

    /// Renders routing identities brightly while leaving surrounding header
    /// wording, task-name context, and message content in the base style.
    fn submitted_agent_message_block(
        &self,
        event: &Event,
        use_local_names: bool,
        include_body: bool,
    ) -> tau_cli_term::StyledBlock {
        use tau_cli_term::resolve::{convert_color, themed_text};
        use tau_themes::{SpanTree, StyleName, ThemedText, names};

        let mut themed = ThemedText::new();
        let body_style = themed.add_style(names::SYSTEM_INFO);
        let marker_style = themed.add_style(names::PROMPT_MARKER_SUBMITTED);
        let identity_style = themed.add_style(names::AGENT_MESSAGE_IDENTITY);
        let mut content = self
            .agent_message_header_parts(event, use_local_names)
            .into_iter()
            .map(|(text, bright)| {
                if bright {
                    SpanTree::span(identity_style, vec![SpanTree::text(text)])
                } else {
                    SpanTree::text(text)
                }
            })
            .collect::<Vec<_>>();
        if include_body {
            content.push(SpanTree::text(format!(
                ":\n{}",
                Self::agent_message_body(event)
            )));
        }
        themed.push_tree(SpanTree::span(
            body_style,
            vec![
                SpanTree::span(
                    marker_style,
                    vec![SpanTree::text(crate::transcript_markers::MESSAGE)],
                ),
                SpanTree::span(body_style, content),
            ],
        ));

        let body_ts = self
            .resources
            .theme
            .resolve_style(&StyleName::new(names::SYSTEM_INFO));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.resources.theme, &themed));
        if let Some(bg) = body_ts.bg {
            block = block.bg(convert_color(bg));
        }
        block
    }

    /// Builds a header from semantic endpoint pieces so task-name text that
    /// happens to contain another routing id cannot acquire identity styling.
    fn agent_message_header_parts(
        &self,
        event: &Event,
        use_local_names: bool,
    ) -> Vec<(String, bool)> {
        let (sender, recipient, kind, local_sender_id, local_recipient_id) = match event {
            Event::AgentMessageSent(message) => {
                let sender = self.message_agent_display_label(&message.sender_id, use_local_names);
                let sender = (sender, Some(format!("@{}", message.sender_id)));
                let recipient = self.agent_message_sent_recipient_display(message, use_local_names);
                let recipient_identity = match &message.recipient {
                    tau_proto::AgentMessageRecipient::Agent { agent_id } => {
                        Some(format!("@{agent_id}"))
                    }
                    tau_proto::AgentMessageRecipient::ExternalAgent {
                        session_id,
                        agent_id,
                    } => Some(format!("{session_id}/@{agent_id}")),
                };
                let recipient = (recipient, recipient_identity);
                let local_recipient_id = match &message.recipient {
                    tau_proto::AgentMessageRecipient::Agent { agent_id } => Some(agent_id.as_str()),
                    tau_proto::AgentMessageRecipient::ExternalAgent { .. } => None,
                };
                (
                    sender,
                    recipient,
                    message.kind,
                    Some(message.sender_id.as_str()),
                    local_recipient_id,
                )
            }
            Event::AgentMessageReceived(message) => {
                let sender = self.agent_message_received_sender_label(message, use_local_names);
                let sender_identity = Some(message.sender_session_id.as_ref().map_or_else(
                    || format!("@{}", message.sender_id),
                    |session_id| format!("{session_id}/@{}", message.sender_id),
                ));
                let sender = (sender, sender_identity);
                let recipient =
                    self.message_agent_display_label(&message.recipient_id, use_local_names);
                let recipient = (recipient, Some(format!("@{}", message.recipient_id)));
                (
                    sender,
                    recipient,
                    message.kind,
                    message
                        .sender_session_id
                        .is_none()
                        .then_some(message.sender_id.as_str()),
                    Some(message.recipient_id.as_str()),
                )
            }
            _ => unreachable!("only agent message events are rendered here"),
        };
        let (prefix, first, separator, second) = if kind == tau_proto::AgentMessageKind::WatchPrompt
        {
            ("Prompt to ", sender, " observed by ", Some(recipient))
        } else if self
            .selection
            .displayed_agent_id
            .as_deref()
            .is_some_and(|displayed| Some(displayed) == local_sender_id)
        {
            ("Message to ", recipient, "", None)
        } else if self
            .selection
            .displayed_agent_id
            .as_deref()
            .is_some_and(|displayed| Some(displayed) == local_recipient_id)
        {
            ("Message from ", sender, "", None)
        } else {
            ("Message from ", sender, " to ", Some(recipient))
        };
        let mut parts = vec![(prefix.to_owned(), false)];
        Self::push_agent_message_endpoint(&mut parts, first);
        if let Some(second) = second {
            parts.push((separator.to_owned(), false));
            Self::push_agent_message_endpoint(&mut parts, second);
        }
        parts
    }

    /// Appends one endpoint with only its canonical routing prefix emphasized.
    fn push_agent_message_endpoint(
        parts: &mut Vec<(String, bool)>,
        (endpoint, identity): (String, Option<String>),
    ) {
        let Some(identity) = identity else {
            parts.push((endpoint, false));
            return;
        };
        let context = endpoint
            .strip_prefix(&identity)
            .expect("formatted endpoint starts with its routing identity");
        parts.push((identity, true));
        parts.push((context.to_owned(), false));
    }

    /// Renders a structured watched-agent work report as purpose-built content,
    /// never as an ordinary message with an empty compatibility body.
    fn watch_work_status_summary_with_local_names(
        &self,
        event: &Event,
        use_local_names: bool,
    ) -> Option<String> {
        let Event::AgentMessageReceived(message) = event else {
            return None;
        };
        let status = message.watch_work_status.as_ref()?;
        let phase_symbol = crate::list_agents::work_status_symbol(Some(status.phase));
        let sender = self.agent_message_received_sender_label(message, use_local_names);
        let title = status
            .title
            .as_deref()
            .map(tau_proto::visible_escape_metadata)
            .unwrap_or_else(|| "no reported task".to_owned());
        Some(format!(
            "Status update from {sender}: {phase_symbol} {title}"
        ))
    }

    #[cfg(test)]
    fn agent_message_summary(&self, event: &Event) -> String {
        self.agent_message_header_parts(event, true)
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    fn agent_message_body(event: &Event) -> String {
        match event {
            Event::AgentMessageSent(message) => message.message.clone(),
            Event::AgentMessageReceived(message) => message.message.clone(),
            _ => unreachable!("only agent message events are rendered here"),
        }
    }

    /// Bounds a supplemental agent name after visibly escaping controls,
    /// structural Unicode, and label delimiters.
    fn bounded_agent_message_name(value: &str) -> String {
        Self::bounded_metadata_with(
            value,
            AGENT_MESSAGE_NAME_MAX_COLUMNS,
            AGENT_MESSAGE_NAME_MAX_BYTES,
            |grapheme| {
                use std::fmt::Write as _;

                let mut escaped = String::new();
                for character in grapheme.chars() {
                    if tau_proto::requires_visible_escape(character) {
                        escaped
                            .push_str(&tau_proto::visible_escape_metadata(&character.to_string()));
                    } else if matches!(character, '(' | ')' | '\\' | '"') {
                        let _ = write!(escaped, "\\u{{{:04X}}}", character as u32);
                    } else {
                        escaped.push(character);
                    }
                }
                escaped
            },
        )
    }

    fn bounded_metadata_with(
        value: &str,
        max_columns: usize,
        max_bytes: usize,
        escape: impl Fn(&str) -> String,
    ) -> String {
        use unicode_segmentation::UnicodeSegmentation as _;
        use unicode_width::UnicodeWidthStr as _;

        let mut output = String::new();
        let mut columns: usize = 0;
        for grapheme in value.graphemes(true) {
            let escaped = escape(grapheme);
            let next_columns = columns.saturating_add(escaped.width());
            if max_columns < next_columns || output.len().saturating_add(escaped.len()) > max_bytes
            {
                if columns < max_columns && output.len().saturating_add('…'.len_utf8()) <= max_bytes
                {
                    output.push('…');
                }
                break;
            }
            output.push_str(&escaped);
            columns = next_columns;
        }
        output
    }

    fn agent_message_sent_recipient_display(
        &self,
        message: &tau_proto::AgentMessageSent,
        use_local_names: bool,
    ) -> String {
        match &message.recipient {
            tau_proto::AgentMessageRecipient::Agent { agent_id } => {
                self.message_agent_display_label(agent_id, use_local_names)
            }
            tau_proto::AgentMessageRecipient::ExternalAgent {
                session_id,
                agent_id,
            } => {
                format!("{session_id}/@{agent_id}")
            }
        }
    }

    fn agent_message_received_sender_label(
        &self,
        message: &tau_proto::AgentMessageReceived,
        use_local_names: bool,
    ) -> String {
        message.sender_session_id.as_ref().map_or_else(
            || self.message_agent_display_label(&message.sender_id, use_local_names),
            |session_id| format!("{session_id}/@{}", message.sender_id),
        )
    }

    fn agent_message_sent_recipient_agent_id(
        message: &tau_proto::AgentMessageSent,
    ) -> Option<&tau_proto::AgentId> {
        match &message.recipient {
            tau_proto::AgentMessageRecipient::Agent { agent_id } => Some(agent_id),
            tau_proto::AgentMessageRecipient::ExternalAgent { .. } => None,
        }
    }

    fn message_render_mode(show_messages: tau_config::settings::ShowMessages) -> MessageRenderMode {
        match show_messages {
            path_tau_config_settings::ShowMessages::None
            | path_tau_config_settings::ShowMessages::SelfSummary
            | path_tau_config_settings::ShowMessages::SelfFull => MessageRenderMode::Hidden,
            path_tau_config_settings::ShowMessages::AllSummary => MessageRenderMode::Summary,
            path_tau_config_settings::ShowMessages::AllFull => MessageRenderMode::Full,
        }
    }

    fn handle_session_events(&mut self, event: &Event) -> bool {
        match event {
            Event::SessionStarted(started) => {
                self.handle_existing_session_started(started);
                true
            }
            _ => false,
        }
    }

    fn handle_existing_session_started(&mut self, started: &tau_proto::SessionStarted) {
        // Work status is runtime-only. A resume must wait for its fresh watch
        // snapshots instead of carrying presentation metadata across a daemon
        // generation, even when the session id is unchanged.
        self.watches.watched_agent_work_statuses.clear();
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(self.session.current_session_id.is_none());
        self.watches.agent_estimated_api_costs.clear();
        self.session.session_token_usage = tau_proto::TokenUsageCounts::default();
        self.clear_agent_display_names();
        self.rerender_message_history();
        self.session.current_session_id = Some(started.session_id.clone());
        self.reconcile_session_context(&started.session_id);
        self.render_model_status();
    }

    fn reconcile_session_context(&self, session_id: &tau_proto::SessionId) {
        if let Some(retargeter) = &self.selection.draft_retargeter
            && let Ok(mut active_session) = retargeter.session_id.lock()
        {
            active_session.clone_from(session_id);
            invalidate_pending_draft(retargeter.handle.as_ref());
        }
        self.render_right_prompt_context(session_id);
    }

    fn render_right_prompt_context(&self, session_id: &tau_proto::SessionId) {
        let Some((cwd, home)) = &self.session.right_prompt_paths else {
            return;
        };
        self.resources
            .handle
            .set_right_prompt(crate::theme::right_prompt_context(
                &self.resources.theme,
                cwd,
                home.as_deref(),
                session_id.as_ref(),
            ));
    }

    fn handle_prompt_events(&mut self, event: &Event) -> bool {
        match event {
            Event::UiPromptSubmitted(prompt) => {
                self.handle_ui_prompt_submitted(prompt);
                true
            }
            Event::AgentPromptSubmitted(prompt) => {
                self.handle_agent_prompt_submitted(prompt);
                true
            }
            Event::AgentPromptQueued(queued) => {
                self.handle_agent_prompt_queued(queued);
                true
            }
            Event::AgentPromptRecalled(recalled) => {
                self.handle_agent_prompt_recalled(recalled);
                true
            }
            Event::AgentPromptRejected(rejected) => {
                self.handle_agent_prompt_rejected(rejected);
                true
            }
            Event::AgentPromptSteered(steered) => {
                self.handle_agent_prompt_steered(steered);
                true
            }
            Event::AgentPromptCreated(prompt) => {
                self.handle_agent_prompt_created(prompt);
                true
            }
            Event::AgentPromptStarted(prompt) => {
                self.handle_agent_prompt_started(prompt);
                true
            }
            Event::AgentPromptTerminated(terminated) => {
                self.handle_agent_prompt_terminated(terminated);
                true
            }
            _ => false,
        }
    }

    fn handle_ui_prompt_submitted(&mut self, prompt: &tau_proto::UiPromptSubmitted) {
        self.handle_submitted_user_prompt(&prompt.text, prompt.message_class);
    }

    fn handle_agent_prompt_submitted(&mut self, prompt: &tau_proto::AgentPromptSubmitted) {
        if let Some(ctx_id) = prompt.ctx_id.as_ref() {
            if !self
                .transcript
                .runtime
                .submitted_prompt_ctx_ids
                .contains(ctx_id)
            {
                self.transcript
                    .runtime
                    .submitted_prompt_ctx_ids
                    .push_back(ctx_id.clone());
            }
            while MAX_SUBMITTED_PROMPT_CORRELATIONS
                < self.transcript.runtime.submitted_prompt_ctx_ids.len()
            {
                self.transcript.runtime.submitted_prompt_ctx_ids.pop_front();
            }
        }
        let queued = if self
            .transcript
            .runtime
            .queued_user_blocks
            .front()
            .is_some_and(|queued| queued.message_class == prompt.message_class)
        {
            self.transcript.runtime.queued_user_blocks.pop_front()
        } else {
            None
        };
        if !prompt.message_class.is_internal()
            && matches!(
                prompt.submission_source,
                tau_proto::PromptSubmissionSource::HumanUi
                    | tau_proto::PromptSubmissionSource::Legacy
            )
            && let Some(queued_id) = queued.as_ref().and_then(|queued| queued.id)
        {
            use tau_themes::names;
            self.resources.handle.remove_block(queued_id);
            self.reset_main_tool_usage();
            let queued = queued.expect("queued block supplied its identifier");
            let id = self.resources.handle.print_output(
                "user-prompt",
                self.submitted_prompt_block(names::USER_PROMPT, &queued.text),
            );
            self.transcript.runtime.last_user_block = Some((id, queued.text));
            return;
        }
        if self.handle_typed_internal_prompt_projection(
            prompt.message_class,
            prompt.internal_kind,
            &prompt.text,
        ) {
            return;
        }
        if self.handle_timer_wakeup_prompt(
            prompt.message_class,
            prompt.ctx_id.as_deref(),
            Some(&prompt.text),
        ) {
            return;
        }
        if let Some(queued_id) = queued.and_then(|queued| queued.id) {
            self.resources.handle.remove_block(queued_id);
        }
        if self.handle_source_aware_prompt_projection(
            &prompt.submission_source,
            prompt.message_class,
            &prompt.text,
        ) {
            return;
        }
        // Legacy records intentionally retain their historical rendering:
        // there is no safe prefix-based way to reclassify them.
        self.handle_submitted_user_prompt(&prompt.text, prompt.message_class);
    }

    /// Applies only the submitted-prompt projection for focused terminal tests.
    #[cfg(test)]
    pub(crate) fn handle_agent_prompt_submitted_for_test(
        &mut self,
        prompt: &tau_proto::AgentPromptSubmitted,
    ) {
        self.handle_agent_prompt_submitted(prompt);
    }

    /// Apply typed internal-prompt UI treatment.
    ///
    /// Returns `true` when the typed category consumed the projection, whether
    /// it rendered a block or deliberately suppressed the prompt.
    fn handle_typed_internal_prompt_projection(
        &mut self,
        message_class: tau_proto::PromptMessageClass,
        internal_kind: Option<tau_proto::InternalPromptKind>,
        text: &str,
    ) -> bool {
        if !message_class.is_internal() {
            return false;
        }
        match internal_kind {
            Some(
                tau_proto::InternalPromptKind::BackgroundToolCompletion
                | tau_proto::InternalPromptKind::OutputLengthContinuation,
            ) => return true,
            Some(tau_proto::InternalPromptKind::ContextSizeAlert) => {}
            None => return false,
        }
        let projection = InternalPromptProjection::ContextSizeAlert {
            text: text.to_owned(),
        };
        let block_id = self.resources.handle.print_output(
            "context-size-alert",
            self.render_internal_prompt_projection(&projection),
        );
        self.transcript
            .history
            .internal_prompt_history
            .push(InternalPromptBlockEntry {
                block_id,
                projection,
            });
        true
    }

    fn handle_timer_wakeup_prompt(
        &mut self,
        message_class: tau_proto::PromptMessageClass,
        ctx_id: Option<&str>,
        text: Option<&str>,
    ) -> bool {
        if !message_class.is_internal() {
            return false;
        }
        let Some((timer_id, _fire_count)) = timer_wakeup_ctx(ctx_id) else {
            return false;
        };
        let projection = InternalPromptProjection::TimerWakeup {
            timer_id: timer_id.to_owned(),
            text: text.map(str::to_owned),
        };
        let block_id = self.resources.handle.print_output(
            "timer-wakeup",
            self.render_internal_prompt_projection(&projection),
        );
        self.transcript
            .history
            .internal_prompt_history
            .push(InternalPromptBlockEntry {
                block_id,
                projection,
            });
        true
    }

    /// Render source-aware prompt provenance before applying ordinary
    /// user-prompt presentation. Returns true when the source owns the
    /// projection.
    fn handle_source_aware_prompt_projection(
        &mut self,
        submission_source: &tau_proto::PromptSubmissionSource,
        _message_class: tau_proto::PromptMessageClass,
        text: &str,
    ) -> bool {
        match submission_source {
            tau_proto::PromptSubmissionSource::Extension { .. } => {
                let block = self.render_source_aware_prompt_block(submission_source, text);
                self.resources
                    .handle
                    .print_output("extension-prompt", block);
                true
            }
            tau_proto::PromptSubmissionSource::HarnessInternal => {
                let block_id = self.resources.handle.print_output(
                    "harness-internal-prompt",
                    self.render_source_aware_prompt_block(submission_source, text),
                );
                self.transcript
                    .history
                    .internal_prompt_history
                    .push(InternalPromptBlockEntry {
                        block_id,
                        projection: InternalPromptProjection::SourceAware {
                            submission_source: submission_source.clone(),
                            text: text.to_owned(),
                        },
                    });
                true
            }
            tau_proto::PromptSubmissionSource::HumanUi
            | tau_proto::PromptSubmissionSource::Legacy => false,
        }
    }

    /// Render one canonical extension or harness-internal prompt projection.
    fn render_source_aware_prompt_block(
        &self,
        submission_source: &tau_proto::PromptSubmissionSource,
        text: &str,
    ) -> tau_cli_term::StyledBlock {
        match submission_source {
            tau_proto::PromptSubmissionSource::Extension { name } => self.marked_plain_block(
                tau_themes::names::SYSTEM_INFO,
                crate::transcript_markers::MESSAGE,
                format!(
                    "External `{}` message:\n{text}",
                    tau_proto::visible_escape_metadata(name.as_str())
                ),
            ),
            tau_proto::PromptSubmissionSource::HarnessInternal
                if self.presentation.verbose_mode && self.presentation.show_internal_prompts =>
            {
                self.internal_notice_block(text)
            }
            tau_proto::PromptSubmissionSource::HarnessInternal
            | tau_proto::PromptSubmissionSource::HumanUi
            | tau_proto::PromptSubmissionSource::Legacy => Self::empty_block(),
        }
    }

    /// Renders one retained model-facing prompt projection under current UI
    /// settings.
    fn render_internal_prompt_projection(
        &self,
        projection: &InternalPromptProjection,
    ) -> tau_cli_term::StyledBlock {
        if !self.notice_visible(
            tau_proto::NoticePurpose::Diagnostic,
            tau_proto::NoticeLevel::Info,
        ) {
            return Self::empty_block();
        }
        match projection {
            InternalPromptProjection::SourceAware {
                submission_source,
                text,
            } => self.render_source_aware_prompt_block(submission_source, text),
            InternalPromptProjection::ContextSizeAlert { text } => {
                self.context_size_alert_block(text)
            }
            InternalPromptProjection::TimerWakeup { timer_id, text } => {
                self.timer_wakeup_block(timer_id, text.as_deref())
            }
        }
    }

    fn handle_submitted_user_prompt(
        &mut self,
        text: &str,
        message_class: tau_proto::PromptMessageClass,
    ) {
        if message_class.is_internal() {
            return;
        }

        use tau_themes::names;

        if self.front_queued_user_prompt_matches(text) {
            let Some(queued) = self.transcript.runtime.queued_user_blocks.pop_front() else {
                return;
            };
            self.resources
                .handle
                .remove_block(queued.id.expect("matched queued user prompt owns a block"));
            self.reset_main_tool_usage();
            let id = self.resources.handle.print_output(
                "user-prompt",
                self.submitted_prompt_block(names::USER_PROMPT, &queued.text),
            );
            self.transcript.runtime.last_user_block = Some((id, queued.text));
            return;
        }
        self.reset_main_tool_usage();
        let block = self.submitted_prompt_block(names::USER_PROMPT, text);
        let id = self.resources.handle.print_output("user-prompt", block);
        self.transcript.runtime.last_user_block = Some((id, text.to_owned()));
    }

    fn handle_agent_prompt_queued(&mut self, queued: &tau_proto::AgentPromptQueued) {
        if queued.message_class.is_internal() {
            self.transcript
                .runtime
                .queued_user_blocks
                .push_back(QueuedUserBlock {
                    id: None,
                    text: queued.text.clone(),
                    message_class: queued.message_class,
                });
            return;
        }

        use tau_themes::names;

        self.reset_main_tool_usage();
        if let Some((id, text)) = self.transcript.runtime.last_user_block.take() {
            if text == queued.text {
                self.resources.handle.remove_block(id);
            } else {
                self.transcript.runtime.last_user_block = Some((id, text));
            }
        }
        let mut block = markdown_prompt_block_with_osc8(
            &self.resources.theme,
            names::USER_PROMPT_QUEUED,
            format!("{} ", self.resources.prompt_symbol),
            "",
            self.presentation.osc8_links,
        );
        let prefix = block.content.clone();
        block = block.two_line_elision(queued_prompt_projection(
            &self.resources.theme,
            self.presentation.osc8_links,
            prefix,
            &queued.text,
        ));
        let queued_id = self.resources.handle.new_block("user-prompt-queued", block);
        self.resources
            .handle
            .push_above_active_before_any(queued_id, self.watched_agent_anchor_ids());
        self.resources.handle.redraw();
        self.transcript
            .runtime
            .queued_user_blocks
            .push_back(QueuedUserBlock {
                id: Some(queued_id),
                text: queued.text.clone(),
                message_class: queued.message_class,
            });
    }

    fn handle_agent_prompt_recalled(&mut self, recalled: &tau_proto::AgentPromptRecalled) {
        if let Some(index) = self
            .transcript
            .runtime
            .queued_user_blocks
            .iter()
            .rposition(|queued| queued.id.is_some())
            && let Some(queued) = self.transcript.runtime.queued_user_blocks.remove(index)
            && let Some(queued_id) = queued.id
        {
            self.resources.handle.remove_block(queued_id);
        }
        self.resources
            .handle
            .recall_prompt_before_current(recalled.text.clone());
        self.resources.handle.redraw();
    }

    fn handle_agent_prompt_rejected(&mut self, rejected: &tau_proto::AgentPromptRejected) {
        let removed_queued_block = if self
            .transcript
            .runtime
            .queued_user_blocks
            .front()
            .is_some_and(|queued| queued.message_class == rejected.message_class)
            && let Some(queued) = self.transcript.runtime.queued_user_blocks.pop_front()
            && let Some(queued_id) = queued.id
        {
            self.resources.handle.remove_block(queued_id);
            true
        } else {
            false
        };
        self.transcript
            .status
            .agent_activity
            .clear_optimistic_submissions();
        self.update_agent_in_progress();
        self.resources.handle.print_output(
            "prompt-rejected",
            render_action_output_block(&self.resources.theme, &rejected.message),
        );
        if !removed_queued_block {
            self.resources.handle.redraw();
        }
    }

    fn handle_agent_prompt_steered(&mut self, steered: &tau_proto::AgentPromptSteered) {
        if self.handle_typed_internal_prompt_projection(
            steered.message_class,
            steered.internal_kind,
            &steered.text,
        ) {
            return;
        }
        if self.handle_timer_wakeup_prompt(
            steered.message_class,
            steered.ctx_id.as_deref(),
            Some(&steered.text),
        ) {
            return;
        }
        use tau_themes::names;

        if !steered.message_class.is_internal()
            && matches!(
                steered.submission_source,
                tau_proto::PromptSubmissionSource::HumanUi
                    | tau_proto::PromptSubmissionSource::Legacy
            )
            && self.front_queued_user_prompt_matches(&steered.text)
        {
            // Queue records lack a submission source. A front-exact match is
            // authoritative only for user or legacy prompt provenance; extension
            // and harness facts retain their source-aware presentation. Never
            // consume a different queued item merely because a later item has
            // the same text.
            let Some(queued) = self.transcript.runtime.queued_user_blocks.pop_front() else {
                return;
            };
            self.resources
                .handle
                .remove_block(queued.id.expect("matched queued user prompt owns a block"));
            self.resources.handle.print_output(
                "user-prompt-steered",
                self.submitted_prompt_block(names::USER_PROMPT, &queued.text),
            );
            return;
        }
        if self.handle_source_aware_prompt_projection(
            &steered.submission_source,
            steered.message_class,
            &steered.text,
        ) {
            self.resources.handle.redraw();
            return;
        }
        if steered.message_class.is_internal() {
            return;
        }

        // No matching "(queued)" block — render the steered text directly so
        // the user still sees their message land.
        self.resources.handle.print_output(
            "user-prompt-steered",
            self.submitted_prompt_block(names::USER_PROMPT, &steered.text),
        );
        self.resources.handle.redraw();
    }

    /// Returns whether `text` can promote the next queued user projection.
    ///
    /// Queue records lack provenance, so only their front entry can establish
    /// that a submitted or steered prompt is the user projection to promote.
    fn front_queued_user_prompt_matches(&self, text: &str) -> bool {
        self.front_queued_prompt_matches(text, tau_proto::PromptMessageClass::User)
    }

    /// Return whether the exact broadcast queue front matches one prompt
    /// lifecycle event.
    fn front_queued_prompt_matches(
        &self,
        text: &str,
        message_class: tau_proto::PromptMessageClass,
    ) -> bool {
        self.transcript
            .runtime
            .queued_user_blocks
            .front()
            .is_some_and(|queued| queued.text == text && queued.message_class == message_class)
    }

    fn handle_agent_prompt_created(&mut self, prompt: &tau_proto::AgentPromptCreated) {
        self.handle_agent_prompt_started(&prompt.into());
    }

    /// Records an accepted self-`compact` request only when its immutable
    /// caller, target, and tool identity prove that it owns one tool row.
    fn register_self_compaction_tool(
        &mut self,
        requested: &tau_proto::AgentManualCompactionRequested,
    ) {
        let Some(source) = requested.tool_source() else {
            return;
        };
        if source.caller_agent_id != requested.target_agent_id
            || !matches!(
                source.initiating_tool_name,
                tau_proto::ManualCompactionTool::Compact
            )
            || source.visible_tool_name.as_str() != "compact"
        {
            return;
        }
        let call_id = source.initiating_tool_call_id.clone();
        if self
            .event_owners
            .tool_agents
            .get(&call_id)
            .is_some_and(|owner| owner != &source.caller_agent_id)
        {
            return;
        }
        match self.transcript.runtime.self_compaction_tools.entry(call_id) {
            Entry::Vacant(entry) => {
                entry.insert(SelfCompactionTool {
                    request_id: requested.request_id.clone(),
                    compact_prompt_id: None,
                    transaction_id: None,
                    status: None,
                });
            }
            Entry::Occupied(entry) if entry.get().request_id == requested.request_id => {}
            Entry::Occupied(_) => {}
        }
    }

    /// Associates a standalone start with its exact accepted self-compaction
    /// call. Missing or contradictory correlation deliberately leaves both
    /// presentation paths independent.
    fn associate_self_compaction_start(
        &mut self,
        started: &tau_proto::AgentStandaloneCompactionStarted,
    ) -> Option<tau_proto::ToolCallId> {
        let tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id,
            caller_agent_id,
            initiating_tool_call_id,
        } = &started.trigger
        else {
            return None;
        };
        if started.agent_id != *caller_agent_id {
            return None;
        }
        let call_id = initiating_tool_call_id.clone();
        let tool = self
            .transcript
            .runtime
            .self_compaction_tools
            .get_mut(&call_id)?;
        if tool.request_id != *request_id
            || tool
                .compact_prompt_id
                .as_ref()
                .is_some_and(|prompt_id| prompt_id != &started.compact_prompt_id)
            || tool
                .transaction_id
                .as_ref()
                .is_some_and(|transaction_id| transaction_id != &started.transaction_id)
        {
            return None;
        }
        tool.compact_prompt_id = Some(started.compact_prompt_id.clone());
        tool.transaction_id = Some(started.transaction_id.clone());
        Some(call_id)
    }

    /// Finds the one exact self-compaction call that owns a private provider
    /// prompt. This intentionally does not infer a match from timing or names.
    fn self_compaction_tool_for_prompt(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<tau_proto::ToolCallId> {
        self.transcript
            .runtime
            .self_compaction_tools
            .iter()
            .find_map(|(call_id, tool)| {
                tool.compact_prompt_id
                    .as_ref()
                    .is_some_and(|known_prompt_id| known_prompt_id == prompt_id)
                    .then(|| call_id.clone())
            })
    }

    /// Repaints the existing generic tool row with self-compaction lifecycle
    /// state. A late reconstructed tool start receives the retained status.
    fn update_self_compaction_tool_status(
        &mut self,
        call_id: &tau_proto::ToolCallId,
        status: CompactionStatus,
        status_text: impl Into<String>,
    ) {
        let status_text = status_text.into();
        let Some(tool) = self
            .transcript
            .runtime
            .self_compaction_tools
            .get_mut(call_id)
        else {
            return;
        };
        tool.status = Some((status, status_text.clone()));

        let Some(state) = self.transcript.runtime.tool_calls.get(call_id) else {
            return;
        };
        if state.is_sub_agent {
            return;
        }
        let Some(block_id) = state.block_id else {
            return;
        };
        let mut display = render_tool_use_state(
            "compact",
            &Self::self_compaction_tool_use_state(status, status_text),
        );
        if let Some(duration) = Self::live_tool_duration(state) {
            Self::upsert_tool_duration_suffix(
                &mut display,
                duration,
                state.effective_shell_timeout,
            );
        }
        let block = self.render_live_tool_block(&display);
        self.resources.handle.set_block(block_id, block);
        if let Some(state) = self.transcript.runtime.tool_calls.get_mut(call_id) {
            state.live_display = Some(display);
        }
        self.resources.handle.redraw();
    }

    /// Separates standalone-compaction measurements from its terminal lifecycle
    /// status so generic tool-row styling treats them as information chips.
    pub(crate) fn self_compaction_tool_use_state(
        status: CompactionStatus,
        status_text: String,
    ) -> tau_proto::ToolUseState {
        let (status_text, info_chips) = match status {
            CompactionStatus::Success => status_text
                .strip_suffix(" ok")
                .filter(|metrics| !metrics.is_empty())
                .map(|metrics| ("ok".to_owned(), vec![metrics.to_owned()]))
                .unwrap_or((status_text, Vec::new())),
            CompactionStatus::Failure | CompactionStatus::Progress => (status_text, Vec::new()),
        };
        tau_proto::ToolUseState {
            status: match status {
                CompactionStatus::Failure => tau_proto::ToolUseStatus::Error,
                CompactionStatus::Success => tau_proto::ToolUseStatus::Success,
                CompactionStatus::Progress => tau_proto::ToolUseStatus::InProgress,
            },
            status_text,
            info_chips,
            ..Default::default()
        }
    }

    fn handle_agent_prompt_started(&mut self, prompt: &tau_proto::AgentPromptStarted) {
        if let Some(ctx_id) = prompt.ctx_id.as_ref()
            && let Some(index) = self
                .transcript
                .runtime
                .submitted_prompt_ctx_ids
                .iter()
                .position(|submitted| submitted == ctx_id)
        {
            self.transcript
                .runtime
                .submitted_prompt_ctx_ids
                .remove(index);
        }
        self.clear_accepted_submission_indicator();
        self.watches
            .finished_provider_prompts
            .remove(&prompt.agent_prompt_id);
        let is_standalone_compaction = matches!(
            prompt.operation,
            tau_proto::PromptOperation::StandaloneCompaction
        );
        let (already_started, had_visible_output) = {
            let state = self
                .transcript
                .runtime
                .prompts
                .entry(prompt.agent_prompt_id.clone())
                .or_default();
            if is_standalone_compaction {
                state.is_standalone_compaction = true;
            }
            (
                state.started_at.is_some(),
                state.response_block_id.is_some() || state.thinking_block_id.is_some(),
            )
        };
        if is_standalone_compaction {
            self.clear_standalone_compaction_output(&prompt.agent_prompt_id);
            if had_visible_output && prompt.originator.is_user() {
                self.set_editor_current_response(None);
            }
            if let Some(call_id) = self.self_compaction_tool_for_prompt(&prompt.agent_prompt_id) {
                self.update_self_compaction_tool_status(
                    &call_id,
                    CompactionStatus::Progress,
                    "Compacting…",
                );
                return;
            }
            self.update_live_compaction_block(
                &prompt.agent_prompt_id,
                Some((CompactionStatus::Progress, "Compacting…".to_owned())),
            );
            return;
        }
        if already_started {
            return;
        }
        self.transcript
            .runtime
            .prompts
            .entry(prompt.agent_prompt_id.clone())
            .or_default()
            .started_at = Some(Instant::now());
        self.clear_editor_current_response_for_user_prompt(prompt.originator.is_user());
        self.transcript.runtime.last_user_block = None;
        self.promote_next_queued_prompt("user-prompt-created");
    }

    fn clear_editor_current_response_for_user_prompt(&mut self, is_user_prompt: bool) {
        if is_user_prompt {
            self.set_editor_current_response(None);
        }
    }

    fn handle_agent_prompt_terminated(&mut self, terminated: &tau_proto::AgentPromptTerminated) {
        if self.finish_standalone_compaction_prompt(
            &terminated.agent_prompt_id,
            Some(("stopped", CompactionStatus::Failure)),
            false,
        ) {
            return;
        }
        self.clear_accepted_submission_indicator();
        self.clear_editor_current_response_for_user_prompt(terminated.originator.is_user());
        self.watches
            .finished_provider_prompts
            .insert(terminated.agent_prompt_id.clone());
        let Some(prompt_state) = self
            .transcript
            .runtime
            .prompts
            .remove(&terminated.agent_prompt_id)
        else {
            return;
        };
        if let Some(block_id) = prompt_state.thinking_block_id {
            self.resources.handle.remove_block(block_id);
        }
        if let Some(block_id) = prompt_state.compaction_block_id {
            self.resources.handle.remove_block(block_id);
        }
        if let Some(block_id) = prompt_state.response_block_id {
            self.resources.handle.remove_block(block_id);
        }
        self.resources.handle.redraw();
    }

    /// Removes any visible response state created before a standalone prompt's
    /// typed lifecycle fact arrived.
    fn clear_standalone_compaction_output(&mut self, prompt_id: &tau_proto::AgentPromptId) {
        let Some(state) = self.transcript.runtime.prompts.get_mut(prompt_id) else {
            return;
        };
        state.response_text_by_index.clear();
        state.thinking_text_by_index.clear();
        state.thinking_text = None;
        state.missing_response_prefix = false;
        state.missing_thinking_prefix = false;
        state.response_markdown_cache = MarkdownStreamCache::default();
        state.thinking_markdown_cache = MarkdownStreamCache::default();
        if let Some(block_id) = state.response_block_id.take() {
            self.resources.handle.remove_block(block_id);
        }
        if let Some(block_id) = state.thinking_block_id.take() {
            self.resources.handle.remove_block(block_id);
        }
        if let Some(block_id) = state.compaction_block_id.take() {
            self.resources.handle.remove_block(block_id);
        }
    }

    /// Retires one private standalone compaction prompt and, when present,
    /// replaces its live marker with a content-free terminal marker.
    fn finish_standalone_compaction_prompt(
        &mut self,
        prompt_id: &tau_proto::AgentPromptId,
        terminal: Option<(&str, CompactionStatus)>,
        render_without_state: bool,
    ) -> bool {
        let terminal = if let Some(call_id) = self.self_compaction_tool_for_prompt(prompt_id) {
            if let Some((text, status)) = terminal {
                self.update_self_compaction_tool_status(&call_id, status, text);
            }
            None
        } else {
            terminal
        };
        let Some(state) = self.transcript.runtime.prompts.remove(prompt_id) else {
            self.transcript
                .runtime
                .standalone_compaction_transactions
                .retain(|_, mapped_prompt_id| mapped_prompt_id != prompt_id);
            if render_without_state && let Some((text, status)) = terminal {
                self.resources.handle.print_output(
                    "standalone-compaction-terminal",
                    render_compaction_block(&self.resources.theme, text, status),
                );
                self.resources.handle.redraw();
                return true;
            }
            return false;
        };
        if !state.is_standalone_compaction {
            self.transcript
                .runtime
                .prompts
                .insert(prompt_id.clone(), state);
            return false;
        }
        self.transcript
            .runtime
            .standalone_compaction_transactions
            .retain(|_, mapped_prompt_id| mapped_prompt_id != prompt_id);
        self.watches
            .finished_provider_prompts
            .insert(prompt_id.clone());
        if let Some(block_id) = state.thinking_block_id {
            self.resources.handle.remove_block(block_id);
        }
        if let Some(block_id) = state.compaction_block_id {
            self.resources.handle.remove_block(block_id);
        }
        if let Some(block_id) = state.response_block_id {
            self.resources.handle.remove_block(block_id);
        }
        if let Some((text, status)) = terminal {
            self.resources.handle.print_output(
                "standalone-compaction-terminal",
                render_compaction_block(&self.resources.theme, text, status),
            );
        }
        self.resources.handle.redraw();
        true
    }

    /// Completes one standalone provider prompt in both the private transcript
    /// projection and the generic prompt-activity fallback.
    fn complete_standalone_compaction_prompt(
        &mut self,
        prompt_id: &tau_proto::AgentPromptId,
        terminal: Option<(&str, CompactionStatus)>,
    ) {
        self.finish_standalone_compaction_prompt(prompt_id, terminal, true);
        self.mark_agent_prompt_inactive(prompt_id);
        self.transcript
            .status
            .agent_activity
            .finish_prompt(prompt_id, &[]);
        if !self.transcript.status.agent_activity.has_active_prompts() {
            self.set_main_agent_turn_active(false);
        }
    }

    fn promote_next_queued_prompt(&mut self, label: &'static str) {
        use tau_themes::names;

        if let Some(queued) = self.transcript.runtime.queued_user_blocks.pop_front()
            && let Some(queued_id) = queued.id
        {
            self.resources.handle.remove_block(queued_id);
            self.resources.handle.print_output(
                label,
                self.submitted_prompt_block(names::USER_PROMPT, &queued.text),
            );
        }
    }

    fn handle_provider_response_events(&mut self, prepared: &PreparedRendererEvent<'_>) -> bool {
        let event = prepared.event();
        match event {
            Event::ProviderPromptSubmitted(submitted) => {
                self.handle_provider_prompt_submitted(submitted);
                true
            }
            Event::ProviderResponseUpdated(update) => {
                self.handle_provider_response_updated(update);
                true
            }
            Event::ProviderResponseFinished(finished) => {
                self.finish_compaction_continuation_measurement(finished);
                self.handle_provider_response_finished(
                    finished,
                    prepared
                        .finished()
                        .expect("provider terminal preserves projection")
                        .1,
                );
                true
            }
            _ => false,
        }
    }

    /// Consumes the first inference owned by one completed compaction and
    /// repaints its stable history row from exact provider input usage.
    fn finish_compaction_continuation_measurement(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) {
        let Some(transaction_id) = self
            .transcript
            .runtime
            .compaction_continuation_prompts
            .remove(&finished.agent_prompt_id)
        else {
            return;
        };
        let Some(presentation) = self
            .transcript
            .runtime
            .completed_compactions
            .remove(&transaction_id)
        else {
            return;
        };
        let Some(after) = finished
            .usage
            .as_ref()
            .map(|usage| tau_proto::TokenCount::new(usage.prompt_sent_tokens))
        else {
            return;
        };
        let status = Self::standalone_compaction_success_status(
            presentation.original_input_tokens,
            Some(after),
        );
        if let Some(call_id) = presentation.self_tool_call_id.as_ref() {
            self.update_self_compaction_tool_status(
                call_id,
                CompactionStatus::Success,
                status.clone(),
            );
        }
        let Some(block_id) = presentation.block_id else {
            return;
        };
        if presentation.self_tool_call_id.is_none() {
            self.resources.handle.set_block(
                block_id,
                render_compaction_block(&self.resources.theme, &status, CompactionStatus::Success),
            );
            self.resources.handle.redraw();
            return;
        }
        let Some(entry_index) = self
            .transcript
            .history
            .tool_history
            .iter()
            .position(|entry| entry.block_id == block_id)
        else {
            return;
        };
        let time_suffixes = self
            .transcript
            .history
            .tool_history
            .get(entry_index)
            .expect("known tool history index")
            .display
            .suffixes
            .iter()
            .filter(|suffix| matches!(suffix.status, crate::tool_render::ToolStatus::Time))
            .cloned()
            .collect::<Vec<_>>();
        let mut display = render_tool_use_state(
            "compact",
            &Self::self_compaction_tool_use_state(CompactionStatus::Success, status),
        );
        display.suffixes.extend(time_suffixes);
        let block = self.render_tool_history_block(&display);
        self.resources.handle.set_block(block_id, block);
        self.transcript.history.tool_history[entry_index].display = display;
        self.resources.handle.redraw();
    }

    fn handle_provider_prompt_submitted(&mut self, submitted: &tau_proto::ProviderPromptSubmitted) {
        self.watches
            .finished_provider_prompts
            .remove(&submitted.agent_prompt_id);
        self.transcript
            .runtime
            .prompts
            .entry(submitted.agent_prompt_id.clone())
            .or_default()
            .started_at = Some(Instant::now());
    }

    fn handle_provider_response_updated(&mut self, update: &tau_proto::ProviderResponseUpdated) {
        let spid = &update.agent_prompt_id;
        self.event_owners
            .prompt_agents
            .entry(update.agent_prompt_id.clone())
            .or_insert_with(|| update.agent_id.clone());
        if self
            .transcript
            .runtime
            .prompts
            .get(spid)
            .is_some_and(|state| state.is_standalone_compaction)
        {
            return;
        }
        if self.is_stale_terminal_stats_only_update(update) {
            return;
        }
        if !provider_response_update_has_visible_content(update)
            && self
                .event_owners
                .prompt_agents
                .get(&update.agent_prompt_id)
                .is_some_and(|agent_id| agent_id != &update.agent_id)
        {
            return;
        }
        // The first empty update creates its pending response marker. Once it
        // exists, another completely empty sample cannot alter the transcript,
        // status, editor context, or terminal frame.
        if update.deltas.is_empty()
            && update.compaction.is_none()
            && update.status.is_none()
            && update.response_stats.is_none()
            && self
                .transcript
                .runtime
                .prompts
                .get(spid)
                .is_some_and(|state| state.response_block_id.is_some())
        {
            return;
        }
        self.ensure_live_response_block_for_update(update);
        if let Some(stats) = update.response_stats {
            self.transcript
                .runtime
                .prompts
                .entry(spid.clone())
                .or_default()
                .provider_response_stats = Some(stats);
        }
        if let Some(status) = &update.status {
            if status.clear_response {
                self.clear_live_response_accumulators(spid);
            }
            self.update_editor_current_response(update, "");
            self.update_live_compaction_block(spid, update_compaction_status(update));
            if update.deltas.is_empty() {
                self.update_live_response_block(spid, &status.text, MarkdownStreamUpdate::Replace);
                if let Some(state) = self.transcript.runtime.prompts.get_mut(spid) {
                    // Status text is transient and is not the prefix of the next
                    // accumulated assistant snapshot.
                    state.response_markdown_cache = MarkdownStreamCache::default();
                }
                return;
            }
        }
        let (text, response_update, thinking_update) = self.accumulate_response_update(update);
        self.update_editor_current_response(update, &text);
        self.update_live_thinking_block(spid, thinking_update);
        self.update_live_compaction_block(spid, update_compaction_status(update));
        self.update_live_response_block(spid, &text, response_update);
    }

    fn is_stale_terminal_stats_only_update(
        &self,
        update: &tau_proto::ProviderResponseUpdated,
    ) -> bool {
        !provider_response_update_has_visible_content(update)
            && self
                .watches
                .finished_provider_prompts
                .contains(&update.agent_prompt_id)
    }

    fn clear_live_response_accumulators(&mut self, spid: &tau_proto::AgentPromptId) {
        if let Some(state) = self.transcript.runtime.prompts.get_mut(spid) {
            state.response_text_by_index.clear();
            state.thinking_text_by_index.clear();
            state.thinking_text = None;
            state.missing_response_prefix = false;
            state.missing_thinking_prefix = false;
            state.response_markdown_cache = MarkdownStreamCache::default();
            state.thinking_markdown_cache = MarkdownStreamCache::default();
            state.provider_response_stats = None;
            state.live_response_is_pending_indicator = false;
            if let Some(block_id) = state.thinking_block_id.take() {
                self.resources.handle.remove_block(block_id);
            }
        }
    }

    fn ensure_live_response_block_for_update(
        &mut self,
        update: &tau_proto::ProviderResponseUpdated,
    ) {
        self.ensure_live_response_block_for_prompt(&update.agent_prompt_id);
    }

    fn ensure_live_response_block_for_prompt(&mut self, spid: &tau_proto::AgentPromptId) {
        use std::collections::hash_map::Entry;

        use tau_themes::names;

        let (state, prompt_was_unknown) = match self.transcript.runtime.prompts.entry(spid.clone())
        {
            Entry::Occupied(entry) => (entry.into_mut(), false),
            Entry::Vacant(entry) => (entry.insert(PromptState::default()), true),
        };
        if state.response_block_id.is_some() {
            return;
        }
        if prompt_was_unknown {
            state.missing_response_prefix = true;
            state.missing_thinking_prefix = true;
        }
        let block = streaming_block(
            &self.resources.theme,
            names::AGENT_PENDING,
            STREAMING_AGENT_RESPONSE_PREFIX.trim_end(),
        );
        let id = self
            .resources
            .handle
            .new_block(format!("agent-response-live:{spid}"), block);
        self.push_live_response_block(id);
        self.resources.handle.redraw();
        self.transcript
            .runtime
            .prompts
            .entry(spid.clone())
            .or_default()
            .response_block_id = Some(id);
    }

    fn accumulate_response_update(
        &mut self,
        update: &tau_proto::ProviderResponseUpdated,
    ) -> (String, MarkdownStreamUpdate, MarkdownStreamUpdate) {
        use std::ops::Bound;

        let state = self
            .transcript
            .runtime
            .prompts
            .entry(update.agent_prompt_id.clone())
            .or_default();
        let mut response_update = MarkdownStreamUpdate::Append;
        let mut thinking_update = MarkdownStreamUpdate::Append;
        for delta in &update.deltas {
            match delta {
                ProviderResponseTextDelta::Message {
                    output_index, text, ..
                } => {
                    if state
                        .response_text_by_index
                        .range((Bound::Excluded(*output_index), Bound::Unbounded))
                        .any(|(_, text)| !text.is_empty())
                    {
                        response_update = MarkdownStreamUpdate::Replace;
                    }
                    state
                        .response_text_by_index
                        .entry(*output_index)
                        .or_default()
                        .push_str(text);
                }
                ProviderResponseTextDelta::ReasoningText {
                    output_index, text, ..
                } => {
                    if state
                        .thinking_text_by_index
                        .range((Bound::Excluded(*output_index), Bound::Unbounded))
                        .any(|(_, text)| !text.is_empty())
                    {
                        thinking_update = MarkdownStreamUpdate::Replace;
                    }
                    state
                        .thinking_text_by_index
                        .entry(*output_index)
                        .or_default()
                        .push_str(text);
                }
            }
        }
        let text = join_indexed_text(&state.response_text_by_index, state.missing_response_prefix);
        let thinking =
            join_indexed_text(&state.thinking_text_by_index, state.missing_thinking_prefix);
        state.thinking_text = (!thinking.is_empty()).then_some(thinking);
        (text, response_update, thinking_update)
    }

    fn update_editor_current_response(
        &mut self,
        update: &tau_proto::ProviderResponseUpdated,
        text: &str,
    ) {
        if update.originator.is_user() {
            self.set_editor_current_response((!text.is_empty()).then(|| text.to_owned()));
        }
    }

    fn update_live_thinking_block(
        &mut self,
        spid: &tau_proto::AgentPromptId,
        update: MarkdownStreamUpdate,
    ) {
        use tau_themes::names;

        let Some(state) = self.transcript.runtime.prompts.get_mut(spid) else {
            return;
        };
        let Some(thinking) = state.thinking_text.as_deref() else {
            return;
        };
        if !self.presentation.verbose_mode || !self.presentation.show_thinking {
            if update == MarkdownStreamUpdate::Replace {
                state.thinking_markdown_cache = MarkdownStreamCache::default();
            }
            return;
        }
        let block = markdown_streaming_block_with_osc8(
            &self.resources.theme,
            names::AGENT_THINKING,
            thinking,
            &mut state.thinking_markdown_cache,
            update,
            self.presentation.osc8_links,
        );
        let existing_tbid = state.thinking_block_id;
        if let Some(tbid) = existing_tbid {
            self.resources.handle.set_block(tbid, block);
        } else {
            self.insert_live_thinking_block(spid, block);
        }
        self.resources.handle.redraw();
    }

    fn insert_live_thinking_block(
        &mut self,
        spid: &tau_proto::AgentPromptId,
        block: tau_cli_term::StyledBlock,
    ) {
        // Insert thinking above the live compaction/response stack while keeping
        // any active tool-call UI pinned below the whole streaming response.
        let tbid = self
            .resources
            .handle
            .new_block(format!("agent-thinking-live:{spid}"), block);
        let anchors = self.live_thinking_anchor_ids(spid);
        self.resources
            .handle
            .push_above_active_before_any(tbid, anchors);
        self.transcript
            .runtime
            .prompts
            .entry(spid.clone())
            .or_default()
            .thinking_block_id = Some(tbid);
    }

    fn update_live_compaction_block(
        &mut self,
        spid: &tau_proto::AgentPromptId,
        status: Option<(CompactionStatus, String)>,
    ) {
        let Some((status, text)) = status else {
            self.remove_live_compaction_block(spid);
            return;
        };
        let block = render_compaction_block(&self.resources.theme, text, status);
        let existing_id = self
            .transcript
            .runtime
            .prompts
            .get(spid)
            .and_then(|s| s.compaction_block_id);
        if let Some(block_id) = existing_id {
            self.resources.handle.set_block(block_id, block);
        } else {
            self.insert_live_compaction_block(spid, block);
        }
        self.resources.handle.redraw();
    }

    fn remove_live_compaction_block(&mut self, spid: &tau_proto::AgentPromptId) {
        let Some(block_id) = self
            .transcript
            .runtime
            .prompts
            .get_mut(spid)
            .and_then(|state| state.compaction_block_id.take())
        else {
            return;
        };
        self.resources.handle.remove_block(block_id);
        self.resources.handle.redraw();
    }

    fn insert_live_compaction_block(
        &mut self,
        spid: &tau_proto::AgentPromptId,
        block: tau_cli_term::StyledBlock,
    ) {
        let block_id = self
            .resources
            .handle
            .new_block(format!("agent-compaction-live:{spid}"), block);
        let anchors = self.live_compaction_anchor_ids(spid);
        self.resources
            .handle
            .push_above_active_before_any(block_id, anchors);
        self.transcript
            .runtime
            .prompts
            .entry(spid.clone())
            .or_default()
            .compaction_block_id = Some(block_id);
    }

    fn push_live_response_block(&self, block_id: tau_cli_term::BlockId) {
        let mut anchors = self.active_tool_anchor_ids();
        anchors.extend(self.non_tool_activity_anchor_ids());
        self.resources
            .handle
            .push_above_active_before_any(block_id, anchors);
    }

    fn live_thinking_anchor_ids(
        &self,
        spid: &tau_proto::AgentPromptId,
    ) -> Vec<tau_cli_term::BlockId> {
        let mut anchors = Vec::new();
        if let Some(state) = self.transcript.runtime.prompts.get(spid) {
            if let Some(block_id) = state.compaction_block_id {
                anchors.push(block_id);
            }
            if let Some(block_id) = state.response_block_id {
                anchors.push(block_id);
            }
        }
        anchors.extend(self.active_tool_anchor_ids());
        anchors
    }

    fn live_compaction_anchor_ids(
        &self,
        spid: &tau_proto::AgentPromptId,
    ) -> Vec<tau_cli_term::BlockId> {
        let mut anchors = Vec::new();
        if let Some(state) = self.transcript.runtime.prompts.get(spid)
            && let Some(block_id) = state.response_block_id
        {
            anchors.push(block_id);
        }
        anchors.extend(self.active_tool_anchor_ids());
        anchors
    }

    fn active_tool_anchor_ids(&self) -> Vec<tau_cli_term::BlockId> {
        let mut anchors = Vec::new();
        if self.transcript.status.prompt_tool_summary_active
            && let Some(block_id) = self.transcript.status.prompt_tool_summary
        {
            anchors.push(block_id);
        }
        for state in self.transcript.runtime.tool_calls.values() {
            if let Some(block_id) = state.summary_block_id {
                anchors.push(block_id);
            }
            if let Some(block_id) = state.block_id {
                anchors.push(block_id);
            }
        }
        anchors
    }

    fn update_live_response_block(
        &mut self,
        spid: &tau_proto::AgentPromptId,
        text: &str,
        update: MarkdownStreamUpdate,
    ) {
        use tau_themes::names;

        let verbose_mode = self.presentation.verbose_mode;
        if let Some(bid) = self
            .transcript
            .runtime
            .prompts
            .get(spid)
            .and_then(|s| s.response_block_id)
        {
            let Some(state) = self.transcript.runtime.prompts.get_mut(spid) else {
                return;
            };
            let block = if text.is_empty() {
                state.live_response_is_pending_indicator = true;
                streaming_block_with_indicator_suffix(
                    &self.resources.theme,
                    names::AGENT_PENDING,
                    STREAMING_AGENT_RESPONSE_PREFIX.trim_end(),
                    response_stats_indicator_for_prompt(state, verbose_mode),
                )
            } else {
                state.live_response_is_pending_indicator = false;
                markdown_prefixed_streaming_block_with_osc8(
                    &self.resources.theme,
                    names::AGENT_RESPONSE,
                    STREAMING_AGENT_RESPONSE_PREFIX,
                    text,
                    &mut state.response_markdown_cache,
                    update,
                    self.presentation.osc8_links,
                )
            };
            self.resources.handle.set_block(bid, block);
            self.resources.handle.redraw();
        }
    }

    fn handle_tool_started(&mut self, started: &tau_proto::ToolStarted, recorded_at: UnixMicros) {
        let call_id = started.call_id.clone();
        self.event_owners
            .tool_agents
            .entry(started.call_id.clone())
            .or_insert_with(|| started.agent_id.clone());
        if self
            .transcript
            .runtime
            .tool_calls
            .get(&call_id)
            .is_some_and(|state| state.is_sub_agent || state.block_id.is_some())
        {
            return;
        }
        let is_blocker = is_blocker_tool_name(started.tool_name.as_str());
        let blocker_action = blocker_action_descriptor(started);
        let effective_shell_timeout = effective_shell_timeout(started);
        let mut display = pending_tool_call_display(started.tool_name.as_str());
        sanitize_blocker_display(&mut display, is_blocker, blocker_action);
        Self::upsert_tool_duration_suffix(&mut display, Duration::ZERO, effective_shell_timeout);
        let live_block = self.render_live_tool_block(&display);
        let live_id = self.resources.handle.new_block(
            format!("tool-call-live:{}:{}", started.tool_name, started.call_id),
            live_block,
        );
        self.resources
            .handle
            .push_above_active_before_any(live_id, self.non_tool_activity_anchor_ids());
        let state = self
            .transcript
            .runtime
            .tool_calls
            .entry(call_id)
            .or_insert_with(|| {
                let history_id = self.resources.handle.new_block(
                    format!(
                        "tool-call-history:{}:{}",
                        started.tool_name, started.call_id
                    ),
                    Self::empty_block(),
                );
                self.resources.handle.push_history(history_id);
                ToolCallState {
                    history_block_id: Some(history_id),
                    is_main_delegate: started.tool_name.as_str() == AGENT_START_TOOL_NAME,
                    blocker_action,
                    is_blocker,
                    ..ToolCallState::default()
                }
            });
        state.blocker_action = blocker_action;
        state.is_blocker = is_blocker;
        state.effective_shell_timeout = effective_shell_timeout;
        state.block_id = Some(live_id);
        state.live_display = Some(display);
        state.started_at = Some(Instant::now());
        state.recorded_started_at = Some(recorded_at);
        let history_block_id = state.history_block_id;
        for presentation in self
            .transcript
            .runtime
            .completed_compactions
            .values_mut()
            .filter(|presentation| {
                presentation.self_tool_call_id.as_ref() == Some(&started.call_id)
            })
        {
            presentation.block_id = history_block_id;
        }
        if let Some(timer) = &self.activity.tool_timer {
            timer.tool_started(&started.call_id);
        }
        if let Some((status, status_text)) = self
            .transcript
            .runtime
            .self_compaction_tools
            .get(&started.call_id)
            .and_then(|tool| tool.status.clone())
        {
            self.update_self_compaction_tool_status(&started.call_id, status, status_text);
        }
    }

    fn handle_tool_events(&mut self, event: &Event, recorded_at: UnixMicros) -> bool {
        match event {
            Event::ToolStarted(started) => {
                self.handle_tool_started(started, recorded_at);
                true
            }
            Event::ToolProgress(progress) => {
                self.handle_tool_progress(progress);
                true
            }
            Event::ProviderToolResult(result) => {
                self.handle_tool_result_fields(
                    &result.call_id,
                    &result.tool_name,
                    result.kind,
                    result.display.as_ref(),
                    result.originator.is_user(),
                    recorded_at,
                );
                true
            }
            Event::ProviderToolError(_) => true,
            Event::ToolResultDisplay(result) => {
                self.handle_tool_result(result, recorded_at);
                true
            }
            Event::ToolResult(result) => {
                self.handle_tool_result_fields(
                    &result.call_id,
                    &result.tool_name,
                    result.kind,
                    result.display.as_ref(),
                    result.originator.is_user(),
                    recorded_at,
                );
                true
            }
            Event::ToolError(error) => {
                self.handle_tool_error(error, recorded_at);
                true
            }
            Event::ToolBackgroundResultDisplay(result) => {
                self.handle_tool_background_result(result, recorded_at);
                true
            }
            Event::ToolBackgroundResult(result) => {
                self.handle_tool_result_fields(
                    &result.call_id,
                    &result.tool_name,
                    tau_proto::ToolResultKind::Final,
                    result.display.as_ref(),
                    result.originator.is_user(),
                    recorded_at,
                );
                true
            }
            Event::ToolBackgroundError(error) => {
                self.handle_tool_background_error(error, recorded_at);
                true
            }
            Event::ToolCancelled(cancelled) => {
                self.handle_tool_cancelled(cancelled, recorded_at);
                true
            }
            _ => false,
        }
    }

    fn handle_tool_progress(&mut self, progress: &tau_proto::ToolProgress) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        let state = self.transcript.runtime.tool_calls.get(&progress.call_id);
        if state.is_some_and(|s| s.is_sub_agent) {
            return;
        }

        if let Some(progress_display) = progress.display.as_ref() {
            let mut update = None;
            let freeze_multiline_payloads = self.freeze_multiline_live_payloads();
            if let Some(state) = self
                .transcript
                .runtime
                .tool_calls
                .get_mut(&progress.call_id)
                && let Some(block_id) = state.block_id
            {
                let mut display = if state.is_blocker {
                    pending_tool_call_display(&progress.tool_name)
                } else {
                    render_tool_use_state(&progress.tool_name, progress_display)
                };
                sanitize_blocker_display(&mut display, state.is_blocker, state.blocker_action);
                if Self::use_static_live_duration(freeze_multiline_payloads, &display) {
                    Self::upsert_static_tool_duration_suffix(
                        &mut display,
                        state.effective_shell_timeout,
                    );
                } else if let Some(duration) = Self::live_tool_duration(state) {
                    Self::upsert_tool_duration_suffix(
                        &mut display,
                        duration,
                        state.effective_shell_timeout,
                    );
                }
                if state.live_display.as_ref() == Some(&display) {
                    return;
                }
                state.live_display = Some(display.clone());
                update = Some((block_id, display));
            }
            if let Some((block_id, display)) = update {
                let block = self.render_live_tool_block(&display);
                self.resources.handle.set_block(block_id, block);
                if self.transcript.runtime.model_status_block.is_some() {
                    self.render_model_status();
                } else {
                    self.resources.handle.redraw();
                }
                return;
            }
        }

        let state = self.transcript.runtime.tool_calls.get(&progress.call_id);
        if state.is_none_or(|s| s.block_id.is_none()) {
            if !self.presentation.verbose_mode {
                return;
            }
            let text = tau_harness::format_tool_progress(progress);
            self.resources.handle.print_output(
                "tool-progress",
                themed_block(&self.resources.theme, names::SHELL_OUTPUT, text),
            );
        }
    }

    pub(crate) fn handle_tool_timer_tick(&mut self) {
        let mut changed = false;
        let mut updates = Vec::new();
        for (call_id, state) in &self.transcript.runtime.tool_calls {
            let (Some(block_id), Some(display)) = (state.block_id, state.live_display.as_ref())
            else {
                continue;
            };
            if Self::use_static_live_duration(self.freeze_multiline_live_payloads(), display) {
                continue;
            }
            let Some(duration) = Self::live_tool_duration(state) else {
                continue;
            };
            let mut display = display.clone();
            Self::upsert_tool_duration_suffix(
                &mut display,
                duration,
                state.effective_shell_timeout,
            );
            if state
                .live_display
                .as_ref()
                .is_some_and(|current| Self::tool_displays_match_time(current, &display))
            {
                continue;
            }
            updates.push((call_id.clone(), block_id, display));
        }
        for (call_id, block_id, display) in updates {
            if let Some(state) = self.transcript.runtime.tool_calls.get_mut(&call_id) {
                state.live_display = Some(display.clone());
            }
            let block = self.render_live_tool_block(&display);
            self.resources.handle.set_block(block_id, block);
            changed = true;
        }
        if changed {
            self.resources.handle.redraw();
        }
        let quota_tick_due = self
            .role
            .last_quota_tick
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(60));
        if quota_tick_due {
            self.role.last_quota_tick = Some(Instant::now());
            self.render_model_status_if_present();
        }
    }

    fn freeze_multiline_live_payloads(&self) -> bool {
        matches!(
            self.presentation.show_tools,
            tau_config::settings::ShowTools::Full
        )
    }

    fn use_static_live_duration(
        freeze_multiline_payloads: bool,
        display: &ToolCallDisplay,
    ) -> bool {
        if !freeze_multiline_payloads {
            return false;
        }
        display.payload.as_ref().is_some_and(|payload| {
            matches!(payload, tau_proto::ToolUsePayload::Text { text } if text.contains('\n'))
        })
    }

    fn normalize_live_tool_duration(
        freeze_multiline_payloads: bool,
        display: &mut ToolCallDisplay,
        duration: Option<Duration>,
        timeout: Option<Duration>,
    ) {
        if Self::use_static_live_duration(freeze_multiline_payloads, display) {
            Self::upsert_static_tool_duration_suffix(display, timeout);
        } else if let Some(duration) = duration {
            Self::upsert_tool_duration_suffix(display, duration, timeout);
        }
    }

    fn tool_displays_match_time(current: &ToolCallDisplay, next: &ToolCallDisplay) -> bool {
        use crate::tool_render::ToolStatus;

        let current_time = current
            .suffixes
            .iter()
            .find(|suffix| matches!(suffix.status, ToolStatus::Time))
            .map(|suffix| suffix.text.as_str());
        let next_time = next
            .suffixes
            .iter()
            .find(|suffix| matches!(suffix.status, ToolStatus::Time))
            .map(|suffix| suffix.text.as_str());
        current_time == next_time
    }

    fn live_tool_duration(state: &ToolCallState) -> Option<Duration> {
        if let Some(recorded_started_at) = state.recorded_started_at {
            let elapsed_micros = UnixMicros::now()
                .get()
                .checked_sub(recorded_started_at.get())?;
            return Some(Duration::from_micros(elapsed_micros));
        }
        state.started_at.map(|started_at| started_at.elapsed())
    }

    fn upsert_tool_duration_suffix(
        display: &mut ToolCallDisplay,
        duration: Duration,
        timeout: Option<Duration>,
    ) {
        let mut suffix = tool_duration_suffix(duration);
        if let Some(timeout) = timeout {
            suffix.text = format!("{}/{}s", duration.as_secs(), timeout.as_secs());
        }
        Self::upsert_tool_duration_suffix_segment(display, suffix);
    }

    fn upsert_static_tool_duration_suffix(
        display: &mut ToolCallDisplay,
        timeout: Option<Duration>,
    ) {
        let mut suffix = tool_duration_suffix(Duration::ZERO);
        suffix.text = timeout
            .map(|timeout| format!("-/{}s", timeout.as_secs()))
            .unwrap_or_else(|| "-s".to_owned());
        Self::upsert_tool_duration_suffix_segment(display, suffix);
    }

    fn upsert_tool_duration_suffix_segment(
        display: &mut ToolCallDisplay,
        mut suffix: crate::tool_render::ToolLineSegment,
    ) {
        use crate::tool_render::ToolStatus;

        display
            .suffixes
            .retain(|suffix| !matches!(suffix.status, ToolStatus::Time));

        let insert_at = display
            .suffixes
            .iter()
            .position(|suffix| {
                matches!(
                    suffix.status,
                    ToolStatus::Success
                        | ToolStatus::Warning
                        | ToolStatus::Error
                        | ToolStatus::Pending
                        | ToolStatus::Progress
                )
            })
            .unwrap_or(display.suffixes.len());

        if insert_at == 0
            && display
                .args
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            suffix.no_leading_space = true;
        }
        if let Some(next) = display.suffixes.get_mut(insert_at)
            && matches!(next.status, ToolStatus::Progress)
        {
            next.no_leading_space = false;
        }

        display.suffixes.insert(insert_at, suffix);
    }

    fn take_finished_tool_call(
        &mut self,
        call_id: &tau_proto::ToolCallId,
        originator_is_user: bool,
    ) -> Option<(ToolCallState, bool)> {
        let prior = self.transcript.runtime.tool_calls.remove(call_id);
        let known_main_tool = prior
            .as_ref()
            .is_some_and(|prior| !prior.is_sub_agent && originator_is_user);
        let prior = prior.unwrap_or_default();
        if prior.is_sub_agent {
            return None;
        }
        if let Some(block_id) = prior.block_id {
            if let Some(timer) = &self.activity.tool_timer {
                timer.tool_finished(call_id);
            }
            self.resources.handle.remove_block(block_id);
        }
        if known_main_tool {
            self.transcript
                .status
                .main_backgrounded_tools
                .remove(call_id);
            self.record_main_tool_completed();
            if self.transcript.status.main_agent_turn_active
                || !self.transcript.status.main_backgrounded_tools.is_empty()
            {
                self.transcript.status.main_tools_visible = true;
            }
        }
        Some((prior, known_main_tool))
    }

    fn handle_tool_result(
        &mut self,
        result: &tau_proto::ToolResultDisplay,
        recorded_at: UnixMicros,
    ) {
        self.handle_tool_result_fields(
            &result.call_id,
            &result.tool_name,
            result.kind,
            result.display.as_ref(),
            result.originator.is_user(),
            recorded_at,
        );
    }

    /// Projects borrowed success-terminal fields without constructing another
    /// complete result DTO around a potentially large display payload.
    fn handle_tool_result_fields(
        &mut self,
        call_id: &tau_proto::ToolCallId,
        tool_name: &tau_proto::ToolName,
        kind: tau_proto::ToolResultKind,
        descriptor: Option<&tau_proto::ToolUseState>,
        originator_is_user: bool,
        recorded_at: UnixMicros,
    ) {
        #[cfg(test)]
        observe_tool_terminal_descriptor(descriptor, None);
        if kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
            self.handle_tool_background_placeholder(call_id);
            return;
        }
        // Sub-agent tool activity stays out of the user's transcript; generic
        // watched-agent stats provide the live activity signal.
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(call_id, originator_is_user)
        else {
            return;
        };
        for presentation in self
            .transcript
            .runtime
            .completed_compactions
            .values_mut()
            .filter(|presentation| presentation.self_tool_call_id.as_ref() == Some(call_id))
        {
            presentation.block_id = prior.history_block_id;
        }
        let is_blocker = prior.is_blocker || is_blocker_tool_name(tool_name.as_str());
        let diff = (!is_blocker)
            .then(|| Self::tool_result_diff(descriptor))
            .flatten();
        let mut display = if let Some((status, status_text)) = self
            .transcript
            .runtime
            .self_compaction_tools
            .get(call_id)
            .and_then(|tool| tool.status.clone())
            .filter(|(status, _)| matches!(status, CompactionStatus::Success))
        {
            render_tool_use_state(
                "compact",
                &Self::self_compaction_tool_use_state(status, status_text),
            )
        } else if is_blocker {
            render_tool_use_state(tool_name, &synthesize_fallback_display(tool_name, None))
        } else {
            Self::tool_result_display(tool_name, descriptor, diff.as_ref())
        };
        sanitize_blocker_display(&mut display, is_blocker, prior.blocker_action);
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(
                &mut display,
                duration,
                prior.effective_shell_timeout,
            );
        }
        self.record_tool_summary_result(
            prior.summary_block_id,
            (!is_blocker).then_some(descriptor).flatten(),
            diff.as_ref(),
            false,
        );
        self.record_tool_result_block(prior.history_block_id, display, diff);
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn handle_tool_background_placeholder(&mut self, call_id: &tau_proto::ToolCallId) {
        let Some(state) = self.transcript.runtime.tool_calls.get(call_id) else {
            return;
        };
        if state.is_sub_agent {
            return;
        }
        self.transcript
            .status
            .main_backgrounded_tools
            .insert(call_id.clone());
        self.transcript.status.main_tools_visible = true;
        self.render_model_status();
    }

    fn handle_tool_background_result(
        &mut self,
        result: &tau_proto::ToolBackgroundResultDisplay,
        recorded_at: UnixMicros,
    ) {
        self.handle_tool_result_fields(
            &result.call_id,
            &result.tool_name,
            tau_proto::ToolResultKind::Final,
            result.display.as_ref(),
            result.originator.is_user(),
            recorded_at,
        );
    }

    fn tool_result_display(
        tool_name: &tau_proto::ToolName,
        descriptor: Option<&tau_proto::ToolUseState>,
        diff: Option<&tau_proto::ToolUsePayload>,
    ) -> ToolCallDisplay {
        let descriptor = match (descriptor, diff) {
            (Some(descriptor), Some(_)) => Self::clone_tool_use_header(descriptor),
            (Some(descriptor), None) => descriptor.clone(),
            (None, _) => synthesize_fallback_display(tool_name, None),
        };
        let descriptor =
            normalize_terminal_tool_use_state(descriptor, TerminalToolOutcome::SuccessResult);
        if let Some(diff) = diff {
            render_tool_use_state_payload_free(tool_name, &descriptor, diff)
        } else {
            render_tool_use_state(tool_name, &descriptor)
        }
    }

    /// Clones lightweight terminal metadata without cloning its separately
    /// retained rich payload.
    fn clone_tool_use_header(display: &tau_proto::ToolUseState) -> tau_proto::ToolUseState {
        tau_proto::ToolUseState {
            args: display.args.clone(),
            mode: display.mode.clone(),
            range: display.range.clone(),
            stats: display.stats,
            progress_counters: display.progress_counters.clone(),
            info_chips: display.info_chips.clone(),
            status: display.status,
            status_text: display.status_text.clone(),
            payload: None,
        }
    }

    fn finished_tool_duration(prior: &ToolCallState, finished_at: UnixMicros) -> Option<Duration> {
        let started_at = prior.recorded_started_at?;
        let elapsed_micros = finished_at.get().checked_sub(started_at.get())?;
        Some(Duration::from_micros(elapsed_micros))
    }

    fn diff_payload_has_changes(payload: &tau_proto::ToolUsePayload) -> bool {
        let (added, removed) = diff_payload_counts(payload);
        0 < added || 0 < removed
    }

    fn tool_result_diff(
        descriptor: Option<&tau_proto::ToolUseState>,
    ) -> Option<tau_proto::ToolUsePayload> {
        descriptor.and_then(|d| match &d.payload {
            Some(payload) if Self::diff_payload_has_changes(payload) => Some(payload.clone()),
            _ => None,
        })
    }

    fn record_tool_result_block(
        &mut self,
        existing_block_id: Option<tau_cli_term::BlockId>,
        display: ToolCallDisplay,
        diff: Option<tau_proto::ToolUsePayload>,
    ) {
        if let Some(diff) = diff {
            let block = self.render_diff_history_block(&display, &diff);
            let bid =
                self.update_existing_or_print_tool_block(existing_block_id, "tool-diff", block);
            self.transcript.history.diff_blocks.push(DiffBlockEntry {
                block_id: bid,
                display,
                diff,
            });
        } else {
            let block = self.render_tool_history_block(&display);
            let bid =
                self.update_existing_or_print_tool_block(existing_block_id, "tool-result", block);
            self.transcript.history.tool_history.push(ToolBlockEntry {
                block_id: bid,
                display,
            });
        }
    }

    fn handle_tool_error(&mut self, error: &tau_proto::ToolError, recorded_at: UnixMicros) {
        self.handle_tool_error_fields(
            BorrowedToolError {
                call_id: &error.call_id,
                tool_name: &error.tool_name,
                message: &error.message,
                details: error.details.as_ref(),
                descriptor: error.display.as_ref(),
                originator_is_user: error.originator.is_user(),
            },
            recorded_at,
        );
    }

    /// Projects borrowed error-terminal fields without constructing another
    /// complete error DTO around display payloads or structured details.
    fn handle_tool_error_fields(&mut self, error: BorrowedToolError<'_>, recorded_at: UnixMicros) {
        #[cfg(test)]
        observe_tool_terminal_descriptor(error.descriptor, error.details);
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(error.call_id, error.originator_is_user)
        else {
            return;
        };
        let is_blocker = prior.is_blocker || is_blocker_tool_name(error.tool_name.as_str());
        let mut display = if is_blocker {
            render_tool_use_state(
                error.tool_name,
                &synthesize_fallback_display(error.tool_name, Some("failed")),
            )
        } else {
            Self::tool_error_display_fields(
                error.tool_name,
                error.message,
                error.details,
                error.descriptor,
            )
        };
        sanitize_blocker_display(&mut display, is_blocker, prior.blocker_action);
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(
                &mut display,
                duration,
                prior.effective_shell_timeout,
            );
        }
        self.record_tool_summary_result(
            prior.summary_block_id,
            (!is_blocker).then_some(error.descriptor).flatten(),
            None,
            true,
        );
        self.record_plain_finished_tool_block(prior.history_block_id, display, "tool-error");
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn handle_tool_background_error(
        &mut self,
        error: &tau_proto::ToolBackgroundError,
        recorded_at: UnixMicros,
    ) {
        self.handle_tool_error_fields(
            BorrowedToolError {
                call_id: &error.call_id,
                tool_name: &error.tool_name,
                message: &error.message,
                details: error.details.as_ref(),
                descriptor: error.display.as_ref(),
                originator_is_user: error.originator.is_user(),
            },
            recorded_at,
        );
    }

    fn tool_error_display_fields(
        tool_name: &tau_proto::ToolName,
        message: &str,
        details: Option<&CborValue>,
        display: Option<&tau_proto::ToolUseState>,
    ) -> ToolCallDisplay {
        let descriptor = if tool_name.as_str() == AGENT_START_TOOL_NAME {
            if let Some(descriptor) = display {
                descriptor.clone()
            } else {
                build_delegate_completion_display(
                    None,
                    details.unwrap_or(&CborValue::Null),
                    Some(message),
                )
            }
        } else if let Some(descriptor) = display {
            descriptor.clone()
        } else {
            synthesize_fallback_display(tool_name, Some(message))
        };
        let descriptor = normalize_terminal_tool_use_state(
            descriptor,
            TerminalToolOutcome::Error {
                canonical_message: message,
            },
        );
        render_tool_use_state(tool_name, &descriptor)
    }

    fn handle_tool_cancelled(
        &mut self,
        cancelled: &tau_proto::ToolCancelled,
        recorded_at: UnixMicros,
    ) {
        let Some((prior, known_main_tool)) = self.take_finished_tool_call(&cancelled.call_id, true)
        else {
            return;
        };
        let descriptor = cancelled.display.clone().unwrap_or_else(|| {
            synthesize_fallback_display(&cancelled.tool_name, Some("cancelled"))
        });
        let descriptor =
            normalize_terminal_tool_use_state(descriptor, TerminalToolOutcome::Cancelled);
        let mut display = render_tool_use_state(&cancelled.tool_name, &descriptor);
        let is_blocker = prior.is_blocker || is_blocker_tool_name(cancelled.tool_name.as_str());
        sanitize_blocker_display(&mut display, is_blocker, prior.blocker_action);
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(
                &mut display,
                duration,
                prior.effective_shell_timeout,
            );
        }
        self.record_tool_summary_result(prior.summary_block_id, None, None, true);
        self.record_plain_finished_tool_block(prior.history_block_id, display, "tool-cancelled");
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn record_plain_finished_tool_block(
        &mut self,
        existing_block_id: Option<tau_cli_term::BlockId>,
        display: ToolCallDisplay,
        label: &'static str,
    ) {
        let block = self.render_tool_history_block(&display);
        let bid = self.update_existing_or_print_tool_block(existing_block_id, label, block);
        self.transcript.history.tool_history.push(ToolBlockEntry {
            block_id: bid,
            display,
        });
    }

    fn update_existing_or_print_tool_block(
        &mut self,
        existing_block_id: Option<tau_cli_term::BlockId>,
        label: &'static str,
        block: tau_cli_term::StyledBlock,
    ) -> tau_cli_term::BlockId {
        if let Some(bid) = existing_block_id {
            self.resources.handle.set_block(bid, block);
            self.resources.handle.redraw();
            bid
        } else {
            self.resources.handle.print_output(label, block)
        }
    }

    fn render_model_status_after_tool_completion(&mut self, known_main_tool: bool) {
        if known_main_tool && self.transcript.status.main_agent_turn_active {
            self.render_model_status();
        }
    }

    fn handle_shell_events(&mut self, event: &Event) -> bool {
        match event {
            Event::UiShellCommand(cmd) => {
                self.handle_ui_shell_command(cmd);
                true
            }
            Event::ShellCommandProgress(progress) => {
                self.handle_shell_command_progress(progress);
                true
            }
            Event::ShellCommandFinished(finished) => {
                self.handle_shell_command_finished(finished);
                true
            }
            _ => false,
        }
    }

    fn shell_running_label(include_in_context: bool) -> String {
        if include_in_context {
            "running".to_owned()
        } else {
            "running [no context]".to_owned()
        }
    }

    fn handle_ui_shell_command(&mut self, cmd: &tau_proto::UiShellCommand) {
        // Create a running block now; the harness will echo progress and a
        // finished event back to us via the bus. Both bangs render the same;
        // the context bit just labels the suffix.
        let label = Self::shell_running_label(cmd.include_in_context);
        let block = render_shell_block(&self.resources.theme, &cmd.command, "", Some(&label));
        let block_id = self
            .resources
            .handle
            .new_block(format!("shell-command:{}", cmd.command_id), block);
        self.resources.handle.push_above_active(block_id);
        self.resources.handle.redraw();
        self.transcript.runtime.shell_blocks.insert(
            cmd.command_id.clone(),
            ShellBlockState {
                block_id,
                command: cmd.command.clone(),
                include_in_context: cmd.include_in_context,
                output: String::new(),
            },
        );
    }

    fn handle_shell_command_progress(&mut self, progress: &tau_proto::ShellCommandProgress) {
        if let Some(state) = self
            .transcript
            .runtime
            .shell_blocks
            .get_mut(&progress.command_id)
        {
            state.output.push_str(&progress.chunk);
            let label = Self::shell_running_label(state.include_in_context);
            let block = render_shell_block(
                &self.resources.theme,
                &state.command,
                &state.output,
                Some(&label),
            );
            self.resources.handle.set_block(state.block_id, block);
            self.resources.handle.redraw();
        }
    }

    fn handle_shell_command_finished(&mut self, finished: &tau_proto::ShellCommandFinished) {
        if self
            .session
            .standalone_shell_terminals
            .remove(&finished.command_id)
        {
            let suffix = Self::shell_finished_suffix(finished, finished.include_in_context);
            let block = render_shell_block(
                &self.resources.theme,
                &finished.command,
                &finished.output,
                Some(&suffix),
            );
            self.resources.handle.print_output("shell-finished", block);
            return;
        }
        let include_in_context = if let Some(state) = self
            .transcript
            .runtime
            .shell_blocks
            .remove(&finished.command_id)
        {
            // Use the final, post-truncation output from the extension rather
            // than our streaming buffer so the UI matches what the harness
            // injected into context.
            self.resources.handle.remove_block(state.block_id);
            state.include_in_context
        } else {
            // Session replay may contain only the durable terminal event. Render
            // it from the self-contained payload instead of dropping it.
            finished.include_in_context
        };
        let suffix = Self::shell_finished_suffix(finished, include_in_context);
        let block = render_shell_block(
            &self.resources.theme,
            &finished.command,
            &finished.output,
            Some(&suffix),
        );
        self.resources.handle.print_output("shell-finished", block);
        self.event_owners.shell_agents.remove(&finished.command_id);
    }

    /// Route one historical shell terminal normally while bypassing active-id
    /// correlation for this synchronous delivery only.
    pub(crate) fn handle_standalone_socket_shell_finished(
        &mut self,
        finished: &tau_proto::ShellCommandFinished,
        recorded_at: UnixMicros,
        delivery_id: RendererDeliveryId,
    ) {
        let event = Event::ShellCommandFinished(finished.clone());
        self.session
            .standalone_shell_terminals
            .insert(finished.command_id.clone());
        let prepared = PreparedRendererEvent::new(&event);
        self.learn_agent_metadata(&prepared);
        tracing::trace!(
            target: "tau_cli::frontend_progress",
            delivery_id = delivery_id.get(),
            event = %event.name(),
            "routing standalone historical shell terminal"
        );
        if let Some(target) = finished.target_agent_id.as_ref()
            && self.selection.displayed_agent_id.as_ref() != Some(target)
        {
            self.handle_recorded_at_for_hidden_agent(&prepared, recorded_at, target.clone());
        } else {
            self.handle_recorded_at_for_visible_agent(&prepared, recorded_at);
        }
        self.update_agent_in_progress();
    }

    /// Remove starts shown before catch-up that the authoritative running-route
    /// snapshot did not confirm.
    pub(crate) fn abandon_shell_starts(&mut self, starts: &[ShellStartPresentation]) {
        for start in starts {
            let command_id = &start.command_id;
            for pending in self.discovery.pending_initial_discovery.values_mut() {
                pending.retain(|deferred| {
                    let Some(event) = deferred.ordinary_event() else {
                        return true;
                    };
                    !matches!(event, Event::UiShellCommand(command)
                        if command.command_id == *command_id
                            && command.target_agent_id == start.target_agent_id)
                });
            }
            if start.target_agent_id.is_none() {
                let agent_ids = self
                    .selection
                    .agents_ui_state
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                for agent_id in agent_ids {
                    if let Some(mut state) = self.selection.agents_ui_state.remove(&agent_id) {
                        if let Some(shell) =
                            state.transcript.runtime.shell_blocks.remove(command_id)
                        {
                            self.resources
                                .handle
                                .select_detached(std::mem::take(&mut state.output));
                            self.resources.handle.remove_block(shell.block_id);
                            state.output = self.resources.handle.take_detached();
                        }
                        self.selection.agents_ui_state.insert(agent_id, state);
                    }
                }
                let mut no_agent = std::mem::take(&mut self.selection.no_agent_ui_state);
                if let Some(shell) = no_agent.transcript.runtime.shell_blocks.remove(command_id) {
                    self.resources
                        .handle
                        .select_detached(std::mem::take(&mut no_agent.output));
                    self.resources.handle.remove_block(shell.block_id);
                    no_agent.output = self.resources.handle.take_detached();
                }
                self.selection.no_agent_ui_state = no_agent;
            }
            if start
                .target_agent_id
                .as_ref()
                .is_some_and(|target| self.selection.displayed_agent_id.as_ref() != Some(target))
            {
                if let Some(target) = start.target_agent_id.as_ref()
                    && let Some(mut state) = self.selection.agents_ui_state.remove(target)
                {
                    if let Some(shell) = state.transcript.runtime.shell_blocks.remove(command_id) {
                        self.resources
                            .handle
                            .select_detached(std::mem::take(&mut state.output));
                        self.resources.handle.remove_block(shell.block_id);
                        state.output = self.resources.handle.take_detached();
                    }
                    self.selection.agents_ui_state.insert(target.clone(), state);
                }
                self.event_owners.shell_agents.remove(command_id);
                continue;
            }
            if let Some(state) = self.transcript.runtime.shell_blocks.remove(command_id) {
                self.resources.handle.remove_block(state.block_id);
            }
            self.event_owners.shell_agents.remove(command_id);
        }
        if !starts.is_empty() {
            self.resources.handle.redraw();
        }
    }

    fn shell_finished_suffix(
        finished: &tau_proto::ShellCommandFinished,
        include_in_context: bool,
    ) -> String {
        let suffix = if finished.cancelled {
            "cancelled".to_owned()
        } else {
            match finished.exit_code {
                Some(0) => "[0]".to_owned(),
                Some(code) => format!("[{code}]"),
                None => "[?]".to_owned(),
            }
        };
        if include_in_context {
            suffix
        } else {
            format!("{suffix} [no context]")
        }
    }

    fn handle_action_events(&mut self, event: &Event) -> bool {
        match event {
            Event::ActionSchemaPublished(published) => {
                self.resources
                    .action_state
                    .apply_schema_published(published);
                self.refresh_action_completions();
                true
            }
            Event::ActionResult(result) => {
                self.handle_action_result(result);
                true
            }
            Event::ActionError(error) => {
                self.handle_action_error(error);
                true
            }
            Event::UiRetryPromptResult(result) => {
                self.resources.handle.print_output(
                    "retry-result",
                    render_action_output_block(&self.resources.theme, &result.message),
                );
                true
            }
            Event::UiSetAgentNavigationModeResult(result) => {
                if let tau_proto::UiSetAgentNavigationModeOutcome::Rejected { reason } =
                    result.outcome
                {
                    let message = match reason {
                        tau_proto::UiSetAgentNavigationModeRejection::StaleSession => {
                            "Agent navigation mode was not changed because the session changed."
                                .to_owned()
                        }
                        tau_proto::UiSetAgentNavigationModeRejection::AgentNotLoaded => format!(
                            "Agent {} is not currently loaded; its navigation mode was not changed.",
                            result.agent_id
                        ),
                    };
                    self.resources.handle.print_output(
                        "agent-navigation-result",
                        render_action_output_block(&self.resources.theme, &message),
                    );
                }
                true
            }
            Event::UiCreateAgentResult(result) => {
                if let tau_proto::UiCreateAgentOutcome::Rejected { message, .. } = &result.outcome {
                    self.transcript
                        .status
                        .agent_activity
                        .clear_optimistic_submissions();
                    self.update_agent_in_progress();
                    self.resources.handle.print_output(
                        "create-agent-result",
                        render_action_output_block(&self.resources.theme, message),
                    );
                }
                true
            }
            Event::AgentPromptFailed(failed) => {
                let submitted = self
                    .transcript
                    .runtime
                    .submitted_prompt_ctx_ids
                    .iter()
                    .position(|ctx_id| ctx_id == &failed.ctx_id)
                    .and_then(|index| {
                        self.transcript
                            .runtime
                            .submitted_prompt_ctx_ids
                            .remove(index)
                    })
                    .is_some();
                if !submitted
                    && let Some(queued) = self.transcript.runtime.queued_user_blocks.pop_front()
                    && let Some(queued_id) = queued.id
                {
                    self.resources.handle.remove_block(queued_id);
                }
                self.transcript
                    .status
                    .agent_activity
                    .clear_optimistic_submissions();
                self.update_agent_in_progress();
                self.resources.handle.print_output(
                    "initial-prompt-failed",
                    render_action_output_block(&self.resources.theme, &failed.message),
                );
                true
            }
            Event::ActionInvoke(_) => true,
            _ => false,
        }
    }

    fn refresh_action_completions(&self) {
        let (commands, arg_completers) = self.resources.action_state.dynamic_completions();
        self.resources
            .completion_data
            .set_dynamic_commands_and_arg_completers(commands, arg_completers);
    }

    fn handle_action_result(&mut self, result: &tau_proto::ActionResult) {
        let text = match &result.output {
            tau_proto::ActionOutput::Text { text } => text.clone(),
            tau_proto::ActionOutput::EditorBuffer {
                title,
                text,
                editable,
            } => {
                let mut rendered = format!("{title}\n{text}");
                if *editable {
                    rendered.push_str("\n[editable buffer]");
                }
                rendered
            }
        };
        self.resources.handle.print_output(
            "action-result",
            render_action_output_block(&self.resources.theme, &text),
        );
        if self.selection.displayed_agent_id.is_none() {
            self.transcript.ownership.preserve_on_fresh_agent_switch = true;
        }
    }

    fn handle_action_error(&mut self, error: &tau_proto::ActionError) {
        use crate::tool_render::render_action_error_block;

        self.resources.handle.print_output(
            "action-error",
            render_action_error_block(&self.resources.theme, &error.action_id, &error.message),
        );
        if self.selection.displayed_agent_id.is_none() {
            self.transcript.ownership.preserve_on_fresh_agent_switch = true;
        }
    }

    fn handle_extension_events(&mut self, event: &Event) -> bool {
        match event {
            Event::ExtensionStarting(starting) => {
                self.handle_extension_starting(starting);
                true
            }
            Event::ExtensionReady(ready) => {
                self.handle_extension_ready(ready);
                true
            }
            Event::ExtensionExited(exited) => {
                self.resources
                    .action_state
                    .remove_extension(&exited.extension_name, exited.instance_id);
                self.refresh_action_completions();
                self.handle_extension_exited(exited);
                true
            }
            Event::ExtensionContextReady(ready) => {
                self.handle_extension_context_ready(ready);
                true
            }
            _ => false,
        }
    }

    fn handle_extension_starting(&mut self, starting: &tau_proto::ExtensionStarting) {
        let owner = self.current_extension_block_owner();
        if matches!(owner, UiSnapshotOwner::NoAgent) {
            self.transcript.ownership.preserve_on_fresh_agent_switch = true;
        }
        let projection = DiagnosticProjection::ExtensionStatus {
            extension_name: starting.extension_name.clone(),
            status: ExtensionLifecycleStatus::Starting,
        };
        let rendered = self.render_diagnostic_projection(tau_proto::NoticeLevel::Info, &projection);
        let id = self.resources.handle.new_block(
            format!("extension-starting:{}", starting.instance_id),
            rendered,
        );
        self.resources.handle.push_above_active(id);
        self.resources.handle.redraw();
        self.session.extension_blocks.insert(
            starting.instance_id,
            ExtensionBlockState {
                block_id: id,
                owner,
            },
        );
        self.transcript
            .history
            .diagnostic_history
            .push(DiagnosticBlockEntry {
                block_id: id,
                level: tau_proto::NoticeLevel::Info,
                projection,
            });
    }

    fn handle_extension_ready(&mut self, ready: &tau_proto::ExtensionReady) {
        if let Some(state) = self.session.extension_blocks.remove(&ready.instance_id) {
            self.resources.handle.remove_block(state.block_id);
            self.transcript
                .history
                .diagnostic_history
                .retain(|entry| entry.block_id != state.block_id);
        }
        self.session
            .ready_extensions
            .insert(ready.extension_name.to_string());
        self.retain_diagnostic_block(
            "extension-ready",
            tau_proto::NoticeLevel::Info,
            DiagnosticProjection::ExtensionStatus {
                extension_name: ready.extension_name.clone(),
                status: ExtensionLifecycleStatus::Ready,
            },
        );
    }

    fn handle_extension_exited(&mut self, exited: &tau_proto::ExtensionExited) {
        if let Some(state) = self.session.extension_blocks.remove(&exited.instance_id) {
            self.resources.handle.remove_block(state.block_id);
            self.transcript
                .history
                .diagnostic_history
                .retain(|entry| entry.block_id != state.block_id);
        }
        self.session
            .ready_extensions
            .remove(exited.extension_name.as_str());
        self.retain_diagnostic_block(
            "extension-exited",
            tau_proto::NoticeLevel::Info,
            DiagnosticProjection::ExtensionStatus {
                extension_name: exited.extension_name.clone(),
                status: ExtensionLifecycleStatus::Exited,
            },
        );
    }

    fn handle_extension_context_ready(&mut self, ready: &tau_proto::ExtensionContextReady) {
        self.retain_diagnostic_block(
            "extension-context-ready",
            tau_proto::NoticeLevel::Debug,
            DiagnosticProjection::ExtensionContextReady {
                agent_id: ready.agent_id.clone(),
            },
        );
    }

    fn handle_harness_status_events(&mut self, event: &Event) -> bool {
        match event {
            Event::HarnessSessionSkillsAvailable(snapshot) => {
                self.resources.skill_state.apply_session_snapshot(snapshot);
                true
            }
            Event::HarnessAgentContextInitialized(initialized) => {
                let key = (
                    initialized.agent_id.clone(),
                    initialized.agent_initialization_id.clone(),
                );
                if self.discovery.initialized_discovery_epochs.insert(key) {
                    self.print_agent_context_initialized(initialized);
                }
                true
            }
            Event::HarnessNotice(info) => {
                self.retain_harness_notice("harness-notice", info.clone());
                true
            }
            Event::AgentManualCompactionRequested(requested) => {
                self.register_self_compaction_tool(requested);
                let authority = requested.tool_source().map_or_else(
                    || format!("UI for agent {}", requested.target_agent_id),
                    |source| format!("Agent {}", source.caller_agent_id),
                );
                let notice = tau_proto::HarnessNotice::diagnostic(
                    tau_proto::notice_kind::HARNESS_NOTICE,
                    format!(
                        "{authority} accepted compaction request for {} ({})",
                        requested.target_agent_id, requested.request_id
                    ),
                    tau_proto::NoticeLevel::Info,
                );
                self.retain_harness_notice("manual-compaction-requested", notice);
                true
            }
            Event::AgentManualCompactionRequestFailed(failed) => {
                let call_id = self
                    .transcript
                    .runtime
                    .self_compaction_tools
                    .iter()
                    .find_map(|(call_id, tool)| {
                        (tool.request_id == failed.request_id && tool.compact_prompt_id.is_none())
                            .then(|| call_id.clone())
                    });
                if let Some(call_id) = call_id {
                    self.update_self_compaction_tool_status(
                        &call_id,
                        CompactionStatus::Failure,
                        "rejected",
                    );
                }
                true
            }
            Event::AgentStandaloneCompactionStarted(started) => {
                if matches!(
                    started.operation,
                    tau_proto::PromptOperation::StandaloneCompaction
                ) {
                    self.transcript
                        .runtime
                        .standalone_compaction_transactions
                        .insert(
                            started.transaction_id.clone(),
                            started.compact_prompt_id.clone(),
                        );
                    if let Some(call_id) = self.associate_self_compaction_start(started) {
                        self.update_self_compaction_tool_status(
                            &call_id,
                            CompactionStatus::Progress,
                            "Compacting…",
                        );
                    }
                }
                true
            }
            Event::AgentCompacted(compacted) => {
                let prompt_id = compacted.compact_prompt_id.as_ref().cloned().or_else(|| {
                    compacted
                        .transaction_id
                        .as_ref()
                        .and_then(|transaction_id| {
                            self.transcript
                                .runtime
                                .standalone_compaction_transactions
                                .remove(transaction_id)
                        })
                });
                if let Some(prompt_id) = prompt_id {
                    let status = Self::standalone_compaction_success_status(
                        compacted.original_input_tokens,
                        None,
                    );
                    let self_tool_call_id = self.self_compaction_tool_for_prompt(&prompt_id);
                    let block_id = if let Some(call_id) = self_tool_call_id.as_ref() {
                        let block_id = self
                            .transcript
                            .runtime
                            .tool_calls
                            .get(call_id)
                            .and_then(|state| state.history_block_id);
                        self.complete_standalone_compaction_prompt(
                            &prompt_id,
                            Some((&status, CompactionStatus::Success)),
                        );
                        block_id
                    } else {
                        self.complete_standalone_compaction_prompt(&prompt_id, None);
                        let block_id = self.resources.handle.print_output(
                            "standalone-compaction-terminal",
                            render_compaction_block(
                                &self.resources.theme,
                                &status,
                                CompactionStatus::Success,
                            ),
                        );
                        self.resources.handle.redraw();
                        Some(block_id)
                    };
                    if let Some(transaction_id) = compacted.transaction_id.as_ref() {
                        self.transcript.runtime.completed_compactions.insert(
                            transaction_id.clone(),
                            CompletedCompactionPresentation {
                                block_id,
                                original_input_tokens: compacted.original_input_tokens,
                                self_tool_call_id,
                            },
                        );
                    }
                }
                true
            }
            Event::AgentInferenceDispatchStarted(started) => {
                if matches!(
                    started.operation,
                    Some(tau_proto::PromptOperation::Inference)
                ) && let Some(transaction_id) = started.transaction_id.as_ref()
                    && self
                        .transcript
                        .runtime
                        .completed_compactions
                        .contains_key(transaction_id)
                    && !self
                        .transcript
                        .runtime
                        .compaction_continuation_prompts
                        .values()
                        .any(|known| known == transaction_id)
                {
                    self.transcript
                        .runtime
                        .compaction_continuation_prompts
                        .insert(started.agent_prompt_id.clone(), transaction_id.clone());
                }
                true
            }
            Event::AgentStandaloneCompactionFailed(failed) => {
                if let Some(prompt_id) = self
                    .transcript
                    .runtime
                    .standalone_compaction_transactions
                    .remove(&failed.transaction_id)
                {
                    self.complete_standalone_compaction_prompt(
                        &prompt_id,
                        Some(("failed", CompactionStatus::Failure)),
                    );
                }
                true
            }
            Event::HarnessSessionDir(session_dir) => {
                self.handle_harness_session_dir(session_dir);
                true
            }
            Event::HarnessUiDir(ui_dir) => {
                self.retain_diagnostic_block(
                    "ui-dir",
                    tau_proto::NoticeLevel::Info,
                    DiagnosticProjection::UiDir {
                        path: ui_dir.path.clone(),
                    },
                );
                true
            }
            Event::HarnessModelsAvailable(models) => {
                self.handle_harness_models_available(models);
                true
            }
            Event::HarnessProviderQuotaChanged(changed) => {
                self.role.quota_pacing.update(changed);
                self.render_model_status_if_present();
                true
            }
            _ => false,
        }
    }

    /// Print one initialization discovery summary into the active transcript.
    fn print_agent_context_initialized(
        &mut self,
        initialized: &tau_proto::HarnessAgentContextInitialized,
    ) {
        self.retain_diagnostic_block(
            "agent-context-initialized",
            tau_proto::NoticeLevel::Info,
            DiagnosticProjection::AgentContextInitialized {
                event: initialized.clone(),
                unadvertised_count: self
                    .resources
                    .skill_state
                    .unadvertised_count(&initialized.listed_skills),
            },
        );
    }

    fn handle_harness_session_dir(&mut self, session_dir: &tau_proto::HarnessSessionDir) {
        self.retain_diagnostic_block(
            "session-dir",
            tau_proto::NoticeLevel::Info,
            DiagnosticProjection::SessionDir {
                event: session_dir.clone(),
            },
        );
        if let Some(selection) = self.session.startup_profile_selection.as_ref() {
            self.retain_diagnostic_block(
                "config-profile-selection",
                tau_proto::NoticeLevel::Info,
                DiagnosticProjection::ConfigProfile {
                    selection: selection.to_string(),
                },
            );
        }
    }

    fn handle_harness_role_events(&mut self, event: &Event) -> bool {
        match event {
            Event::HarnessRolesAvailable(roles) => {
                self.handle_harness_roles_available(roles);
                true
            }
            Event::HarnessRoleSelected(selected) => {
                self.handle_harness_role_selected(selected);
                true
            }
            Event::HarnessContextUsageChanged(changed) => {
                self.transcript.status.current_context_input_tokens = changed.input_tokens;
                self.transcript.status.current_context_percent = changed.percent_used;
                self.render_model_status();
                true
            }
            Event::HarnessAgentContextUsageChanged(changed) => {
                self.transcript.status.current_context_input_tokens = changed.input_tokens;
                self.transcript.status.current_context_window = changed
                    .context_window
                    .or(self.transcript.status.current_context_window);
                self.transcript.status.current_context_percent = changed.percent_used;
                self.render_model_status();
                true
            }
            _ => false,
        }
    }

    fn handle_harness_models_available(&mut self, models: &tau_proto::HarnessModelsAvailable) {
        let model_items: Vec<tau_cli_term::CompletionItem> = models
            .models
            .iter()
            .map(|model| tau_cli_term::CompletionItem::new(model.to_string(), "agent model"))
            .collect();
        self.resources
            .completion_data
            .set_arg_completions(tau_cli_term::CommandName::new(":model"), model_items);
    }

    fn handle_harness_roles_available(&mut self, roles: &tau_proto::HarnessRolesAvailable) {
        let role_defaults: HashMap<String, RoleCompletionDetails> = roles
            .roles
            .iter()
            .map(|r| (r.name.clone(), RoleCompletionDetails::from_role_info(r)))
            .collect();
        let role_items = Self::role_completion_items(roles, &role_defaults, false);
        if let Ok(mut available) = self.role.roles_available.lock() {
            *available = roles.roles.iter().map(|r| r.name.clone()).collect();
        }
        if let Ok(mut available) = self.role.role_groups_available.lock() {
            *available = roles.groups.clone();
        }
        if let Ok(mut prompts) = self.role.custom_prompts.lock() {
            *prompts = roles.custom_prompts.clone();
        }
        let prompt_items = roles
            .custom_prompts
            .iter()
            .map(|prompt| tau_cli_term::CompletionItem::plain(prompt.id.clone()))
            .collect();
        self.resources
            .completion_data
            .set_arg_completions(tau_cli_term::CommandName::new(":prompt"), prompt_items);
        let new_agent_role_items = Self::role_completion_items(roles, &role_defaults, true)
            .iter()
            .map(|(item, _)| item.clone())
            .collect::<Vec<_>>();
        self.resources
            .completion_data
            .set_arg_completions(tau_cli_term::CommandName::new(":new"), new_agent_role_items);
        self.role.role_defaults = role_defaults;
        if self.role.current_role.is_some() && self.transcript.runtime.model_status_block.is_some()
        {
            self.render_model_status();
        }
        let completer: tau_cli_term::ArgCompleter =
            path_std_sync::Arc::new(move |args| role_command_completions(&role_items, args));
        self.resources
            .completion_data
            .set_arg_completer(tau_cli_term::CommandName::new(":role"), completer);
    }

    fn role_completion_items(
        roles: &tau_proto::HarnessRolesAvailable,
        role_defaults: &HashMap<String, RoleCompletionDetails>,
        include_tool_details: bool,
    ) -> Vec<(tau_cli_term::CompletionItem, RoleCompletionDetails)> {
        roles
            .roles
            .iter()
            .filter_map(|role| {
                let details = role_defaults.get(&role.name)?.clone();
                Some((
                    tau_cli_term::CompletionItem::new(
                        &role.name,
                        details.completion_description(include_tool_details),
                    ),
                    details,
                ))
            })
            .collect()
    }

    fn handle_harness_role_selected(&mut self, selected: &tau_proto::HarnessRoleSelected) {
        self.role.current_model = selected.model.clone();
        self.role.current_role = Some(selected.role.clone());
        self.role.baseline_params = selected.baseline_params;
        self.role.model_params = selected.model_params;
        self.role.verbosity_state.store(
            selected.model_params.verbosity.as_u8(),
            path_std_sync_atomic::Ordering::Relaxed,
        );
        self.role.thinking_summary_state.store(
            selected.model_params.thinking_summary.as_u8(),
            path_std_sync_atomic::Ordering::Relaxed,
        );
        self.role.fast_service_tier_state.store(
            matches!(
                selected.model_params.service_tier,
                Some(tau_proto::ServiceTier::Fast)
            ),
            path_std_sync_atomic::Ordering::Relaxed,
        );
        if let Ok(mut role) = self.role.current_role_state.lock() {
            *role = Some(selected.role.clone());
        }
        if let (Ok(groups), Ok(mut memory)) = (
            self.role.role_groups_available.lock(),
            self.role.role_group_memory.lock(),
        ) && let Some(group) = groups
            .iter()
            .find(|group| group.roles.iter().any(|role| role == &selected.role))
        {
            memory.insert(group.name.clone(), selected.role.clone());
        }
        let prompt = crate::theme::active_prompt_marker(
            &self.resources.theme,
            &self.resources.prompt_symbol,
            Some(&selected.role),
        );
        self.resources.handle.set_left_prompt(prompt);
        self.refresh_prompt_placeholder();
        self.resources.handle.redraw();
        self.transcript.status.current_context_window = selected.context_window;
        self.render_model_status();
    }

    fn handle_harness_available_events(&mut self, event: &Event) -> bool {
        match event {
            Event::HarnessEffortsAvailable(avail) => {
                self.handle_harness_efforts_available(avail);
                true
            }
            Event::HarnessVerbositiesAvailable(avail) => {
                self.handle_harness_verbosities_available(avail);
                true
            }
            Event::HarnessThinkingSummariesAvailable(avail) => {
                self.handle_harness_thinking_summaries_available(avail);
                true
            }
            _ => false,
        }
    }

    fn handle_harness_efforts_available(&mut self, avail: &tau_proto::HarnessEffortsAvailable) {
        let _ = avail;
    }

    fn handle_harness_verbosities_available(
        &mut self,
        avail: &tau_proto::HarnessVerbositiesAvailable,
    ) {
        let _ = avail;
    }

    fn handle_harness_thinking_summaries_available(
        &mut self,
        avail: &tau_proto::HarnessThinkingSummariesAvailable,
    ) {
        let _ = avail;
    }

    fn handle_terminal_events(&mut self, event: &Event) -> bool {
        match event {
            Event::Osc1337SetUserVar(req) => {
                let in_tmux = std::env::var_os("TMUX").is_some();
                self.resources
                    .handle
                    .print_osc1337_set_user_var(&req.name, &req.value, in_tmux);
                true
            }
            Event::TermBell(_) => {
                self.resources.handle.print_terminal_bell();
                true
            }
            _ => false,
        }
    }
}

mod finished_response_projection;
mod prepared_renderer_event;
pub(crate) mod renderer_state;
mod terminal_tool_calls;
#[cfg(test)]
mod terminal_tool_calls_tests;
use finished_response_projection::FinishedResponseProjection;
use renderer_state::AgentUiState;
pub(crate) use renderer_state::EventRenderer;

#[cfg(test)]
mod tests;

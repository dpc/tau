//! Drains the event stream from the harness socket and paints it into
//! the terminal UI. Stateful: tracks per-prompt and per-tool-call UI
//! state so streaming updates land in the right block.
//!
//! Provider delta ordering and accumulation follow
//! `SPEC-tau-cli-provider-stream-rendering`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextRole, Event, MessageItem,
    ProviderResponseCompactionStatus, ProviderResponseTextDelta, ToolCallItem, UnixMicros,
};

use crate::action_commands::ActionCommandState;
use crate::agent_activity::AgentActivity;
use crate::agent_navigation::AgentNavigation;
use crate::build_banner;
use crate::chat::{DraftSlot, invalidate_pending_draft, retarget_prompt_draft_snapshot};
use crate::markdown_render::{
    MarkdownStreamCache, markdown_block, markdown_prefixed_block,
    markdown_prefixed_streaming_block, markdown_prompt_block, markdown_streaming_block,
};
use crate::skill_commands::SkillCommandState;
use crate::tool_render::{
    CompactionStatus, ToolCallDisplay, ToolStatus, ToolSuffixSegment, ToolSummaryDisplay,
    build_delegate_completion_display, build_tool_summary_display, diff_payload_counts,
    extension_status_block, extract_diff, format_token_count, pending_tool_call_display,
    render_compaction_block, render_diff_tool_block, render_harness_notice,
    render_multi_diff_tool_block, render_shell_block, render_tool_block, render_tool_use_state,
    render_turn_stats_block, session_status_block, streaming_block,
    streaming_block_with_indicator_suffix, synthesize_fallback_display, system_loaded_block,
    tool_duration_suffix, ui_dir_block,
};
use crate::watch_activity::WatchActivityProjection;

pub(crate) const UI_IO_MEDIUM_BYTES_PER_SEC: u64 = 10 * 1024;
const UI_IO_HIGH_BYTES_PER_SEC: u64 = 100 * 1024;

const AGENT_START_TOOL_NAME: &str = "agent_start";
const TIMER_WAKEUP_CTX_PREFIX: &str = "timer:";
const COMPLETED_AGENT_RESPONSE_PREFIX: &str = "◆ ";
const STREAMING_AGENT_RESPONSE_PREFIX: &str = "◇ ";
/// Maximum rendered terminal columns for a supplemental agent message name.
const AGENT_MESSAGE_NAME_MAX_COLUMNS: usize = 48;
/// Maximum rendered UTF-8 bytes for a supplemental agent message name.
const AGENT_MESSAGE_NAME_MAX_BYTES: usize = 192;

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

pub(crate) struct EventRenderer {
    handle: tau_cli_term::TermHandle,
    completion_data: tau_cli_term::CompletionData,
    action_state: ActionCommandState,
    skill_state: SkillCommandState,
    theme: tau_themes::Theme,
    /// Agent that prompt input targets. `None` means the UI is in the
    /// start-new-agent state; the next user prompt will start/select an agent.
    current_agent_id: Option<String>,
    /// Agent transcript currently rendered in the output area. This is tracked
    /// separately from [`Self::current_agent_id`] so changing the input target
    /// from the empty start-new-agent screen does not force a transcript swap.
    displayed_agent_id: Option<String>,
    /// True after the user explicitly cleared selection to start a new agent.
    /// While set, already-visible agents must not be reselected by late
    /// background prompt lifecycle events.
    awaiting_new_agent_selection: bool,
    /// Output and renderer bookkeeping for the no-agent screen shown after
    /// `/agent none`. This keeps deselection from leaving the previously
    /// selected agent's output in the visible renderer fields.
    no_agent_ui_state: AgentUiState,
    /// Output and renderer bookkeeping for agents that are not currently
    /// visible. The currently visible agent lives in the fields on this struct
    /// so existing rendering code can stay direct and efficient.
    agents_ui_state: HashMap<String, AgentUiState>,
    /// Whether the visible no-agent transcript has output that may need a
    /// snapshot before switching to a fresh agent transcript.
    ///
    /// Preservation is additionally gated by
    /// [`Self::awaiting_new_agent_selection`]: startup and post-`/session
    /// new` no-agent output is adopted by the first selected agent, while
    /// explicit `/agent none`/`/agent new` output remains a protected
    /// no-agent snapshot.
    preserve_on_fresh_agent_switch: bool,
    /// Whether the visible snapshot contains a message fact owned by the global
    /// no-agent view rather than an agent transcript.
    contains_global_message_fact: bool,
    /// Whether the visible snapshot contains an inter-agent message copied into
    /// the all-agent overview.
    contains_overview_message: bool,
    /// Originating session and message ids already projected into the all-agent
    /// overview.
    ///
    /// Local agent delivery emits sender and recipient projections with the
    /// same id. The overview presents that semantic message once while each
    /// agent transcript keeps its own projection.
    overview_message_ids: HashSet<(Option<tau_proto::SessionId>, tau_proto::AgentMessageId)>,
    /// Agent ids known to the UI for `/agent` completion.
    known_agents: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Session-scoped authoritative display names keyed by local agent id.
    ///
    /// Folded from `agent.started` and `agent.display_name_set`, retained
    /// across unload, cleared on session-id changes, and never applied to
    /// remote routes.
    agent_display_names: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Atomic per-UI navigation modes, live membership, and runtime states.
    agent_navigation: Arc<Mutex<AgentNavigation>>,
    /// Agent ids whose transcripts are memory-only in the current daemon.
    ephemeral_agents: std::sync::Arc<std::sync::Mutex<HashSet<String>>>,
    /// Map side-query ids to the accepted agent id for routing prompt/provider
    /// events whose originator only carries `query_id`.
    query_agents: HashMap<String, String>,
    /// Map provider prompt ids to the agent transcript they belong to.
    prompt_agents: HashMap<String, String>,
    /// Map tool call ids to the agent transcript they belong to.
    tool_agents: HashMap<String, String>,
    /// Map user-shell command ids to the agent transcript where they started.
    shell_agents: HashMap<String, String>,
    /// Current watch sets keyed by watcher agent id.
    watched_agents: HashMap<String, Vec<String>>,
    /// Reverse watch sets keyed by watched agent id.
    agent_watchers: HashMap<String, Vec<String>>,
    /// Latest generic operational stats keyed by agent id.
    agent_stats: HashMap<String, tau_proto::AgentStatsUpdated>,
    /// Last exact dispatched model per browsable agent.
    agent_models: HashMap<String, tau_proto::ModelId>,
    /// Latest harness-authored agent-turn state keyed by `(watcher, watched)`.
    ///
    /// Once present, this outer lifecycle is authoritative over provider-prompt
    /// activity across model rounds and intervening tool rounds.
    watched_agent_turn_states:
        HashMap<String, HashMap<String, tau_proto::AgentWatchTurnStateNotification>>,
    /// In-flight `agent_prompt_id`s keyed by the agent currently producing a
    /// response.
    active_agent_prompts: HashMap<String, HashSet<String>>,
    /// Prompt ids whose terminal event has already arrived.
    ///
    /// Provider and harness events can be delayed or replayed out of the ideal
    /// order from the renderer's perspective. Once a prompt is terminal, later
    /// start/create/update events for the same id must not resurrect watched
    /// status blocks or the active side-agent count.
    terminal_agent_prompts: HashSet<String>,
    /// Provider prompt ids whose final response was already rendered.
    ///
    /// Late stats-only provider updates for these prompts are stale and must
    /// not recreate live response indicators, while stats-only updates for
    /// unknown prompts are still allowed for the no-agent adoptable
    /// transcript path.
    finished_provider_prompts: HashSet<String>,
    /// Active watched-agent indicator blocks keyed by watched agent id.
    watched_agent_blocks: HashMap<String, tau_cli_term::BlockId>,
    /// Shared current visible agent mirror for prompt submission.
    current_agent_state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Shared prompt-draft mailbox used to retarget pending drafts when remote
    /// events auto-select an agent.
    draft_retargeter: Option<DraftRetargeter>,
    /// Per-`agent_prompt_id` UI state. An entry is created on
    /// `AgentPromptStarted` (or `ProviderPromptSubmitted` for prompts
    /// without an explicit start event) and torn down on
    /// `ProviderResponseFinished` or `AgentPromptTerminated`. Storing the
    /// response block id, thinking block id/text, and dispatch timestamp in one
    /// place means every
    /// per-prompt cleanup is a single `prompts.remove(spid)` instead of
    /// four separate `.remove()` calls easy to forget when extending.
    prompts: HashMap<String, PromptState>,
    /// Last locally-echoed user message that has not yet been classified
    /// as a normal or queued prompt. Used to replace only the matching
    /// echo when the harness reports that prompt as queued.
    last_user_block: Option<(tau_cli_term::BlockId, String)>,
    /// Queued user-message blocks (in above_sticky zone).
    /// When `AgentPromptStarted` fires for a dequeued prompt,
    /// the first entry is popped and moved back to history.
    queued_user_blocks: VecDeque<(tau_cli_term::BlockId, String)>,
    /// Per-`call_id` UI state. Tracks the live block (if any), the
    /// cached tool args/progress for in-place re-renders, and
    /// whether the call belongs to a sub-agent side-conversation (in
    /// which case the UI suppresses its progress and result events).
    /// Entries are removed on terminal logical completion events.
    tool_calls: HashMap<String, ToolCallState>,
    /// Wakes the timer thread whenever visible tool activity starts or stops.
    tool_timer: Option<ToolTimerNotifier>,
    /// Live user-shell blocks (from `!`/`!!`) keyed by command_id.
    /// Updated in place as progress chunks arrive, finalized on
    /// `ShellCommandFinished`.
    shell_blocks: HashMap<String, ShellBlockState>,
    /// Live extension lifecycle blocks keyed by instance_id. Shown in
    /// above_active while starting, then completed in the same transcript
    /// snapshot that originally owned the starting block.
    extension_blocks: HashMap<tau_proto::ExtensionInstanceId, ExtensionBlockState>,
    /// Dynamic action invocations keyed by invocation id. Action results and
    /// errors do not carry an agent id, so the CLI snapshots the viewed
    /// transcript when the slash command is invoked and routes completion
    /// output back to that transcript, per
    /// `SPEC-tau-cli-action-completions`.
    action_invocation_owners: HashMap<tau_proto::ActionInvocationId, UiSnapshotOwner>,
    /// Extensions that are already up in this daemon. `/session new` starts a
    /// fresh session, but these processes are intentionally kept.
    ready_extensions: HashSet<String>,
    /// Persistent status bar block showing the current model + effort.
    model_status_block: Option<tau_cli_term::BlockId>,
    /// Current session id used to scope events and detect session transitions.
    current_session_id: Option<tau_proto::SessionId>,
    /// Filesystem context used to rebuild the right prompt after session
    /// events.
    right_prompt_paths: Option<(std::path::PathBuf, Option<std::path::PathBuf>)>,
    /// Live history of completed diff-capable tool blocks plus the data
    /// needed to re-render them. `/set show-diff` flips
    /// `diffs_expanded` and walks this list calling `set_block` so
    /// the entire transcript switches mode at once.
    diff_blocks: Vec<DiffBlockEntry>,
    /// Global expand-diffs toggle.
    diffs_expanded: bool,
    /// Global show-thinking toggle. When false, agent reasoning
    /// summaries are not rendered (live or in history). Controlled
    /// by `/set show-thinking`; persisted in `<state_dir>/cli.json`.
    show_thinking: bool,
    /// Persisted thinking blocks (one per finished assistant turn).
    /// When `show-thinking` flips, every entry is re-rendered as
    /// either the full text or removed, so the toggle takes effect
    /// retroactively across the visible transcript.
    thinking_history: Vec<ThinkingBlockEntry>,
    turn_stats_history: Vec<TurnStatsBlockEntry>,
    tool_history: Vec<ToolBlockEntry>,
    /// Durable message blocks and payloads, kept so `/set show-messages`
    /// can re-render the current transcript retroactively.
    message_history: Vec<MessageBlockEntry>,
    /// Where to persist `show_diff` / `show_thinking` /
    /// `show_turn_stats` / `show_tools` toggles.
    state_dirs: tau_config::settings::TauDirs,
    /// Model currently resolved for the selected role. `None` until the first
    /// `HarnessRoleSelected`, or while the selected role has no available
    /// provider-published model.
    current_model: Option<tau_proto::ModelId>,
    /// Provider-neutral quota state and per-cycle pacing hysteresis.
    quota_pacing: crate::provider_quota::QuotaPacingState,
    /// Last quota-only timer repaint, used to keep its cadence coarse.
    last_quota_tick: Option<Instant>,
    /// Currently selected agent role, as last announced by
    /// `HarnessRoleSelected`. `None` only before the first selection event.
    /// The status bar shows this instead of the derived model id.
    current_role: Option<String>,
    /// Current role details advertised for completion menus. Status
    /// chips compare against `baseline_params` instead, because these
    /// role details include persisted state overrides.
    role_defaults: HashMap<String, RoleCompletionDetails>,
    /// Role/provider baseline knobs for the current selection.
    /// Persisted state is intentionally excluded by the harness so the
    /// status bar can surface state adjustments from that baseline.
    baseline_params: Option<tau_proto::ModelParams>,
    /// Effective per-prompt model knobs derived from the selected role and
    /// role overrides. Mirrored into input-thread atomics for cycling helpers.
    model_params: tau_proto::ModelParams,
    /// Current model context usage percent. `None` when the context
    /// window is unknown for the selected model.
    current_context_percent: Option<u8>,
    /// Input tokens consumed by the most recent agent response. `None`
    /// until the first usage report for the current model.
    current_context_input_tokens: Option<u64>,
    /// Current model context window, in tokens, if known.
    current_context_window: Option<u64>,
    /// Main-agent tool calls completed for the current user task. Rendered
    /// in the status bar alongside [`Self::main_tools_total`].
    main_tools_completed: u64,
    /// Main-agent tool calls requested for the current user task. Sub-agent
    /// calls are excluded because they roll up under their `agent_start`
    /// parent.
    main_tools_total: u64,
    /// Main-agent tool call ids whose foreground placeholder has returned, but
    /// whose real background result is still pending. These keep the status-bar
    /// tool chip visible and incomplete.
    main_backgrounded_tools: HashSet<String>,
    /// Whether the currently active prompt/agent lifecycle belongs to the
    /// user-facing main agent. Side conversations temporarily make this
    /// false while preserving the main task's counters.
    main_agent_turn_active: bool,
    /// Whether the main-agent tool usage chip should be painted. This is
    /// separate from the counters so side-conversation lifecycles can hide
    /// the chip until a main lifecycle event makes the main turn active
    /// again.
    main_tools_visible: bool,
    /// Whether to render per-turn token usage stats below completed
    /// agent responses.
    show_turn_stats: bool,
    /// Whether to show a temporary full-redraw counter in the status bar.
    redraw_counter: bool,
    /// Maximum number of rendered history lines replayed on full redraw.
    redraw_history_size: usize,
    /// Whether to show UI↔harness socket throughput in the status bar.
    show_ui_io: bool,
    /// Latest rolling UI↔harness socket throughput maxima.
    ui_io_stats: UiIoStats,
    last_full_render_count: u64,
    last_full_render_at: Option<Instant>,
    /// Tool block visibility mode.
    show_tools: tau_config::settings::ShowTools,
    /// Agent/user message visibility mode.
    show_messages: tau_config::settings::ShowMessages,
    /// Harness/UI notice visibility threshold.
    notice_level: tau_proto::NoticeLevel,
    /// Whether to show an indicator when prompt input rows are hidden.
    show_prompt_scroll_indicator: bool,
    /// Tool summary blocks keyed by their block id. Hidden when
    /// `show_tools` is `Full` or `Compact`, rendered in summarize modes.
    tool_summaries: HashMap<tau_cli_term::BlockId, ToolSummaryDisplay>,
    /// In `summarize-prompt` mode, the single summary block for the
    /// active user prompt. Reused across the follow-up agent turns the
    /// harness creates while feeding tool results back to the model.
    prompt_tool_summary: Option<tau_cli_term::BlockId>,
    /// Whether [`Self::prompt_tool_summary`] currently lives in the bottom
    /// active-tools area. In `summarize-prompt` mode the summary stays sticky
    /// across tool follow-up turns, then moves to history when the assistant
    /// finishes without requesting more tools.
    prompt_tool_summary_active: bool,
    /// Snapshot of persisted CLI settings, kept in sync with visible UI
    /// toggles by [`Self::save_cli_state`]. The input loop captures this
    /// handle in the `/set` name-completion closure so the menu can show each
    /// setting's current value without snooping on renderer-thread fields
    /// directly.
    cli_state_mirror: std::sync::Arc<std::sync::Mutex<tau_config::settings::CliState>>,
    /// Cumulative end-to-end time spent waiting for agent responses.
    cumulative_agent_latency: Duration,
    /// Shared effort mirror kept in sync with harness state.
    effort_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Shared fast-service-tier mirror for the input thread's `fast-toggle`
    /// binding.
    fast_service_tier_state: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Shared active-role mirror for input-thread role cycling.
    current_role_state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Shared ordered role names for input-thread role cycling.
    roles_available: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Shared custom prompt templates announced by the running harness.
    custom_prompts: std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessCustomPrompt>>>,
    /// Shared ordered role groups for input-thread role cycling.
    role_groups_available: std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessRoleGroup>>>,
    /// Last selected role per role group for in-memory group cycling.
    role_group_memory: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Shared verbosity mirror kept symmetric with `effort_state`.
    verbosity_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Shared thinking-summary mirror. Kept symmetric with the
    /// other knobs for future cycle helpers.
    thinking_summary_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Context appended to files opened by the external prompt editor.
    /// Locked with `if let Ok(...)` rather than [`crate::locked`] because
    /// this is best-effort UI metadata: if another holder panicked we'd
    /// rather drop one editor-context update than crash the renderer
    /// thread.
    editor_context: std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>>,
    /// Per-visible-transcript response context published into
    /// [`Self::editor_context`]. Prompt-local editor fields such as previous
    /// prompt and trailer recovery remain in the shared context.
    editor_conversation_context: EditorConversationContext,
    /// True while folding an event for a hidden agent transcript. During this
    /// window renderer fields contain the hidden agent's snapshot, but
    /// input-loop mirrors must continue exposing the actually visible
    /// transcript.
    suppress_editor_context_publish: bool,
    /// Symbol shown before the active prompt input.
    prompt_symbol: String,
    /// Symbol shown before submitted prompts in the transcript.
    submitted_prompt_symbol: String,
    /// Shared flag telling the input loop whether Tau knows about
    /// in-flight agent/session work. Updated before side-conversation
    /// filtering so sub-agent activity protects Ctrl-D too.
    agent_in_progress: Arc<AtomicBool>,
    /// Detailed lifecycle bookkeeping backing [`Self::agent_in_progress`].
    agent_activity: AgentActivity,
}

/// Shared state needed by renderer-owned selection changes to retarget prompt
/// drafts without sending protocol events directly from the renderer thread.
struct DraftRetargeter {
    /// Debounce mailbox owned by the CLI input/draft subsystem.
    handle: Arc<(Mutex<DraftSlot>, Condvar)>,
    /// Authoritative current session id shared with input routing.
    session_id: Arc<Mutex<String>>,
}

#[derive(Default)]
struct AgentUiState {
    output: tau_cli_term::OutputSnapshot,
    /// Active watched-agent indicator blocks that belong to this output
    /// snapshot.
    ///
    /// These rows are transient global UI for the currently selected watcher,
    /// but their block ids live inside the terminal output snapshot. Keeping
    /// the ids with the snapshot prevents a restored transcript from retaining
    /// an old `watching [...]` row while the global renderer state has
    /// forgotten it and creates a second row for the same watched agent.
    watched_agent_blocks: HashMap<String, tau_cli_term::BlockId>,
    editor_conversation_context: EditorConversationContext,
    prompts: HashMap<String, PromptState>,
    last_user_block: Option<(tau_cli_term::BlockId, String)>,
    queued_user_blocks: VecDeque<(tau_cli_term::BlockId, String)>,
    tool_calls: HashMap<String, ToolCallState>,
    shell_blocks: HashMap<String, ShellBlockState>,
    model_status_block: Option<tau_cli_term::BlockId>,
    diff_blocks: Vec<DiffBlockEntry>,
    thinking_history: Vec<ThinkingBlockEntry>,
    turn_stats_history: Vec<TurnStatsBlockEntry>,
    tool_history: Vec<ToolBlockEntry>,
    message_history: Vec<MessageBlockEntry>,
    current_context_percent: Option<u8>,
    current_context_input_tokens: Option<u64>,
    current_context_window: Option<u64>,
    main_tools_completed: u64,
    main_tools_total: u64,
    main_backgrounded_tools: HashSet<String>,
    main_agent_turn_active: bool,
    main_tools_visible: bool,
    tool_summaries: HashMap<tau_cli_term::BlockId, ToolSummaryDisplay>,
    prompt_tool_summary: Option<tau_cli_term::BlockId>,
    prompt_tool_summary_active: bool,
    preserve_on_fresh_agent_switch: bool,
    /// Whether this snapshot contains globally owned message-fact output.
    contains_global_message_fact: bool,
    /// Whether this snapshot contains all-agent overview message output.
    contains_overview_message: bool,
    cumulative_agent_latency: Duration,
    agent_activity: AgentActivity,
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
    Agent(String),
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
    Agent(String),
}

impl EventAgentIdResolution {
    fn from_agent_id(agent_id: Option<String>) -> Self {
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

    fn into_agent_id(self, current_agent_id: Option<&str>) -> Option<String> {
        match self {
            Self::Unhandled => current_agent_id.map(str::to_owned),
            Self::NoAgent => None,
            Self::Agent(agent_id) => Some(agent_id),
        }
    }
}

/// One completed file-mutation tool block. Held so `/set show-diff` can
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
    pub(crate) active_tool_ids: HashSet<String>,
    /// Whether quota pacing currently needs minute-boundary repainting.
    pub(crate) quota_active: bool,
    pub(crate) done: bool,
}

impl ToolTimerNotifier {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new((
                std::sync::Mutex::new(ToolTimerState {
                    active_tool_ids: HashSet::new(),
                    quota_active: false,
                    done: false,
                }),
                std::sync::Condvar::new(),
            )),
        }
    }

    pub(crate) fn inner(&self) -> Arc<(std::sync::Mutex<ToolTimerState>, std::sync::Condvar)> {
        self.inner.clone()
    }

    fn tool_started(&self, call_id: &str) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.active_tool_ids.insert(call_id.to_owned());
            cv.notify_all();
        }
    }

    fn tool_finished(&self, call_id: &str) {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageRenderMode {
    Hidden,
    Summary,
    Full,
}

/// One finished thinking block. Held so `/set show-thinking` can swap
/// its content between the original reasoning text (visible) and
/// empty content (hidden) without losing the block's position in
/// the transcript.
struct ThinkingBlockEntry {
    block_id: tau_cli_term::BlockId,
    text: String,
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
                _ => {}
            }
        }

        details
    }

    fn short_description(&self) -> String {
        let mut parts = Vec::new();
        if let Some(model) = self.model.as_deref() {
            parts.push(model.to_owned());
        }
        if let Some(effort) = self.effort.as_deref() {
            parts.push(format!("e={effort}"));
        }
        if let Some(verbosity) = self.verbosity.as_deref() {
            parts.push(format!("v={verbosity}"));
        }
        if let Some(thinking_summary) = self.thinking_summary.as_deref() {
            parts.push(format!("ts={thinking_summary}"));
        }
        if let Some(service_tier) = self.service_tier.as_deref() {
            parts.push(format!("st={service_tier}"));
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
            _ => "unset".to_owned(),
        }
    }
}

fn role_value_completion(setting: &str, value: &str) -> tau_cli_term::CompletionItem {
    let description = match (setting, value) {
        (_, "reset") => "clear this role setting",
        ("effort", "off") => "disable reasoning effort",
        ("effort", "minimal") => "minimum reasoning effort",
        ("effort", "low") => "light reasoning effort",
        ("effort", "medium") => "balanced reasoning effort",
        ("effort", "high") => "strong reasoning effort",
        ("effort", "xhigh") => "extra-high reasoning effort",
        ("effort", "max") => "maximum reasoning effort for GPT-5.6",
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
            "reset", "off", "minimal", "low", "medium", "high", "xhigh", "max",
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
    usage: tau_proto::ProviderTokenUsage,
    /// Same-agent previous response usage captured when this block was
    /// recorded. Re-render paths must use this stored baseline instead of
    /// recomputing from whichever agent/transcript is currently visible.
    previous_usage: Option<tau_proto::ProviderTokenUsage>,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
}

/// Per-prompt UI state held by [`EventRenderer`]. Lives from the first
/// event observed for the prompt (`AgentPromptStarted`,
/// fallback `AgentPromptCreated`, or
/// `ProviderPromptSubmitted`) through `ProviderResponseFinished` or
/// `AgentPromptTerminated`.
#[derive(Default)]
struct PromptState {
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
    /// Live provider-side compaction block. Created only while a provider emits
    /// an in-progress compaction item, then removed on completion/cancel.
    compaction_block_id: Option<tau_cli_term::BlockId>,
    /// Dispatch timestamp, used to compute end-to-end latency on
    /// `ProviderResponseFinished`.
    started_at: Option<Instant>,
}

/// Per-tool-call UI state held by [`EventRenderer`]. Created when the
/// harness publishes `ToolStarted` (or when a sub-agent's finish marks the call
/// as suppressed) and torn down on `ToolResult`/`ToolError`.
#[derive(Default)]
struct ToolCallState {
    /// Live tool-call block in the active-tools area. `None` for sub-agent
    /// tool calls whose UI is suppressed while generic watched-agent indicators
    /// summarize their owner agent's activity.
    block_id: Option<tau_cli_term::BlockId>,
    /// Empty history placeholder allocated at the tool call's logical
    /// transcript position. Final results fill this block so live progress
    /// can update the bottom active-tools area without mutating old
    /// transcript rows.
    history_block_id: Option<tau_cli_term::BlockId>,
    /// Latest live display for the block, used when `/set show-tools`
    /// flips while the call is still running.
    live_display: Option<ToolCallDisplay>,
    /// Monotonic start time for live duration updates.
    started_at: Option<Instant>,
    /// Harness log timestamp for final duration chips.
    recorded_started_at: Option<UnixMicros>,
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

/// In-flight state for a user `!`/`!!` shell block.
struct ShellBlockState {
    block_id: tau_cli_term::BlockId,
    command: String,
    include_in_context: bool,
    /// Output accumulated from `ShellCommandProgress` chunks. Rendered
    /// under the header each redraw.
    output: String,
}

fn push_status_chip(
    themed: &mut tau_themes::ThemedText,
    style: tau_themes::StyleIdx,
    needs_space: &mut bool,
    text: impl Into<String>,
) {
    if *needs_space {
        themed.push_default(" ");
    }
    themed.push(style, text.into());
    *needs_space = true;
}

pub(crate) fn unix_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn push_ui_io_status_chip(
    themed: &mut tau_themes::ThemedText,
    needs_space: &mut bool,
    stats: UiIoStats,
    low_style: tau_themes::StyleIdx,
    medium_style: tau_themes::StyleIdx,
    high_style: tau_themes::StyleIdx,
) {
    let style = ui_io_status_style(stats, low_style, medium_style, high_style);
    push_status_chip(
        themed,
        style,
        needs_space,
        format!(
            "io ↑{} ↓{}",
            format_ui_io_rate(stats.uplink_max_bytes_per_sec),
            format_ui_io_rate(stats.downlink_max_bytes_per_sec)
        ),
    );
}

fn ui_io_status_style(
    stats: UiIoStats,
    low_style: tau_themes::StyleIdx,
    medium_style: tau_themes::StyleIdx,
    high_style: tau_themes::StyleIdx,
) -> tau_themes::StyleIdx {
    let max_bytes_per_sec = stats
        .uplink_max_bytes_per_sec
        .max(stats.downlink_max_bytes_per_sec);
    if max_bytes_per_sec < UI_IO_MEDIUM_BYTES_PER_SEC {
        low_style
    } else if max_bytes_per_sec < UI_IO_HIGH_BYTES_PER_SEC {
        medium_style
    } else {
        high_style
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
    format!(" ({elapsed_seconds}s, {bytes}, Δ{delta_rate}, {total_rate})")
}

fn response_stats_indicator_for_prompt(state: &PromptState) -> String {
    state
        .provider_response_stats
        .as_ref()
        .map_or_else(String::new, response_stats_indicator_suffix)
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
            EventRenderer::compaction_success_status(
                compaction.original_input_tokens,
                compaction.compacted_input_tokens,
            ),
        )),
    }
}

fn reasoning_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ReasoningText(reasoning) => Some(reasoning.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn assistant_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(assistant_text_from_context_item)
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn assistant_text_from_context_item(item: &ContextItem) -> Option<String> {
    match item {
        ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content,
            ..
        }) => Some(
            content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => text.as_str(),
                })
                .collect::<String>(),
        ),
        _ => None,
    }
}

fn assistant_text_from_message_item(message: &MessageItem) -> Option<String> {
    if message.role != ContextRole::Assistant {
        return None;
    }
    let text = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn tool_calls_from_output_items(output_items: &[ContextItem]) -> Vec<ToolCallItem> {
    output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// Semantic state of a visible direct watched-agent row.
pub(crate) enum WatchedAgentActivity<'a> {
    /// The directed watch edge reports a running outer turn.
    Running,
    /// The edge is idle but its target watches an active descendant.
    Watching {
        /// Nearest directly running descendant, identified by stable id.
        witness: &'a str,
    },
}

/// Builds the generic tool-block-shaped display for a watched-agent indicator.
///
/// This intentionally reuses [`tau_proto::ToolUseState`] counter formatting so
/// rows keep the compact generic layout, an explicit `@agent_id` chip, and no
/// in-progress status suffix. Both direct `running` and transitive `watching`
/// labels use the historical `watching.name` style so watched-agent activity
/// remains visually distinct from actual tool calls.
pub(crate) fn watched_agent_tool_display(
    label: &str,
    agent_id: &str,
    stats: Option<&tau_proto::AgentStatsUpdated>,
    activity: WatchedAgentActivity<'_>,
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
        args: format!("[{label}]"),
        progress_counters,
        status: ToolUseStatus::Success,
        status_text: String::new(),
        ..Default::default()
    };
    let (name, witness) = match activity {
        WatchedAgentActivity::Running => ("running", None),
        WatchedAgentActivity::Watching { witness } => ("watching", Some(witness)),
    };
    let mut rendered = render_tool_use_state(name, &display);
    rendered.tool_name_style = Some(tau_themes::names::WATCHING_NAME);
    rendered.suffixes.retain(|suffix| !suffix.text.is_empty());
    rendered.suffixes.insert(
        0,
        ToolSuffixSegment {
            text: format!("@{agent_id}"),
            status: ToolStatus::Info,
            no_leading_space: false,
        },
    );
    if let Some(witness) = witness {
        rendered.suffixes.insert(
            1,
            ToolSuffixSegment {
                text: format!("-> @{witness}"),
                status: ToolStatus::Info,
                no_leading_space: false,
            },
        );
    }
    rendered
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
            tau_config::settings::CliState::default(),
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
        let cli_state_mirror = std::sync::Arc::new(std::sync::Mutex::new(state.clone()));
        handle.set_redraw_history_size(state.redraw_history_size);
        Self {
            handle,
            completion_data,
            action_state: ActionCommandState::new(std::iter::empty::<&str>()),
            skill_state: SkillCommandState::new(),
            theme,
            current_agent_id: None,
            displayed_agent_id: None,
            awaiting_new_agent_selection: false,
            no_agent_ui_state: AgentUiState::default(),
            agents_ui_state: HashMap::new(),
            preserve_on_fresh_agent_switch: false,
            contains_global_message_fact: false,
            contains_overview_message: false,
            overview_message_ids: HashSet::new(),
            known_agents: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            agent_display_names: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            agent_navigation: Arc::new(Mutex::new(AgentNavigation::default())),
            ephemeral_agents: std::sync::Arc::new(std::sync::Mutex::new(HashSet::new())),
            query_agents: HashMap::new(),
            prompt_agents: HashMap::new(),
            tool_agents: HashMap::new(),
            shell_agents: HashMap::new(),
            watched_agents: HashMap::new(),
            agent_watchers: HashMap::new(),
            agent_stats: HashMap::new(),
            agent_models: HashMap::new(),
            watched_agent_turn_states: HashMap::new(),
            active_agent_prompts: HashMap::new(),
            terminal_agent_prompts: HashSet::new(),
            finished_provider_prompts: HashSet::new(),
            watched_agent_blocks: HashMap::new(),
            current_agent_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
            draft_retargeter: None,
            prompts: HashMap::new(),
            last_user_block: None,
            queued_user_blocks: VecDeque::new(),
            tool_calls: HashMap::new(),
            tool_timer: None,
            shell_blocks: HashMap::new(),
            extension_blocks: HashMap::new(),
            action_invocation_owners: HashMap::new(),
            ready_extensions: HashSet::new(),
            model_status_block: None,
            current_session_id: None,
            right_prompt_paths: None,
            diff_blocks: Vec::new(),
            diffs_expanded: state.show_diff,
            show_thinking: state.show_thinking,
            show_turn_stats: state.show_turn_stats,
            show_tools: state.show_tools,
            show_messages: state.show_messages,
            notice_level: state.notice_level,
            show_prompt_scroll_indicator: state.show_prompt_scroll_indicator,
            show_ui_io: state.show_ui_io,
            ui_io_stats: UiIoStats::default(),
            tool_summaries: HashMap::new(),
            prompt_tool_summary: None,
            prompt_tool_summary_active: false,
            cli_state_mirror,
            thinking_history: Vec::new(),
            turn_stats_history: Vec::new(),
            tool_history: Vec::new(),
            message_history: Vec::new(),
            state_dirs,
            current_model: None,
            quota_pacing: crate::provider_quota::QuotaPacingState::default(),
            last_quota_tick: None,
            current_role: None,
            model_params: tau_proto::ModelParams::default(),
            role_defaults: HashMap::new(),
            baseline_params: None,
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_window: None,
            main_tools_completed: 0,
            main_tools_total: 0,
            main_backgrounded_tools: HashSet::new(),
            main_agent_turn_active: false,
            main_tools_visible: false,
            redraw_counter: state.redraw_counter,
            redraw_history_size: state.redraw_history_size,
            last_full_render_count: 0,
            last_full_render_at: None,
            cumulative_agent_latency: Duration::ZERO,
            effort_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                tau_proto::Effort::Off.as_u8(),
            )),
            fast_service_tier_state: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            current_role_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
            roles_available: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            custom_prompts: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            role_groups_available: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            role_group_memory: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            verbosity_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                tau_proto::Verbosity::default().as_u8(),
            )),
            thinking_summary_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                tau_proto::ThinkingSummary::default().as_u8(),
            )),
            editor_context: std::sync::Arc::new(std::sync::Mutex::new(
                tau_cli_term::EditorContext::default(),
            )),
            editor_conversation_context: EditorConversationContext::default(),
            suppress_editor_context_publish: false,
            prompt_symbol,
            submitted_prompt_symbol,
            agent_in_progress: Arc::new(AtomicBool::new(false)),
            agent_activity: AgentActivity::default(),
        }
    }

    pub(crate) fn set_tool_timer(&mut self, timer: ToolTimerNotifier) {
        self.tool_timer = Some(timer);
    }

    /// Returns test-only generic tool bookkeeping without exposing a production
    /// inspection side channel.
    #[cfg(test)]
    pub(crate) fn test_active_tool_count(&self) -> usize {
        self.tool_calls.len()
    }

    pub(crate) fn set_draft_retargeter(
        &mut self,
        handle: Arc<(Mutex<DraftSlot>, Condvar)>,
        session_id: Arc<Mutex<String>>,
    ) {
        self.draft_retargeter = Some(DraftRetargeter { handle, session_id });
    }

    /// Configures the filesystem context rendered beside the current session.
    pub(crate) fn set_right_prompt_paths(
        &mut self,
        cwd: std::path::PathBuf,
        home: Option<std::path::PathBuf>,
    ) {
        self.right_prompt_paths = Some((cwd, home));
    }

    pub(crate) fn known_agents(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        self.known_agents.clone()
    }

    pub(crate) fn agent_display_names(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<String, String>>> {
        self.agent_display_names.clone()
    }

    pub(crate) fn ephemeral_agents(&self) -> std::sync::Arc<std::sync::Mutex<HashSet<String>>> {
        self.ephemeral_agents.clone()
    }

    pub(crate) fn agent_navigation(&self) -> Arc<Mutex<AgentNavigation>> {
        self.agent_navigation.clone()
    }

    pub(crate) fn current_agent_state(&self) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
        self.current_agent_state.clone()
    }

    #[cfg(test)]
    pub(crate) fn tool_agent_for_test(&self, call_id: &str) -> Option<String> {
        self.tool_agents.get(call_id).cloned()
    }

    #[cfg(test)]
    /// Removes the status block so tests can exercise placeholder-only redraws.
    pub(crate) fn clear_model_status_for_test(&mut self) {
        self.model_status_block = None;
    }

    #[cfg(test)]
    pub(crate) fn agent_id_for_event_for_test(&self, event: &Event) -> Option<String> {
        self.agent_id_for_event(event)
    }

    pub(crate) fn switch_agent(&mut self, agent_id: String) {
        self.switch_agent_after_display_update(agent_id, || {});
    }

    #[cfg(test)]
    /// Invokes `after_display_update` after restoring the destination
    /// transcript but before updating the selected target, status, or
    /// placeholder.
    pub(crate) fn switch_agent_after_display_update_for_test(
        &mut self,
        agent_id: String,
        after_display_update: impl FnOnce(),
    ) {
        self.switch_agent_after_display_update(agent_id, after_display_update);
    }

    fn switch_agent_after_display_update(
        &mut self,
        agent_id: String,
        after_display_update: impl FnOnce(),
    ) {
        let handle = self.handle.clone();
        // A selection transition publishes one coherent transcript, target,
        // status, and placeholder frame. Input routing is mirrored earlier by
        // the input thread and is intentionally outside this renderer batch.
        handle.with_redraw_suppressed(|| {
            self.remember_agent(agent_id.clone());
            let target_changed = self.current_agent_id.as_deref() != Some(agent_id.as_str());
            let display_changed = self.displayed_agent_id.as_deref() != Some(agent_id.as_str());

            if display_changed {
                // Let transcript switching see the previous awaiting flag so it can
                // distinguish initial no-agent adoption from explicit `/agent new`.
                self.show_agent_transcript(agent_id.clone());
            }
            after_display_update();
            self.awaiting_new_agent_selection = false;

            if target_changed {
                self.set_current_agent_id(Some(agent_id), false);
                self.render_model_status();
                self.refresh_prompt_placeholder();
                handle.redraw();
            }
        });
    }

    pub(crate) fn clear_selected_agent(&mut self) {
        self.clear_selected_agent_after_display_update(|| {});
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

    fn clear_selected_agent_after_display_update(&mut self, after_display_update: impl FnOnce()) {
        let handle = self.handle.clone();
        handle.with_redraw_suppressed(|| {
            let target_changed = self.current_agent_id.is_some();
            let display_changed = self.displayed_agent_id.is_some();
            if target_changed || display_changed {
                // Only a clear that actually leaves an agent creates the explicit
                // no-agent boundary. A delayed clear command that arrives after
                // `/session new` while the UI is already on the fresh initial
                // screen must stay a no-op, otherwise the first new-session agent
                // would incorrectly clear startup history instead of adopting it.
                self.awaiting_new_agent_selection = true;
            }

            if display_changed {
                self.store_visible_agent_state();
                let state = std::mem::take(&mut self.no_agent_ui_state);
                self.restore_visible_agent_state(state);
                self.rerender_visible_for_current_settings();
                self.displayed_agent_id = None;
            }
            after_display_update();

            if target_changed {
                self.set_current_agent_id(None, false);
                self.render_model_status();
                self.refresh_prompt_placeholder();
                handle.redraw();
            }
        });
    }

    fn store_visible_agent_state(&mut self) {
        let state = self.take_visible_agent_state();
        if let Some(displayed) = self.displayed_agent_id.clone() {
            self.agents_ui_state.insert(displayed, state);
        } else {
            self.no_agent_ui_state = state;
        }
    }

    fn show_agent_transcript(&mut self, agent_id: String) {
        let needs_snapshot_swap = self.displayed_agent_id.is_some()
            || self.agents_ui_state.contains_key(&agent_id)
            || self.visible_no_agent_snapshot_needs_preservation();
        if needs_snapshot_swap {
            self.store_visible_agent_state();
            let state = self.agents_ui_state.remove(&agent_id).unwrap_or_default();
            self.restore_visible_agent_state(state);
            self.rerender_visible_for_current_settings();
        }
        if !needs_snapshot_swap && self.displayed_agent_id.is_none() {
            self.adopt_visible_no_agent_owners(agent_id.as_str());
        }
        self.displayed_agent_id = Some(agent_id);
    }

    fn adopt_visible_no_agent_owners(&mut self, agent_id: &str) {
        let owner = UiSnapshotOwner::Agent(agent_id.to_owned());
        for state in self.extension_blocks.values_mut() {
            if matches!(state.owner, UiSnapshotOwner::NoAgent) {
                state.owner = owner.clone();
            }
        }
        for invocation_owner in self.action_invocation_owners.values_mut() {
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
        // `/agent none` or `/agent new`, when the user has deliberately left a
        // previous transcript and the no-agent output must remain available.
        self.displayed_agent_id.is_none()
            && (self.contains_global_message_fact
                || self.contains_overview_message
                || (self.awaiting_new_agent_selection
                    && (self.preserve_on_fresh_agent_switch || self.has_pending_no_agent_owner())))
    }

    fn has_pending_no_agent_owner(&self) -> bool {
        self.action_invocation_owners
            .values()
            .any(|owner| matches!(owner, UiSnapshotOwner::NoAgent))
            || self
                .extension_blocks
                .values()
                .any(|state| matches!(state.owner, UiSnapshotOwner::NoAgent))
    }

    fn set_current_agent_id(&mut self, agent_id: Option<String>, retarget_draft: bool) {
        self.current_agent_id = agent_id.clone();
        if let Ok(mut current) = self.current_agent_state.lock() {
            *current = agent_id;
        }
        if retarget_draft {
            self.retarget_prompt_draft();
        }
        self.refresh_watched_agent_blocks();
    }

    fn handle_agent_watches_updated(&mut self, updated: &tau_proto::AgentWatchesUpdated) {
        if self
            .current_session_id
            .as_ref()
            .is_some_and(|session_id| session_id != &updated.session_id)
        {
            return;
        }
        let watcher_id = updated.watcher_id.to_string();
        let next_watched: HashSet<_> = updated
            .watched_agent_ids
            .iter()
            .map(ToString::to_string)
            .collect();
        let previous_watched: HashSet<_> = self
            .watched_agents
            .get(&watcher_id)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        if let Some(states) = self.watched_agent_turn_states.get_mut(&watcher_id) {
            states.retain(|state_watched, _| {
                next_watched.contains(state_watched)
                    && (matches!(
                        updated.cause,
                        tau_proto::AgentWatchUpdateCause::SessionSnapshot
                    ) || previous_watched.contains(state_watched.as_str()))
            });
            if states.is_empty() {
                self.watched_agent_turn_states.remove(&watcher_id);
            }
        }
        if let Some(previous) = self.watched_agents.get(&watcher_id) {
            for watched_agent_id in previous {
                if let Some(watchers) = self.agent_watchers.get_mut(watched_agent_id) {
                    watchers.retain(|candidate| candidate != &watcher_id);
                    if watchers.is_empty() {
                        self.agent_watchers.remove(watched_agent_id);
                    }
                }
            }
        }
        for watched_agent_id in &updated.watched_agent_ids {
            let watchers = self
                .agent_watchers
                .entry(watched_agent_id.to_string())
                .or_default();
            if !watchers.iter().any(|candidate| candidate == &watcher_id) {
                watchers.push(watcher_id.clone());
                watchers.sort();
            }
        }
        self.watched_agents.insert(
            watcher_id,
            updated
                .watched_agent_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
        );
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn handle_agent_stats_updated(&mut self, updated: &tau_proto::AgentStatsUpdated) {
        if self
            .current_session_id
            .as_ref()
            .is_some_and(|session_id| session_id != &updated.session_id)
        {
            return;
        }
        if let Ok(mut navigation) = self.agent_navigation.lock() {
            navigation.apply_stats(
                updated.agent_id.as_str(),
                updated.navigation_mode,
                updated.runtime_state,
            );
        }
        self.agent_stats
            .insert(updated.agent_id.to_string(), updated.clone());
        self.render_model_status_if_present();
        if self.current_agent_id.as_deref() == Some(updated.agent_id.as_str()) {
            self.refresh_prompt_placeholder();
            self.handle.redraw();
        }
        self.refresh_watched_agent_blocks();
    }

    fn refresh_watched_agent_blocks(&mut self) {
        let Some(current) = self.current_agent_id.clone() else {
            self.clear_watched_agent_blocks();
            return;
        };
        let watched = self
            .watched_agents
            .get(&current)
            .cloned()
            .unwrap_or_default();
        let projection = self.watch_activity_projection();
        let mut active: Vec<String> = watched
            .into_iter()
            .filter(|agent_id| {
                projection.edge_is_directly_running(&current, agent_id)
                    || projection.watcher_is_active(agent_id)
            })
            .collect();
        active.sort();
        let active_set: HashSet<_> = active.iter().cloned().collect();
        let stale: Vec<_> = self
            .watched_agent_blocks
            .keys()
            .filter(|agent_id| !active_set.contains(*agent_id))
            .cloned()
            .collect();
        for agent_id in stale {
            if let Some(block_id) = self.watched_agent_blocks.remove(&agent_id) {
                self.handle.remove_block(block_id);
            }
        }
        for (index, agent_id) in active.iter().enumerate() {
            let block = self.watched_agent_block(&current, agent_id, &projection);
            let block_id = if let Some(block_id) = self.watched_agent_blocks.get(agent_id).copied()
            {
                self.handle.set_block(block_id, block);
                block_id
            } else {
                let block_id = self
                    .handle
                    .new_block(format!("watched-agent:{agent_id}"), block);
                self.watched_agent_blocks.insert(agent_id.clone(), block_id);
                block_id
            };
            let later_blocks = active[index + 1..].iter().filter_map(|later_agent_id| {
                self.watched_agent_blocks.get(later_agent_id).copied()
            });
            self.handle
                .push_above_active_before_any(block_id, later_blocks);
        }
        self.handle.redraw();
    }

    fn agent_has_active_prompt(&self, agent_id: &str) -> bool {
        self.active_agent_prompts
            .get(agent_id)
            .is_some_and(|prompts| !prompts.is_empty())
    }

    /// Returns the authoritative agent-turn state for one directed watch.
    ///
    /// Prompt activity is retained as a compatibility/catch-up fallback until
    /// the first structured lifecycle snapshot for this watch is observed.
    fn watched_agent_is_running(&self, watcher_id: &str, watched_agent_id: &str) -> bool {
        self.watched_agent_turn_states
            .get(watcher_id)
            .and_then(|states| states.get(watched_agent_id))
            .map_or_else(
                || self.agent_has_active_prompt(watched_agent_id),
                |state| state.state == tau_proto::AgentRuntimeState::Running,
            )
    }

    /// Derives exact recursive watch activity from current live topology and
    /// edge-authoritative direct lifecycle facts.
    fn watch_activity_projection(&self) -> WatchActivityProjection {
        let direct_edges = self
            .watched_agents
            .iter()
            .flat_map(|(watcher, watched)| {
                watched
                    .iter()
                    .filter(|target| self.watched_agent_is_running(watcher, target))
                    .map(|target| (watcher.clone(), target.clone()))
            })
            .collect();
        WatchActivityProjection::new(&self.watched_agents, &self.agent_watchers, direct_edges)
    }

    /// Records a structured outer agent-turn snapshot or edge for a watch.
    fn handle_watched_agent_turn_state(
        &mut self,
        message: &tau_proto::AgentMessageReceived,
        state: &tau_proto::AgentWatchTurnStateNotification,
    ) {
        if self
            .current_session_id
            .as_ref()
            .is_some_and(|session_id| session_id != state.session_id.as_str())
        {
            return;
        }
        let watcher_id = message.recipient_id.as_str();
        let watched_agent_id = message.sender_id.as_str();
        let stale = self
            .watched_agent_turn_states
            .get(watcher_id)
            .and_then(|states| states.get(watched_agent_id))
            .is_some_and(|current| {
                current.subscription_id == state.subscription_id
                    && (state.turn_generation < current.turn_generation
                        || (state.turn_generation == current.turn_generation
                            && current.state == tau_proto::AgentRuntimeState::Idle
                            && state.state == tau_proto::AgentRuntimeState::Running))
            });
        if !stale {
            self.watched_agent_turn_states
                .entry(watcher_id.to_owned())
                .or_default()
                .insert(watched_agent_id.to_owned(), state.clone());
            self.render_model_status_if_present();
            self.refresh_watched_agent_blocks();
        }
    }

    fn mark_agent_prompt_active(&mut self, agent_id: &str, agent_prompt_id: &str) {
        if self.terminal_agent_prompts.contains(agent_prompt_id) {
            return;
        }
        self.remove_active_agent_prompt(agent_prompt_id);
        self.active_agent_prompts
            .entry(agent_id.to_owned())
            .or_default()
            .insert(agent_prompt_id.to_owned());
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn mark_known_agent_prompt_active(
        &mut self,
        agent_prompt_id: &str,
        originator: &tau_proto::PromptOriginator,
    ) {
        if let Some(agent_id) = self
            .prompt_agents
            .get(agent_prompt_id)
            .cloned()
            .or_else(|| self.agent_id_for_originator(originator))
        {
            self.mark_agent_prompt_active(&agent_id, agent_prompt_id);
        }
    }

    fn mark_agent_prompt_inactive(&mut self, _agent_id: &str, agent_prompt_id: &str) {
        self.terminal_agent_prompts
            .insert(agent_prompt_id.to_owned());
        self.remove_active_agent_prompt(agent_prompt_id);
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn remove_active_agent_prompt(&mut self, agent_prompt_id: &str) {
        self.active_agent_prompts.retain(|_, prompts| {
            prompts.remove(agent_prompt_id);
            !prompts.is_empty()
        });
    }

    fn clear_watched_agent_blocks(&mut self) {
        for (_, block_id) in self.watched_agent_blocks.drain() {
            self.handle.remove_block(block_id);
        }
        self.handle.redraw();
    }

    /// Retires all cached watch topology and state involving an unloaded agent.
    fn remove_agent_watch_endpoint(&mut self, agent_id: &str) {
        self.watched_agents.remove(agent_id);
        for watched in self.watched_agents.values_mut() {
            watched.retain(|watched_id| watched_id != agent_id);
        }
        self.agent_watchers.remove(agent_id);
        for watchers in self.agent_watchers.values_mut() {
            watchers.retain(|watcher_id| watcher_id != agent_id);
        }
        self.agent_watchers
            .retain(|_, watchers| !watchers.is_empty());
        self.watched_agent_turn_states.remove(agent_id);
        for states in self.watched_agent_turn_states.values_mut() {
            states.remove(agent_id);
        }
        self.watched_agent_turn_states
            .retain(|_, states| !states.is_empty());
        self.render_model_status_if_present();
        self.refresh_watched_agent_blocks();
    }

    fn watched_agent_block(
        &self,
        watcher_id: &str,
        agent_id: &str,
        projection: &WatchActivityProjection,
    ) -> tau_cli_term::StyledBlock {
        let label = self
            .agent_display_names
            .lock()
            .ok()
            .and_then(|names| names.get(agent_id).cloned())
            .unwrap_or_else(|| agent_id.to_owned());
        let stats = self.agent_stats.get(agent_id);
        let directly_running = projection.edge_is_directly_running(watcher_id, agent_id);
        let witness = (!directly_running)
            .then(|| projection.witness_for(agent_id, &self.watched_agents))
            .flatten();
        let activity = if directly_running {
            WatchedAgentActivity::Running
        } else {
            WatchedAgentActivity::Watching {
                witness: witness
                    .as_deref()
                    .expect("recursive activity has a directly running witness"),
            }
        };
        let display = watched_agent_tool_display(&label, agent_id, stats, activity);
        render_tool_block(&self.theme, &display)
    }

    fn retarget_prompt_draft(&self) {
        let Some(retargeter) = &self.draft_retargeter else {
            return;
        };
        let session_id = retargeter
            .session_id
            .lock()
            .map(|session_id| session_id.clone())
            .unwrap_or_default();
        let target_agent_id = self.current_agent_id.as_deref().map(|agent_id| {
            tau_proto::AgentId::parse(agent_id).expect("renderer stores valid agent ids")
        });
        retarget_prompt_draft_snapshot(
            retargeter.handle.as_ref(),
            session_id.into(),
            target_agent_id,
            self.handle.get_buffer(),
        );
    }

    fn refresh_prompt_placeholder(&mut self) {
        let current_agent_navigation = self.current_agent_id.as_deref().and_then(|agent_id| {
            self.agent_navigation
                .lock()
                .ok()
                .map(|navigation| (navigation.mode(agent_id), navigation.is_active(agent_id)))
        });
        self.handle
            .set_input_placeholder(crate::theme::prompt_input_placeholder(
                &self.theme,
                self.current_role.as_deref(),
                self.current_agent_id.as_deref(),
                current_agent_navigation,
            ));
    }

    pub(crate) fn set_action_state(&mut self, action_state: ActionCommandState) {
        self.action_state = action_state;
        self.refresh_action_completions();
    }

    /// Remember which transcript was viewed when a dynamic action was invoked.
    pub(crate) fn record_action_invocation(
        &mut self,
        invocation_id: tau_proto::ActionInvocationId,
        owner_agent_id: Option<String>,
    ) {
        let owner = owner_agent_id
            .map(UiSnapshotOwner::Agent)
            .unwrap_or(UiSnapshotOwner::NoAgent);
        self.action_invocation_owners.insert(invocation_id, owner);
    }

    pub(crate) fn skill_arg_completer(&self) -> tau_cli_term::ArgCompleter {
        self.skill_state.arg_completer()
    }

    fn take_visible_agent_state(&mut self) -> AgentUiState {
        AgentUiState {
            output: self.handle.output_snapshot(),
            watched_agent_blocks: std::mem::take(&mut self.watched_agent_blocks),
            editor_conversation_context: std::mem::take(&mut self.editor_conversation_context),
            prompts: std::mem::take(&mut self.prompts),
            last_user_block: self.last_user_block.take(),
            queued_user_blocks: std::mem::take(&mut self.queued_user_blocks),
            tool_calls: std::mem::take(&mut self.tool_calls),
            shell_blocks: std::mem::take(&mut self.shell_blocks),
            model_status_block: self.model_status_block.take(),
            diff_blocks: std::mem::take(&mut self.diff_blocks),
            thinking_history: std::mem::take(&mut self.thinking_history),
            turn_stats_history: std::mem::take(&mut self.turn_stats_history),
            tool_history: std::mem::take(&mut self.tool_history),
            message_history: std::mem::take(&mut self.message_history),
            current_context_percent: self.current_context_percent.take(),
            current_context_input_tokens: self.current_context_input_tokens.take(),
            current_context_window: self.current_context_window.take(),
            main_tools_completed: std::mem::take(&mut self.main_tools_completed),
            main_tools_total: std::mem::take(&mut self.main_tools_total),
            main_backgrounded_tools: std::mem::take(&mut self.main_backgrounded_tools),
            main_agent_turn_active: std::mem::take(&mut self.main_agent_turn_active),
            main_tools_visible: std::mem::take(&mut self.main_tools_visible),
            tool_summaries: std::mem::take(&mut self.tool_summaries),
            prompt_tool_summary: self.prompt_tool_summary.take(),
            prompt_tool_summary_active: std::mem::take(&mut self.prompt_tool_summary_active),
            preserve_on_fresh_agent_switch: std::mem::take(
                &mut self.preserve_on_fresh_agent_switch,
            ),
            contains_global_message_fact: std::mem::take(&mut self.contains_global_message_fact),
            contains_overview_message: std::mem::take(&mut self.contains_overview_message),
            cumulative_agent_latency: std::mem::take(&mut self.cumulative_agent_latency),
            agent_activity: std::mem::take(&mut self.agent_activity),
        }
    }

    fn restore_visible_agent_state(&mut self, state: AgentUiState) {
        self.restore_visible_agent_state_inner(state, true, true);
    }

    fn restore_hidden_agent_state(&mut self, state: AgentUiState) {
        self.restore_visible_agent_state_inner(state, false, false);
    }

    fn restore_visible_agent_state_inner(
        &mut self,
        state: AgentUiState,
        redraw: bool,
        publish_editor_context: bool,
    ) {
        if redraw {
            self.handle.replace_output_snapshot(state.output);
        } else {
            self.handle.replace_output_snapshot_quiet(state.output);
        }
        self.editor_conversation_context = state.editor_conversation_context;
        self.watched_agent_blocks = state.watched_agent_blocks;
        if publish_editor_context {
            self.publish_editor_conversation_context();
        }
        self.prompts = state.prompts;
        self.last_user_block = state.last_user_block;
        self.queued_user_blocks = state.queued_user_blocks;
        self.tool_calls = state.tool_calls;
        self.shell_blocks = state.shell_blocks;
        self.model_status_block = state.model_status_block;
        self.diff_blocks = state.diff_blocks;
        self.thinking_history = state.thinking_history;
        self.turn_stats_history = state.turn_stats_history;
        self.tool_history = state.tool_history;
        self.message_history = state.message_history;
        self.current_context_percent = state.current_context_percent;
        self.current_context_input_tokens = state.current_context_input_tokens;
        self.current_context_window = state.current_context_window;
        self.main_tools_completed = state.main_tools_completed;
        self.main_tools_total = state.main_tools_total;
        self.main_backgrounded_tools = state.main_backgrounded_tools;
        self.main_agent_turn_active =
            state.main_agent_turn_active && state.agent_activity.is_in_progress();
        self.main_tools_visible = state.main_tools_visible && self.main_agent_turn_active;
        self.tool_summaries = state.tool_summaries;
        self.prompt_tool_summary = state.prompt_tool_summary;
        self.prompt_tool_summary_active = state.prompt_tool_summary_active;
        self.preserve_on_fresh_agent_switch = state.preserve_on_fresh_agent_switch;
        self.contains_global_message_fact = state.contains_global_message_fact;
        self.contains_overview_message = state.contains_overview_message;
        self.cumulative_agent_latency = state.cumulative_agent_latency;
        self.agent_activity = state.agent_activity;
    }

    fn agent_display_label(&self, agent_id: &str) -> String {
        self.agent_display_names
            .lock()
            .ok()
            .and_then(|names| names.get(agent_id).cloned())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty() && name != agent_id)
            .map(|name| format!("{agent_id} ({name})"))
            .unwrap_or_else(|| agent_id.to_owned())
    }

    /// Builds a message-safe label from current local presentation metadata.
    fn message_agent_display_label(&self, agent_id: &str, use_local_names: bool) -> String {
        if !use_local_names {
            return format!("@{agent_id}");
        }
        let display_name = self
            .agent_display_names
            .lock()
            .ok()
            .and_then(|names| names.get(agent_id).cloned());
        Self::agent_identity_with_name(&format!("@{agent_id}"), display_name.as_deref(), agent_id)
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

    fn watched_by_status(&self, agent_id: &str) -> Option<String> {
        let watchers = self.agent_watchers.get(agent_id)?;
        let first = watchers.first()?;
        match watchers.len() {
            0 => None,
            1 => Some(first.to_owned()),
            count => Some(format!("{first}, +{} more agents", count.saturating_sub(1))),
        }
    }

    fn remember_agent(&mut self, agent_id: String) {
        if let Ok(mut agents) = self.known_agents.lock()
            && !agents.iter().any(|known| known == &agent_id)
        {
            agents.push(agent_id);
            agents.sort();
        }
    }

    fn remember_agent_display_name(&mut self, agent_id: &str, display_name: &str) {
        let mut changed = false;
        if let Ok(mut names) = self.agent_display_names.lock() {
            let display_name = display_name.trim();
            if !display_name.is_empty() {
                changed = names
                    .get(agent_id)
                    .is_none_or(|known| known != display_name);
                names.insert(agent_id.to_owned(), display_name.to_owned());
            }
        }
        if changed {
            self.rerender_message_history();
        }
    }

    /// Clears presentation metadata whose authority is limited to one session.
    fn clear_agent_display_names(&self) {
        if let Ok(mut names) = self.agent_display_names.lock() {
            names.clear();
        }
    }

    /// Reprojects visible message blocks from semantic events and current UI
    /// settings, including the latest session-scoped agent metadata.
    fn rerender_message_history(&mut self) {
        for entry in &self.message_history {
            let use_local_names = entry.session_id == self.current_session_id;
            self.handle.set_block(
                entry.block_id,
                self.render_agent_message_block_with_local_names(&entry.event, use_local_names),
            );
        }
        if !self.message_history.is_empty() {
            self.handle.redraw();
        }
    }

    fn mark_agent_live(&mut self, agent_id: String) {
        self.remember_agent(agent_id.clone());
        if let Ok(mut navigation) = self.agent_navigation.lock() {
            navigation.mark_live(agent_id);
        }
        self.render_model_status_if_present();
    }

    fn remember_agent_ephemeral(&mut self, agent_id: &str) {
        if let Ok(mut agents) = self.ephemeral_agents.lock() {
            agents.insert(agent_id.to_owned());
        }
    }

    fn render_model_status_if_present(&mut self) {
        if self.model_status_block.is_some() {
            self.render_model_status();
        }
    }

    fn save_cli_state(&self) {
        let state = tau_config::settings::CliState {
            show_diff: self.diffs_expanded,
            show_thinking: self.show_thinking,
            show_turn_stats: self.show_turn_stats,
            redraw_counter: self.redraw_counter,
            redraw_history_size: self.redraw_history_size,
            show_ui_io: self.show_ui_io,
            show_tools: self.show_tools,
            show_messages: self.show_messages,
            notice_level: self.notice_level,
            show_status: tau_config::settings::ShowStatus::All,
            show_prompt_scroll_indicator: self.show_prompt_scroll_indicator,
        };
        if let Ok(mut mirror) = self.cli_state_mirror.lock() {
            *mirror = state.clone();
        }
        state.save(&self.state_dirs);
    }

    /// Shared snapshot of the persisted CLI settings, updated in sync
    /// with every successful `/set` (i.e. on every
    /// [`Self::save_cli_state`] call). Cloned by the input loop so the
    /// `/set` name-completion menu can show each setting's current
    /// value without touching renderer-thread fields directly.
    pub(crate) fn cli_state_mirror(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_config::settings::CliState>> {
        self.cli_state_mirror.clone()
    }

    pub(crate) fn editor_context(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>> {
        self.editor_context.clone()
    }

    fn publish_editor_conversation_context(&self) {
        if self.suppress_editor_context_publish {
            return;
        }
        if let Ok(mut context) = self.editor_context.lock() {
            context.current_response = self.editor_conversation_context.current_response.clone();
            context.last_response = self.editor_conversation_context.last_response.clone();
        }
    }

    fn set_editor_current_response(&mut self, text: Option<String>) {
        self.editor_conversation_context.current_response = text;
        self.publish_editor_conversation_context();
    }

    fn set_editor_last_response(&mut self, text: String) {
        self.editor_conversation_context.last_response = Some(text);
        self.editor_conversation_context.current_response = None;
        self.publish_editor_conversation_context();
    }

    fn with_editor_context_publish_suppressed<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let previous = self.suppress_editor_context_publish;
        self.suppress_editor_context_publish = true;
        let result = f(self);
        self.suppress_editor_context_publish = previous;
        result
    }

    /// Returns a shared flag that is true while any agent/session work
    /// is in flight. The input loop uses it to keep Ctrl-D from
    /// terminating an active session accidentally.
    pub(crate) fn agent_in_progress_state(&self) -> Arc<AtomicBool> {
        self.agent_in_progress.clone()
    }

    #[cfg(test)]
    pub(crate) fn main_agent_turn_active_for_test(&self) -> bool {
        self.main_agent_turn_active
    }

    /// Reports whether the selected main agent has effective live activity.
    #[cfg(test)]
    fn main_agent_is_in_progress_for_test(&self) -> bool {
        self.main_agent_turn_active && self.agent_activity.is_in_progress()
    }

    /// Returns a clone of the shared Fast-mode mirror, used by configurable
    /// bindings.
    pub(crate) fn fast_service_tier_state(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.fast_service_tier_state.clone()
    }

    /// Returns a clone of the shared active-role mirror used by role cycling.
    pub(crate) fn current_role_state(&self) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
        self.current_role_state.clone()
    }

    /// Returns a clone of the shared ordered role list used by role cycling.
    pub(crate) fn roles_available(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        self.roles_available.clone()
    }

    /// Returns a clone of the shared custom prompts announced by the harness.
    pub(crate) fn custom_prompts(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessCustomPrompt>>> {
        self.custom_prompts.clone()
    }

    /// Returns a clone of the shared ordered role groups used by role cycling.
    pub(crate) fn role_groups_available(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessRoleGroup>>> {
        self.role_groups_available.clone()
    }

    /// Returns a clone of the per-group runtime role memory used by role
    /// cycling.
    pub(crate) fn role_group_memory(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<String, String>>> {
        self.role_group_memory.clone()
    }

    /// Applies a runtime `/theme` change to this renderer-only UI process.
    pub(crate) fn apply_theme(&mut self, theme: tau_themes::Theme) {
        self.theme = theme;
        self.handle
            .set_left_prompt(crate::theme::active_prompt_marker(
                &self.theme,
                &self.prompt_symbol,
                self.current_role.as_deref(),
            ));
        self.refresh_prompt_placeholder();
        let effective_session_id = self
            .draft_retargeter
            .as_ref()
            .and_then(|retargeter| {
                retargeter
                    .session_id
                    .lock()
                    .ok()
                    .map(|session_id| tau_proto::SessionId::from(session_id.clone()))
            })
            .or_else(|| self.current_session_id.clone());
        if let Some(session_id) = effective_session_id {
            self.render_right_prompt_context(&session_id);
        }
        self.render_model_status_if_present();
        self.rerender_visible_for_current_settings();
        self.handle.invalidate_screen();
    }

    /// Apply a `/set <name> <value>` change. The caller (input loop)
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
                if let Some(show_tools) = tau_config::settings::ShowTools::parse(value) {
                    self.set_show_tools(show_tools);
                }
            }
            "show-messages" => {
                if let Some(show_messages) = tau_config::settings::ShowMessages::parse(value) {
                    self.set_show_messages(show_messages);
                }
            }
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
        if self.diffs_expanded == on {
            return;
        }
        self.diffs_expanded = on;
        for entry in &self.diff_blocks {
            let block = self.render_diff_history_block(&entry.display, &entry.diff);
            self.handle.set_block(entry.block_id, block);
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
        if self.show_thinking == on {
            return;
        }
        self.show_thinking = on;
        for entry in &self.thinking_history {
            let display = if self.show_thinking {
                entry.text.as_str()
            } else {
                ""
            };
            self.handle.set_block(
                entry.block_id,
                markdown_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        for state in self.prompts.values_mut() {
            let Some(bid) = state.thinking_block_id else {
                continue;
            };
            let block = if self.show_thinking {
                let display = state.thinking_text.clone().unwrap_or_default();
                markdown_streaming_block(
                    &self.theme,
                    names::AGENT_THINKING,
                    &display,
                    &mut state.thinking_markdown_cache,
                )
            } else {
                markdown_block(&self.theme, names::AGENT_THINKING, "")
            };
            self.handle.set_block(bid, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Force a full repaint after a `/set show-*` change. Edited blocks
    /// from earlier in the transcript may already have scrolled out of
    /// the visible window, so the renderer needs to redraw from scratch
    /// for the change to take effect retroactively across scrollback.
    fn invalidate_for_retroactive_toggle(&mut self) {
        self.handle.invalidate_screen();
    }

    fn set_redraw_counter(&mut self, on: bool) {
        if self.redraw_counter == on {
            return;
        }
        self.redraw_counter = on;
        self.render_model_status();
        self.save_cli_state();
    }

    fn set_redraw_history_size(&mut self, redraw_history_size: usize) {
        if self.redraw_history_size == redraw_history_size {
            return;
        }
        let previous = self.redraw_history_size;
        self.redraw_history_size = redraw_history_size;
        self.handle.set_redraw_history_size(redraw_history_size);
        if previous < redraw_history_size {
            self.handle.invalidate_screen();
        }
        self.save_cli_state();
    }

    fn set_show_ui_io(&mut self, on: bool) {
        if self.show_ui_io == on {
            return;
        }
        self.show_ui_io = on;
        self.render_model_status();
        self.save_cli_state();
    }

    pub(crate) fn handle_ui_io_sample(&mut self, stats: UiIoStats) {
        if self.ui_io_stats == stats {
            return;
        }
        self.ui_io_stats = stats;
        if self.show_ui_io {
            self.render_model_status();
        }
    }

    fn set_show_turn_stats(&mut self, on: bool) {
        if self.show_turn_stats == on {
            return;
        }
        self.show_turn_stats = on;
        for entry in &self.turn_stats_history {
            let block = self.render_turn_stats_entry(entry);
            self.handle.set_block(entry.block_id, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }
    fn render_turn_stats_entry(&self, entry: &TurnStatsBlockEntry) -> tau_cli_term::StyledBlock {
        if self.show_turn_stats {
            render_turn_stats_block(
                &self.theme,
                &entry.usage,
                entry.previous_usage.as_ref(),
                entry.turn_latency,
                entry.total_latency,
            )
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

    fn compaction_success_status(
        original_input_tokens: Option<u64>,
        compacted_input_tokens: Option<u64>,
    ) -> String {
        match (original_input_tokens, compacted_input_tokens) {
            (Some(original), Some(compacted)) => format!(
                "{} ok: {}",
                Self::compaction_token_chip(original),
                Self::compaction_token_chip(compacted)
            ),
            (Some(original), None) => format!("{} ok", Self::compaction_token_chip(original)),
            (None, Some(compacted)) => format!("ok: {}", Self::compaction_token_chip(compacted)),
            (None, None) => "ok".to_owned(),
        }
    }

    fn render_tool_history_block(&self, display: &ToolCallDisplay) -> tau_cli_term::StyledBlock {
        match self.show_tools {
            tau_config::settings::ShowTools::Full => render_tool_block(&self.theme, display),
            tau_config::settings::ShowTools::Compact => self.render_compact_tool_block(display),
            tau_config::settings::ShowTools::Off
            | tau_config::settings::ShowTools::SummarizeTurn
            | tau_config::settings::ShowTools::SummarizePrompt => Self::empty_block(),
        }
    }

    fn render_compact_tool_block(&self, display: &ToolCallDisplay) -> tau_cli_term::StyledBlock {
        let mut display = display.clone();
        display.payload = None;
        render_tool_block(&self.theme, &display)
    }

    fn render_diff_history_block(
        &self,
        display: &ToolCallDisplay,
        diff: &tau_proto::ToolUsePayload,
    ) -> tau_cli_term::StyledBlock {
        match self.show_tools {
            tau_config::settings::ShowTools::Full => match diff {
                tau_proto::ToolUsePayload::Diff(summary) => {
                    render_diff_tool_block(&self.theme, display, summary, self.diffs_expanded)
                }
                tau_proto::ToolUsePayload::Diffs { files } => {
                    render_multi_diff_tool_block(&self.theme, display, files, self.diffs_expanded)
                }
                tau_proto::ToolUsePayload::Text { .. } => render_tool_block(&self.theme, display),
            },
            tau_config::settings::ShowTools::Compact => self.render_compact_tool_block(display),
            tau_config::settings::ShowTools::Off
            | tau_config::settings::ShowTools::SummarizeTurn
            | tau_config::settings::ShowTools::SummarizePrompt => Self::empty_block(),
        }
    }

    fn render_summary_block(&self, summary: &ToolSummaryDisplay) -> tau_cli_term::StyledBlock {
        if matches!(
            self.show_tools,
            tau_config::settings::ShowTools::SummarizeTurn
                | tau_config::settings::ShowTools::SummarizePrompt
        ) {
            render_tool_block(&self.theme, &build_tool_summary_display(summary))
        } else {
            Self::empty_block()
        }
    }

    fn update_tool_summary_block(&mut self, block_id: tau_cli_term::BlockId) {
        let Some(summary) = self.tool_summaries.get(&block_id) else {
            return;
        };
        self.handle
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
        if let Some(summary) = self.tool_summaries.get_mut(&block_id) {
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
            .tool_summaries
            .get(&block_id)
            .is_some_and(|summary| summary.completed == summary.total);
        if finished {
            if self.prompt_tool_summary == Some(block_id) && self.prompt_tool_summary_active {
                self.update_tool_summary_block(block_id);
                return;
            }
            let Some(summary) = self.tool_summaries.remove(&block_id) else {
                return;
            };
            self.handle.remove_block(block_id);
            let new_block_id = self
                .handle
                .print_output("tool-summary", self.render_summary_block(&summary));
            self.tool_summaries.insert(new_block_id, summary);
        } else {
            self.update_tool_summary_block(block_id);
        }
    }

    fn rerender_visible_for_current_settings(&mut self) {
        use tau_themes::names;
        self.rerender_message_history();
        for entry in &self.tool_history {
            self.handle.set_block(
                entry.block_id,
                self.render_tool_history_block(&entry.display),
            );
        }
        for entry in &self.diff_blocks {
            self.handle.set_block(
                entry.block_id,
                self.render_diff_history_block(&entry.display, &entry.diff),
            );
        }
        for (block_id, summary) in &self.tool_summaries {
            self.handle
                .set_block(*block_id, self.render_summary_block(summary));
        }
        for entry in &self.thinking_history {
            let display = if self.show_thinking {
                entry.text.as_str()
            } else {
                ""
            };
            self.handle.set_block(
                entry.block_id,
                markdown_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        for entry in &self.turn_stats_history {
            let block = self.render_turn_stats_entry(entry);
            self.handle.set_block(entry.block_id, block);
        }
    }

    fn set_show_messages(&mut self, show_messages: tau_config::settings::ShowMessages) {
        if self.show_messages == show_messages {
            return;
        }
        self.show_messages = show_messages;
        self.rerender_message_history();
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    fn set_notice_level(&mut self, notice_level: tau_proto::NoticeLevel) {
        if self.notice_level == notice_level {
            return;
        }
        self.notice_level = notice_level;
        self.save_cli_state();
    }

    fn set_show_prompt_scroll_indicator(&mut self, enabled: bool) {
        if self.show_prompt_scroll_indicator == enabled {
            return;
        }
        self.show_prompt_scroll_indicator = enabled;
        self.handle.set_prompt_scroll_indicator(enabled);
        self.handle.redraw();
        self.save_cli_state();
    }

    /// Applies the UI-owned threshold and override policy from
    /// `SPEC-tau-cli-notice-filtering`.
    fn notice_visible(&self, level: tau_proto::NoticeLevel, always_show: bool) -> bool {
        level == tau_proto::NoticeLevel::Critical
            || always_show
            || level.visible_at(self.notice_level)
    }

    fn set_show_tools(&mut self, show_tools: tau_config::settings::ShowTools) {
        if self.show_tools == show_tools {
            return;
        }
        self.show_tools = show_tools;
        for entry in &self.tool_history {
            self.handle.set_block(
                entry.block_id,
                self.render_tool_history_block(&entry.display),
            );
        }
        for entry in &self.diff_blocks {
            self.handle.set_block(
                entry.block_id,
                self.render_diff_history_block(&entry.display, &entry.diff),
            );
        }
        for (block_id, summary) in &self.tool_summaries {
            self.handle
                .set_block(*block_id, self.render_summary_block(summary));
        }
        let freeze_multiline_payloads = self.freeze_multiline_live_payloads();
        let mut live_updates = Vec::new();
        for state in self.tool_calls.values_mut() {
            if let Some(block_id) = state.block_id {
                let display = if let Some(display) = state.live_display.as_ref() {
                    let mut display = display.clone();
                    let duration = Self::live_tool_duration(state);
                    Self::normalize_live_tool_duration(
                        freeze_multiline_payloads,
                        &mut display,
                        duration,
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
                .map(|display| self.render_tool_history_block(display))
                .unwrap_or_else(Self::empty_block);
            self.handle.set_block(block_id, block);
        }
        for state in self.tool_calls.values() {
            if let Some(block_id) = state.summary_block_id
                && let Some(summary) = self.tool_summaries.get(&block_id)
            {
                self.handle
                    .set_block(block_id, self.render_summary_block(summary));
            }
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Clears all session-scoped UI state and re-renders an empty
    /// transcript. Persistent user preferences such as `show-diff`
    /// and `show-thinking` are intentionally preserved.
    fn clear_for_new_session(&mut self) {
        self.agents_ui_state.clear();
        self.no_agent_ui_state = AgentUiState::default();
        self.overview_message_ids.clear();
        self.query_agents.clear();
        self.prompt_agents.clear();
        self.tool_agents.clear();
        self.shell_agents.clear();
        self.watched_agents.clear();
        self.agent_watchers.clear();
        self.agent_stats.clear();
        self.watched_agent_turn_states.clear();
        self.active_agent_prompts.clear();
        self.terminal_agent_prompts.clear();
        self.clear_watched_agent_blocks();
        if let Ok(mut agents) = self.known_agents.lock() {
            agents.clear();
        }
        if let Ok(mut navigation) = self.agent_navigation.lock() {
            navigation.clear();
        }
        if let Ok(mut agents) = self.ephemeral_agents.lock() {
            agents.clear();
        }
        self.clear_agent_display_names();
        self.clear_selected_agent();
        // A new session starts from the same append-in-place no-agent state as
        // process startup. Unlike explicit `/agent none`, there is no previous
        // in-session agent transcript to protect from the first new agent.
        self.awaiting_new_agent_selection = false;
        self.agents_ui_state.clear();
        self.prompts.clear();
        self.last_user_block = None;
        self.queued_user_blocks.clear();
        self.tool_calls.clear();
        if let Some(timer) = &self.tool_timer {
            timer.clear_active();
        }
        self.shell_blocks.clear();
        self.extension_blocks.clear();
        self.action_invocation_owners.clear();
        self.model_status_block = None;
        self.diff_blocks.clear();
        self.thinking_history.clear();
        self.turn_stats_history.clear();
        self.tool_history.clear();
        self.message_history.clear();
        self.tool_summaries.clear();
        self.prompt_tool_summary = None;
        self.prompt_tool_summary_active = false;
        self.preserve_on_fresh_agent_switch = false;
        self.contains_global_message_fact = false;
        self.contains_overview_message = false;
        // Model selection and effort are harness-global, not
        // session-scoped. `/session new` only causes a SessionStarted event;
        // the harness does not re-emit HarnessRoleSelected for the
        // unchanged model. Keep the cached selection so the status bar
        // can be recreated after clearing the terminal output.
        self.current_context_percent = None;
        self.current_context_input_tokens = None;
        self.main_tools_completed = 0;
        self.main_tools_total = 0;
        self.main_backgrounded_tools.clear();
        self.main_agent_turn_active = false;
        self.main_tools_visible = false;
        self.cumulative_agent_latency = Duration::ZERO;
        self.agent_activity.clear();
        self.update_agent_in_progress();
        self.handle.clear_output();
        self.render_session_preamble();
        if self.current_session_id.is_some()
            || self.current_model.is_some()
            || self.current_role.is_some()
        {
            self.render_model_status();
        }
    }

    fn render_session_preamble(&mut self) {
        if !self.notice_visible(tau_proto::NoticeLevel::Info, false) {
            return;
        }
        self.handle.print_output(
            "banner",
            tau_cli_term::StyledBlock::new(build_banner(&self.theme)),
        );
        let mut extensions: Vec<_> = self.ready_extensions.iter().collect();
        extensions.sort();
        for extension_name in extensions {
            self.handle.print_output(
                "extension-kept",
                extension_status_block(&self.theme, extension_name, "kept"),
            );
        }
    }

    fn render_model_status(&mut self) {
        use tau_cli_term::StyledBlock;
        use tau_cli_term::resolve::{convert_color, themed_text};
        use tau_themes::{StyleName, ThemedText, names};

        let mut themed = ThemedText::new();
        let mut right_themed = ThemedText::new();
        let status_style = themed.add_style(names::MODEL_STATUS);
        let model_style = themed.add_style(names::STATUS_MODEL);
        let role_style = themed.add_style(names::STATUS_ROLE);
        let effort_style = themed.add_style(names::STATUS_EFFORT);
        let verbosity_style = themed.add_style(names::STATUS_VERBOSITY);
        let service_tier_style = themed.add_style(names::STATUS_SERVICE_TIER);
        let tools_style = right_themed.add_style(names::STATUS_TOOLS);
        let agents_style = right_themed.add_style(names::STATUS_AGENTS);
        let context_style = right_themed.add_style(names::STATUS_CONTEXT);
        let quota_under_style = right_themed.add_style(names::STATUS_QUOTA_UNDER);
        let quota_aligned_style = right_themed.add_style(names::STATUS_QUOTA_ALIGNED);
        let quota_over_style = right_themed.add_style(names::STATUS_QUOTA_OVER);
        let quota_danger_style = right_themed.add_style(names::STATUS_QUOTA_DANGER);
        let quota_unknown_style = right_themed.add_style(names::STATUS_QUOTA_UNKNOWN);
        let ui_io_low_style = right_themed.add_style(names::STATUS_UI_IO_LOW);
        let ui_io_medium_style = right_themed.add_style(names::STATUS_UI_IO_MEDIUM);
        let ui_io_high_style = right_themed.add_style(names::STATUS_UI_IO_HIGH);
        let redraw_style = right_themed.add_style(names::REDRAW_COUNTER);
        let mut needs_space = false;
        let mut right_needs_space = false;

        match (
            self.current_agent_id.as_deref(),
            self.current_role.as_deref(),
            self.current_model.as_ref(),
        ) {
            (Some(agent_id), _, _) => {
                push_status_chip(
                    &mut themed,
                    role_style,
                    &mut needs_space,
                    format!("@{}", self.agent_display_label(agent_id)),
                );
                if let Some(watched_by) = self.watched_by_status(agent_id) {
                    push_status_chip(&mut themed, status_style, &mut needs_space, watched_by);
                }
            }
            (None, Some(role), _) => push_status_chip(
                &mut themed,
                role_style,
                &mut needs_space,
                format!("+{role}"),
            ),
            (None, None, Some(model)) => push_status_chip(
                &mut themed,
                model_style,
                &mut needs_space,
                format!("={model}"),
            ),
            (None, None, None) if self.current_session_id.is_none() => push_status_chip(
                &mut themed,
                status_style,
                &mut needs_space,
                "no role selected".to_owned(),
            ),
            (None, None, None) => {}
        }
        let show_effort = self.baseline_params.map_or_else(
            || {
                self.role_default_effort()
                    .map_or(!self.model_params.effort.is_default(), |default| {
                        self.model_params.effort != default
                    })
            },
            |default| self.model_params.effort != default.effort,
        );
        if show_effort {
            push_status_chip(
                &mut themed,
                effort_style,
                &mut needs_space,
                format!("^{}", self.model_params.effort.as_str()),
            );
        }
        let show_verbosity = self.baseline_params.map_or_else(
            || {
                self.role_default_verbosity()
                    .map_or(!self.model_params.verbosity.is_default(), |default| {
                        self.model_params.verbosity != default
                    })
            },
            |default| self.model_params.verbosity != default.verbosity,
        );
        if show_verbosity {
            push_status_chip(
                &mut themed,
                verbosity_style,
                &mut needs_space,
                format!("~{}", self.model_params.verbosity.as_str()),
            );
        }
        let show_service_tier = self
            .baseline_params
            .map_or(self.model_params.service_tier.is_some(), |default| {
                self.model_params.service_tier != default.service_tier
            });
        if show_service_tier {
            let service_tier = self
                .model_params
                .service_tier
                .map(|tier| tier.as_str())
                .unwrap_or("off");
            push_status_chip(
                &mut themed,
                service_tier_style,
                &mut needs_space,
                format!("!{service_tier}"),
            );
        }
        if let Some(tools) = self.main_tools_status_chip() {
            push_status_chip(
                &mut right_themed,
                tools_style,
                &mut right_needs_space,
                format!("%{tools}"),
            );
        }
        let active_agents = self.active_side_agent_count();
        if 0 < active_agents {
            push_status_chip(
                &mut right_themed,
                agents_style,
                &mut right_needs_space,
                format!("@{active_agents}"),
            );
        }
        if let Some(context) = self.context_status_chip() {
            push_status_chip(
                &mut right_themed,
                context_style,
                &mut right_needs_space,
                format!("#{context}"),
            );
        }
        if self.show_ui_io {
            push_ui_io_status_chip(
                &mut right_themed,
                &mut right_needs_space,
                self.ui_io_stats,
                ui_io_low_style,
                ui_io_medium_style,
                ui_io_high_style,
            );
        }

        let full_render_count = self.handle.full_render_count();
        if self.last_full_render_count < full_render_count {
            self.last_full_render_count = full_render_count;
            self.last_full_render_at = Some(Instant::now());
        }
        let show_redraw_counter = self.redraw_counter
            && self
                .last_full_render_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(5 * 60));
        if show_redraw_counter {
            push_status_chip(
                &mut right_themed,
                redraw_style,
                &mut right_needs_space,
                full_render_count.to_string(),
            );
        }

        let quota_model = self.current_agent_id.as_ref().map_or_else(
            || self.current_model.clone(),
            |agent_id| self.agent_models.get(agent_id).cloned(),
        );
        let quota =
            quota_model.and_then(|model| self.quota_pacing.classify(&model, unix_time_millis()));
        if let Some(quota) = quota {
            let style = match quota {
                crate::provider_quota::QuotaPacing::FarUnder => quota_under_style,
                crate::provider_quota::QuotaPacing::Aligned => quota_aligned_style,
                crate::provider_quota::QuotaPacing::Over => quota_over_style,
                crate::provider_quota::QuotaPacing::Danger => quota_danger_style,
                crate::provider_quota::QuotaPacing::Unknown => quota_unknown_style,
            };
            push_status_chip(
                &mut right_themed,
                style,
                &mut right_needs_space,
                quota.chip().to_owned(),
            );
        }
        if let Some(timer) = &self.tool_timer {
            timer.set_quota_active(quota.is_some());
        }

        let bg = self
            .theme
            .resolve_style(&StyleName::new(names::MODEL_STATUS))
            .bg;
        let mut block = StyledBlock::new(themed_text(&self.theme, &themed))
            .right_content(themed_text(&self.theme, &right_themed));
        if let Some(bg) = bg {
            block = block.bg(convert_color(bg));
        }
        match self.model_status_block {
            Some(bid) => {
                self.handle.set_block(bid, block);
            }
            None => {
                let bid = self.handle.new_block("model-status", block);
                self.handle.push_below(bid);
                self.model_status_block = Some(bid);
            }
        }
        self.handle.redraw();
    }

    fn role_default_effort(&self) -> Option<tau_proto::Effort> {
        let role = self.current_role.as_deref()?;
        self.role_defaults
            .get(role)?
            .effort
            .as_deref()?
            .parse()
            .ok()
    }

    fn role_default_verbosity(&self) -> Option<tau_proto::Verbosity> {
        let role = self.current_role.as_deref()?;
        self.role_defaults
            .get(role)?
            .verbosity
            .as_deref()?
            .parse()
            .ok()
    }

    fn active_side_agent_count(&self) -> usize {
        let mut watched = HashSet::new();
        for watched_agent_ids in self.watched_agents.values() {
            for agent_id in watched_agent_ids {
                watched.insert(agent_id.as_str());
            }
        }
        let projection = self.watch_activity_projection();
        let prompt_only = self
            .active_agent_prompts
            .iter()
            .filter(|(agent_id, prompts)| {
                !prompts.is_empty()
                    && !watched.contains(agent_id.as_str())
                    && self.current_agent_id.as_deref() != Some(agent_id.as_str())
            });
        projection
            .effective_targets()
            .iter()
            .filter(|agent_id| self.current_agent_id.as_deref() != Some(agent_id.as_str()))
            .count()
            + prompt_only.count()
    }

    fn main_tools_status_chip(&self) -> Option<String> {
        ((self.main_tools_visible || !self.main_backgrounded_tools.is_empty())
            && self.main_tools_total != 0)
            .then(|| format!("{}/{}", self.main_tools_completed, self.main_tools_total))
    }

    fn record_main_tool_completed(&mut self) {
        if self.main_tools_completed < self.main_tools_total {
            self.main_tools_completed += 1;
        }
    }

    fn set_main_tools_visible(&mut self, visible: bool) {
        if self.main_tools_visible == visible {
            return;
        }
        self.main_tools_visible = visible;
        if self.model_status_block.is_some() {
            self.render_model_status();
        }
    }

    fn set_main_agent_turn_active(&mut self, active: bool) {
        self.main_agent_turn_active = active;
        self.set_main_tools_visible(active && self.main_tools_total != 0);
    }

    fn clear_main_agent_turn_active_everywhere(&mut self) {
        self.set_main_agent_turn_active(false);
        for state in self.agents_ui_state.values_mut() {
            state.main_agent_turn_active = false;
            state.main_tools_visible = false;
        }
    }

    fn has_live_main_delegate_tool_call(&self) -> bool {
        self.tool_calls
            .values()
            .any(|state| state.is_main_delegate && !state.is_sub_agent)
    }

    fn sync_agent_activity_for_lifecycle(&mut self, event: &Event) {
        match event {
            Event::UiPromptSubmitted(_) => self.agent_activity.mark_optimistic_submission(),
            Event::AgentCompactionTriggered(triggered) => {
                let agent_id = triggered.agent_id.to_string();
                if triggered.originator.is_user() {
                    self.mark_agent_live(agent_id);
                } else {
                    self.remember_agent(agent_id);
                }
            }
            Event::AgentPromptCreated(prompt) => {
                self.agent_activity.start_prompt(&prompt.agent_prompt_id);
            }
            Event::AgentPromptStarted(prompt) => {
                self.agent_activity.start_prompt(&prompt.agent_prompt_id);
            }
            Event::ProviderPromptSubmitted(submitted) => {
                self.agent_activity.start_prompt(&submitted.agent_prompt_id);
            }
            Event::ProviderResponseUpdated(update) => {
                if !self.is_stale_terminal_stats_only_update(update) {
                    self.agent_activity.start_prompt(&update.agent_prompt_id);
                }
            }
            Event::ProviderResponseFinished(finished) => {
                self.agent_activity
                    .finish_prompt_if_active(&finished.agent_prompt_id, &finished.output_items);
            }
            Event::AgentPromptTerminated(terminated) => {
                self.agent_activity
                    .finish_prompt(&terminated.agent_prompt_id, &[]);
            }
            Event::ToolRequest(_) => {}
            Event::ToolStarted(invoke) => self.agent_activity.start_tool(&invoke.call_id),
            Event::ToolRejected(rejected) => {
                self.agent_activity.finish_tool(&rejected.call_id);
            }
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                    self.agent_activity.background_tool(&result.call_id);
                } else {
                    self.agent_activity.finish_tool(&result.call_id);
                }
            }
            Event::ToolError(error) => {
                self.agent_activity.finish_tool(&error.call_id);
            }
            Event::ToolBackgroundResult(result) => {
                self.agent_activity.finish_background_tool(&result.call_id);
            }
            Event::ToolBackgroundError(error) => {
                self.agent_activity.finish_background_tool(&error.call_id);
            }
            Event::ToolCancelled(cancelled) => {
                self.agent_activity
                    .finish_background_tool(&cancelled.call_id);
            }
            Event::UiCancelPrompt(_) => self.agent_activity.clear_optimistic_submissions(),
            Event::SessionShutdown(_) => self.agent_activity.clear(),
            _ => {}
        }
    }

    fn sync_main_tools_visibility_for_prompt_lifecycle(&mut self, event: &Event) {
        match event {
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
                if tool_calls_from_output_items(&finished.output_items).is_empty() {
                    self.clear_main_agent_turn_active_everywhere();
                }
            }
            Event::ProviderResponseFinished(finished)
                if !finished.originator.is_user() && !self.has_live_main_delegate_tool_call() =>
            {
                self.set_main_agent_turn_active(false);
            }
            Event::AgentPromptTerminated(terminated) if terminated.originator.is_user() => {
                if !self.agent_activity.has_active_prompts() {
                    self.set_main_agent_turn_active(false);
                }
            }
            Event::AgentPromptTerminated(terminated)
                if !terminated.originator.is_user() && !self.has_live_main_delegate_tool_call() =>
            {
                self.set_main_agent_turn_active(false);
            }
            _ => {}
        }
    }

    fn reset_main_tool_usage(&mut self) {
        if self.main_tools_completed == 0
            && self.main_tools_total == 0
            && !self.main_tools_visible
            && self.main_backgrounded_tools.is_empty()
        {
            return;
        }
        if self.main_backgrounded_tools.is_empty() {
            self.main_tools_completed = 0;
            self.main_tools_total = 0;
            self.main_tools_visible = false;
        } else {
            self.main_tools_visible = true;
        }
        if self.model_status_block.is_some() {
            self.render_model_status();
        }
    }

    fn context_status_chip(&self) -> Option<String> {
        match (
            self.current_context_percent,
            self.current_context_input_tokens,
            self.current_context_window,
        ) {
            (_, Some(input), Some(window)) => Some(format!(
                "{}/{}",
                format_token_count(input),
                format_token_count(window)
            )),
            (Some(percent), _, Some(window)) => {
                Some(format!("{percent}%/{}", format_token_count(window)))
            }
            (Some(percent), _, None) => Some(format!("{percent}%")),
            (None, Some(input), None) => Some(format_token_count(input)),
            (None, None, Some(window)) => Some(format!("-/{}", format_token_count(window))),
            (None, None, None) => None,
        }
    }

    fn submitted_prompt_block(
        &self,
        body_name: &str,
        body_text: impl Into<String>,
    ) -> tau_cli_term::StyledBlock {
        let body_text = body_text.into();
        markdown_prompt_block(
            &self.theme,
            body_name,
            format!("{} ", self.submitted_prompt_symbol),
            &body_text,
        )
    }

    fn submitted_plain_block(
        &self,
        body_name: &str,
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
                SpanTree::span(
                    marker_style,
                    vec![SpanTree::text(format!("{} ", self.submitted_prompt_symbol))],
                ),
                SpanTree::text(body_text.into()),
            ],
        ));

        let body_ts = self.theme.resolve_style(&StyleName::new(body_name));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.theme, &themed));
        if let Some(bg) = body_ts.bg {
            block = block.bg(convert_color(bg));
        }
        block
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
                    vec![SpanTree::text(format!("{} ", self.submitted_prompt_symbol))],
                ),
                SpanTree::span(body_style, body),
            ],
        ));

        let body_theme_style = self.theme.resolve_style(&StyleName::new(body_name));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.theme, &themed));
        if let Some(background) = body_theme_style.bg {
            block = block.bg(convert_color(background));
        }
        block
    }

    pub(crate) fn handle_disconnect(&mut self, reason: Option<String>) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        self.agent_activity.clear();
        self.agent_in_progress.store(false, Ordering::Relaxed);
        let mut summary_blocks = HashSet::new();
        for state in self.tool_calls.values() {
            if let Some(block_id) = state.block_id {
                self.handle.remove_block(block_id);
            }
            if let Some(block_id) = state.summary_block_id {
                summary_blocks.insert(block_id);
            }
        }
        for block_id in summary_blocks {
            self.handle.remove_block(block_id);
            self.tool_summaries.remove(&block_id);
            if self.prompt_tool_summary == Some(block_id) {
                self.prompt_tool_summary = None;
                self.prompt_tool_summary_active = false;
            }
        }
        if self.prompt_tool_summary_active {
            self.finish_prompt_tool_summary();
        }
        self.tool_calls.clear();
        if let Some(timer) = &self.tool_timer {
            timer.clear_active();
        }
        let reason = reason.as_deref().unwrap_or("disconnected");
        self.handle.print_output(
            "system-disconnect",
            themed_block(&self.theme, names::SYSTEM_DISCONNECT, reason),
        );
    }

    #[cfg(test)]
    pub(crate) fn handle(&mut self, event: &Event) {
        self.handle_recorded_at(event, UnixMicros::now());
    }

    pub(crate) fn handle_recorded_at(&mut self, event: &Event, recorded_at: UnixMicros) {
        self.learn_agent_metadata(event);
        let inter_agent_message = Self::is_inter_agent_message(event);
        self.project_agent_message_to_overview(event);
        if let Some(owner) = self.extension_lifecycle_owner(event) {
            self.handle_recorded_at_for_snapshot_owner(event, recorded_at, owner);
            self.update_agent_in_progress();
            return;
        }
        if let Some(owner) = self.take_action_completion_owner(event) {
            self.handle_recorded_at_for_snapshot_owner(event, recorded_at, owner);
            self.update_agent_in_progress();
            return;
        }
        if let Some(owner) = self.message_fact_snapshot_owner(event) {
            self.handle_recorded_at_for_snapshot_owner(event, recorded_at, owner);
            self.update_agent_in_progress();
            return;
        }
        let target_agent_id = self.agent_id_for_event(event);
        let Some(target_agent_id) = target_agent_id else {
            self.handle_recorded_at_for_visible_agent(event, recorded_at);
            self.update_agent_in_progress();
            return;
        };
        if self.current_agent_id.is_none() {
            if self.event_selects_agent_from_empty(event, &target_agent_id) {
                if self.displayed_agent_id.as_deref() != Some(target_agent_id.as_str()) {
                    self.show_agent_transcript(target_agent_id.clone());
                }
                self.awaiting_new_agent_selection = false;
                self.set_current_agent_id(Some(target_agent_id.clone()), true);
                self.refresh_prompt_placeholder();
                self.render_model_status();
                self.handle_recorded_at_for_visible_agent(event, recorded_at);
                self.update_agent_in_progress();
                return;
            }
            if matches!(
                event,
                Event::AgentMessageSent(_) | Event::AgentMessageReceived(_)
            ) && !inter_agent_message
                && self.agent_message_visible_on_empty_screen(event, &target_agent_id)
            {
                self.handle_recorded_at_for_visible_agent(event, recorded_at);
                self.update_agent_in_progress();
                return;
            }
            if !inter_agent_message
                && !Self::event_originator_is_extension(event)
                && !Self::event_has_explicit_ui_target(event)
                && !self.agents_ui_state.contains_key(&target_agent_id)
            {
                self.handle_recorded_at_for_visible_agent(event, recorded_at);
                self.update_agent_in_progress();
                return;
            }
            let handle = self.handle.clone();
            handle.with_output_transaction(|| {
                let visible_state = self.take_visible_agent_state();
                let target_state = self
                    .agents_ui_state
                    .remove(&target_agent_id)
                    .unwrap_or_default();
                handle.with_redraw_suppressed(|| {
                    self.with_editor_context_publish_suppressed(|this| {
                        this.restore_hidden_agent_state(target_state);
                        this.displayed_agent_id = Some(target_agent_id.clone());
                        this.handle_recorded_at_for_visible_agent(event, recorded_at);
                        let target_state = this.take_visible_agent_state();
                        this.agents_ui_state.insert(target_agent_id, target_state);
                        this.restore_hidden_agent_state(visible_state);
                    });
                });
            });
            self.displayed_agent_id = None;
            self.publish_editor_conversation_context();
            self.update_agent_in_progress();
            return;
        }
        if self.displayed_agent_id.as_deref() == Some(target_agent_id.as_str()) {
            self.handle_recorded_at_for_visible_agent(event, recorded_at);
            self.update_agent_in_progress();
            return;
        }

        let visible_agent_id = self
            .displayed_agent_id
            .clone()
            .or_else(|| self.current_agent_id.clone())
            .unwrap_or_else(|| target_agent_id.clone());
        let handle = self.handle.clone();
        handle.with_output_transaction(|| {
            let visible_state = self.take_visible_agent_state();
            self.agents_ui_state
                .insert(visible_agent_id.clone(), visible_state);
            let target_state = self
                .agents_ui_state
                .remove(&target_agent_id)
                .unwrap_or_default();
            handle.with_redraw_suppressed(|| {
                self.with_editor_context_publish_suppressed(|this| {
                    this.restore_hidden_agent_state(target_state);
                    this.displayed_agent_id = Some(target_agent_id.clone());
                    this.handle_recorded_at_for_visible_agent(event, recorded_at);
                    let target_state = this.take_visible_agent_state();
                    this.agents_ui_state.insert(target_agent_id, target_state);
                    let visible_state = this
                        .agents_ui_state
                        .remove(&visible_agent_id)
                        .unwrap_or_default();
                    this.restore_hidden_agent_state(visible_state);
                });
            });
        });
        self.displayed_agent_id = Some(visible_agent_id);
        self.publish_editor_conversation_context();
        self.update_agent_in_progress();
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
            .overview_message_ids
            .insert((self.current_session_id.clone(), message_id.clone()))
        {
            return;
        }
        if self.displayed_agent_id.is_none() {
            self.contains_overview_message = true;
            self.handle_agent_message_event(event);
            return;
        }

        self.update_hidden_no_agent_state(|this| {
            this.contains_overview_message = true;
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
                ) && !matches!(message.recipient, tau_proto::AgentMessageRecipient::User) =>
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
        match event {
            Event::AgentMessageSent(message) => {
                !matches!(message.recipient, tau_proto::AgentMessageRecipient::User)
            }
            Event::AgentMessageReceived(_) => true,
            _ => false,
        }
    }

    fn extension_lifecycle_owner(&self, event: &Event) -> Option<UiSnapshotOwner> {
        let instance_id = match event {
            Event::ExtensionReady(ready) => ready.instance_id,
            Event::ExtensionExited(exited) => exited.instance_id,
            _ => return None,
        };
        self.extension_blocks
            .get(&instance_id)
            .map(|state| state.owner.clone())
    }

    fn current_extension_block_owner(&self) -> UiSnapshotOwner {
        self.displayed_agent_id
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
        self.action_invocation_owners.remove(invocation_id)
    }

    fn handle_recorded_at_for_snapshot_owner(
        &mut self,
        event: &Event,
        recorded_at: UnixMicros,
        owner: UiSnapshotOwner,
    ) {
        let is_global_message_fact = crate::message_fact_render::target_agent_id(event).is_some();
        match owner {
            UiSnapshotOwner::Agent(agent_id)
                if self.displayed_agent_id.as_deref() == Some(agent_id.as_str()) =>
            {
                self.handle_recorded_at_for_visible_agent(event, recorded_at);
            }
            UiSnapshotOwner::NoAgent if self.displayed_agent_id.is_none() => {
                self.contains_global_message_fact |= is_global_message_fact;
                self.handle_recorded_at_for_visible_agent(event, recorded_at);
            }
            UiSnapshotOwner::Agent(agent_id) => {
                self.handle_recorded_at_for_hidden_agent(event, recorded_at, agent_id);
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
        event: &Event,
        recorded_at: UnixMicros,
        target_agent_id: String,
    ) {
        let handle = self.handle.clone();
        let visible_agent_id = self.displayed_agent_id.clone();
        handle.with_output_transaction(|| {
            let visible_state = self.take_visible_agent_state();
            if let Some(visible_agent_id) = visible_agent_id.as_ref() {
                self.agents_ui_state
                    .insert(visible_agent_id.clone(), visible_state);
            } else {
                self.no_agent_ui_state = visible_state;
            }
            let target_state = self
                .agents_ui_state
                .remove(&target_agent_id)
                .unwrap_or_default();
            handle.with_redraw_suppressed(|| {
                self.with_editor_context_publish_suppressed(|this| {
                    this.restore_hidden_agent_state(target_state);
                    this.displayed_agent_id = Some(target_agent_id.clone());
                    this.handle_recorded_at_for_visible_agent(event, recorded_at);
                    let target_state = this.take_visible_agent_state();
                    this.agents_ui_state
                        .insert(target_agent_id.clone(), target_state);
                    let visible_state = visible_agent_id
                        .as_ref()
                        .and_then(|id| this.agents_ui_state.remove(id))
                        .unwrap_or_else(|| std::mem::take(&mut this.no_agent_ui_state));
                    this.restore_hidden_agent_state(visible_state);
                });
            });
        });
        self.displayed_agent_id = visible_agent_id;
        self.publish_editor_conversation_context();
    }

    fn handle_recorded_at_for_hidden_no_agent(
        &mut self,
        event: &Event,
        recorded_at: UnixMicros,
        is_global_message_fact: bool,
    ) {
        self.update_hidden_no_agent_state(|this| {
            this.contains_global_message_fact |= is_global_message_fact;
            this.handle_recorded_at_for_visible_agent(event, recorded_at);
        });
    }

    /// Temporarily restore and update the hidden no-agent snapshot without
    /// publishing its editor context or disturbing the visible agent
    /// transcript.
    fn update_hidden_no_agent_state(&mut self, update: impl FnOnce(&mut Self)) {
        let handle = self.handle.clone();
        let visible_agent_id = self.displayed_agent_id.clone();
        handle.with_output_transaction(|| {
            let visible_state = self.take_visible_agent_state();
            if let Some(visible_agent_id) = visible_agent_id.as_ref() {
                self.agents_ui_state
                    .insert(visible_agent_id.clone(), visible_state);
            } else {
                self.no_agent_ui_state = visible_state;
            }
            let no_agent_state = std::mem::take(&mut self.no_agent_ui_state);
            handle.with_redraw_suppressed(|| {
                self.with_editor_context_publish_suppressed(|this| {
                    this.restore_hidden_agent_state(no_agent_state);
                    this.displayed_agent_id = None;
                    update(this);
                    this.no_agent_ui_state = this.take_visible_agent_state();
                    let visible_state = visible_agent_id
                        .as_ref()
                        .and_then(|id| this.agents_ui_state.remove(id))
                        .unwrap_or_default();
                    this.restore_hidden_agent_state(visible_state);
                });
            });
        });
        self.displayed_agent_id = visible_agent_id;
        self.publish_editor_conversation_context();
    }

    fn agent_message_visible_on_empty_screen(&self, event: &Event, target_agent_id: &str) -> bool {
        Self::is_user_broadcast_agent_message(event)
            || !self.awaiting_new_agent_selection
            || !self.agents_ui_state.contains_key(target_agent_id)
    }

    fn event_selects_agent_from_empty(&self, event: &Event, target_agent_id: &str) -> bool {
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

    fn can_select_target_from_empty(&self, target_agent_id: &str) -> bool {
        // When the UI is in the explicit start-new-agent state (`/agent new` or
        // `/agent switch none`), background activity from the previously visible
        // agent must not steal selection while the user is typing the prompt
        // meant to create a fresh agent. An event for an agent whose transcript
        // is already hidden here is therefore treated as background work, not as
        // the new agent.
        !self.awaiting_new_agent_selection || !self.agents_ui_state.contains_key(target_agent_id)
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
            _ => false,
        }
    }

    fn update_agent_in_progress(&self) {
        let hidden_in_progress = self
            .agents_ui_state
            .values()
            .any(|state| state.agent_activity.is_in_progress());
        self.agent_in_progress.store(
            self.agent_activity.is_in_progress() || hidden_in_progress,
            Ordering::Relaxed,
        );
    }

    fn learn_agent_metadata(&mut self, event: &Event) {
        if self.learn_agent_lifecycle_metadata(event) {
            return;
        }
        if self.learn_agent_prompt_metadata(event) {
            return;
        }
        if self.learn_provider_tool_metadata(event) {
            return;
        }
        if self.learn_shell_metadata(event) {
            return;
        }
        self.learn_agent_message_metadata(event);
    }

    fn learn_agent_lifecycle_metadata(&mut self, event: &Event) -> bool {
        match event {
            Event::StartAgentRequest(_) => true,
            Event::StartAgentAccepted(accepted) => {
                let agent_id = accepted.agent_id.to_string();
                self.query_agents
                    .insert(accepted.query_id.clone(), agent_id.clone());
                self.remember_agent(agent_id);
                true
            }
            Event::AgentStarted(started) => {
                let agent_id = started.agent_id.to_string();
                self.mark_agent_live(agent_id.clone());
                if started.ephemeral {
                    self.remember_agent_ephemeral(&agent_id);
                }
                if let Some(display_name) = started.display_name.as_ref() {
                    self.remember_agent_display_name(&agent_id, display_name);
                }
                true
            }
            Event::AgentDisplayNameSet(name) => {
                let agent_id = name.agent_id.to_string();
                self.remember_agent(agent_id.clone());
                self.remember_agent_display_name(&agent_id, &name.display_name);
                if self.current_agent_id.as_deref() == Some(agent_id.as_str()) {
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
                self.remember_agent(updated.agent_id.to_string());
                self.handle_agent_stats_updated(updated);
                true
            }
            Event::SessionAgentUnloaded(unloaded) => {
                if let Ok(mut navigation) = self.agent_navigation.lock() {
                    navigation.unload(unloaded.agent_id.as_str());
                }
                self.remove_agent_watch_endpoint(unloaded.agent_id.as_str());
                self.agent_models.remove(unloaded.agent_id.as_str());
                true
            }
            Event::HarnessAgentContextUsageChanged(changed) => {
                self.remember_agent(changed.agent_id.to_string());
                true
            }
            _ => false,
        }
    }

    fn learn_agent_prompt_metadata(&mut self, event: &Event) -> bool {
        match event {
            Event::UiPromptSubmitted(prompt) => {
                let agent_id = prompt.agent_id.to_string();
                // This is only a transient UI request. Activation waits for an
                // accepted queue or committed submission event from the harness.
                self.remember_agent(agent_id.clone());
                if let tau_proto::PromptOriginator::Extension { query_id, .. } = &prompt.originator
                {
                    self.query_agents.insert(query_id.clone(), agent_id);
                }
                true
            }
            Event::AgentPromptQueued(queued) => {
                self.mark_agent_live(queued.agent_id.to_string());
                true
            }
            Event::AgentPromptSubmitted(prompt) => {
                let agent_id = prompt.agent_id.to_string();
                if let tau_proto::PromptOriginator::Extension { query_id, .. } = &prompt.originator
                {
                    self.query_agents.insert(query_id.clone(), agent_id.clone());
                    self.mark_agent_live(agent_id);
                } else {
                    self.mark_agent_live(agent_id);
                }
                true
            }
            Event::AgentCompactionTriggered(triggered) => {
                let agent_id = triggered.agent_id.to_string();
                if triggered.originator.is_user() {
                    self.mark_agent_live(agent_id);
                } else {
                    self.remember_agent(agent_id);
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
                let agent_id = terminated.agent_id.to_string();
                self.mark_agent_live(agent_id);
                self.mark_agent_prompt_inactive(
                    terminated.agent_id.as_str(),
                    terminated.agent_prompt_id.as_str(),
                );
                true
            }
            Event::ProviderPromptSubmitted(submitted) => {
                self.mark_known_agent_prompt_active(
                    submitted.agent_prompt_id.as_str(),
                    &submitted.originator,
                );
                true
            }
            Event::ProviderResponseUpdated(update) => {
                let agent_id = update.agent_id.to_string();
                let agent_prompt_id = update.agent_prompt_id.as_str();
                if self.is_stale_terminal_stats_only_update(update) {
                    self.clear_main_agent_turn_active_everywhere();
                    return true;
                }
                if provider_response_update_has_visible_content(update)
                    || !self.prompt_agents.contains_key(agent_prompt_id)
                {
                    self.prompt_agents
                        .insert(agent_prompt_id.to_owned(), agent_id.clone());
                    self.mark_agent_prompt_active(&agent_id, agent_prompt_id);
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
        let agent_id_string = agent_id.to_string();
        self.agent_models
            .insert(agent_id_string.clone(), model.clone());
        self.mark_agent_live(agent_id_string.clone());
        self.prompt_agents
            .insert(agent_prompt_id.to_string(), agent_id_string);
        self.mark_agent_prompt_active(agent_id.as_str(), agent_prompt_id.as_str());
    }

    fn learn_provider_tool_metadata(&mut self, event: &Event) -> bool {
        match event {
            Event::ProviderResponseFinished(finished) => {
                let agent_id = finished.agent_id.to_string();
                self.agent_activity
                    .finish_prompt(&finished.agent_prompt_id, &finished.output_items);
                if finished.originator.is_user()
                    && tool_calls_from_output_items(&finished.output_items).is_empty()
                {
                    self.clear_main_agent_turn_active_everywhere();
                }
                self.mark_agent_prompt_inactive(
                    finished.agent_id.as_str(),
                    finished.agent_prompt_id.as_str(),
                );
                let requested_tools = tool_calls_from_output_items(&finished.output_items);
                self.mark_agent_live(agent_id.clone());
                self.prompt_agents
                    .insert(finished.agent_prompt_id.to_string(), agent_id.clone());
                for call in requested_tools {
                    self.tool_agents
                        .insert(call.call_id.to_string(), agent_id.clone());
                }
                true
            }
            Event::ToolStarted(started) => {
                let agent_id = started.agent_id.to_string();
                self.remember_agent(agent_id.clone());
                self.tool_agents
                    .entry(started.call_id.to_string())
                    .or_insert(agent_id);
                true
            }
            _ => false,
        }
    }

    fn learn_shell_metadata(&mut self, event: &Event) -> bool {
        match event {
            Event::UiShellCommand(command) => {
                if let Some(agent_id) = command.target_agent_id.as_deref() {
                    self.remember_agent(agent_id.to_owned());
                    self.shell_agents
                        .insert(command.command_id.to_string(), agent_id.to_owned());
                }
                true
            }
            Event::ShellCommandProgress(progress) => {
                if let Some(agent_id) = progress.target_agent_id.as_deref() {
                    self.remember_agent(agent_id.to_owned());
                    self.shell_agents
                        .insert(progress.command_id.to_string(), agent_id.to_owned());
                }
                true
            }
            Event::ShellCommandFinished(finished) => {
                if let Some(agent_id) = finished.target_agent_id.as_deref() {
                    self.remember_agent(agent_id.to_owned());
                    self.shell_agents
                        .insert(finished.command_id.to_string(), agent_id.to_owned());
                }
                true
            }
            _ => false,
        }
    }

    fn learn_agent_message_metadata(&mut self, event: &Event) {
        match event {
            Event::AgentMessageSent(message) => {
                self.remember_agent(message.sender_id.to_string());
                if let Some(agent_id) = Self::agent_message_sent_recipient_agent_id(message) {
                    self.remember_agent(agent_id.to_owned());
                }
            }
            Event::AgentMessageReceived(message) => {
                self.remember_agent(message.sender_id.to_string());
                self.mark_agent_live(message.recipient_id.to_string());
                if let Some(state) = &message.watch_turn_state {
                    self.handle_watched_agent_turn_state(message, state);
                }
            }
            _ => {}
        }
    }

    fn agent_id_for_event(&self, event: &Event) -> Option<String> {
        self.tool_event_agent_id(event)
            .or_else(|| Self::agent_message_event_agent_id(event))
            .or_else(|| Self::direct_agent_event_agent_id(event))
            .or_else(|| self.shell_event_agent_id(event))
            .or_else(|| self.prompt_event_agent_id(event))
            .into_agent_id(self.current_agent_id.as_deref())
    }

    /// Resolve every message fact to its loaded transcript or the no-agent
    /// snapshot, preserving unavailable and invalid facts as global output.
    fn message_fact_snapshot_owner(&self, event: &Event) -> Option<UiSnapshotOwner> {
        match crate::message_fact_render::target_agent_id(event)? {
            crate::message_fact_render::MessageFactTarget::Valid(agent_id)
                if self
                    .agent_navigation
                    .lock()
                    .expect("agent navigation lock")
                    .is_live(agent_id.as_str()) =>
            {
                Some(UiSnapshotOwner::Agent(agent_id.to_string()))
            }
            crate::message_fact_render::MessageFactTarget::Valid(_)
            | crate::message_fact_render::MessageFactTarget::Invalid => {
                Some(UiSnapshotOwner::NoAgent)
            }
        }
    }

    fn tool_event_agent_id(&self, event: &Event) -> EventAgentIdResolution {
        match event {
            Event::ToolRequest(request) => EventAgentIdResolution::from_agent_id(
                self.tool_agents.get(request.call_id.as_str()).cloned(),
            ),
            Event::ToolStarted(started) => EventAgentIdResolution::from_agent_id(
                self.tool_agents
                    .get(started.call_id.as_str())
                    .cloned()
                    .or_else(|| Some(started.agent_id.to_string())),
            ),
            Event::ToolProgress(progress) => EventAgentIdResolution::from_agent_id(
                self.tool_agents.get(progress.call_id.as_str()).cloned(),
            ),
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                EventAgentIdResolution::from_agent_id(
                    self.tool_agents
                        .get(result.call_id.as_str())
                        .cloned()
                        .or_else(|| self.agent_id_for_originator(&result.originator)),
                )
            }
            Event::ToolError(error) => EventAgentIdResolution::from_agent_id(
                self.tool_agents
                    .get(error.call_id.as_str())
                    .cloned()
                    .or_else(|| self.agent_id_for_originator(&error.originator)),
            ),
            Event::ToolBackgroundResult(result) => EventAgentIdResolution::from_agent_id(
                self.tool_agents.get(result.call_id.as_str()).cloned(),
            ),
            Event::ToolBackgroundError(error) => EventAgentIdResolution::from_agent_id(
                self.tool_agents.get(error.call_id.as_str()).cloned(),
            ),
            Event::ToolCancelled(cancelled) => EventAgentIdResolution::from_agent_id(
                self.tool_agents.get(cancelled.call_id.as_str()).cloned(),
            ),
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn agent_message_event_agent_id(event: &Event) -> EventAgentIdResolution {
        match event {
            Event::AgentMessageSent(message)
                if Self::is_user_broadcast_agent_message_sent(message) =>
            {
                EventAgentIdResolution::NoAgent
            }
            Event::AgentMessageSent(message) => {
                EventAgentIdResolution::Agent(message.sender_id.to_string())
            }
            Event::AgentMessageReceived(message) => {
                EventAgentIdResolution::Agent(message.recipient_id.to_string())
            }
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn direct_agent_event_agent_id(event: &Event) -> EventAgentIdResolution {
        match event {
            Event::AgentStarted(started) => {
                EventAgentIdResolution::Agent(started.agent_id.to_string())
            }
            Event::AgentDisplayNameSet(name) => {
                EventAgentIdResolution::Agent(name.agent_id.to_string())
            }
            Event::UiPromptSubmitted(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.to_string())
            }
            Event::AgentPromptSubmitted(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.to_string())
            }
            Event::AgentPromptQueued(queued) => {
                EventAgentIdResolution::Agent(queued.agent_id.to_string())
            }
            Event::AgentPromptRecalled(recalled) => {
                EventAgentIdResolution::Agent(recalled.agent_id.to_string())
            }
            Event::AgentPromptSteered(steered) => {
                EventAgentIdResolution::Agent(steered.agent_id.to_string())
            }
            Event::AgentCompactionTriggered(triggered) => {
                EventAgentIdResolution::Agent(triggered.agent_id.to_string())
            }
            Event::HarnessAgentContextUsageChanged(changed) => {
                EventAgentIdResolution::Agent(changed.agent_id.to_string())
            }
            Event::ExtensionContextReady(ready) => {
                EventAgentIdResolution::Agent(ready.agent_id.to_string())
            }
            Event::UiCancelPrompt(cancel) => EventAgentIdResolution::from_agent_id(
                cancel.target_agent_id.as_ref().map(ToString::to_string),
            ),
            Event::UiRecallQueuedPrompt(recall) => EventAgentIdResolution::from_agent_id(
                recall.target_agent_id.as_ref().map(ToString::to_string),
            ),
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn shell_event_agent_id(&self, event: &Event) -> EventAgentIdResolution {
        match event {
            Event::UiShellCommand(command) => EventAgentIdResolution::from_agent_id(
                command.target_agent_id.as_ref().map(ToString::to_string),
            ),
            Event::ShellCommandProgress(progress) => EventAgentIdResolution::from_agent_id(
                progress
                    .target_agent_id
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| self.shell_agents.get(progress.command_id.as_str()).cloned()),
            ),
            Event::ShellCommandFinished(finished) => EventAgentIdResolution::from_agent_id(
                finished
                    .target_agent_id
                    .as_ref()
                    .map(ToString::to_string)
                    .or_else(|| self.shell_agents.get(finished.command_id.as_str()).cloned()),
            ),
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn prompt_event_agent_id(&self, event: &Event) -> EventAgentIdResolution {
        match event {
            Event::AgentPromptCreated(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.to_string())
            }
            Event::AgentPromptStarted(prompt) => {
                EventAgentIdResolution::Agent(prompt.agent_id.to_string())
            }
            Event::AgentPromptTerminated(terminated) => {
                EventAgentIdResolution::from_agent_id(self.agent_id_for_prompt(
                    terminated.agent_prompt_id.as_str(),
                    &terminated.originator,
                ))
            }
            Event::ProviderPromptSubmitted(submitted) => EventAgentIdResolution::from_agent_id(
                self.prompt_agents
                    .get(submitted.agent_prompt_id.as_str())
                    .cloned()
                    .or_else(|| self.agent_id_for_originator(&submitted.originator)),
            ),
            Event::ProviderResponseUpdated(update) => EventAgentIdResolution::from_agent_id(
                self.prompt_agents
                    .get(update.agent_prompt_id.as_str())
                    .cloned()
                    .or_else(|| Some(update.agent_id.to_string())),
            ),
            Event::ProviderResponseFinished(finished) => {
                EventAgentIdResolution::Agent(finished.agent_id.to_string())
            }
            _ => EventAgentIdResolution::Unhandled,
        }
    }

    fn agent_id_for_prompt(
        &self,
        agent_prompt_id: &str,
        originator: &tau_proto::PromptOriginator,
    ) -> Option<String> {
        self.prompt_agents
            .get(agent_prompt_id)
            .cloned()
            .or_else(|| self.agent_id_for_originator(originator))
    }

    fn agent_id_for_originator(&self, originator: &tau_proto::PromptOriginator) -> Option<String> {
        match originator {
            tau_proto::PromptOriginator::User => self.current_agent_id.clone(),
            tau_proto::PromptOriginator::Extension { query_id, .. } => {
                self.query_agents.get(query_id).cloned()
            }
        }
    }

    fn handle_recorded_at_for_visible_agent(&mut self, event: &Event, recorded_at: UnixMicros) {
        self.sync_agent_activity_for_lifecycle(event);

        self.sync_main_tools_visibility_for_prompt_lifecycle(event);

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
            || self.handle_provider_response_events(event)
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
                let crate::message_fact_render::MessageFactTarget::Valid(agent_id) = target else {
                    return false;
                };
                self.displayed_agent_id.as_deref() == Some(agent_id.as_str())
            });
        let target_context = if target_context {
            crate::message_fact_render::MessageFactTargetContext::Implied
        } else {
            crate::message_fact_render::MessageFactTargetContext::Explicit
        };
        let Some(rendered) = crate::message_fact_render::render(event, target_context) else {
            return false;
        };
        self.handle
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
        let block = self.render_agent_message_block(event);
        let block_id = self.handle.print_output("agent-message", block);
        self.message_history.push(MessageBlockEntry {
            block_id,
            event: event.clone(),
            session_id: self.current_session_id.clone(),
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
        if let Some(summary) =
            self.watch_turn_state_summary_with_local_names(event, use_local_names)
        {
            return self.submitted_plain_block(tau_themes::names::SYSTEM_INFO, summary);
        }
        match Self::message_render_mode(self.show_messages, event) {
            MessageRenderMode::Hidden => Self::empty_block(),
            MessageRenderMode::Summary => {
                self.submitted_agent_message_block(event, use_local_names, false)
            }
            MessageRenderMode::Full => {
                self.submitted_agent_message_block(event, use_local_names, true)
            }
        }
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
                    vec![SpanTree::text(format!("{} ", self.submitted_prompt_symbol))],
                ),
                SpanTree::span(body_style, content),
            ],
        ));

        let body_ts = self
            .theme
            .resolve_style(&StyleName::new(names::SYSTEM_INFO));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.theme, &themed));
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
        let (prefix, first, separator, second) = match event {
            Event::AgentMessageSent(message) => {
                let sender =
                    self.message_agent_display_label(message.sender_id.as_str(), use_local_names);
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
                    tau_proto::AgentMessageRecipient::User => None,
                };
                let recipient = (recipient, recipient_identity);
                match message.kind {
                    tau_proto::AgentMessageKind::WatchResponse => {
                        ("Response from ", sender, " to ", recipient)
                    }
                    tau_proto::AgentMessageKind::WatchPrompt => {
                        ("Prompt to ", sender, " observed by ", recipient)
                    }
                    _ => ("Message from ", sender, " to ", recipient),
                }
            }
            Event::AgentMessageReceived(message) => {
                let sender = self.agent_message_received_sender_label(message, use_local_names);
                let sender_identity = Some(message.sender_session_id.as_ref().map_or_else(
                    || format!("@{}", message.sender_id),
                    |session_id| format!("{session_id}/@{}", message.sender_id),
                ));
                let sender = (sender, sender_identity);
                let recipient = self
                    .message_agent_display_label(message.recipient_id.as_str(), use_local_names);
                let recipient = (recipient, Some(format!("@{}", message.recipient_id)));
                match message.kind {
                    tau_proto::AgentMessageKind::WatchResponse => {
                        ("Response from ", sender, " to ", recipient)
                    }
                    tau_proto::AgentMessageKind::WatchPrompt => {
                        ("Prompt to ", sender, " observed by ", recipient)
                    }
                    _ => ("Message from ", sender, " to ", recipient),
                }
            }
            _ => unreachable!("only agent message events are rendered here"),
        };
        let mut parts = vec![(prefix.to_owned(), false)];
        Self::push_agent_message_endpoint(&mut parts, first);
        parts.push((separator.to_owned(), false));
        Self::push_agent_message_endpoint(&mut parts, second);
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

    /// Render structured watch lifecycle state as a compact status rather than
    /// attributing the harness-authored event as a message from the watched
    /// agent.
    #[cfg(test)]
    fn watch_turn_state_summary(&self, event: &Event) -> Option<String> {
        self.watch_turn_state_summary_with_local_names(event, true)
    }

    fn watch_turn_state_summary_with_local_names(
        &self,
        event: &Event,
        use_local_names: bool,
    ) -> Option<String> {
        let Event::AgentMessageReceived(message) = event else {
            return None;
        };
        let state = message.watch_turn_state.as_ref()?;
        let watched = self.agent_message_received_sender_label(message, use_local_names);
        Some(if state.initial {
            let state_label = match state.state {
                tau_proto::AgentRuntimeState::Running => "running",
                tau_proto::AgentRuntimeState::Idle => "idle",
            };
            format!("Watching {watched} · {state_label}")
        } else {
            let transition = match state.state {
                tau_proto::AgentRuntimeState::Running => "turn started",
                tau_proto::AgentRuntimeState::Idle => "turn stopped",
            };
            format!("{watched} · {transition}")
        })
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
            if next_columns > max_columns || output.len().saturating_add(escaped.len()) > max_bytes
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
                self.message_agent_display_label(agent_id.as_str(), use_local_names)
            }
            tau_proto::AgentMessageRecipient::ExternalAgent {
                session_id,
                agent_id,
            } => {
                format!("{session_id}/@{agent_id}")
            }
            tau_proto::AgentMessageRecipient::User => "user".to_owned(),
        }
    }

    fn agent_message_received_sender_label(
        &self,
        message: &tau_proto::AgentMessageReceived,
        use_local_names: bool,
    ) -> String {
        message.sender_session_id.as_ref().map_or_else(
            || self.message_agent_display_label(message.sender_id.as_str(), use_local_names),
            |session_id| format!("{session_id}/@{}", message.sender_id),
        )
    }

    fn is_user_broadcast_agent_message(event: &Event) -> bool {
        matches!(
            event,
            Event::AgentMessageSent(message)
                if Self::is_user_broadcast_agent_message_sent(message)
        )
    }

    fn is_user_broadcast_agent_message_sent(message: &tau_proto::AgentMessageSent) -> bool {
        matches!(message.recipient, tau_proto::AgentMessageRecipient::User)
    }

    fn agent_message_sent_recipient_agent_id(
        message: &tau_proto::AgentMessageSent,
    ) -> Option<&str> {
        match &message.recipient {
            tau_proto::AgentMessageRecipient::Agent { agent_id } => Some(agent_id.as_str()),
            tau_proto::AgentMessageRecipient::ExternalAgent { .. } => None,
            tau_proto::AgentMessageRecipient::User => None,
        }
    }

    fn message_render_mode(
        show_messages: tau_config::settings::ShowMessages,
        event: &Event,
    ) -> MessageRenderMode {
        if Self::is_user_broadcast_agent_message(event) {
            return MessageRenderMode::Full;
        }

        let self_msg = matches!(
            event,
            Event::AgentMessageSent(tau_proto::AgentMessageSent {
                recipient: tau_proto::AgentMessageRecipient::User,
                ..
            })
        );
        match (show_messages, self_msg) {
            (tau_config::settings::ShowMessages::None, _) => MessageRenderMode::Hidden,
            (tau_config::settings::ShowMessages::SelfSummary, true) => MessageRenderMode::Summary,
            (tau_config::settings::ShowMessages::SelfSummary, false) => MessageRenderMode::Hidden,
            (tau_config::settings::ShowMessages::SelfFull, true) => MessageRenderMode::Full,
            (tau_config::settings::ShowMessages::SelfFull, false) => MessageRenderMode::Hidden,
            (tau_config::settings::ShowMessages::AllSummary, true) => MessageRenderMode::Full,
            (tau_config::settings::ShowMessages::AllSummary, false) => MessageRenderMode::Summary,
            (tau_config::settings::ShowMessages::AllFull, _) => MessageRenderMode::Full,
        }
    }

    fn handle_session_events(&mut self, event: &Event) -> bool {
        match event {
            Event::SessionStarted(started)
                if matches!(started.reason, tau_proto::SessionStartReason::New) =>
            {
                self.handle_new_session_started(started);
                true
            }
            Event::SessionStarted(started) => {
                self.handle_existing_session_started(started);
                true
            }
            _ => false,
        }
    }

    fn handle_new_session_started(&mut self, started: &tau_proto::SessionStarted) {
        self.current_session_id = Some(started.session_id.clone());
        self.reconcile_session_context(&started.session_id);
        self.clear_for_new_session();
    }

    fn handle_existing_session_started(&mut self, started: &tau_proto::SessionStarted) {
        if self.current_session_id.as_ref() != Some(&started.session_id) {
            self.clear_agent_display_names();
            self.rerender_message_history();
        }
        self.current_session_id = Some(started.session_id.clone());
        self.reconcile_session_context(&started.session_id);
        self.render_model_status();
    }

    fn reconcile_session_context(&self, session_id: &tau_proto::SessionId) {
        if let Some(retargeter) = &self.draft_retargeter
            && let Ok(mut active_session) = retargeter.session_id.lock()
        {
            *active_session = session_id.to_string();
            invalidate_pending_draft(retargeter.handle.as_ref());
        }
        self.render_right_prompt_context(session_id);
    }

    fn render_right_prompt_context(&self, session_id: &tau_proto::SessionId) {
        let Some((cwd, home)) = &self.right_prompt_paths else {
            return;
        };
        self.handle
            .set_right_prompt(crate::theme::right_prompt_context(
                &self.theme,
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
        if self.handle_visible_internal_prompt(
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
        if !prompt.message_class.is_internal()
            && matches!(
                prompt.submission_source,
                tau_proto::PromptSubmissionSource::Extension { .. }
                    | tau_proto::PromptSubmissionSource::HarnessInternal
            )
        {
            let block =
                self.submitted_plain_block(tau_themes::names::SYSTEM_INFO, prompt.text.clone());
            self.handle.print_output("extension-prompt", block);
        } else {
            // Legacy records intentionally retain their historical rendering:
            // there is no safe prefix-based way to reclassify them.
            self.handle_submitted_user_prompt(&prompt.text, prompt.message_class);
        }
    }

    fn handle_visible_internal_prompt(
        &mut self,
        message_class: tau_proto::PromptMessageClass,
        internal_kind: Option<tau_proto::InternalPromptKind>,
        text: &str,
    ) -> bool {
        if !message_class.is_internal()
            || internal_kind != Some(tau_proto::InternalPromptKind::ContextSizeAlert)
        {
            return false;
        }
        let block = self.submitted_plain_block(
            tau_themes::names::SYSTEM_INFO,
            format!("[tau-internal]: {text}"),
        );
        self.handle.print_output("context-size-alert", block);
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
        let summary = timer_wakeup_summary(timer_id, text);
        let block = self.submitted_plain_block(tau_themes::names::SYSTEM_INFO, summary);
        self.handle.print_output("timer-wakeup", block);
        true
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

        if self
            .queued_user_blocks
            .front()
            .is_some_and(|(_, queued_text)| queued_text == text)
        {
            let Some((queued_id, queued_text)) = self.queued_user_blocks.pop_front() else {
                return;
            };
            self.handle.remove_block(queued_id);
            self.reset_main_tool_usage();
            let id = self.handle.print_output(
                "user-prompt",
                self.submitted_prompt_block(names::USER_PROMPT, queued_text.clone()),
            );
            self.last_user_block = Some((id, queued_text));
            self.handle.redraw();
            return;
        }
        self.reset_main_tool_usage();
        let block = self.submitted_prompt_block(names::USER_PROMPT, text.to_owned());
        let id = self.handle.print_output("user-prompt", block);
        self.last_user_block = Some((id, text.to_owned()));
    }

    fn handle_agent_prompt_queued(&mut self, queued: &tau_proto::AgentPromptQueued) {
        if queued.message_class.is_internal() {
            return;
        }

        use tau_themes::names;

        self.reset_main_tool_usage();
        if let Some((id, text)) = self.last_user_block.take() {
            if text == queued.text {
                self.handle.remove_block(id);
            } else {
                self.last_user_block = Some((id, text));
            }
        }
        let block = markdown_prompt_block(
            &self.theme,
            names::USER_PROMPT_QUEUED,
            format!("{} ", self.prompt_symbol),
            &format!("{} (queued)", queued.text),
        );
        let queued_id = self.handle.new_block("user-prompt-queued", block);
        self.handle.push_above_sticky(queued_id);
        self.handle.redraw();
        self.queued_user_blocks
            .push_back((queued_id, queued.text.clone()));
    }

    fn handle_agent_prompt_recalled(&mut self, recalled: &tau_proto::AgentPromptRecalled) {
        if let Some((queued_id, _text)) = self.queued_user_blocks.pop_back() {
            self.handle.remove_above_sticky(queued_id);
            self.handle.remove_block(queued_id);
        }
        self.handle
            .recall_prompt_before_current(recalled.text.clone());
        self.handle.redraw();
    }

    fn handle_agent_prompt_steered(&mut self, steered: &tau_proto::AgentPromptSteered) {
        if self.handle_visible_internal_prompt(
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
        if steered.message_class.is_internal() {
            return;
        }

        use tau_themes::names;

        // The harness folded a queued prompt into the current turn's next
        // round (alongside tool results) instead of waiting for `Idle`.
        // Promote the "(queued)" rendering to a regular user prompt so the
        // transcript reads naturally above the agent's continuing response.
        if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
            self.handle.remove_block(queued_id);
            self.handle.print_output(
                "user-prompt-steered",
                self.submitted_prompt_block(names::USER_PROMPT, text),
            );
            self.handle.redraw();
        } else {
            // No matching "(queued)" block — fall back to rendering the
            // steered text directly so the user still sees their message land.
            self.handle.print_output(
                "user-prompt-steered",
                self.submitted_prompt_block(names::USER_PROMPT, steered.text.clone()),
            );
            self.handle.redraw();
        }
    }

    fn handle_agent_prompt_created(&mut self, prompt: &tau_proto::AgentPromptCreated) {
        self.handle_agent_prompt_started(&prompt.into());
    }

    fn handle_agent_prompt_started(&mut self, prompt: &tau_proto::AgentPromptStarted) {
        self.finished_provider_prompts
            .remove(prompt.agent_prompt_id.as_str());
        let state = self
            .prompts
            .entry(prompt.agent_prompt_id.to_string())
            .or_default();
        if state.started_at.is_some() {
            return;
        }
        state.started_at = Some(Instant::now());
        self.clear_editor_current_response_for_user_prompt(prompt.originator.is_user());
        self.last_user_block = None;
        self.promote_next_queued_prompt("user-prompt-created");
    }

    fn clear_editor_current_response_for_user_prompt(&mut self, is_user_prompt: bool) {
        if is_user_prompt {
            self.set_editor_current_response(None);
        }
    }

    fn handle_agent_prompt_terminated(&mut self, terminated: &tau_proto::AgentPromptTerminated) {
        self.clear_editor_current_response_for_user_prompt(terminated.originator.is_user());
        self.finished_provider_prompts
            .insert(terminated.agent_prompt_id.to_string());
        let Some(prompt_state) = self.prompts.remove(terminated.agent_prompt_id.as_str()) else {
            return;
        };
        if let Some(block_id) = prompt_state.thinking_block_id {
            self.handle.remove_block(block_id);
        }
        if let Some(block_id) = prompt_state.compaction_block_id {
            self.handle.remove_block(block_id);
        }
        if let Some(block_id) = prompt_state.response_block_id {
            self.handle.remove_block(block_id);
        }
        self.handle.redraw();
    }

    fn promote_next_queued_prompt(&mut self, label: &'static str) {
        use tau_themes::names;

        if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
            self.handle.remove_block(queued_id);
            self.handle
                .print_output(label, self.submitted_prompt_block(names::USER_PROMPT, text));
        }
    }

    fn handle_provider_response_events(&mut self, event: &Event) -> bool {
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
                self.handle_provider_response_finished(finished);
                true
            }
            _ => false,
        }
    }

    fn handle_provider_prompt_submitted(&mut self, submitted: &tau_proto::ProviderPromptSubmitted) {
        self.finished_provider_prompts
            .remove(submitted.agent_prompt_id.as_str());
        self.prompts
            .entry(submitted.agent_prompt_id.to_string())
            .or_default()
            .started_at = Some(Instant::now());
    }

    fn handle_provider_response_updated(&mut self, update: &tau_proto::ProviderResponseUpdated) {
        let spid = update.agent_prompt_id.as_str();
        self.prompt_agents
            .entry(spid.to_owned())
            .or_insert_with(|| update.agent_id.to_string());
        if self.is_stale_terminal_stats_only_update(update) {
            return;
        }
        if !provider_response_update_has_visible_content(update)
            && self
                .prompt_agents
                .get(spid)
                .is_some_and(|agent_id| agent_id != update.agent_id.as_str())
        {
            return;
        }
        self.ensure_live_response_block_for_update(update);
        if let Some(stats) = update.response_stats {
            self.prompts
                .entry(spid.to_owned())
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
                self.update_live_response_block(spid, &status.text);
                return;
            }
        }
        let (text, thinking) = self.accumulate_response_update(update);
        self.update_editor_current_response(update, &text);
        self.update_live_thinking_block(spid, thinking.as_deref());
        self.update_live_compaction_block(spid, update_compaction_status(update));
        self.update_live_response_block(spid, &text);
    }

    fn is_stale_terminal_stats_only_update(
        &self,
        update: &tau_proto::ProviderResponseUpdated,
    ) -> bool {
        !provider_response_update_has_visible_content(update)
            && self
                .finished_provider_prompts
                .contains(update.agent_prompt_id.as_str())
    }

    fn clear_live_response_accumulators(&mut self, spid: &str) {
        if let Some(state) = self.prompts.get_mut(spid) {
            state.response_text_by_index.clear();
            state.thinking_text_by_index.clear();
            state.thinking_text = None;
            state.missing_response_prefix = false;
            state.missing_thinking_prefix = false;
            state.response_markdown_cache = MarkdownStreamCache::default();
            state.thinking_markdown_cache = MarkdownStreamCache::default();
            state.provider_response_stats = None;
            if let Some(block_id) = state.thinking_block_id.take() {
                self.handle.remove_block(block_id);
            }
        }
    }

    fn ensure_live_response_block_for_update(
        &mut self,
        update: &tau_proto::ProviderResponseUpdated,
    ) {
        self.ensure_live_response_block_for_prompt(update.agent_prompt_id.as_str());
    }

    fn ensure_live_response_block_for_prompt(&mut self, spid: &str) {
        use std::collections::hash_map::Entry;

        use tau_themes::names;

        let (state, prompt_was_unknown) = match self.prompts.entry(spid.to_owned()) {
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
            &self.theme,
            names::AGENT_PENDING,
            STREAMING_AGENT_RESPONSE_PREFIX.trim_end(),
        );
        let id = self
            .handle
            .new_block(format!("agent-response-live:{spid}"), block);
        self.push_live_response_block(id);
        self.handle.redraw();
        self.prompts
            .entry(spid.to_owned())
            .or_default()
            .response_block_id = Some(id);
    }

    fn accumulate_response_update(
        &mut self,
        update: &tau_proto::ProviderResponseUpdated,
    ) -> (String, Option<String>) {
        let state = self
            .prompts
            .entry(update.agent_prompt_id.to_string())
            .or_default();
        for delta in &update.deltas {
            match delta {
                ProviderResponseTextDelta::Message {
                    output_index, text, ..
                } => {
                    state
                        .response_text_by_index
                        .entry(*output_index)
                        .or_default()
                        .push_str(text);
                }
                ProviderResponseTextDelta::ReasoningText {
                    output_index, text, ..
                } => {
                    state
                        .thinking_text_by_index
                        .entry(*output_index)
                        .or_default()
                        .push_str(text);
                }
            }
        }
        let mut text = state
            .response_text_by_index
            .values()
            .cloned()
            .collect::<String>();
        if state.missing_response_prefix && !text.is_empty() {
            text.insert(0, '…');
        }
        let mut thinking = state
            .thinking_text_by_index
            .values()
            .cloned()
            .collect::<String>();
        if state.missing_thinking_prefix && !thinking.is_empty() {
            thinking.insert(0, '…');
        }
        state.thinking_text = (!thinking.is_empty()).then_some(thinking.clone());
        (text, (!thinking.is_empty()).then_some(thinking))
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

    fn update_live_thinking_block(&mut self, spid: &str, thinking: Option<&str>) {
        use tau_themes::names;

        let Some(thinking) = thinking else {
            return;
        };
        if thinking.is_empty() {
            return;
        }
        self.prompts
            .entry(spid.to_owned())
            .or_default()
            .thinking_text = Some(thinking.to_owned());
        if !self.show_thinking {
            return;
        }
        let state = self.prompts.entry(spid.to_owned()).or_default();
        let block = markdown_streaming_block(
            &self.theme,
            names::AGENT_THINKING,
            thinking,
            &mut state.thinking_markdown_cache,
        );
        let existing_tbid = self.prompts.get(spid).and_then(|s| s.thinking_block_id);
        if let Some(tbid) = existing_tbid {
            self.handle.set_block(tbid, block);
        } else {
            self.insert_live_thinking_block(spid, block);
        }
        self.handle.redraw();
    }

    fn insert_live_thinking_block(&mut self, spid: &str, block: tau_cli_term::StyledBlock) {
        // Insert thinking above the live compaction/response stack while keeping
        // any active tool-call UI pinned below the whole streaming response.
        let tbid = self
            .handle
            .new_block(format!("agent-thinking-live:{spid}"), block);
        let anchors = self.live_thinking_anchor_ids(spid);
        self.handle.push_above_active_before_any(tbid, anchors);
        self.prompts
            .entry(spid.to_owned())
            .or_default()
            .thinking_block_id = Some(tbid);
    }

    fn update_live_compaction_block(
        &mut self,
        spid: &str,
        status: Option<(CompactionStatus, String)>,
    ) {
        let Some((status, text)) = status else {
            self.remove_live_compaction_block(spid);
            return;
        };
        let block = render_compaction_block(&self.theme, text, status);
        let existing_id = self.prompts.get(spid).and_then(|s| s.compaction_block_id);
        if let Some(block_id) = existing_id {
            self.handle.set_block(block_id, block);
        } else {
            self.insert_live_compaction_block(spid, block);
        }
        self.handle.redraw();
    }

    fn remove_live_compaction_block(&mut self, spid: &str) {
        let Some(block_id) = self
            .prompts
            .get_mut(spid)
            .and_then(|state| state.compaction_block_id.take())
        else {
            return;
        };
        self.handle.remove_block(block_id);
        self.handle.redraw();
    }

    fn insert_live_compaction_block(&mut self, spid: &str, block: tau_cli_term::StyledBlock) {
        let block_id = self
            .handle
            .new_block(format!("agent-compaction-live:{spid}"), block);
        let anchors = self.live_compaction_anchor_ids(spid);
        self.handle.push_above_active_before_any(block_id, anchors);
        self.prompts
            .entry(spid.to_owned())
            .or_default()
            .compaction_block_id = Some(block_id);
    }

    fn push_live_response_block(&self, block_id: tau_cli_term::BlockId) {
        self.handle
            .push_above_active_before_any(block_id, self.active_tool_anchor_ids());
    }

    fn live_thinking_anchor_ids(&self, spid: &str) -> Vec<tau_cli_term::BlockId> {
        let mut anchors = Vec::new();
        if let Some(state) = self.prompts.get(spid) {
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

    fn live_compaction_anchor_ids(&self, spid: &str) -> Vec<tau_cli_term::BlockId> {
        let mut anchors = Vec::new();
        if let Some(state) = self.prompts.get(spid)
            && let Some(block_id) = state.response_block_id
        {
            anchors.push(block_id);
        }
        anchors.extend(self.active_tool_anchor_ids());
        anchors
    }

    fn active_tool_anchor_ids(&self) -> Vec<tau_cli_term::BlockId> {
        let mut anchors = Vec::new();
        if self.prompt_tool_summary_active
            && let Some(block_id) = self.prompt_tool_summary
        {
            anchors.push(block_id);
        }
        for state in self.tool_calls.values() {
            if let Some(block_id) = state.summary_block_id {
                anchors.push(block_id);
            }
            if let Some(block_id) = state.block_id {
                anchors.push(block_id);
            }
        }
        anchors
    }

    fn update_live_response_block(&mut self, spid: &str, text: &str) {
        use tau_themes::names;

        if let Some(bid) = self.prompts.get(spid).and_then(|s| s.response_block_id) {
            let Some(state) = self.prompts.get_mut(spid) else {
                return;
            };
            let block = if text.is_empty() {
                streaming_block_with_indicator_suffix(
                    &self.theme,
                    names::AGENT_PENDING,
                    STREAMING_AGENT_RESPONSE_PREFIX.trim_end(),
                    response_stats_indicator_for_prompt(state),
                )
            } else {
                markdown_prefixed_streaming_block(
                    &self.theme,
                    names::AGENT_RESPONSE,
                    STREAMING_AGENT_RESPONSE_PREFIX,
                    text,
                    &mut state.response_markdown_cache,
                )
            };
            self.handle.set_block(bid, block);
            self.handle.redraw();
        }
    }

    fn handle_provider_response_finished(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) {
        if finished.originator.is_user()
            && tool_calls_from_output_items(&finished.output_items).is_empty()
        {
            self.clear_main_agent_turn_active_everywhere();
        }
        self.finished_provider_prompts
            .insert(finished.agent_prompt_id.to_string());
        let (prompt_state, turn_latency) = self.take_finished_prompt_state(finished);
        let thinking =
            reasoning_text_from_output_items(&finished.output_items).or(prompt_state.thinking_text);
        self.finalize_finished_thinking_block(prompt_state.thinking_block_id, thinking);
        self.finalize_finished_compaction_block(prompt_state.compaction_block_id);
        self.finalize_finished_response_block(prompt_state.response_block_id);

        let full_assistant_text = assistant_text_from_output_items(&finished.output_items);
        self.record_finished_assistant_context(finished, full_assistant_text.as_deref());
        self.record_finished_turn_stats(finished, turn_latency);
        self.render_user_provider_response_items(finished);
        self.render_model_status();
    }

    fn take_finished_prompt_state(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) -> (PromptState, Option<Duration>) {
        let spid = finished.agent_prompt_id.as_str();
        // Drain the whole per-prompt state in one shot — every field tracked
        // through the stream is consumed here.
        let prompt_state = self.prompts.remove(spid).unwrap_or_default();
        let turn_latency = prompt_state
            .started_at
            .map(|started_at| started_at.elapsed());
        if let Some(latency) = turn_latency {
            self.cumulative_agent_latency += latency;
        }
        (prompt_state, turn_latency)
    }

    fn finalize_finished_thinking_block(
        &mut self,
        thinking_block_id: Option<tau_cli_term::BlockId>,
        thinking: Option<String>,
    ) {
        use tau_themes::names;

        // Finalize the thinking block above the response, using the final
        // item-model reasoning text or the latest streamed snapshot if one was
        // captured.
        if let Some(tbid) = thinking_block_id {
            self.handle.remove_block(tbid);
        }
        if self.show_thinking
            && let Some(thinking) = thinking.filter(|t| !t.is_empty())
        {
            let bid = self.handle.print_output(
                "agent-thinking",
                markdown_block(&self.theme, names::AGENT_THINKING, &thinking),
            );
            self.thinking_history.push(ThinkingBlockEntry {
                block_id: bid,
                text: thinking,
            });
        }
    }

    fn finalize_finished_compaction_block(
        &mut self,
        compaction_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if let Some(block_id) = compaction_block_id {
            self.handle.remove_block(block_id);
        }
    }

    fn finalize_finished_response_block(
        &mut self,
        response_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if let Some(bid) = response_block_id {
            self.handle.remove_block(bid);
        }
    }

    fn record_finished_assistant_context(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        full_assistant_text: Option<&str>,
    ) {
        let Some(text) = full_assistant_text else {
            return;
        };
        if finished.originator.is_user() {
            self.set_editor_last_response(text.to_owned());
        }
    }

    fn record_finished_turn_stats(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        turn_latency: Option<Duration>,
    ) {
        let Some(usage) = finished.usage.clone() else {
            return;
        };
        let previous_usage = self
            .turn_stats_history
            .last()
            .map(|entry| entry.usage.clone());
        let block = if self.show_turn_stats {
            render_turn_stats_block(
                &self.theme,
                &usage,
                previous_usage.as_ref(),
                turn_latency,
                Some(self.cumulative_agent_latency),
            )
        } else {
            Self::empty_block()
        };
        let bid = self.handle.print_output("turn-stats", block);
        self.turn_stats_history.push(TurnStatsBlockEntry {
            block_id: bid,
            usage,
            previous_usage,
            turn_latency,
            total_latency: Some(self.cumulative_agent_latency),
        });
    }

    fn render_user_provider_response_items(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) {
        // The event has already been routed into the owning agent transcript.
        // Only the main agent's tool calls land in the UI as their own blocks.
        // Sub-agent activity is summarized through generic watched-agent stats,
        // so the user sees one activity line per watched agent rather than a
        // flood of nested invocations.
        self.main_agent_turn_active = true;
        if finished.output_items.is_empty() {
            self.finish_prompt_tool_summary();
            self.render_provider_response_placeholder(
                finished
                    .error
                    .as_deref()
                    .unwrap_or("(provider returned an empty response)"),
            );
            return;
        }
        let tool_calls = tool_calls_from_output_items(&finished.output_items);
        self.main_tools_total += tool_calls.len() as u64;
        self.set_main_tools_visible(!tool_calls.is_empty());
        let summary_block_id = self.prepare_tool_summary_for_finished_calls(&tool_calls);
        for item in &finished.output_items {
            self.render_finished_context_item(item, summary_block_id, finished);
        }
        self.handle.redraw();
    }

    fn render_provider_response_placeholder(&mut self, text: &str) {
        use tau_themes::names;

        self.handle.print_output(
            "agent-response-placeholder",
            markdown_prefixed_block(
                &self.theme,
                names::AGENT_RESPONSE,
                COMPLETED_AGENT_RESPONSE_PREFIX,
                text,
            ),
        );
    }

    fn prepare_tool_summary_for_finished_calls(
        &mut self,
        tool_calls: &[ToolCallItem],
    ) -> Option<tau_cli_term::BlockId> {
        if tool_calls.is_empty() {
            self.finish_prompt_tool_summary();
            return None;
        }
        if matches!(
            self.show_tools,
            tau_config::settings::ShowTools::SummarizePrompt
        ) {
            return Some(self.create_or_update_prompt_tool_summary(tool_calls.len() as u64));
        }
        Some(self.create_turn_tool_summary(tool_calls.len() as u64))
    }

    fn create_or_update_prompt_tool_summary(&mut self, total_delta: u64) -> tau_cli_term::BlockId {
        if let Some(id) = self.prompt_tool_summary {
            if let Some(summary) = self.tool_summaries.get_mut(&id) {
                summary.total += total_delta;
            }
            if self.prompt_tool_summary_active {
                self.update_tool_summary_block(id);
                return id;
            }
            if let Some(summary) = self.tool_summaries.remove(&id) {
                return self.create_prompt_tool_summary(summary);
            }
        }
        let summary = ToolSummaryDisplay {
            total: total_delta,
            ..ToolSummaryDisplay::default()
        };
        self.create_prompt_tool_summary(summary)
    }

    fn create_prompt_tool_summary(&mut self, summary: ToolSummaryDisplay) -> tau_cli_term::BlockId {
        let block = self.render_summary_block(&summary);
        let id = self.handle.new_block("tool-summary:prompt", block);
        self.handle.push_above_active(id);
        self.tool_summaries.insert(id, summary);
        self.prompt_tool_summary = Some(id);
        self.prompt_tool_summary_active = true;
        id
    }

    fn finish_prompt_tool_summary(&mut self) {
        let Some(block_id) = self.prompt_tool_summary.take() else {
            self.prompt_tool_summary_active = false;
            return;
        };
        self.prompt_tool_summary_active = false;
        let Some(summary) = self.tool_summaries.remove(&block_id) else {
            return;
        };
        self.handle.remove_block(block_id);
        let new_block_id = self
            .handle
            .print_output("tool-summary", self.render_summary_block(&summary));
        self.tool_summaries.insert(new_block_id, summary);
    }

    fn create_turn_tool_summary(&mut self, total: u64) -> tau_cli_term::BlockId {
        let summary = ToolSummaryDisplay {
            total,
            ..ToolSummaryDisplay::default()
        };
        let block = self.render_summary_block(&summary);
        let id = self.handle.new_block("tool-summary:turn", block);
        self.handle.push_above_active(id);
        self.tool_summaries.insert(id, summary);
        id
    }

    fn render_finished_context_item(
        &mut self,
        item: &ContextItem,
        summary_block_id: Option<tau_cli_term::BlockId>,
        finished: &tau_proto::ProviderResponseFinished,
    ) {
        use tau_themes::names;

        match item {
            ContextItem::Message(message) => {
                if let Some(text) = assistant_text_from_message_item(message) {
                    self.handle.print_output(
                        "agent-response",
                        markdown_prefixed_block(
                            &self.theme,
                            names::AGENT_RESPONSE,
                            COMPLETED_AGENT_RESPONSE_PREFIX,
                            &text,
                        ),
                    );
                }
            }
            ContextItem::ToolCall(call) => {
                self.render_tool_call_placeholder(call, summary_block_id);
            }
            ContextItem::Compaction(_) => {
                let status = Self::compaction_success_status(
                    finished.compaction_original_input_tokens,
                    finished.compaction_compacted_input_tokens,
                );
                self.handle.print_output(
                    "compaction-completed",
                    render_compaction_block(&self.theme, status, CompactionStatus::Success),
                );
            }
            _ => {}
        }
    }

    fn render_tool_call_placeholder(
        &mut self,
        call: &ToolCallItem,
        summary_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if self.tool_calls.contains_key(call.call_id.as_str()) {
            return;
        }
        let history_id = self.handle.new_block(
            format!("tool-call-history:{}:{}", call.name, call.call_id),
            Self::empty_block(),
        );
        self.handle.push_history(history_id);
        self.tool_calls.insert(
            call.call_id.to_string(),
            ToolCallState {
                history_block_id: Some(history_id),
                summary_block_id,
                is_main_delegate: call.name.as_str() == AGENT_START_TOOL_NAME,
                ..ToolCallState::default()
            },
        );
    }

    fn handle_tool_started(&mut self, started: &tau_proto::ToolStarted, recorded_at: UnixMicros) {
        let call_id = started.call_id.to_string();
        self.tool_agents
            .entry(call_id.clone())
            .or_insert_with(|| started.agent_id.to_string());
        if self
            .tool_calls
            .get(call_id.as_str())
            .is_some_and(|state| state.is_sub_agent || state.block_id.is_some())
        {
            return;
        }
        let mut display = pending_tool_call_display(started.tool_name.as_str());
        Self::upsert_tool_duration_suffix(&mut display, Duration::ZERO);
        let live_block = self.render_tool_history_block(&display);
        let live_id = self.handle.new_block(
            format!("tool-call-live:{}:{}", started.tool_name, started.call_id),
            live_block,
        );
        self.handle.push_above_active(live_id);
        let state = self.tool_calls.entry(call_id).or_insert_with(|| {
            let history_id = self.handle.new_block(
                format!(
                    "tool-call-history:{}:{}",
                    started.tool_name, started.call_id
                ),
                Self::empty_block(),
            );
            self.handle.push_history(history_id);
            ToolCallState {
                history_block_id: Some(history_id),
                is_main_delegate: started.tool_name.as_str() == AGENT_START_TOOL_NAME,
                ..ToolCallState::default()
            }
        });
        state.block_id = Some(live_id);
        state.live_display = Some(display);
        state.started_at = Some(Instant::now());
        state.recorded_started_at = Some(recorded_at);
        if let Some(timer) = &self.tool_timer {
            timer.tool_started(started.call_id.as_str());
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
            Event::ProviderToolResult(result)
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder =>
            {
                self.handle_tool_background_placeholder(result.call_id.as_str());
                true
            }
            Event::ProviderToolResult(result)
                if self.tool_calls.contains_key(result.call_id.as_str()) =>
            {
                self.handle_tool_result(result, recorded_at);
                true
            }
            Event::ProviderToolError(_) => true,
            Event::ToolResult(result) => {
                self.handle_tool_result(result, recorded_at);
                true
            }
            Event::ToolError(error) => {
                self.handle_tool_error(error, recorded_at);
                true
            }
            Event::ToolBackgroundResult(result) => {
                self.handle_tool_background_result(result, recorded_at);
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

        let state = self.tool_calls.get(progress.call_id.as_str());
        if state.is_some_and(|s| s.is_sub_agent) {
            return;
        }

        if let Some(progress_display) = progress.display.as_ref() {
            let mut update = None;
            let freeze_multiline_payloads = self.freeze_multiline_live_payloads();
            if let Some(state) = self.tool_calls.get_mut(progress.call_id.as_str())
                && let Some(block_id) = state.block_id
            {
                let mut display = render_tool_use_state(&progress.tool_name, progress_display);
                if Self::use_static_live_duration(freeze_multiline_payloads, &display) {
                    Self::upsert_static_tool_duration_suffix(&mut display);
                } else if let Some(duration) = Self::live_tool_duration(state) {
                    Self::upsert_tool_duration_suffix(&mut display, duration);
                }
                state.live_display = Some(display.clone());
                update = Some((block_id, display));
            }
            if let Some((block_id, display)) = update {
                let block = self.render_tool_history_block(&display);
                self.handle.set_block(block_id, block);
                if self.model_status_block.is_some() {
                    self.render_model_status();
                } else {
                    self.handle.redraw();
                }
                return;
            }
        }

        let state = self.tool_calls.get(progress.call_id.as_str());
        if state.is_none_or(|s| s.block_id.is_none()) {
            let text = tau_harness::format_tool_progress(progress);
            self.handle.print_output(
                "tool-progress",
                themed_block(&self.theme, names::SHELL_OUTPUT, text),
            );
        }
    }

    pub(crate) fn handle_tool_timer_tick(&mut self) {
        let mut changed = false;
        let mut updates = Vec::new();
        for (call_id, state) in &self.tool_calls {
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
            Self::upsert_tool_duration_suffix(&mut display, duration);
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
            if let Some(state) = self.tool_calls.get_mut(&call_id) {
                state.live_display = Some(display.clone());
            }
            let block = self.render_tool_history_block(&display);
            self.handle.set_block(block_id, block);
            changed = true;
        }
        if changed {
            self.handle.redraw();
        }
        let quota_tick_due = self
            .last_quota_tick
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(60));
        if quota_tick_due {
            self.last_quota_tick = Some(Instant::now());
            self.render_model_status_if_present();
        }
    }

    fn freeze_multiline_live_payloads(&self) -> bool {
        matches!(self.show_tools, tau_config::settings::ShowTools::Full)
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
    ) {
        if Self::use_static_live_duration(freeze_multiline_payloads, display) {
            Self::upsert_static_tool_duration_suffix(display);
        } else if let Some(duration) = duration {
            Self::upsert_tool_duration_suffix(display, duration);
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

    fn upsert_tool_duration_suffix(display: &mut ToolCallDisplay, duration: Duration) {
        let suffix = tool_duration_suffix(duration);
        Self::upsert_tool_duration_suffix_segment(display, suffix);
    }

    fn upsert_static_tool_duration_suffix(display: &mut ToolCallDisplay) {
        let mut suffix = tool_duration_suffix(Duration::ZERO);
        suffix.text = "-s".to_owned();
        Self::upsert_tool_duration_suffix_segment(display, suffix);
    }

    fn upsert_tool_duration_suffix_segment(
        display: &mut ToolCallDisplay,
        mut suffix: crate::tool_render::ToolSuffixSegment,
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
        call_id: &str,
        originator_is_user: bool,
    ) -> Option<(ToolCallState, bool)> {
        let prior = self.tool_calls.remove(call_id);
        let known_main_tool = prior
            .as_ref()
            .is_some_and(|prior| !prior.is_sub_agent && originator_is_user);
        let prior = prior.unwrap_or_default();
        if prior.is_sub_agent {
            return None;
        }
        if let Some(block_id) = prior.block_id {
            if let Some(timer) = &self.tool_timer {
                timer.tool_finished(call_id);
            }
            self.handle.remove_block(block_id);
        }
        if known_main_tool {
            self.main_backgrounded_tools.remove(call_id);
            self.record_main_tool_completed();
            if self.main_agent_turn_active || !self.main_backgrounded_tools.is_empty() {
                self.main_tools_visible = true;
            }
        }
        Some((prior, known_main_tool))
    }

    fn handle_tool_result(&mut self, result: &tau_proto::ToolResult, recorded_at: UnixMicros) {
        let call_id = result.call_id.as_str();
        if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
            self.handle_tool_background_placeholder(call_id);
            return;
        }
        // Sub-agent tool activity stays out of the user's transcript; generic
        // watched-agent stats provide the live activity signal.
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(call_id, result.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_result_display(result);
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        let diff = Self::tool_result_diff(result);
        self.record_tool_summary_result(
            prior.summary_block_id,
            result.display.as_ref(),
            diff.as_ref(),
            false,
        );
        self.record_tool_result_block(prior.history_block_id, display, diff);
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn handle_tool_background_placeholder(&mut self, call_id: &str) {
        let Some(state) = self.tool_calls.get(call_id) else {
            return;
        };
        if state.is_sub_agent {
            return;
        }
        self.main_backgrounded_tools.insert(call_id.to_owned());
        self.main_tools_visible = true;
        self.render_model_status();
    }

    fn handle_tool_background_result(
        &mut self,
        result: &tau_proto::ToolBackgroundResult,
        recorded_at: UnixMicros,
    ) {
        let result = tau_proto::ToolResult {
            call_id: result.call_id.clone(),
            tool_name: result.tool_name.clone(),
            tool_type: result.tool_type,
            result: result.result.clone(),
            provider_content: Vec::new(),
            kind: tau_proto::ToolResultKind::Final,
            display: result.display.clone(),
            originator: result.originator.clone(),
        };
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(result.call_id.as_str(), result.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_result_display(&result);
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        let diff = Self::tool_result_diff(&result);
        self.record_tool_summary_result(
            prior.summary_block_id,
            result.display.as_ref(),
            diff.as_ref(),
            false,
        );
        self.record_tool_result_block(prior.history_block_id, display, diff);
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn tool_result_display(result: &tau_proto::ToolResult) -> ToolCallDisplay {
        if result.tool_name.as_str() == AGENT_START_TOOL_NAME {
            if let Some(descriptor) = &result.display {
                return render_tool_use_state(&result.tool_name, descriptor);
            }
            let descriptor = build_delegate_completion_display(None, &result.result, None);
            render_tool_use_state(&result.tool_name, &descriptor)
        } else if let Some(descriptor) = &result.display {
            render_tool_use_state(&result.tool_name, descriptor)
        } else {
            render_tool_use_state(
                &result.tool_name,
                &synthesize_fallback_display(&result.tool_name, None),
            )
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

    fn tool_result_diff(result: &tau_proto::ToolResult) -> Option<tau_proto::ToolUsePayload> {
        let display_diff = result.display.as_ref().and_then(|d| match &d.payload {
            Some(payload) if Self::diff_payload_has_changes(payload) => Some(payload.clone()),
            _ => None,
        });
        if result.display.is_some() {
            display_diff
        } else {
            display_diff
                .or_else(|| extract_diff(&result.result).map(tau_proto::ToolUsePayload::Diff))
        }
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
            self.diff_blocks.push(DiffBlockEntry {
                block_id: bid,
                display,
                diff,
            });
        } else {
            let block = self.render_tool_history_block(&display);
            let bid =
                self.update_existing_or_print_tool_block(existing_block_id, "tool-result", block);
            self.tool_history.push(ToolBlockEntry {
                block_id: bid,
                display,
            });
        }
    }

    fn handle_tool_error(&mut self, error: &tau_proto::ToolError, recorded_at: UnixMicros) {
        let call_id = error.call_id.as_str();
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(call_id, error.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_error_display(error);
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        self.record_tool_summary_result(prior.summary_block_id, error.display.as_ref(), None, true);
        self.record_plain_finished_tool_block(prior.history_block_id, display, "tool-error");
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn handle_tool_background_error(
        &mut self,
        error: &tau_proto::ToolBackgroundError,
        recorded_at: UnixMicros,
    ) {
        let error = tau_proto::ToolError {
            call_id: error.call_id.clone(),
            tool_name: error.tool_name.clone(),
            tool_type: error.tool_type,
            message: error.message.clone(),
            details: error.details.clone(),
            display: error.display.clone(),
            originator: error.originator.clone(),
        };
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(error.call_id.as_str(), error.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_error_display(&error);
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        self.record_tool_summary_result(prior.summary_block_id, error.display.as_ref(), None, true);
        self.record_plain_finished_tool_block(prior.history_block_id, display, "tool-error");
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn tool_error_display(error: &tau_proto::ToolError) -> ToolCallDisplay {
        let cbor = error.details.as_ref();
        if error.tool_name.as_str() == AGENT_START_TOOL_NAME {
            if let Some(descriptor) = &error.display {
                return render_tool_use_state(&error.tool_name, descriptor);
            }
            let descriptor = build_delegate_completion_display(
                None,
                cbor.unwrap_or(&CborValue::Null),
                Some(&error.message),
            );
            render_tool_use_state(&error.tool_name, &descriptor)
        } else if let Some(descriptor) = &error.display {
            render_tool_use_state(&error.tool_name, descriptor)
        } else {
            render_tool_use_state(
                &error.tool_name,
                &synthesize_fallback_display(&error.tool_name, Some(&error.message)),
            )
        }
    }

    fn handle_tool_cancelled(
        &mut self,
        cancelled: &tau_proto::ToolCancelled,
        recorded_at: UnixMicros,
    ) {
        let call_id = cancelled.call_id.as_str();
        let Some((prior, known_main_tool)) = self.take_finished_tool_call(call_id, true) else {
            return;
        };
        let mut display = render_tool_use_state(
            &cancelled.tool_name,
            &synthesize_fallback_display(&cancelled.tool_name, Some("cancelled")),
        );
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
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
        self.tool_history.push(ToolBlockEntry {
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
            self.handle.set_block(bid, block);
            self.handle.redraw();
            bid
        } else {
            self.handle.print_output(label, block)
        }
    }

    fn render_model_status_after_tool_completion(&mut self, known_main_tool: bool) {
        if known_main_tool && self.main_agent_turn_active {
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
        let block = render_shell_block(&self.theme, &cmd.command, "", Some(&label));
        let block_id = self
            .handle
            .new_block(format!("shell-command:{}", cmd.command_id), block);
        self.handle.push_above_active(block_id);
        self.handle.redraw();
        self.shell_blocks.insert(
            cmd.command_id.to_string(),
            ShellBlockState {
                block_id,
                command: cmd.command.clone(),
                include_in_context: cmd.include_in_context,
                output: String::new(),
            },
        );
    }

    fn handle_shell_command_progress(&mut self, progress: &tau_proto::ShellCommandProgress) {
        if let Some(state) = self.shell_blocks.get_mut(progress.command_id.as_str()) {
            state.output.push_str(&progress.chunk);
            let label = Self::shell_running_label(state.include_in_context);
            let block =
                render_shell_block(&self.theme, &state.command, &state.output, Some(&label));
            self.handle.set_block(state.block_id, block);
            self.handle.redraw();
        }
    }

    fn handle_shell_command_finished(&mut self, finished: &tau_proto::ShellCommandFinished) {
        let include_in_context =
            if let Some(state) = self.shell_blocks.remove(finished.command_id.as_str()) {
                // Use the final, post-truncation output from the extension rather
                // than our streaming buffer so the UI matches what the harness
                // injected into context.
                self.handle.remove_block(state.block_id);
                state.include_in_context
            } else {
                // Session replay may contain only the durable terminal event. Render
                // it from the self-contained payload instead of dropping it.
                finished.include_in_context
            };
        let suffix = Self::shell_finished_suffix(finished, include_in_context);
        let block = render_shell_block(
            &self.theme,
            &finished.command,
            &finished.output,
            Some(&suffix),
        );
        self.handle.print_output("shell-finished", block);
        self.shell_agents.remove(finished.command_id.as_str());
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
                self.action_state.apply_schema_published(published);
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
                use crate::tool_render::render_action_output_block;
                self.handle.print_output(
                    "retry-result",
                    render_action_output_block(&self.theme, &result.message),
                );
                true
            }
            Event::UiSetAgentNavigationModeResult(result) => {
                if let tau_proto::UiSetAgentNavigationModeOutcome::Rejected { reason } =
                    result.outcome
                {
                    use crate::tool_render::render_action_output_block;
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
                    self.handle.print_output(
                        "agent-navigation-result",
                        render_action_output_block(&self.theme, &message),
                    );
                }
                true
            }
            Event::ActionInvoke(_) => true,
            _ => false,
        }
    }

    fn refresh_action_completions(&self) {
        let (commands, arg_completers) = self.action_state.dynamic_completions();
        self.completion_data
            .set_dynamic_commands_and_arg_completers(commands, arg_completers);
    }

    fn handle_action_result(&mut self, result: &tau_proto::ActionResult) {
        use crate::tool_render::render_action_output_block;

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
        self.handle.print_output(
            "action-result",
            render_action_output_block(&self.theme, &text),
        );
        if self.displayed_agent_id.is_none() {
            self.preserve_on_fresh_agent_switch = true;
        }
    }

    fn handle_action_error(&mut self, error: &tau_proto::ActionError) {
        use crate::tool_render::render_action_error_block;

        self.handle.print_output(
            "action-error",
            render_action_error_block(&self.theme, &error.action_id, &error.message),
        );
        if self.displayed_agent_id.is_none() {
            self.preserve_on_fresh_agent_switch = true;
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
                self.action_state
                    .remove_extension(&exited.extension_name, exited.instance_id);
                self.refresh_action_completions();
                self.handle_extension_exited(exited);
                true
            }
            Event::ExtSkillAvailable(skill) => {
                self.skill_state.apply_skill_available(skill);
                true
            }
            Event::ExtAgentsMdAvailable(agents) => {
                self.handle_agents_md_available(agents);
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
        if !self.notice_visible(tau_proto::NoticeLevel::Info, false) {
            return;
        }
        let owner = self.current_extension_block_owner();
        if matches!(owner, UiSnapshotOwner::NoAgent) {
            self.preserve_on_fresh_agent_switch = true;
        }
        let block = extension_status_block(&self.theme, &starting.extension_name, "starting");
        let id = self.handle.new_block(
            format!("extension-starting:{}", starting.instance_id),
            block,
        );
        self.handle.push_above_active(id);
        self.handle.redraw();
        self.extension_blocks.insert(
            starting.instance_id,
            ExtensionBlockState {
                block_id: id,
                owner,
            },
        );
    }

    fn handle_extension_ready(&mut self, ready: &tau_proto::ExtensionReady) {
        let removed_starting = if let Some(state) = self.extension_blocks.remove(&ready.instance_id)
        {
            self.handle.remove_block(state.block_id);
            true
        } else {
            false
        };
        self.ready_extensions
            .insert(ready.extension_name.to_string());
        if !self.notice_visible(tau_proto::NoticeLevel::Info, false) {
            if removed_starting {
                self.handle.redraw();
            }
            return;
        }
        self.handle.print_output(
            "extension-ready",
            extension_status_block(&self.theme, &ready.extension_name, "ready"),
        );
    }

    fn handle_extension_exited(&mut self, exited: &tau_proto::ExtensionExited) {
        let removed_starting =
            if let Some(state) = self.extension_blocks.remove(&exited.instance_id) {
                self.handle.remove_block(state.block_id);
                true
            } else {
                false
            };
        self.ready_extensions.remove(exited.extension_name.as_str());
        if !self.notice_visible(tau_proto::NoticeLevel::Info, false) {
            if removed_starting {
                self.handle.redraw();
            }
            return;
        }
        self.handle.print_output(
            "extension-exited",
            extension_status_block(&self.theme, &exited.extension_name, "exited"),
        );
    }

    fn handle_agents_md_available(&mut self, agents: &tau_proto::ExtAgentsMdAvailable) {
        self.handle.print_output(
            "agents-md",
            system_loaded_block(&self.theme, &agents.file_path, &agents.content),
        );
    }

    fn handle_extension_context_ready(&mut self, ready: &tau_proto::ExtensionContextReady) {
        if !self.notice_visible(tau_proto::NoticeLevel::Debug, false) {
            return;
        }
        self.handle.print_output(
            "extension-context-ready",
            crate::tool_render::agent_context_ready_block(&self.theme, &ready.agent_id),
        );
    }

    fn handle_harness_status_events(&mut self, event: &Event) -> bool {
        match event {
            Event::HarnessNotice(info) => {
                if info.visible_at(self.notice_level) {
                    self.handle
                        .print_output("harness-notice", render_harness_notice(&self.theme, info));
                }
                true
            }
            Event::HarnessSessionDir(session_dir) => {
                if self.notice_visible(tau_proto::NoticeLevel::Info, false) {
                    self.handle_harness_session_dir(session_dir);
                }
                true
            }
            Event::HarnessUiDir(ui_dir) => {
                if self.notice_visible(tau_proto::NoticeLevel::Info, false) {
                    self.handle
                        .print_output("ui-dir", ui_dir_block(&self.theme, &ui_dir.path));
                }
                true
            }
            Event::HarnessModelsAvailable(models) => {
                self.handle_harness_models_available(models);
                true
            }
            Event::HarnessProviderQuotaChanged(changed) => {
                self.quota_pacing.update(changed);
                self.render_model_status_if_present();
                true
            }
            _ => false,
        }
    }

    fn handle_harness_session_dir(&mut self, session_dir: &tau_proto::HarnessSessionDir) {
        self.handle.print_output(
            "session-dir",
            session_status_block(
                &self.theme,
                &session_dir.path,
                "/",
                session_dir.status.as_str(),
            ),
        );
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
                self.current_context_input_tokens = changed.input_tokens;
                self.current_context_percent = changed.percent_used;
                self.render_model_status();
                true
            }
            Event::HarnessAgentContextUsageChanged(changed) => {
                self.current_context_input_tokens = changed.input_tokens;
                self.current_context_window =
                    changed.context_window.or(self.current_context_window);
                self.current_context_percent = changed.percent_used;
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
        self.completion_data
            .set_arg_completions(tau_cli_term::CommandName::new("/model"), model_items);
    }

    fn handle_harness_roles_available(&mut self, roles: &tau_proto::HarnessRolesAvailable) {
        let role_defaults: HashMap<String, RoleCompletionDetails> = roles
            .roles
            .iter()
            .map(|r| (r.name.clone(), RoleCompletionDetails::from_role_info(r)))
            .collect();
        let role_items = Self::role_completion_items(roles, &role_defaults);
        if let Ok(mut available) = self.roles_available.lock() {
            *available = roles.roles.iter().map(|r| r.name.clone()).collect();
        }
        if let Ok(mut available) = self.role_groups_available.lock() {
            *available = roles.groups.clone();
        }
        if let Ok(mut prompts) = self.custom_prompts.lock() {
            *prompts = roles.custom_prompts.clone();
        }
        let prompt_items = roles
            .custom_prompts
            .iter()
            .map(|prompt| tau_cli_term::CompletionItem::plain(prompt.id.clone()))
            .collect();
        self.completion_data
            .set_arg_completions(tau_cli_term::CommandName::new("/prompt"), prompt_items);
        let new_agent_role_items = role_items
            .iter()
            .map(|(item, _)| item.clone())
            .collect::<Vec<_>>();
        self.completion_data
            .set_arg_completions(tau_cli_term::CommandName::new("/new"), new_agent_role_items);
        self.role_defaults = role_defaults;
        if self.current_role.is_some() && self.model_status_block.is_some() {
            self.render_model_status();
        }
        let completer: tau_cli_term::ArgCompleter =
            std::sync::Arc::new(move |args| role_command_completions(&role_items, args));
        self.completion_data
            .set_arg_completer(tau_cli_term::CommandName::new("/role"), completer);
    }

    fn role_completion_items(
        roles: &tau_proto::HarnessRolesAvailable,
        role_defaults: &HashMap<String, RoleCompletionDetails>,
    ) -> Vec<(tau_cli_term::CompletionItem, RoleCompletionDetails)> {
        roles
            .roles
            .iter()
            .filter_map(|role| {
                let details = role_defaults.get(&role.name)?.clone();
                Some((
                    tau_cli_term::CompletionItem::new(&role.name, details.short_description()),
                    details,
                ))
            })
            .collect()
    }

    fn handle_harness_role_selected(&mut self, selected: &tau_proto::HarnessRoleSelected) {
        self.current_model = selected.model.clone();
        self.current_role = Some(selected.role.clone());
        self.baseline_params = selected.baseline_params;
        self.model_params = selected.model_params;
        self.effort_state.store(
            selected.model_params.effort.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.verbosity_state.store(
            selected.model_params.verbosity.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.thinking_summary_state.store(
            selected.model_params.thinking_summary.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.fast_service_tier_state.store(
            matches!(
                selected.model_params.service_tier,
                Some(tau_proto::ServiceTier::Fast)
            ),
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Ok(mut role) = self.current_role_state.lock() {
            *role = Some(selected.role.clone());
        }
        if let (Ok(groups), Ok(mut memory)) = (
            self.role_groups_available.lock(),
            self.role_group_memory.lock(),
        ) && let Some(group) = groups
            .iter()
            .find(|group| group.roles.iter().any(|role| role == &selected.role))
        {
            memory.insert(group.name.clone(), selected.role.clone());
        }
        let prompt = crate::theme::active_prompt_marker(
            &self.theme,
            &self.prompt_symbol,
            Some(&selected.role),
        );
        self.handle.set_left_prompt(prompt);
        self.refresh_prompt_placeholder();
        self.handle.redraw();
        self.current_context_window = selected.context_window;
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
                self.handle
                    .print_osc1337_set_user_var(&req.name, &req.value, in_tmux);
                true
            }
            Event::TermBell(_) => {
                self.handle.print_terminal_bell();
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests;

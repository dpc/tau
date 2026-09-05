//! Coherent ownership records for the terminal event renderer.
//!
//! The renderer coordinates event handling, while these records make lifecycle
//! and reset boundaries explicit. Transcript state is shared by the visible
//! renderer and detached agent snapshots so switching transcripts moves one
//! complete ownership unit.

#[cfg(test)]
use std::cell::Cell;

use super::*;
use crate::turn_stats_projection::PreviousTurnUsageProjection;

/// Terminal renderer and its logically coherent runtime state.
pub(crate) struct EventRenderer {
    /// Terminal resources and command completion state.
    pub(super) resources: RendererResourcesState,
    /// Agent discovery and navigation projections.
    pub(super) discovery: AgentDiscoveryState,
    /// Current transcript selection and detached snapshots.
    pub(super) selection: AgentSelectionState,
    /// Correlation maps that route events to transcript owners.
    pub(super) event_owners: EventOwnershipState,
    /// Cross-agent watch and prompt activity projections.
    pub(super) watches: WatchActivityState,
    /// State owned by the currently folded transcript.
    pub(super) transcript: TranscriptState,
    /// Session-wide presentation and extension lifecycle state.
    pub(super) session: SessionPresentationState,
    /// Persisted and process-local presentation settings.
    pub(super) presentation: PresentationSettingsState,
    /// Current role, model, quota, and input-thread mirrors.
    pub(super) role: RolePresentationState,
    /// External editor context publication state.
    pub(super) editor: EditorPublicationState,
    /// Renderer-wide activity notification state.
    pub(super) activity: RendererActivityState,
    /// Ordinary selected final staged before its redraw-suppressed routing cut.
    pub(super) staged_finished_response: Option<FinishedResponseProjection>,
    /// Selected standalone final status staged before redraw suppression.
    pub(super) staged_finished_status: Option<tau_cli_term::StyledBlock>,
    /// Whether selected-final routing defers intermediate status projections.
    pub(super) final_publication_in_progress: bool,
    /// Whether hidden-final routing must avoid visible watched-row projection.
    pub(super) hidden_finalization_in_progress: bool,
    /// Cold-attach redraw scope retained through the replay boundary.
    pub(super) cold_attach_redraw: Option<tau_cli_term::RedrawSuppressionGuard>,
    /// Test-only midpoint invoked after expensive ordinary-final projection.
    #[cfg(test)]
    pub(super) finished_staging_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Test-only midpoint after transient final blocks retire.
    #[cfg(test)]
    pub(super) finished_commit_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Test-only midpoint after complete final publication but before cut exit.
    #[cfg(test)]
    pub(super) finished_published_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Terminal handles, command completion state, and stable display resources.
pub(super) struct RendererResourcesState {
    /// Terminal output handle.
    pub(super) handle: RendererHandle,
    /// Command completion catalog.
    pub(super) completion_data: tau_cli_term::CompletionData,
    /// Dynamic action command state.
    pub(super) action_state: ActionCommandState,
    /// Skill command state.
    pub(super) skill_state: SkillCommandState,
    /// Active terminal theme.
    pub(super) theme: tau_themes::Theme,
    /// Symbol shown before active input.
    pub(super) prompt_symbol: String,
    /// Symbol shown before submitted prompts.
    pub(super) submitted_prompt_symbol: String,
}

/// Agent discovery projections and shared navigation data.
pub(super) struct AgentDiscoveryState {
    /// Initialization epochs already projected.
    pub(super) initialized_discovery_epochs:
        HashSet<(tau_proto::AgentId, tau_proto::AgentInitializationId)>,
    /// Discovery events awaiting initial transcript adoption.
    pub(super) pending_initial_discovery: HashMap<tau_proto::AgentId, Vec<DeferredRendererEvent>>,
    /// Agent ids offered by completion.
    pub(super) known_agents: Arc<Mutex<Vec<String>>>,
    /// Authoritative local display names.
    pub(super) agent_display_names: Arc<Mutex<HashMap<tau_proto::AgentId, String>>>,
    /// Atomic navigation modes and membership.
    pub(super) agent_navigation: Arc<Mutex<AgentNavigation>>,
    /// Memory-only transcript owners.
    pub(super) ephemeral_agents: Arc<Mutex<HashSet<tau_proto::AgentId>>>,
}

use super::selection_intent::SelectionIntent;

/// Current input target, visible transcript, and detached transcript snapshots.
pub(super) struct AgentSelectionState {
    /// Agent targeted by prompt input.
    pub(super) current_agent_id: Option<tau_proto::AgentId>,
    /// Agent whose transcript is visible.
    pub(super) displayed_agent_id: Option<tau_proto::AgentId>,
    /// Whether explicit deselection protects the empty view.
    pub(super) awaiting_new_agent_selection: bool,
    /// Detached no-agent transcript.
    pub(super) no_agent_ui_state: AgentUiState,
    /// Detached per-agent transcripts.
    pub(super) agents_ui_state: HashMap<tau_proto::AgentId, AgentUiState>,
    /// Messages already copied to the all-agent overview.
    pub(super) overview_message_ids:
        HashSet<(Option<tau_proto::SessionId>, tau_proto::AgentMessageId)>,
    /// Input-thread mirror of the selected agent.
    pub(super) current_agent_state: Arc<Mutex<SelectionIntent>>,
    /// Mailbox used to retarget pending drafts.
    pub(super) draft_retargeter: Option<DraftRetargeter>,
}

/// Correlation maps that preserve the transcript owner of asynchronous work.
#[derive(Default)]
pub(super) struct EventOwnershipState {
    /// Side-query owners.
    pub(super) query_agents: HashMap<String, tau_proto::AgentId>,
    /// Provider-prompt owners.
    pub(super) prompt_agents: HashMap<tau_proto::AgentPromptId, tau_proto::AgentId>,
    /// Tool-call owners.
    pub(super) tool_agents: HashMap<tau_proto::ToolCallId, tau_proto::AgentId>,
    /// User-shell owners.
    pub(super) shell_agents: HashMap<tau_proto::ShellCommandId, tau_proto::AgentId>,
    /// Dynamic-action snapshot owners.
    pub(super) action_invocation_owners: HashMap<tau_proto::ActionInvocationId, UiSnapshotOwner>,
}

/// Cross-agent watch projections and prompt terminal ordering guards.
#[derive(Default)]
pub(super) struct WatchActivityState {
    /// Watched agents keyed by watcher.
    pub(super) watched_agents: HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
    /// Watchers keyed by watched agent.
    pub(super) agent_watchers: HashMap<tau_proto::AgentId, Vec<tau_proto::AgentId>>,
    /// Latest generic agent stats.
    pub(super) agent_stats: HashMap<tau_proto::AgentId, tau_proto::AgentStatsUpdated>,
    /// Cumulative per-agent API costs.
    pub(super) agent_estimated_api_costs: crate::estimated_cost::AgentCostProjection,
    /// Latest dispatched model per agent.
    pub(super) agent_models: HashMap<tau_proto::AgentId, tau_proto::ModelId>,
    /// Latest watched-agent work status.
    pub(super) watched_agent_work_statuses:
        HashMap<tau_proto::AgentId, tau_proto::AgentWatchWorkStatusNotification>,
    /// Active prompt ids keyed by agent.
    pub(super) active_agent_prompts: HashMap<tau_proto::AgentId, HashSet<tau_proto::AgentPromptId>>,
    /// Prompt terminals that block stale resurrection.
    pub(super) terminal_agent_prompts: HashSet<tau_proto::AgentPromptId>,
    /// Provider prompts whose final output was rendered.
    pub(super) finished_provider_prompts: HashSet<tau_proto::AgentPromptId>,
}

/// State that moves atomically with one rendered transcript.
#[derive(Default)]
pub(super) struct TranscriptState {
    /// In-flight prompt, tool, shell, and block state.
    pub(super) runtime: TranscriptRuntimeState,
    /// Retained blocks used for reversible reprojection.
    pub(super) history: TranscriptHistoryState,
    /// Status-bar counters and agent activity.
    pub(super) status: TranscriptStatusState,
    /// Snapshot adoption and global-output ownership flags.
    pub(super) ownership: TranscriptOwnershipState,
}

/// In-flight lifecycle records owned by one transcript.
#[derive(Default)]
pub(super) struct TranscriptRuntimeState {
    /// Visible watched-agent status blocks.
    pub(super) watched_agent_blocks: HashMap<tau_proto::AgentId, tau_cli_term::BlockId>,
    /// Editor response context for this transcript.
    pub(super) editor_conversation_context: EditorConversationContext,
    /// Prompt lifecycle records.
    pub(super) prompts: HashMap<tau_proto::AgentPromptId, PromptState>,
    /// Standalone compaction prompt correlations.
    pub(super) standalone_compaction_transactions:
        HashMap<tau_proto::CompactionTransactionId, tau_proto::AgentPromptId>,
    /// Completed compaction rows awaiting exact continuation request usage.
    pub(super) completed_compactions:
        HashMap<tau_proto::CompactionTransactionId, CompletedCompactionPresentation>,
    /// First inference prompts owned by completed compaction transactions.
    pub(super) compaction_continuation_prompts:
        HashMap<tau_proto::AgentPromptId, tau_proto::CompactionTransactionId>,
    /// Self-compaction tool correlations.
    pub(super) self_compaction_tools: HashMap<tau_proto::ToolCallId, SelfCompactionTool>,
    /// Last unclassified local user echo.
    pub(super) last_user_block: Option<(tau_cli_term::BlockId, String)>,
    /// Queued user prompt blocks.
    pub(super) queued_user_blocks: VecDeque<QueuedUserBlock>,
    /// Submitted correlated prompt contexts.
    pub(super) submitted_prompt_ctx_ids: VecDeque<String>,
    /// Provider-neutral accepted-submission block.
    pub(super) accepted_submission_block: Option<tau_cli_term::BlockId>,
    /// Tool-call lifecycle records.
    pub(super) tool_calls: HashMap<tau_proto::ToolCallId, ToolCallState>,
    /// User-shell lifecycle records keyed by their protocol identities.
    pub(super) shell_blocks: HashMap<tau_proto::ShellCommandId, ShellBlockState>,
    /// Persistent model status block.
    pub(super) model_status_block: Option<tau_cli_term::BlockId>,
}

/// Retained transcript blocks needed for retroactive presentation changes.
#[derive(Default)]
pub(super) struct TranscriptHistoryState {
    /// Completed diff-capable tool blocks.
    pub(super) diff_blocks: Vec<DiffBlockEntry>,
    /// Completed thinking blocks.
    pub(super) thinking_history: Vec<ThinkingBlockEntry>,
    /// Completed turn-stat blocks.
    pub(super) turn_stats_history: Vec<TurnStatsBlockEntry>,
    /// Usage from the immediately preceding provider terminal, or absence when
    /// a usage-less/standalone terminal broke cache-estimate continuity.
    pub(super) turn_stats_predecessor: Option<PreviousTurnUsageProjection>,
    /// Completed tool blocks.
    pub(super) tool_history: Vec<ToolBlockEntry>,
    /// Durable message blocks.
    pub(super) message_history: Vec<MessageBlockEntry>,
    /// Accepted harness notice blocks.
    pub(super) notice_history: Vec<NoticeBlockEntry>,
    /// Typed diagnostic blocks.
    pub(super) diagnostic_history: Vec<DiagnosticBlockEntry>,
    /// Harness-internal prompt blocks.
    pub(super) internal_prompt_history: Vec<InternalPromptBlockEntry>,
}

/// Status counters and activity owned by one transcript.
#[derive(Clone, Default)]
pub(super) struct TranscriptStatusState {
    /// Current model-context percentage.
    pub(super) current_context_percent: Option<u8>,
    /// Current model-context input tokens.
    pub(super) current_context_input_tokens: Option<u64>,
    /// Current model context window.
    pub(super) current_context_window: Option<u64>,
    /// Completed main-agent tools.
    pub(super) main_tools_completed: u64,
    /// Requested main-agent tools.
    pub(super) main_tools_total: u64,
    /// Backgrounded tools awaiting real completion.
    pub(super) main_backgrounded_tools: HashSet<tau_proto::ToolCallId>,
    /// Whether the main-agent turn is active.
    pub(super) main_agent_turn_active: bool,
    /// Whether the main tool chip is visible.
    pub(super) main_tools_visible: bool,
    /// Tool summaries keyed by block.
    pub(super) tool_summaries: HashMap<tau_cli_term::BlockId, ToolSummaryDisplay>,
    /// Active prompt-level tool summary.
    pub(super) prompt_tool_summary: Option<tau_cli_term::BlockId>,
    /// Whether the prompt summary is in the active area.
    pub(super) prompt_tool_summary_active: bool,
    /// Cumulative response latency.
    pub(super) cumulative_agent_latency: Duration,
    /// Cumulative response token usage.
    pub(super) cumulative_agent_token_usage: tau_proto::TokenUsageCounts,
    /// Detailed transcript activity.
    pub(super) agent_activity: AgentActivity,
}

/// Output ownership flags used when adopting or protecting snapshots.
#[derive(Default)]
pub(super) struct TranscriptOwnershipState {
    /// Whether fresh selection preserves the no-agent snapshot.
    pub(super) preserve_on_fresh_agent_switch: bool,
    /// Whether the snapshot contains globally owned message output.
    pub(super) contains_global_message_fact: bool,
    /// Whether the snapshot contains overview message output.
    pub(super) contains_overview_message: bool,
}

/// Detached output plus all bookkeeping owned by that output.
#[derive(Default)]
pub(super) struct AgentUiState {
    /// Detached terminal output model.
    pub(super) output: tau_cli_term::OutputSnapshot,
    /// Bookkeeping that must move with the output model.
    pub(super) transcript: TranscriptState,
}

/// Session-wide extension and presentation state.
#[derive(Default)]
pub(super) struct SessionPresentationState {
    /// Historical shell terminals that must not consume current lifecycles.
    pub(super) standalone_shell_terminals: HashSet<tau_proto::ShellCommandId>,
    /// Live extension blocks.
    pub(super) extension_blocks: HashMap<tau_proto::ExtensionInstanceId, ExtensionBlockState>,
    /// Extensions ready in this daemon.
    pub(super) ready_extensions: HashSet<String>,
    /// Current session identity.
    pub(super) current_session_id: Option<tau_proto::SessionId>,
    /// Irreversible fail-closed latch after a conflicting session identity.
    pub(super) session_binding_failed: bool,
    /// Session-wide provider token totals.
    pub(super) session_token_usage: tau_proto::TokenUsageCounts,
    /// Startup profile selection.
    pub(super) startup_profile_selection: Option<tau_config::settings::ProfileSelection>,
    /// Paths rendered in the right prompt.
    pub(super) right_prompt_paths: Option<(std::path::PathBuf, Option<std::path::PathBuf>)>,
}

/// Persisted and process-local presentation controls.
pub(super) struct PresentationSettingsState {
    /// Configuration directories used to save settings.
    pub(super) state_dirs: tau_config::settings::TauDirs,
    /// Whether diffs are expanded.
    pub(super) diffs_expanded: bool,
    /// Whether thinking is shown.
    pub(super) show_thinking: bool,
    /// Whether verbose presentation is active.
    pub(super) verbose_mode: bool,
    /// Whether turn stats are shown.
    pub(super) show_turn_stats: bool,
    /// Whether redraw counts are shown.
    pub(super) redraw_counter: bool,
    /// Maximum history lines replayed on redraw.
    pub(super) redraw_history_size: usize,
    /// Whether Markdown links emit OSC 8 metadata.
    pub(super) osc8_links: bool,
    /// Whether UI throughput is shown.
    pub(super) show_ui_io: bool,
    /// Latest UI throughput statistics.
    pub(super) ui_io_stats: UiIoStats,
    /// Output count at the last full render.
    pub(super) last_full_render_count: u64,
    /// Timestamp of the last full render.
    pub(super) last_full_render_at: Option<Instant>,
    /// Tool visibility mode.
    pub(super) show_tools: tau_config::settings::ShowTools,
    /// Message visibility mode.
    pub(super) show_messages: tau_config::settings::ShowMessages,
    /// Whether internal prompts are shown.
    pub(super) show_internal_prompts: bool,
    /// Visible diagnostic threshold.
    pub(super) notice_level: tau_proto::NoticeLevel,
    /// Whether hidden prompt rows get a scroll indicator.
    pub(super) show_prompt_scroll_indicator: bool,
    /// Input-thread mirror of persisted CLI settings.
    pub(super) cli_state_mirror: Arc<Mutex<tau_config::settings::CliState>>,
}

/// Role, model, quota, and input-thread control mirrors.
pub(super) struct RolePresentationState {
    /// Current resolved model.
    pub(super) current_model: Option<tau_proto::ModelId>,
    /// Provider quota pacing state.
    pub(super) quota_pacing: crate::provider_quota::QuotaPacingState,
    /// Last quota-only repaint.
    pub(super) last_quota_tick: Option<Instant>,
    /// Current selected role.
    pub(super) current_role: Option<String>,
    /// Role completion details.
    pub(super) role_defaults: HashMap<String, RoleCompletionDetails>,
    /// Baseline model parameters.
    pub(super) baseline_params: Option<tau_proto::ModelParams>,
    /// Effective model parameters.
    pub(super) model_params: tau_proto::ModelParams,
    /// Shared effort control.
    /// Shared fast-service-tier control.
    pub(super) fast_service_tier_state: Arc<AtomicBool>,
    /// Shared active role.
    pub(super) current_role_state: Arc<Mutex<Option<String>>>,
    /// Shared ordered roles.
    pub(super) roles_available: Arc<Mutex<Vec<String>>>,
    /// Shared custom prompts.
    pub(super) custom_prompts: Arc<Mutex<Vec<tau_proto::HarnessCustomPrompt>>>,
    /// Shared ordered role groups.
    pub(super) role_groups_available: Arc<Mutex<Vec<tau_proto::HarnessRoleGroup>>>,
    /// Last role selected per group.
    pub(super) role_group_memory: Arc<Mutex<HashMap<String, String>>>,
    /// Shared verbosity control.
    pub(super) verbosity_state: Arc<path_std_sync_atomic::AtomicU8>,
    /// Shared thinking-summary control.
    pub(super) thinking_summary_state: Arc<path_std_sync_atomic::AtomicU8>,
}

/// External editor context and publication suppression.
pub(super) struct EditorPublicationState {
    /// Shared editor context.
    pub(super) editor_context: Arc<Mutex<tau_cli_term::EditorContext>>,
    /// Whether hidden folding suppresses publication.
    pub(super) suppress_editor_context_publish: bool,
    /// Number of response bytes copied into the externally visible editor
    /// context by test builds.
    #[cfg(test)]
    pub(super) response_copy_bytes: Cell<u64>,
    /// Exact final semantic projection work performed by production paths in
    /// test builds.
    #[cfg(test)]
    pub(super) final_semantic_projection: FinalSemanticProjectionCounts,
}

/// Production-coupled final semantic string projection counters.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FinalSemanticProjectionCounts {
    /// Non-empty assistant message bodies projected for terminal display.
    pub(crate) message_materializations: u64,
    /// Displayed message bodies that required concatenating multiple text
    /// parts.
    pub(crate) message_concat_allocations: u64,
    /// Non-empty assistant aggregates requested for editor retention.
    pub(crate) assistant_materializations: u64,
    /// Assistant aggregates that required concatenating multiple text parts.
    pub(crate) assistant_concat_allocations: u64,
    /// External-editor publication copies prepared before selected final
    /// commits.
    pub(crate) editor_publication_clones: u64,
    /// Bytes copied into those staged external-editor publication values.
    pub(crate) editor_publication_clone_bytes: u64,
    /// Non-empty reasoning aggregates requested for display or retention.
    pub(crate) reasoning_materializations: u64,
    /// Reasoning aggregates that required concatenating multiple text items.
    pub(crate) reasoning_concat_allocations: u64,
}

/// Renderer-global activity notifications.
#[derive(Default)]
pub(super) struct RendererActivityState {
    /// Timer notifier for tool and quota activity.
    pub(super) tool_timer: Option<ToolTimerNotifier>,
    /// Shared in-progress mirror used by input handling.
    pub(super) agent_in_progress: Arc<AtomicBool>,
}

//! Per-agent runtime state tracked by the harness.
//!
//! An [`Agent`] is one live prompt/tool execution context loaded into the
//! current harness session. The semantic transcript lives in `tau-core`'s
//! `AgentTree`; this module stores the harness-owned runtime state layered on
//! top of that transcript: the selected branch head, queued prompts, turn
//! lifecycle, tool progress, and side-agent ancestry used for routing.
//!
//! The harness multiplexes incoming agent and tool events back to the owning
//! agent via two id maps it owns:
//! `prompt_agents: HashMap<AgentPromptId, AgentId>` and
//! `tool_agents: HashMap<ToolCallId, AgentId>`.

mod loop_guard;

use std::collections::{HashSet, VecDeque};

pub(crate) use loop_guard::{LoopCycleState, LoopGuardState, LoopGuardTrigger, LoopTurnSignature};
use tau_core::{AgentPersistenceMode, NodeId};
use tau_proto::{
    AgentId, AgentPromptId, ConnectionId, ModelId, PromptMessageClass, PromptOriginator, SessionId,
    ToolCallId, ToolUseStats,
};

use crate::dedup::ResultDedupMap;

/// Runtime ownership state for durable activation dispatch.
#[derive(Clone, Debug, Default)]
pub(crate) enum ActivationDispatchState {
    /// No standalone transaction is active or recovery-blocked.
    #[default]
    None,
    /// One durable transaction owns the compact provider request.
    Running {
        /// Durable transaction id.
        id: tau_proto::CompactionTransactionId,
        /// Immutable compact request cut.
        cut: tau_proto::AgentHead,
        /// Activation still owed inference.
        resume_through: Option<tau_proto::AgentHead>,
        /// Model operation identity captured by the durable start.
        model: ModelId,
        /// Explicit branch-navigation generation captured by the start.
        branch_generation: u64,
        /// Provider prompt id pre-minted before the durable start commits.
        compact_prompt_id: AgentPromptId,
    },
    /// Compaction succeeded and the durable inference checkpoint is not
    /// committed yet.
    AwaitingCheckpoint {
        /// Durable owner of the inference checkpoint.
        owner: InferenceCheckpointOwner,
        /// Exact provider prompt id reserved by the checkpoint.
        agent_prompt_id: AgentPromptId,
        /// Immutable inference snapshot covered by the checkpoint.
        through: tau_proto::AgentHead,
        /// Complete provider dispatch ownership captured before checkpoint
        /// commit.
        dispatch: InferenceDispatchOwnership,
    },
    /// The checkpoint committed; remote inference completion is not durable
    /// yet.
    DispatchUncertain {
        /// Durable owner of the uncertain inference dispatch.
        owner: InferenceCheckpointOwner,
        /// Exact provider prompt id committed in the checkpoint.
        agent_prompt_id: AgentPromptId,
        /// Immutable inference snapshot sent to the provider.
        through: tau_proto::AgentHead,
        /// Exact provider-qualified model owned by the durable checkpoint.
        model: Option<ModelId>,
        /// Provider operation owned by the durable checkpoint.
        operation: Option<tau_proto::PromptOperation>,
        /// Immutable activation cut owned by the durable checkpoint.
        activation_cut: Option<tau_proto::AgentHead>,
    },
    /// A durable planned context recovery awaits authoritative provider model
    /// discovery before it can be claimed or terminalized.
    ContextRecoveryPending {
        /// Ordinary inference checkpoint rejected for context length.
        checkpoint: tau_proto::AgentInferenceDispatchStarted,
    },
    /// One reactive claim has been enqueued but its durable start has not yet
    /// committed through interception.
    ContextRecoveryClaimPending {
        /// Source inference checkpoint claimed by the queued start.
        checkpoint: tau_proto::AgentInferenceDispatchStarted,
        /// Durable transaction reserved by the queued start.
        transaction_id: tau_proto::CompactionTransactionId,
    },
    /// Terminal failure retained its recovery obligation.
    Blocked {
        /// Failed durable transaction id.
        failed_id: tau_proto::CompactionTransactionId,
        /// Failed transaction cut.
        cut: tau_proto::AgentHead,
        /// Activation still owed inference.
        resume_through: Option<tau_proto::AgentHead>,
    },
}

/// Complete provider-facing ownership of an inference dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InferenceDispatchOwnership {
    /// Exact provider-qualified model owned by the checkpoint.
    pub(crate) model: ModelId,
    /// Provider operation owned by the checkpoint.
    pub(crate) operation: tau_proto::PromptOperation,
    /// Immutable activation cut owned by the checkpoint.
    pub(crate) activation_cut: tau_proto::AgentHead,
}

/// Durable inference checkpoint ownership.
#[derive(Clone, Debug)]
pub(crate) enum InferenceCheckpointOwner {
    /// Ordinary inference activation unrelated to standalone compaction.
    Inference,
    /// Inference owed by one successful standalone compaction.
    Standalone {
        /// Successful transaction id.
        id: tau_proto::CompactionTransactionId,
    },
}

impl InferenceCheckpointOwner {
    /// Returns standalone transaction ownership, if this checkpoint has it.
    pub(crate) fn transaction_id(&self) -> Option<&tau_proto::CompactionTransactionId> {
        match self {
            Self::Inference => None,
            Self::Standalone { id } => Some(id),
        }
    }
}

impl ActivationDispatchState {
    /// Returns the durable recovery details when inference is blocked.
    pub(crate) fn blocked_recovery(
        &self,
    ) -> Option<(
        &tau_proto::CompactionTransactionId,
        tau_proto::AgentHead,
        Option<tau_proto::AgentHead>,
    )> {
        match self {
            Self::Blocked {
                failed_id,
                cut,
                resume_through,
            } => Some((failed_id, *cut, *resume_through)),
            Self::None
            | Self::Running { .. }
            | Self::AwaitingCheckpoint { .. }
            | Self::DispatchUncertain { .. }
            | Self::ContextRecoveryPending { .. }
            | Self::ContextRecoveryClaimPending { .. } => None,
        }
    }
}

/// Typed source of one committed message activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PendingMessageWakeSource {
    /// Extension-published message fact activation.
    MessageFact {
        /// Sequence of the canonical raw fact in the owning agent log.
        durable_event_seq: tau_core::PersistedAgentEventSeq,
    },
}

/// One committed message activation and its transcript placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingMessageWake {
    /// Typed durable source without cross-domain synthetic identifiers.
    pub(crate) source: PendingMessageWakeSource,
    /// Transcript node once tool-round adjacency permits materialization.
    pub(crate) node_id: Option<NodeId>,
}

/// Per-agent outer-turn state from activating input through terminal response.
///
/// `AgentThinking` represents an inner model round and `ToolsRunning` an
/// intervening tool round. There is no global execution slot — each loaded
/// agent tracks whether its next prompt can be dispatched.
#[derive(Clone, Debug, Default)]
pub(crate) enum AgentTurnState {
    #[default]
    Idle,
    AgentThinking {
        #[allow(dead_code)]
        agent_prompt_id: AgentPromptId,
    },
    ToolsRunning {
        remaining_calls: Vec<ToolCallId>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingCancel {
    pub(crate) reason: String,
}

/// One loaded agent tracked by the harness.
///
/// The user's main interactive agent is always present while the harness runs.
/// Additional agents may be loaded for extension side work, delegated tasks, or
/// compaction flows, and can later be removed from live runtime state while
/// leaving their semantic transcripts intact (durable by default, memory-only
/// for ephemeral agents).
#[derive(Debug)]
pub(crate) struct Agent {
    /// Owning agent id. Duplicates the key in the harness's agent map, but
    /// pinning it on the struct itself
    /// lets future code carry a `&Agent` without also threading
    /// the id through every call site.
    #[allow(dead_code)]
    pub(crate) id: AgentId,
    pub(crate) session_id: SessionId,
    pub(crate) originator: PromptOriginator,
    /// Local cursor — where the *next* transcript event for this agent
    /// should be parented in the owning agent tree. The tree's own `head`
    /// is whichever loaded agent appended last; this field is what
    /// `publish_for_agent` snaps the tree head back to before
    /// emitting an event for this agent.
    pub(crate) head: Option<NodeId>,
    /// Increments on every explicit head move to invalidate asynchronous work.
    pub(crate) branch_generation: u64,
    /// For [`PromptOriginator::Extension`] agents: the
    /// connection id of the extension that issued the
    /// [`tau_proto::StartAgentRequest`], so the harness knows where to
    /// route the matching [`tau_proto::StartAgentResult`].
    pub(crate) source_connection: Option<ConnectionId>,
    /// Agent prompt id of the prompt currently in flight for this agent, or
    /// `None` if nothing is pending.
    pub(crate) in_flight_prompt: Option<AgentPromptId>,
    /// Per-agent prompt queue: prompts waiting to be dispatched once this
    /// agent's `turn_state` returns to `Idle`. Other loaded agents dispatch
    /// independently; the provider extension
    /// serializes its own consumption of `AgentPromptCreated`.
    pub(crate) pending_prompts: VecDeque<PendingPrompt>,
    /// Canonical incoming facts waiting to activate one coalesced agent turn.
    pub(crate) pending_message_wakes: VecDeque<PendingMessageWake>,
    /// Replay found a committed activation after the latest completed dispatch.
    pub(crate) pending_replay_activation: bool,
    /// Whether terminal teardown has begun and new work must be rejected.
    pub(crate) terminating: bool,
    /// Durable standalone-compaction runtime projection.
    pub(crate) activation_dispatch: ActivationDispatchState,
    /// Pending user/control-plane request to stop this conversation at
    /// the next stable turn boundary. Stored like queued prompts so
    /// races between provider responses and UI cancel events are
    /// resolved by the conversation state machine instead of by the UI
    /// boundary.
    pub(crate) pending_cancel: Option<PendingCancel>,
    /// Most recent materialized prompt emitted for this conversation.
    /// The next prompt can reference its message prefix instead of
    /// repeating the full conversation history.
    pub(crate) last_prompt_id: Option<AgentPromptId>,
    /// Next per-agent index used when minting an [`AgentPromptId`] for this
    /// conversation. Initialized from the known agent event stream when the
    /// agent is loaded, then incremented for each materialized provider prompt.
    pub(crate) next_prompt_index: u64,
    /// Whether [`Self::next_prompt_index`] has been initialized from the loaded
    /// agent state for this harness run.
    pub(crate) prompt_index_initialized: bool,
    /// Correlation tag carried in by a [`tau_proto::UiPromptSubmitted`]
    /// and copied onto the next [`tau_proto::AgentPromptCreated`] this
    /// conversation emits. Cleared once consumed. Queued prompt submissions
    /// should carry their own [`PendingPrompt::ctx_id`] and copy it here only
    /// when that exact prompt is dispatched.
    pub(crate) next_ctx_id: Option<String>,
    pub(crate) turn_state: AgentTurnState,
    /// Last externally published runtime state, independent of internal
    /// continuation bookkeeping that may temporarily use `Idle`.
    pub(crate) published_runtime_state: tau_proto::AgentRuntimeState,
    /// Runtime-scoped outer agent-turn generation used by watch state
    /// notifications.
    pub(crate) turn_generation: u64,
    /// Whether the current turn was caused only by lifecycle notifications.
    pub(crate) lifecycle_notification_only_turn: bool,
    /// Watch subscription ids present when a notification-only turn suppressed
    /// its start and therefore eligible for a delayed start on promotion.
    pub(crate) suppressed_start_subscriptions: Vec<String>,
    /// For side agents spawned by a tool-implementing extension
    /// (currently just `agent_start`): the parent agent's tool call id
    /// that this conversation is fulfilling. Kept for teardown/routing of
    /// tool-backed side agents. `None` for user agents and for non-tool
    /// ext-queries (e.g. notifications' idle summary).
    pub(crate) parent_tool_call_id: Option<ToolCallId>,
    /// Direct parent agent resolved when this side agent is
    /// spawned. Kept alongside `parent_tool_call_id` because the tool-call
    /// routing map can be cleared before teardown needs to hand background
    /// completions back to the parent.
    pub(crate) parent_agent_id: Option<AgentId>,
    /// Human-friendly name shown in UIs. Falls back to the stable agent id.
    pub(crate) display_name: Option<String>,
    /// Display name supplied by the parent agent for the delegated
    /// task, surfaced in the UI alongside `parent_tool_call_id`. Only
    /// set when `parent_tool_call_id` is.
    pub(crate) task_name: Option<String>,
    /// Line and byte stats for the user-provided delegate prompt.
    /// Excludes any hidden prefix added by the delegate extension.
    pub(crate) delegate_input_stats: ToolUseStats,
    /// Agent role used for this conversation. `None` means the conversation
    /// follows the harness's globally selected interactive role.
    pub(crate) role: Option<String>,
    /// Model explicitly selected for this conversation. When set, future
    /// prompts for this loaded agent use it instead of resolving the model
    /// from the role.
    pub(crate) model_override: Option<ModelId>,
    /// Stable id assigned when this conversation first starts an agent turn.
    pub(crate) agent_id: Option<String>,
    /// Whether this agent's semantic transcript is durable or memory-only.
    pub(crate) persistence: AgentPersistenceMode,
    /// Durable semantic lifecycle marker for a peer-created entrypoint
    /// endpoint.
    pub(crate) peer_entrypoint_endpoint: bool,
    /// Number of tool calls currently in flight on this conversation.
    pub(crate) tools_in_flight: u32,
    /// Cumulative tool calls this conversation has started (in-flight
    /// + completed). Used in generic agent stats snapshots.
    pub(crate) tools_total: u32,
    /// Most recent input-token count this agent's agent
    /// reported on a finished response. Used for generic agent stats snapshots.
    pub(crate) context_input_tokens: Option<u64>,
    /// Transcript head represented by `context_input_tokens`.
    pub(crate) context_usage_head: Option<NodeId>,
    /// Provider-qualified model that produced `context_input_tokens`.
    pub(crate) context_usage_model: Option<ModelId>,
    /// Most recent cached input-token count this agent's provider reported on
    /// a finished response.
    pub(crate) context_cached_tokens: Option<u64>,
    /// Most recent percent-of-context-window this conversation's
    /// agent has used. Computed from `context_input_tokens` and the
    /// model's window size; `None` when the window is unknown.
    pub(crate) context_percent_used: Option<u8>,
    /// Named context-size alerts already emitted for the current usage climb.
    /// An alert becomes eligible again after usage falls back to or below its
    /// threshold or context accounting is reset.
    pub(crate) fired_context_size_alerts: HashSet<String>,

    /// Per-conversation map from tool-result-content hash to the first
    /// `call_id` on this branch that produced that content. Consulted
    /// at intake of every `ToolResult` / `ToolError` to collapse a
    /// duplicate's payload into a short pointer that refers back to
    /// the original. Branch-scoped: rebuilt from
    /// [`Agent::head`] whenever the cursor moves
    /// non-linearly. See `crate::dedup` for the full rationale.
    pub(crate) result_dedup: ResultDedupMap,
    /// Runtime-only conservative loop guard state for this agent branch.
    pub(crate) loop_guard: LoopGuardState,
}

/// Where a queued prompt came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PendingPromptSource {
    /// A normal user or harness steering prompt.
    General,
    /// A user-style prompt whose watchers should be notified when it becomes
    /// part of the watched agent's active turn.
    WatchNotifiedUser,
    /// A prompt created from an `agent.message_received` delivery.
    AgentMessageReceived,
    /// A lifecycle/provider watch notification; isolated to prevent
    /// watch-derived activity from cascading along chains.
    WatchNotification,
    /// Internal loop-guard pivot prompt.
    LoopGuard,
    /// Advisory prompt created by a named context-size alert.
    ContextSizeAlert,
    /// An activating notice for an unsuppressed background completion.
    ActivatingBackgroundCompletion,
    /// A passive background-completion notice that should be folded into the
    /// next real user prompt, but must not make an idle agent runnable by
    /// itself.
    PassiveBackgroundCompletion,
    /// A harness-authored restore notice that waits for a separate activation.
    PassiveRestoreNotice,
}

/// A queued prompt plus its user/internal classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingPrompt {
    /// Prompt text to fold into the conversation.
    pub(crate) text: String,
    /// Whether this queued prompt is visible user text or hidden internal text.
    pub(crate) message_class: PromptMessageClass,
    /// Source marker for lifecycle decisions that must not confuse internal
    /// prompts.
    pub(crate) source: PendingPromptSource,
    /// Harness-stamped provenance carried into the durable prompt fact.
    pub(crate) submission_source: tau_proto::PromptSubmissionSource,
    /// Optional caller correlation id carried with this exact prompt.
    pub(crate) ctx_id: Option<String>,
    /// Original peer body bytes retained while this prompt awaits dispatch.
    ///
    /// This is present only for bounded peer-entrypoint inputs. Keeping the
    /// admission weight on the queued prompt makes cancellation and queue
    /// clearing release capacity without a second shadow queue.
    pub(crate) peer_admission_bytes: Option<usize>,
}

impl From<String> for PendingPrompt {
    fn from(text: String) -> Self {
        Self::user(text)
    }
}

impl PartialEq<str> for PendingPrompt {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for PendingPrompt {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PendingPrompt {
    /// Create a user-visible queued prompt.
    pub(crate) fn user(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::User,
            source: PendingPromptSource::General,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            peer_admission_bytes: None,
        }
    }

    /// Create a prompt accepted from the authenticated interactive UI path.
    pub(crate) fn human_ui(text: String) -> Self {
        let mut prompt = Self::user(text);
        prompt.submission_source = tau_proto::PromptSubmissionSource::HumanUi;
        prompt
    }

    /// Create a watcher-notifying prompt from the interactive UI path.
    pub(crate) fn human_ui_watch_notified(text: String) -> Self {
        let mut prompt = Self::user(text);
        prompt.source = PendingPromptSource::WatchNotifiedUser;
        prompt.submission_source = tau_proto::PromptSubmissionSource::HumanUi;
        prompt
    }

    /// Create a hidden internal queued prompt.
    pub(crate) fn internal(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::General,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            peer_admission_bytes: None,
        }
    }

    /// Create a hidden advisory prompt from a named context-size alert.
    pub(crate) fn context_size_alert(text: String) -> Self {
        let mut prompt = Self::internal(text);
        prompt.source = PendingPromptSource::ContextSizeAlert;
        prompt
    }

    /// Create a hidden watch notification prompt.
    pub(crate) fn watch_notification(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::WatchNotification,
            ctx_id: None,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            peer_admission_bytes: None,
        }
    }

    /// Create a hidden queued prompt from an `agent.message_received` delivery.
    pub(crate) fn agent_message_received(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::AgentMessageReceived,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            peer_admission_bytes: None,
        }
    }

    /// Retain original peer body bytes while this prompt awaits dispatch.
    pub(crate) fn with_peer_admission_bytes(mut self, bytes: Option<usize>) -> Self {
        self.peer_admission_bytes = bytes;
        self
    }

    /// Create an internal loop-guard pivot prompt.
    pub(crate) fn loop_guard(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::LoopGuard,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            peer_admission_bytes: None,
        }
    }

    /// Create a hidden background-completion notice that waits for the next
    /// user-driven continuation instead of starting a standalone agent turn.
    pub(crate) fn passive_background_completion(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::PassiveBackgroundCompletion,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            peer_admission_bytes: None,
        }
    }

    /// Create an inference-activating background-completion notice.
    pub(crate) fn activating_background_completion(text: String) -> Self {
        let mut prompt = Self::internal(text);
        prompt.source = PendingPromptSource::ActivatingBackgroundCompletion;
        prompt
    }

    /// Create a hidden restore notice that waits for a separate activation.
    pub(crate) fn passive_restore_notice(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::PassiveRestoreNotice,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            peer_admission_bytes: None,
        }
    }

    /// Attach a caller correlation id to this exact queued prompt.
    pub(crate) fn with_ctx_id(mut self, ctx_id: Option<String>) -> Self {
        self.ctx_id = ctx_id;
        self
    }

    /// Whether this prompt should be hidden from user-facing UI and metadata.
    #[must_use]
    pub(crate) fn is_internal(&self) -> bool {
        self.message_class.is_internal()
    }

    /// Whether this prompt came from an `agent.message_received` delivery.
    #[must_use]
    pub(crate) fn is_agent_message_received(&self) -> bool {
        matches!(
            self.source,
            PendingPromptSource::AgentMessageReceived | PendingPromptSource::WatchNotification
        )
    }

    /// Whether this prompt is a harness-authored watch notification.
    #[must_use]
    pub(crate) fn is_watch_notification(&self) -> bool {
        self.source == PendingPromptSource::WatchNotification
    }

    /// Whether this user prompt should produce a watcher context notification.
    #[must_use]
    pub(crate) fn should_notify_watchers(&self) -> bool {
        self.source == PendingPromptSource::WatchNotifiedUser
    }

    /// Whether this queued prompt is a loop-guard pivot.
    #[must_use]
    pub(crate) fn is_loop_guard(&self) -> bool {
        self.source == PendingPromptSource::LoopGuard
    }

    /// Whether this queued prompt came from a named context-size alert.
    #[must_use]
    pub(crate) fn is_context_size_alert(&self) -> bool {
        self.source == PendingPromptSource::ContextSizeAlert
    }

    /// Whether this prompt is a passive background-completion notice.
    #[must_use]
    pub(crate) fn is_passive_background_completion(&self) -> bool {
        self.source == PendingPromptSource::PassiveBackgroundCompletion
    }

    /// Whether this prompt announces an unsuppressed background completion.
    #[must_use]
    pub(crate) fn is_activating_background_completion(&self) -> bool {
        self.source == PendingPromptSource::ActivatingBackgroundCompletion
    }

    /// Whether committing this prompt creates checkpoint-governed inference.
    #[must_use]
    pub(crate) fn creates_inference_activation(&self) -> bool {
        !matches!(
            self.source,
            PendingPromptSource::PassiveBackgroundCompletion
                | PendingPromptSource::PassiveRestoreNotice
        )
    }
}

impl Agent {
    /// Return the immutable transcript head selected by current dispatch
    /// ownership.
    pub(crate) fn selected_prompt_context_head(&self) -> Option<NodeId> {
        match &self.activation_dispatch {
            ActivationDispatchState::Running { cut, .. } => cut.as_option(),
            ActivationDispatchState::DispatchUncertain { through, .. } => through.as_option(),
            _ => self.head,
        }
    }

    pub(crate) fn new(
        id: AgentId,
        session_id: SessionId,
        originator: PromptOriginator,
        head: Option<NodeId>,
        source_connection: Option<ConnectionId>,
    ) -> Self {
        Self {
            id,
            session_id,
            originator,
            head,
            branch_generation: 0,
            source_connection,
            in_flight_prompt: None,
            pending_prompts: VecDeque::new(),
            pending_message_wakes: VecDeque::new(),
            pending_replay_activation: false,
            terminating: false,
            activation_dispatch: ActivationDispatchState::None,
            pending_cancel: None,
            last_prompt_id: None,
            next_prompt_index: 0,
            prompt_index_initialized: false,
            next_ctx_id: None,
            turn_state: AgentTurnState::Idle,
            published_runtime_state: tau_proto::AgentRuntimeState::Idle,
            turn_generation: 0,
            lifecycle_notification_only_turn: false,
            suppressed_start_subscriptions: Vec::new(),
            parent_tool_call_id: None,
            parent_agent_id: None,
            display_name: None,
            task_name: None,
            delegate_input_stats: ToolUseStats::default(),
            role: None,
            model_override: None,
            agent_id: None,
            persistence: AgentPersistenceMode::Durable,
            peer_entrypoint_endpoint: false,
            tools_in_flight: 0,
            tools_total: 0,
            context_input_tokens: None,
            context_usage_head: None,
            context_usage_model: None,
            context_cached_tokens: None,
            context_percent_used: None,
            fired_context_size_alerts: HashSet::new(),
            result_dedup: ResultDedupMap::new(),
            loop_guard: LoopGuardState::default(),
        }
    }
}

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

mod dispatch_state;
mod execution_state;
mod identity_state;
mod loop_guard;
mod turn_runtime_state;
mod work_status;

use std::collections::{BTreeMap, HashSet, VecDeque};

pub(crate) use dispatch_state::AgentDispatchState;
pub(crate) use execution_state::AgentExecutionState;
pub(crate) use identity_state::AgentIdentityState;
pub(crate) use loop_guard::{LoopCycleState, LoopGuardState, LoopGuardTrigger, LoopTurnSignature};
use tau_core::{AgentPersistenceMode, NodeId};
use tau_proto::{
    AgentId, AgentPromptId, ConnectionId, ModelId, PromptMessageClass, PromptOriginator, SessionId,
    ToolCallId, ToolUseStats,
};
pub(crate) use turn_runtime_state::AgentTurnRuntimeState;
pub use work_status::WorkStatusReport;
pub(crate) use work_status::{
    CrossedWaitThresholds, FinalStatusChallenge, FinalStatusDecision, FinalStatusInput, WorkStatus,
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

/// Runtime plan whose exact steer has not committed yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputLengthContinuationPlan {
    /// Reserved provider prompt correlation.
    pub(crate) agent_prompt_id: AgentPromptId,
    /// Durable source and outer-turn correlation.
    pub(crate) owner: tau_proto::OutputLengthContinuationOwner,
    /// Provider model and immutable activation authority captured by the
    /// source.
    pub(crate) dispatch: InferenceDispatchOwnership,
}

/// Owner-ready runtime data after the exact continuation steer commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputLengthContinuationDispatch {
    /// Reserved prompt, source owner, and provider dispatch authority.
    pub(crate) plan: OutputLengthContinuationPlan,
    /// Exact committed steer cut that the successor owner must extend.
    pub(crate) through: tau_proto::AgentHead,
}

/// Runtime ownership of one ordinary outer turn and its durable finish.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum OuterTurnRuntimeState {
    /// No ordinary outer turn is open.
    #[default]
    None,
    /// The turn is open and may own same-turn continuation work.
    Active(tau_proto::AgentOuterTurnId),
    /// The exact settled finish is queued or intercepted.
    FinishInFlight(tau_proto::AgentOuterTurnId),
    /// The exact settled finish was append-rejected and may be retried.
    FinishRetry(tau_proto::AgentOuterTurnId),
}

impl OuterTurnRuntimeState {
    /// Returns the turn id only while new same-turn work remains legal.
    pub(crate) fn active_id(&self) -> Option<&tau_proto::AgentOuterTurnId> {
        match self {
            Self::Active(id) => Some(id),
            Self::None | Self::FinishInFlight(_) | Self::FinishRetry(_) => None,
        }
    }

    /// Returns the turn id in every open or finish-pending phase.
    pub(crate) fn owned_id(&self) -> Option<&tau_proto::AgentOuterTurnId> {
        match self {
            Self::Active(id) | Self::FinishInFlight(id) | Self::FinishRetry(id) => Some(id),
            Self::None => None,
        }
    }
}

/// Runtime projection of one output-length successor per reasoning-only run.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) enum OutputLengthContinuationState {
    /// This outer turn has not planned a continuation.
    #[default]
    None,
    /// The plan committed and its exact continuation steer remains owed.
    Planned(OutputLengthContinuationPlan),
    /// The steer committed and its reserved successor awaits dispatch
    /// ownership.
    OwnerReady(OutputLengthContinuationDispatch),
    /// The successor owner publication was claimed but has not committed.
    OwnerPending(OutputLengthContinuationDispatch),
    /// The successor owner committed and now permits one terminal response.
    Active(OutputLengthContinuationDispatch),
    /// The continuation ended while the outer turn remains open.
    Spent {
        /// Outer turn whose current reasoning-only run consumed its budget.
        outer_turn_id: tau_proto::AgentOuterTurnId,
    },
}

impl OutputLengthContinuationState {
    /// Atomically claim the pending successor for either dispatch path.
    pub(crate) fn claim_pending(&mut self) -> Option<OutputLengthContinuationDispatch> {
        let Self::OwnerReady(dispatch) = self else {
            return None;
        };
        let dispatch = dispatch.clone();
        *self = Self::OwnerPending(dispatch.clone());
        Some(dispatch)
    }

    /// Return the outer turn that has consumed its continuation budget.
    pub(crate) fn outer_turn_id(&self) -> Option<&tau_proto::AgentOuterTurnId> {
        match self {
            Self::None => None,
            Self::Planned(dispatch) => Some(&dispatch.owner.outer_turn_id),
            Self::OwnerReady(dispatch) | Self::OwnerPending(dispatch) | Self::Active(dispatch) => {
                Some(&dispatch.plan.owner.outer_turn_id)
            }
            Self::Spent { outer_turn_id } => Some(outer_turn_id),
        }
    }

    /// Whether a committed continuation owner pins this prompt to this model.
    pub(crate) fn owns_prompt_model(
        &self,
        agent_prompt_id: &AgentPromptId,
        model: &ModelId,
    ) -> bool {
        matches!(
            self,
            Self::Active(dispatch)
                if &dispatch.plan.agent_prompt_id == agent_prompt_id
                    && &dispatch.plan.dispatch.model == model
        )
    }
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

/// Typed source of one committed message activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PendingMessageWakeSource {
    /// Harness-owned inbound agent-message occurrence.
    AgentMessageReceived {
        /// Sequence of the canonical received fact in the owning agent log.
        durable_event_seq: tau_core::PersistedAgentEventSeq,
        /// Whether this input may create ordinary watcher lifecycle edges.
        activation_class: AgentMessageActivationClass,
        /// Peer body weight retained until checkpoint or lifecycle cleanup.
        peer_admission_bytes: Option<usize>,
    },
    /// Canonical external-message fact activation.
    MessageFact {
        /// Sequence of the canonical raw fact in the owning agent log.
        durable_event_seq: tau_core::PersistedAgentEventSeq,
    },
}

impl PendingMessageWakeSource {
    /// Returns the canonical owning-journal sequence for this wake.
    pub(crate) fn durable_event_seq(&self) -> tau_core::PersistedAgentEventSeq {
        match self {
            Self::AgentMessageReceived {
                durable_event_seq, ..
            }
            | Self::MessageFact { durable_event_seq } => *durable_event_seq,
        }
    }

    /// Returns the lifecycle class used when coalescing selected-branch wakes.
    pub(crate) fn activation_class(&self) -> AgentMessageActivationClass {
        match self {
            Self::AgentMessageReceived {
                activation_class, ..
            } => *activation_class,
            Self::MessageFact { .. } => AgentMessageActivationClass::OrdinaryAgentInput,
        }
    }

    /// Returns peer admission bytes retained by this wake, if any.
    pub(crate) fn peer_admission_bytes(&self) -> Option<usize> {
        match self {
            Self::AgentMessageReceived {
                peer_admission_bytes,
                ..
            } => *peer_admission_bytes,
            Self::MessageFact { .. } => None,
        }
    }
}

/// Runtime lifecycle class for one activating received agent-message fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentMessageActivationClass {
    /// Ordinary agent input that can participate in normal lifecycle fanout.
    OrdinaryAgentInput,
    /// Isolated watch input that must not cascade watch lifecycle edges.
    IsolatedWatchNotification,
}

/// One committed message activation and its transcript placement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingMessageWake {
    /// Typed durable source without cross-domain synthetic identifiers.
    pub(crate) source: PendingMessageWakeSource,
    /// Transcript node once tool-round adjacency permits materialization.
    pub(crate) node_id: Option<NodeId>,
    /// Exact activation observation allocated when this wake entered the queue.
    pub(crate) activation_observation: Option<tau_proto::ObservationId>,
    /// Canonical durable message occurrence that triggered this wake.
    pub(crate) source_observation: Option<tau_proto::ObservationId>,
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
    /// UI that initiated this cancellation.
    pub(crate) requester_client_id: tau_proto::ConnectionId,
    /// Exact prompt whose canonical terminal this cancellation may claim, or
    /// `None` while cancellation owns only a non-provider tool phase.
    pub(crate) agent_prompt_id: Option<tau_proto::AgentPromptId>,
    /// Canonical cancellation reason.
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
    /// Load-scoped transcript ownership, routing, and conversation
    /// configuration.
    pub(crate) identity: AgentIdentityState,
    /// Queued work and provider-prompt dispatch bookkeeping.
    pub(crate) dispatch: AgentDispatchState,
    /// State owned by the currently active outer turn.
    pub(crate) turn: AgentTurnRuntimeState,
    /// Per-branch execution counters, context telemetry, and guards.
    pub(crate) execution: AgentExecutionState,
}

/// Where a queued prompt came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PendingPromptSource {
    /// A normal user or harness steering prompt.
    General,
    /// A user-style prompt whose watchers should be notified when it becomes
    /// part of the watched agent's active turn.
    WatchNotifiedUser,
    /// Internal loop-guard pivot prompt.
    LoopGuard,
    /// Advisory prompt created by a named context-size alert.
    ContextSizeAlert,
    /// Internal prompt emitted by a configured timer extension.
    Timer,
    /// An activating notice for an unsuppressed background completion.
    ActivatingBackgroundCompletion,
    /// A passive background-completion notice that should be folded into the
    /// next real user prompt, but must not make an idle agent runnable by
    /// itself.
    PassiveBackgroundCompletion,
    /// A harness-authored restore notice that waits for a separate activation.
    PassiveRestoreNotice,
    /// Harness-owned continuation after replay-safe reasoning reached its cap.
    OutputLengthContinuation,
}

/// A queued prompt plus its user/internal classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingPrompt {
    /// Prompt text to fold into the conversation.
    pub(crate) text: String,
    /// Harness-authenticated byte spans in [`Self::text`].
    pub(crate) trusted_internal_spans: Vec<tau_proto::TrustedInternalSpan>,
    /// Whether this queued prompt is user text or internal model context.
    pub(crate) message_class: PromptMessageClass,
    /// Source marker for lifecycle decisions that must not confuse internal
    /// prompts.
    pub(crate) source: PendingPromptSource,
    /// Harness-stamped provenance carried into the durable prompt fact.
    pub(crate) submission_source: tau_proto::PromptSubmissionSource,
    /// Optional caller correlation id carried with this exact prompt.
    pub(crate) ctx_id: Option<String>,
    /// Resolve a non-literal user skill command against the target agent's
    /// frozen discovery snapshot immediately before durable submission.
    pub(crate) expand_user_skill_on_dispatch: bool,
    /// Exact activation observation allocated when this prompt entered the
    /// queue.
    pub(crate) activation_observation: Option<tau_proto::ObservationId>,
    /// Correlation retained until this accepted initial prompt materializes.
    pub(crate) initial_prompt_correlation: Option<InitialPromptCorrelation>,
    /// Typed durable authority when this prompt delivers self compaction.
    pub(crate) self_compaction_terminal: Option<tau_proto::SelfCompactionTerminal>,
}

/// Correlation for an accepted initial prompt before provider materialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InitialPromptCorrelation {
    /// Create request that introduced the prompt.
    pub(crate) request_id: String,
    /// Created agent that owns the prompt.
    pub(crate) agent_id: tau_proto::AgentId,
    /// Prompt-chain correlation copied from the create request.
    pub(crate) ctx_id: String,
    /// Exact committed activation watermark owned by this prompt.
    ///
    /// `None` until the initial prompt submission commits.
    pub(crate) activation_through: Option<tau_proto::AgentHead>,
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
            trusted_internal_spans: Vec::new(),
            message_class: PromptMessageClass::User,
            source: PendingPromptSource::General,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            expand_user_skill_on_dispatch: false,
            activation_observation: None,
            initial_prompt_correlation: None,
            self_compaction_terminal: None,
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
        let end = u32::try_from(text.len()).expect("prompt length fits u32");
        Self {
            text,
            trusted_internal_spans: vec![tau_proto::TrustedInternalSpan { start: 0, end }],
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::General,
            submission_source: tau_proto::PromptSubmissionSource::HarnessInternal,
            ctx_id: None,
            expand_user_skill_on_dispatch: false,
            activation_observation: None,
            initial_prompt_correlation: None,
            self_compaction_terminal: None,
        }
    }

    /// Create an internal-class prompt whose bytes came from an authenticated
    /// UI or configured extension. Its class affects lifecycle only; it has no
    /// provider-presentation authority.
    pub(crate) fn untrusted_internal(text: String) -> Self {
        let mut prompt = Self::user(text);
        prompt.message_class = PromptMessageClass::Internal;
        prompt
    }

    /// Create an internal advisory prompt from a named context-size alert.
    pub(crate) fn context_size_alert(text: String) -> Self {
        let mut prompt = Self::internal(text);
        prompt.source = PendingPromptSource::ContextSizeAlert;
        prompt
    }

    /// Create an internal loop-guard pivot prompt.
    pub(crate) fn loop_guard(text: String) -> Self {
        let mut prompt = Self::internal(text);
        prompt.source = PendingPromptSource::LoopGuard;
        prompt
    }

    /// Create a hidden background-completion notice that waits for the next
    /// user-driven continuation instead of starting a standalone agent turn.
    pub(crate) fn passive_background_completion(text: String) -> Self {
        let mut prompt = Self::internal(text);
        prompt.source = PendingPromptSource::PassiveBackgroundCompletion;
        prompt
    }

    /// Create an inference-activating background-completion notice.
    pub(crate) fn activating_background_completion(text: String) -> Self {
        let mut prompt = Self::internal(text);
        prompt.source = PendingPromptSource::ActivatingBackgroundCompletion;
        prompt
    }

    /// Create a hidden restore notice that waits for a separate activation.
    pub(crate) fn passive_restore_notice(text: String) -> Self {
        let mut prompt = Self::internal(text);
        prompt.source = PendingPromptSource::PassiveRestoreNotice;
        prompt
    }

    /// Create the exact inference-activating output-length instruction.
    pub(crate) fn output_length_continuation() -> Self {
        let mut prompt =
            Self::internal(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION.to_owned());
        prompt.source = PendingPromptSource::OutputLengthContinuation;
        prompt
    }

    /// Return whether this prompt is the reserved durable length successor
    /// steer.
    pub(crate) fn is_output_length_continuation(&self) -> bool {
        self.source == PendingPromptSource::OutputLengthContinuation
    }

    /// Attach a caller correlation id to this exact queued prompt.
    pub(crate) fn with_ctx_id(mut self, ctx_id: Option<String>) -> Self {
        self.ctx_id = ctx_id;
        self
    }

    /// Attach the typed one-shot self-compaction delivery carried by this
    /// internal prompt.
    pub(crate) fn with_self_compaction_terminal(
        mut self,
        terminal: tau_proto::SelfCompactionTerminal,
    ) -> Self {
        self.self_compaction_terminal = Some(terminal);
        self
    }

    /// Whether this prompt is excluded from user-prompt metadata.
    ///
    /// A separately typed internal prompt may still have a UI presentation.
    #[must_use]
    pub(crate) fn is_internal(&self) -> bool {
        self.message_class.is_internal()
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

    /// Whether this prompt directly delivers a terminal self-compaction
    /// outcome and therefore survives cancellation cleanup.
    pub(crate) fn is_self_compaction_terminal(&self) -> bool {
        self.self_compaction_terminal.is_some()
    }

    /// Durable internal-prompt subtype stamped when this prompt is delivered.
    #[must_use]
    pub(crate) fn internal_kind(&self) -> Option<tau_proto::InternalPromptKind> {
        match self.source {
            PendingPromptSource::ContextSizeAlert => {
                Some(tau_proto::InternalPromptKind::ContextSizeAlert)
            }
            // Self-compaction shares completion-driven scheduling but owns a
            // distinct diagnostic projection, not the generic tool notice.
            PendingPromptSource::ActivatingBackgroundCompletion
                if self.self_compaction_terminal.is_none() =>
            {
                Some(tau_proto::InternalPromptKind::BackgroundToolCompletion)
            }
            PendingPromptSource::PassiveBackgroundCompletion => {
                Some(tau_proto::InternalPromptKind::BackgroundToolCompletion)
            }
            PendingPromptSource::OutputLengthContinuation => {
                Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
            }
            PendingPromptSource::General
            | PendingPromptSource::WatchNotifiedUser
            | PendingPromptSource::LoopGuard
            | PendingPromptSource::Timer
            | PendingPromptSource::ActivatingBackgroundCompletion
            | PendingPromptSource::PassiveRestoreNotice => None,
        }
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

    /// Maps this accepted queue item to its content-free activation class.
    #[must_use]
    pub(crate) fn activation_kind(&self) -> tau_proto::ActivationKind {
        match self.source {
            PendingPromptSource::LoopGuard => tau_proto::ActivationKind::LoopGuard,
            PendingPromptSource::Timer => tau_proto::ActivationKind::Timer,
            PendingPromptSource::ActivatingBackgroundCompletion => {
                tau_proto::ActivationKind::BackgroundCompletion
            }
            PendingPromptSource::WatchNotifiedUser => tau_proto::ActivationKind::VisibleUser,
            PendingPromptSource::General
                if self.submission_source == tau_proto::PromptSubmissionSource::HumanUi =>
            {
                tau_proto::ActivationKind::VisibleUser
            }
            PendingPromptSource::General | PendingPromptSource::ContextSizeAlert => {
                tau_proto::ActivationKind::InternalPrompt
            }
            PendingPromptSource::OutputLengthContinuation => {
                tau_proto::ActivationKind::InternalPrompt
            }
            PendingPromptSource::PassiveBackgroundCompletion
            | PendingPromptSource::PassiveRestoreNotice => tau_proto::ActivationKind::Other,
        }
    }
}

impl Agent {
    /// Return the immutable transcript head selected by current dispatch
    /// ownership.
    pub(crate) fn selected_prompt_context_head(&self) -> Option<NodeId> {
        match &self.dispatch.activation_dispatch {
            ActivationDispatchState::Running { cut, .. } => cut.as_option(),
            ActivationDispatchState::DispatchUncertain { through, .. } => through.as_option(),
            _ => self.identity.head,
        }
    }

    pub(crate) fn new(
        id: AgentId,
        runtime_incarnation: u64,
        session_id: SessionId,
        originator: PromptOriginator,
        head: Option<NodeId>,
        source_connection: Option<ConnectionId>,
    ) -> Self {
        Self {
            identity: AgentIdentityState {
                id,
                runtime_incarnation,
                session_id,
                originator,
                head,
                branch_generation: 0,
                source_connection,
                parent_tool_call_id: None,
                parent_agent_id: None,
                restored_tool_backed_start: false,
                display_name: None,
                task_name: None,
                delegate_input_stats: ToolUseStats::default(),
                role: None,
                model_override: None,
                agent_id: None,
                persistence: AgentPersistenceMode::Durable,
                peer_entrypoint_endpoint: false,
            },
            dispatch: AgentDispatchState {
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
            },
            turn: AgentTurnRuntimeState {
                turn_state: AgentTurnState::Idle,
                published_runtime_state: tau_proto::AgentRuntimeState::Idle,
                turn_generation: tau_proto::AgentOuterTurnGeneration::initial(),
                work_status: WorkStatus::default(),
                terminal_status_was_available: false,
                terminal_notice_eligible: false,
                terminal_notice_outer_turn_id: None,
                terminal_context_size_alerts: BTreeMap::new(),
                automatic_compaction: Default::default(),
                outer_turn: OuterTurnRuntimeState::None,
                output_length_continuation: OutputLengthContinuationState::None,
                lifecycle_notification_only_turn: false,
            },
            execution: AgentExecutionState {
                tools_in_flight: 0,
                tools_total: 0,
                context_input_tokens: None,
                context_usage_head: None,
                context_usage_model: None,
                context_usage_prompt_id: None,
                context_cached_tokens: None,
                context_percent_used: None,
                fired_context_size_alerts: HashSet::new(),
                result_dedup: ResultDedupMap::new(),
                loop_guard: LoopGuardState::default(),
            },
        }
    }
}

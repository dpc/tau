//! Tree-structured agent transcript types and the persisted-event
//! record they are derived from.
//!
//! The on-disk source of truth is the per-agent protocol-event log
//! ([`PersistedAgentEvent`] / `events.cbor`); the in-memory
//! [`AgentTree`] is built from it via [`AgentTree::from_events`]
//! and kept in sync incrementally by [`AgentTree::apply_event`]. No
//! other API mutates the tree, so the on-disk log and the cached
//! view cannot drift.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io as path_std_io;
use std::num::NonZeroU64;

#[cfg(test)]
thread_local! {
    static BRANCH_PATH_MATERIALIZATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

use serde::{Deserialize, Serialize};
use tau_proto::{
    AgentHead, AgentHeadMoved, AgentId, AgentMessageId, AgentMessageKind, AgentMessageReceived,
    AgentMessageRecipient, AgentMessageSent, ConnectionId, ContentPart, ContextItem, ContextRole,
    Event, ExtensionName, MessageItem, PromptOriginator, ProviderBackend, ProviderTokenUsage,
    ToolBackgroundError, ToolBackgroundResult, ToolCallId, ToolCallItem, ToolName, ToolResultItem,
    ToolResultKind, ToolResultStatus, ToolType, UnixMicros,
};

const MAX_RETAINED_PROVIDER_IMAGE_BYTES_PER_AGENT: u64 = 128 * 1024 * 1024;

/// Process-local generation of durable ordinary-inference prompt
/// materialization.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct InferenceGeneration(u64);

impl InferenceGeneration {
    /// Advances the generation with the existing saturating overflow behavior.
    #[must_use]
    const fn saturating_next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Projects this authority into the durable prompt-generation domain.
    #[must_use]
    const fn materialized_prompt_generation(self) -> tau_proto::MaterializedPromptGeneration {
        tau_proto::MaterializedPromptGeneration::from_inference_generation(self.0)
    }
}

/// Process-local authority generation for selected-head async completions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HeadMoveGeneration(u64);

impl HeadMoveGeneration {
    /// Advances the generation with the existing saturating overflow behavior.
    #[must_use]
    const fn saturating_next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEventValidationError {
    message: String,
}

/// Reject malformed durable authority ranges before they can reach transcript
/// folding. Empty lists remain the compatibility-safe interpretation for old
/// event records.
fn check_trusted_internal_spans(
    text: &str,
    spans: &[tau_proto::TrustedInternalSpan],
) -> Result<(), AgentEventValidationError> {
    let mut offset = 0_usize;
    for span in spans {
        let start = span.start as usize;
        let end = span.end as usize;
        if start < offset
            || end < start
            || text.len() < end
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(AgentEventValidationError::new(
                "trusted internal prompt spans must be ordered UTF-8 byte ranges within prompt text",
            ));
        }
        offset = end;
    }
    Ok(())
}

impl AgentEventValidationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AgentEventValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentEventValidationError {}

fn validate_optional_display_name(
    display_name: &Option<String>,
) -> Result<(), AgentEventValidationError> {
    if let Some(name) = display_name {
        validate_display_name(name)?;
    }
    Ok(())
}

fn validate_display_name(display_name: &str) -> Result<(), AgentEventValidationError> {
    if display_name.trim().is_empty() {
        return Err(AgentEventValidationError::new(
            "agent display name must not be empty",
        ));
    }
    Ok(())
}

/// Monotonic sequence number in one persisted agent event log.
///
/// This sequence is relative only to one agent's `events.cbor` stream: the
/// first record in that file has sequence 0, the second has sequence 1, and so
/// on. It is not comparable to the harness runtime event sequence or to
/// [`crate::PersistedSessionEventSeq`]. The value is persisted as corruption
/// detection metadata; replay semantics are still defined by file order, so
/// load code verifies that stored values match their implied position.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PersistedAgentEventSeq(u64);

impl PersistedAgentEventSeq {
    /// Creates a sequence value from its raw integer representation.
    #[must_use]
    pub fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw integer representation.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Returns the next sequence in the same agent event log.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for PersistedAgentEventSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Direction of a cross-agent message projection in one agent transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageDirection {
    /// The current agent sent the message.
    Outbound,
    /// The current agent received the message.
    Inbound,
}

/// One persisted chat, tool, or cross-agent-message entry belonging to an
/// agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentEntry {
    /// User-style model input recorded in the transcript.
    UserInput {
        /// Context items appended by the user or harness. Prompt-derived
        /// entries contain exactly one user-role message with one text
        /// part; compaction replacement windows use their separate
        /// entry variant.
        items: Vec<ContextItem>,
        /// Typed accepted-prompt provenance, absent for synthetic injections.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submission_source: Option<tau_proto::PromptSubmissionSource>,
        /// Whether this node creates checkpoint-governed inference work.
        #[serde(default)]
        inference_activation: bool,
    },
    /// Assistant output accepted from a provider.
    AssistantResponse {
        /// Provider-native response id, when available.
        provider_response_id: Option<String>,
        /// Backend that produced the response, when known.
        backend: Option<ProviderBackend>,
        /// Provider output items folded into the transcript.
        output_items: Vec<ContextItem>,
        /// Provider token usage for the response, when available.
        usage: Option<ProviderTokenUsage>,
    },
    /// Terminal provider-visible results for a tool round.
    ToolResults {
        /// Results ordered by the model's original tool-call order.
        items: Vec<ToolResultItem>,
    },
    /// Cross-agent message projection stored in this agent's transcript.
    AgentMessage {
        /// Sequence of the canonical directional fact in the owning journal.
        durable_event_seq: PersistedAgentEventSeq,
        /// Stable logical message id shared by sender and recipient
        /// projections.
        message_id: AgentMessageId,
        /// Whether this projection is outbound or inbound for this agent.
        direction: AgentMessageDirection,
        /// Sender agent id.
        sender_id: AgentId,
        /// Active session id for an external sender, if this inbound message
        /// originated in another harness session.
        sender_session_id: Option<tau_proto::SessionId>,
        /// Recipient agent.
        recipient: AgentMessageRecipient,
        /// Delivery source semantics.
        kind: AgentMessageKind,
        /// Typed provider status for receiver-only watch projections.
        watch_provider_status: Option<tau_proto::AgentWatchProviderStatusNotification>,
        /// Typed self-reported work status for receiver-only watch projections.
        watch_work_status: Option<Box<tau_proto::AgentWatchWorkStatusNotification>>,
        /// Typed long-wait threshold for receiver-only watch projections.
        watch_long_wait: Option<Box<tau_proto::AgentWatchLongWaitNotification>>,
        /// Structured watched-agent lifecycle terminal.
        watch_lifecycle: Option<Box<tau_proto::AgentWatchLifecycleNotification>>,
        /// Message body.
        message: String,
    },
    /// Escaped ordinary context message derived from a committed message fact.
    MessageFact {
        /// Model-facing message with user/assistant role fixed by fact type.
        item: Box<tau_proto::MessageItem>,
        /// Sequence of the canonical raw fact in the owning agent journal.
        durable_event_seq: PersistedAgentEventSeq,
    },
    /// Standalone compaction boundary whose replacement window becomes the
    /// complete model-visible history.
    Compaction {
        /// Ordered provider items replacing all preceding prompt context.
        replacement_window: Vec<ContextItem>,
        /// Transaction correlation for new suffix-preserving boundaries.
        transaction_id: Option<tau_proto::CompactionTransactionId>,
        /// Immutable compact-input cut for new boundaries.
        cut: Option<tau_proto::AgentHead>,
        /// Last suffix node before the boundary for new boundaries.
        suffix_end: Option<tau_proto::AgentHead>,
    },
    /// Durable request for either legacy inline or standalone compaction.
    CompactionTrigger {
        /// Whether successful standalone compaction resumes an
        /// already-published inference turn.
        resume_inference: bool,
    },
}

/// One reconstructed provider-visible active window.
///
/// A suffix-preserving compaction boundary replaces a logical prefix even when
/// the preserved suffix precedes that boundary in physical journal ancestry.
/// Keeping the replacement and surviving transcript nodes separate lets
/// callers select later logical prefixes without treating physical ancestry as
/// provider-visible order.
#[derive(Clone, Debug)]
pub struct ActiveProviderWindow<'a> {
    /// Latest compacted replacement, when the active window has one.
    pub replacement: Option<&'a [ContextItem]>,
    /// Boundary node that owns `replacement`.
    pub replacement_boundary: Option<NodeId>,
    /// Surviving ordinary transcript nodes in provider-visible order.
    pub transcript: Vec<(NodeId, &'a AgentEntry)>,
}

/// Replay-derived logical position for one materialized transcript node.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ProviderWindowPosition {
    /// Previous surviving transcript node in provider-visible order.
    predecessor: ProviderWindowNodeId,
    /// Latest compaction boundary governing this node's logical window.
    replacement_boundary: ProviderWindowNodeId,
}

/// Replay-derived start anchor for one compacted replacement window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ProviderWindowBoundaryAnchor {
    /// First consumed transcript node immediately before the surviving suffix.
    start_exclusive: ProviderWindowNodeId,
}

/// Complete non-persisted provider-window query index rebuilt by replay.
#[derive(Clone, Debug, Default, PartialEq)]
struct ProviderWindowIndex {
    /// Logical position corresponding to every materialized transcript node.
    positions: Vec<ProviderWindowPosition>,
    /// Surviving-suffix start keyed by each replacement boundary node.
    boundary_anchors: HashMap<NodeId, ProviderWindowBoundaryAnchor>,
}

/// Complete non-persisted query indexes rebuilt by agent-journal replay.
#[derive(Clone, Debug, Default, PartialEq)]
struct AgentTreeIndexes {
    /// Materialized provider-context input nodes keyed by journal occurrence.
    context_nodes_by_event_seq: HashMap<PersistedAgentEventSeq, NodeId>,
    /// Logical active provider-window positions and boundary anchors.
    provider_window: ProviderWindowIndex,
}

/// Niche-optimized optional materialized node id for replay-only indexes.
///
/// The stored nonzero value is one greater than the node id. `u64::MAX` cannot
/// identify a materialized vector entry and therefore collapses to `None`, just
/// like every other dangling synthetic parent.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ProviderWindowNodeId(Option<NonZeroU64>);

impl ProviderWindowNodeId {
    /// Encode one optional materialized-node candidate in eight bytes.
    fn new(node_id: Option<NodeId>) -> Self {
        Self(node_id.and_then(|node_id| node_id.get().checked_add(1).and_then(NonZeroU64::new)))
    }

    /// Decode the optional materialized-node candidate.
    fn get(self) -> Option<NodeId> {
        self.0.map(|encoded| NodeId::new(encoded.get() - 1))
    }
}

/// One committed context input whose exact marked inference owns placement.
#[derive(Clone, Debug, PartialEq)]
struct PendingInferenceInput {
    /// Exact marked ordinary prompt that owns this occurrence.
    owner_prompt_id: tau_proto::AgentPromptId,
    /// Canonical journal occurrence order.
    durable_event_seq: PersistedAgentEventSeq,
    /// Real branch parent accepted before any virtual deferred tail.
    accepted_real_parent: Option<NodeId>,
    /// Earlier deferred occurrence on this branch, if any.
    virtual_predecessor_seq: Option<PersistedAgentEventSeq>,
    /// Exact typed entry to materialize once the barrier closes.
    entry: Box<AgentEntry>,
}

/// One committed context input waiting behind an open provider tool round.
#[derive(Clone, Debug, PartialEq)]
struct PendingToolContextInput {
    /// Canonical journal occurrence order.
    durable_event_seq: PersistedAgentEventSeq,
    /// Exact typed entry to materialize after the aggregate tool result.
    entry: Box<AgentEntry>,
}

impl From<PendingInferenceInput> for PendingToolContextInput {
    fn from(input: PendingInferenceInput) -> Self {
        Self {
            durable_event_seq: input.durable_event_seq,
            entry: input.entry,
        }
    }
}

/// The sole unfinished foreground provider tool round in one agent tree.
#[derive(Clone, Debug, Default, PartialEq)]
struct PendingToolRound {
    /// Tool-calling assistant node that owns the round and its result
    /// aggregate.
    assistant_node_id: NodeId,
    /// Provider call IDs in model-authored order.
    call_order: Vec<ToolCallId>,
    /// Terminal results received so far, keyed by their owning call ID.
    terminal_results: HashMap<ToolCallId, ToolResultItem>,
}

/// A synthetic provider placeholder that moved a tool call to the background.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundToolPlaceholder {
    /// Tool call id whose provider round was closed by the placeholder.
    pub call_id: ToolCallId,
    /// Model-visible tool name recorded on the placeholder.
    pub tool_name: ToolName,
    /// Tool type recorded on the placeholder.
    pub tool_type: ToolType,
    /// Prompt originator recorded on the placeholder.
    pub originator: PromptOriginator,
}

/// Durable completion, if any, for a backgrounded tool call.
#[derive(Clone, Debug, PartialEq)]
pub enum BackgroundToolCompletion {
    /// The backgrounded tool eventually returned successfully.
    Result(ToolBackgroundResult),
    /// The backgrounded tool eventually returned an error.
    Error(ToolBackgroundError),
}

/// Background state reconstructed from durable events for one tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundToolCallState {
    /// The placeholder that closed the provider-visible tool round.
    pub placeholder: BackgroundToolPlaceholder,
    /// The later real background completion, when one is present.
    pub completion: Option<BackgroundToolCompletion>,
    /// Exact provider declaration occurrence, when retained in the journal.
    pub call_ref: Option<tau_proto::ToolCallRef>,
    /// Canonical persisted completion occurrence, when complete.
    pub terminal_observation: Option<tau_proto::ObservationId>,
}

// `NodeId` lives on the wire (tree-folding events carry their own
// `parent_node_id`), so the canonical definition moved to
// `tau-proto`. Re-exported here for ergonomic backward compatibility
// with existing `tau_core::NodeId` consumers.
pub use tau_proto::NodeId;

/// Durable encoding of the explicit fold parent chosen for one agent event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "node_id")]
pub enum AgentEventParent {
    /// Inherit the agent tree's current head when folding this event.
    InheritHead,
    /// Fold this event at the transcript root with no parent node.
    Root,
    /// Fold this event under the given existing node.
    Under(NodeId),
}

impl AgentEventParent {
    /// Encode one exact transcript head as a persisted parent.
    #[must_use]
    pub const fn from_head(head: AgentHead) -> Self {
        match head {
            AgentHead::Root => Self::Root,
            AgentHead::Node(node_id) => Self::Under(node_id),
        }
    }

    #[must_use]
    pub const fn resolve(self, head: Option<NodeId>) -> Option<NodeId> {
        match self {
            Self::InheritHead => head,
            Self::Root => None,
            Self::Under(node_id) => Some(node_id),
        }
    }
}

/// One node in the agent tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentNode {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub entry: AgentEntry,
}

/// Tree-structured agent transcript with branching.
///
/// Each entry is a node with a unique id and parent pointer. The
/// `head` tracks the *write cursor* — where the next append will
/// land. Branching = moving the cursor back to an earlier node; the
/// next append creates a new branch off that node. There is only ever
/// one cursor; multiple "branch tips" are derived as the leaves of
/// the tree (see [`AgentTree::leaves`]).
///
/// The tree is never mutated through any imperative API on this type
/// from outside `tau-core`; it is built by folding the per-agent
/// durable event log via [`AgentTree::from_events`] /
/// [`AgentTree::apply_event`]. That keeps a single source of truth
/// (the event log on disk) and removes the possibility for the tree
/// and the events log to disagree.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentTree {
    pub(crate) agent_id: AgentId,
    pub(crate) metadata: BTreeMap<tau_proto::AgentMetadataKey, AgentMetadataEntry>,
    pub(crate) nodes: Vec<AgentNode>,
    /// Complete replay-derived in-memory query indexes.
    indexes: AgentTreeIndexes,
    pub(crate) head: Option<NodeId>,
    pub(crate) display_name: Option<String>,
    /// Latest durable initialization bootstrap and frozen discovery
    /// replacement.
    initialization_context: Option<tau_proto::AgentInitializationContextSet>,
    /// Sequence the next durable event appended to this agent's log
    /// should receive. Cached here so that
    /// [`AgentStore::append_agent_event_at`] doesn't have to
    /// re-decode the entire on-disk log on every write to look at
    /// the last sequence (the previous behaviour was O(N) per append,
    /// quadratic over a long agent).
    pub(crate) next_event_seq: PersistedAgentEventSeq,
    /// Number of ordinary inference prompts, excluding compaction operations.
    ordinary_inference_generation: InferenceGeneration,
    /// Unique content-free materialization facts keyed by provider prompt id.
    prompt_starts: HashMap<tau_proto::AgentPromptId, tau_proto::AgentPromptStarted>,
    /// Durable outer turns keyed by stable identity and their session/terminal
    /// state.
    outer_turns: HashMap<tau_proto::AgentOuterTurnId, OuterTurnFold>,
    /// Sole currently open durable outer turn.
    active_outer_turn: Option<tau_proto::AgentOuterTurnId>,
    /// Canonical provider image bytes retained across all durable transcript
    /// events, including branches and compaction replacement windows.
    retained_provider_image_bytes: u64,
    /// Sole tree-global foreground round, keyed by its assistant node.
    pending_tool_rounds: HashMap<NodeId, PendingToolRound>,
    /// Materialized terminal result node indexed by globally unique call id.
    terminal_tool_result_nodes: HashMap<ToolCallId, NodeId>,
    /// Reverse ownership from every open call ID to the sole round's assistant.
    tool_call_rounds: HashMap<ToolCallId, NodeId>,
    /// Branch-applicable context committed while provider tool adjacency is
    /// open.
    ///
    /// Agent messages and extension facts share exact durable acceptance order
    /// and materialize after the round's complete aggregate result.
    pending_context_inputs: Vec<PendingToolContextInput>,
    /// Globally unique tool calls that already have one real background
    /// completion event.
    background_completed_tool_calls: HashSet<ToolCallId>,
    /// Durable standalone-compaction transactions folded from control facts.
    compaction_transactions: HashMap<tau_proto::CompactionTransactionId, CompactionTransactionFold>,
    /// Durable insertion order for deterministic recovery projection.
    compaction_transaction_order: Vec<tau_proto::CompactionTransactionId>,
    /// Failed authorities cleared by a successful explicit successor chain.
    resolved_compaction_failures: HashSet<tau_proto::CompactionTransactionId>,
    /// Terminal-owned eager decisions keyed by eventual transaction id.
    automatic_compaction_decisions:
        HashMap<tau_proto::CompactionTransactionId, AutomaticCompactionDecisionFold>,
    /// Durable decision insertion order for deterministic recovery.
    automatic_compaction_decision_order: Vec<tau_proto::CompactionTransactionId>,
    /// Durable model-requested compactions, including requests not started yet.
    manual_compaction_requests:
        HashMap<tau_proto::CompactionRequestId, ManualCompactionRequestFold>,
    /// Durable request insertion order for deterministic recovery.
    manual_compaction_request_order: Vec<tau_proto::CompactionRequestId>,
    /// Typed self-compaction deliveries keyed by accepted request.
    self_compaction_deliveries: HashMap<tau_proto::CompactionRequestId, SelfCompactionDelivery>,
    /// All durable inference checkpoints keyed by their provider prompt id.
    inference_dispatches: HashMap<tau_proto::AgentPromptId, InferenceDispatchFold>,
    /// Durable inference checkpoint insertion order.
    inference_dispatch_order: Vec<tau_proto::AgentPromptId>,
    /// Context occurrences waiting behind marked ordinary inference ownership.
    pending_inference_inputs: Vec<PendingInferenceInput>,
    /// Number of durable selected-head moves folded so far.
    head_move_generation: HeadMoveGeneration,
}

/// Folded durable state for one outer turn.
#[derive(Clone, Debug, PartialEq)]
struct OuterTurnFold {
    /// Session attributed by the immutable start.
    session_id: tau_proto::SessionId,
    /// Whether a matching terminal fact was observed.
    finished: bool,
    /// Harness runtime that authored the start.
    runtime_id: tau_proto::AccountingRuntimeId,
    /// Initial inference prompt that opened the turn.
    agent_prompt_id: tau_proto::AgentPromptId,
}

/// Folded state for one durable inference checkpoint.
#[derive(Clone, Debug, PartialEq)]
struct InferenceDispatchFold {
    checkpoint: tau_proto::AgentInferenceDispatchStarted,
    /// Journal projection semantics selected by the checkpoint record.
    fold_semantics: AgentJournalFoldSemantics,
    /// Selected-head generation in which this owner was opened.
    head_move_generation: HeadMoveGeneration,
    finished: bool,
    recovery_disposition: tau_proto::ContextRecoveryDisposition,
    /// Output-length terminal or plan recorded by this exact response owner.
    output_length_disposition: tau_proto::OutputLengthDisposition,
    /// Harness-authored finite transport attempt for the terminal response.
    provider_attempt: Option<tau_proto::ProviderAttempt>,
    /// Canonical provider stop reason retained for cold terminal projection.
    provider_stop_reason: Option<tau_proto::ProviderStopReason>,
    /// Exact provider-reported ordinary input retained as proactive authority.
    provider_input_tokens: Option<tau_proto::TokenCount>,
    /// Whether this ordinary response opened a foreground tool round and
    /// therefore rearms output-length recovery.
    rearms_output_length: bool,
    /// Transcript node containing this dispatch's planned length response.
    output_length_plan_node: Option<NodeId>,
    /// Exact trusted steer node that claimed this dispatch's plan.
    output_length_steer_node: Option<NodeId>,
    /// Transcript node containing this dispatch's terminal response.
    response_node: Option<NodeId>,
}

/// Derives the durable action boundary that starts a new reasoning-only run.
fn output_length_response_rearms_budget(
    checkpoint: &tau_proto::AgentInferenceDispatchStarted,
    response: &tau_proto::ProviderResponseFinished,
) -> bool {
    checkpoint.transaction_id.is_none()
        && checkpoint.operation == Some(tau_proto::PromptOperation::Inference)
        && response.originator.is_user()
        && response.error.is_none()
        && response.failure_kind.is_none()
        && matches!(
            response.stop_reason,
            tau_proto::ProviderStopReason::ToolCalls | tau_proto::ProviderStopReason::EndTurn
        )
        && response
            .output_items
            .iter()
            .any(|item| matches!(item, ContextItem::ToolCall(_)))
}

/// Selected-branch repair state for one durable output-length continuation.
///
/// The projection deliberately stops at the durable dispatch-owner boundary.
/// Once an owner exists, restart must not reconstruct or resend its request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputLengthContinuationRecovery {
    /// The planned response committed but its exact internal steer did not.
    SteerNeeded {
        /// Source inference checkpoint that captured model and branch
        /// authority.
        source: tau_proto::AgentInferenceDispatchStarted,
        /// Reserved successor provider prompt.
        successor_agent_prompt_id: tau_proto::AgentPromptId,
        /// Outer turn retained by the successor.
        outer_turn_id: tau_proto::AgentOuterTurnId,
    },
    /// The exact steer committed but its matching dispatch owner did not.
    OwnerNeeded {
        /// Source inference checkpoint that captured model and branch
        /// authority.
        source: tau_proto::AgentInferenceDispatchStarted,
        /// Reserved successor provider prompt.
        successor_agent_prompt_id: tau_proto::AgentPromptId,
        /// Outer turn retained by the successor.
        outer_turn_id: tau_proto::AgentOuterTurnId,
        /// Selected transcript through which the successor must dispatch.
        through: tau_proto::AgentHead,
    },
    /// The active turn's plan or steer is no longer the exact selected head.
    BranchInvalid {
        /// Source inference checkpoint that owns the stranded plan.
        source: tau_proto::AgentInferenceDispatchStarted,
        /// Reserved successor provider prompt.
        successor_agent_prompt_id: tau_proto::AgentPromptId,
        /// Outer turn whose budget remains spent.
        outer_turn_id: tau_proto::AgentOuterTurnId,
    },
}

/// Exact next durable fact needed to close an output-length lineage whose plan
/// branch is dormant after selected-head movement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputLengthDormantRepair {
    /// Append the reserved internal steer beneath the dormant plan response.
    Steer {
        /// Durable source checkpoint that captured dispatch authority.
        source: tau_proto::AgentInferenceDispatchStarted,
        /// Reserved successor provider prompt.
        successor_agent_prompt_id: tau_proto::AgentPromptId,
        /// Outer turn retained by the successor.
        outer_turn_id: tau_proto::AgentOuterTurnId,
        /// Exact dormant plan node that must parent the steer.
        parent: tau_proto::AgentHead,
    },
    /// Append the reserved successor owner beneath the dormant steer.
    Owner {
        /// Durable source checkpoint that captured dispatch authority.
        source: tau_proto::AgentInferenceDispatchStarted,
        /// Reserved successor provider prompt.
        successor_agent_prompt_id: tau_proto::AgentPromptId,
        /// Outer turn retained by the successor.
        outer_turn_id: tau_proto::AgentOuterTurnId,
        /// Exact dormant steer head owned by the successor.
        through: tau_proto::AgentHead,
        /// Exact planned-response parent consumed by the dormant steer.
        plan_parent: tau_proto::AgentHead,
    },
    /// Append one harness failure beneath the dormant steer without
    /// prompt-start.
    Terminal {
        /// Exact reserved successor dispatch owner.
        owner: tau_proto::AgentInferenceDispatchStarted,
        /// Exact dormant steer node that must parent the failure.
        parent: tau_proto::AgentHead,
    },
    /// Append the stamped owed finish beneath the dormant terminal.
    Finish {
        /// Exact open outer turn to settle.
        outer_turn_id: tau_proto::AgentOuterTurnId,
        /// Dormant terminal response node that must parent the finish.
        parent: tau_proto::AgentHead,
    },
}

/// Selected-lineage output-length terminal projected for cold watcher restore.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputLengthTerminalIncomplete {
    /// Reserved successor prompt that reached the output limit.
    pub agent_prompt_id: tau_proto::AgentPromptId,
    /// Finite provider transport attempt that produced the terminal response.
    pub provider_attempt: tau_proto::ProviderAttempt,
}

/// Validated durable state for one standalone compaction transaction.
#[derive(Clone, Debug, PartialEq)]
struct CompactionTransactionFold {
    started: tau_proto::AgentStandaloneCompactionStarted,
    outcome: Option<CompactionTransactionOutcome>,
    checkpoint: Option<tau_proto::AgentInferenceDispatchStarted>,
    inference_finished: bool,
    standalone_context_rejection: Option<tau_proto::ProviderResponseFinished>,
}

/// Terminal-owned eager automatic-compaction authority.
#[derive(Clone, Debug, PartialEq)]
struct AutomaticCompactionDecisionFold {
    decision: tau_proto::AutomaticCompactionDecision,
    cut: tau_proto::AgentHead,
    finish_committed: bool,
    claimed: bool,
    closed: bool,
}

/// Exactly one terminal compact outcome.
#[derive(Clone, Debug, PartialEq)]
enum CompactionTransactionOutcome {
    Succeeded(tau_proto::AgentCompacted),
    Failed(tau_proto::AgentStandaloneCompactionFailed),
}

/// Folded state for one accepted model-requested compaction.
#[derive(Clone, Debug, PartialEq)]
struct ManualCompactionRequestFold {
    /// Immutable harness-owned acceptance fact.
    requested: tau_proto::AgentManualCompactionRequested,
    /// Exactly one legal lifecycle state.
    state: ManualCompactionRequestState,
}

/// Durable lifecycle of one accepted manual compaction request.
#[derive(Clone, Debug, PartialEq)]
enum ManualCompactionRequestState {
    /// Accepted but not yet claimed.
    Waiting,
    /// Claimed by exactly one standalone transaction.
    Started(tau_proto::CompactionTransactionId),
    /// Consumed by one categorical pre-start failure.
    Failed(tau_proto::AgentManualCompactionRequestFailed),
    /// Consumed by the eligible automatic transaction.
    Satisfied(tau_proto::CompactionTransactionId),
}

/// Folded authority for one committed self-compaction terminal delivery.
#[derive(Clone, Debug, PartialEq)]
struct SelfCompactionDelivery {
    /// Typed request, call, transaction, and outcome correlation.
    terminal: tau_proto::SelfCompactionTerminal,
    /// Exact transcript node whose activation the continuation must cover.
    node_id: NodeId,
}

/// Durable state of an accepted model-requested compaction.
///
/// See `SPEC-compaction-and-context-recovery`.
#[derive(Clone, Debug, PartialEq)]
pub enum ManualCompactionRecovery {
    /// The accepted request has not started or failed.
    Waiting(tau_proto::AgentManualCompactionRequested),
    /// The request has started a durable standalone transaction.
    Started {
        /// Immutable acceptance fact.
        requested: tau_proto::AgentManualCompactionRequested,
        /// Matching transaction.
        started: Box<tau_proto::AgentStandaloneCompactionStarted>,
        /// Durable transaction outcome, when provider work terminated.
        outcome: Option<Box<ManualCompactionOutcome>>,
    },
    /// The request failed before transaction start.
    Failed {
        /// Immutable acceptance fact.
        requested: tau_proto::AgentManualCompactionRequested,
        /// Matching terminal pre-start failure.
        failed: tau_proto::AgentManualCompactionRequestFailed,
    },
}

/// Durable terminal transaction outcome for a model-requested compaction.
#[derive(Clone, Debug, PartialEq)]
pub enum ManualCompactionOutcome {
    /// Compaction committed a replacement boundary.
    Succeeded(tau_proto::AgentCompacted),
    /// Compaction failed; durable history may suppress matching automatic work
    /// without blocking ordinary prompt dispatch.
    Failed(tau_proto::AgentStandaloneCompactionFailed),
}

/// Durable standalone-compaction state reconstructed by the core fold.
#[derive(Clone, Debug, PartialEq)]
pub enum StandaloneCompactionRecovery {
    /// A terminal-owned eager decision is durable but has not started.
    AwaitingAutomaticStart {
        /// Bounded terminal-owned authority.
        decision: tau_proto::AutomaticCompactionDecision,
        /// Exact canonical terminal node selected as the compact cut.
        cut: tau_proto::AgentHead,
        /// Whether the matching outer-turn finish has committed.
        finish_committed: bool,
    },
    /// A start has no terminal outcome and must be repaired as interrupted.
    Interrupted(tau_proto::AgentStandaloneCompactionStarted),
    /// A canonical standalone provider rejection committed before its exact
    /// typed failure and optional retreat plan.
    RejectedAwaitingFailure {
        /// Durable transaction start.
        started: tau_proto::AgentStandaloneCompactionStarted,
        /// Canonical control-only provider rejection.
        response: Box<tau_proto::ProviderResponseFinished>,
    },
    /// The latest transaction failed terminally.
    ///
    /// Harness recovery may use this as nonblocking automatic-suppression
    /// authority; this projection does not itself require dispatch to block.
    Blocked {
        /// Durable terminal failure.
        failed: tau_proto::AgentStandaloneCompactionFailed,
        /// Actual provider prompt reserved by the matching durable start.
        compact_prompt_id: tau_proto::AgentPromptId,
    },
    /// A typed failure pre-minted an exact retreat successor that has not
    /// committed yet.
    AwaitingContextRetreat {
        /// Durable failed transaction.
        failed: tau_proto::AgentStandaloneCompactionFailed,
        /// Exact successor plan authorized by that failure.
        plan: tau_proto::ContextRetreatPlan,
    },
    /// Success still owes a durable inference-dispatch checkpoint.
    AwaitingCheckpoint {
        /// Successful transaction id.
        transaction_id: tau_proto::CompactionTransactionId,
        /// Immutable compact cut.
        cut: tau_proto::AgentHead,
        /// Exact provider-qualified model captured by the successful start.
        model: tau_proto::ModelId,
        /// Snapshot through which inference remains owed.
        through: tau_proto::AgentHead,
        /// Whether the successful transaction permits automatic rolling.
        automatic: bool,
    },
    /// A checkpoint exists without a matching durable provider terminal
    /// response.
    DispatchUncertain(tau_proto::AgentInferenceDispatchStarted),
}

/// Progress of a durable rolling chain rooted at a rejected inference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReactiveCompactionProgress {
    /// Reducible provider-closed history remains before the rejected
    /// inference's activation cut.
    NeedsContinuation {
        /// Furthest logical provider-window cut the rolling chain may consume
        /// before inference resumes.
        target_cut: tau_proto::AgentHead,
    },
    /// The latest successful cut reached the end of the logical provider
    /// window preceding the rejected activation, so inference must resume with
    /// the activating input retained.
    ReachedTargetCut,
}

/// Recovery projection for the latest durable inference checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub enum InferenceDispatchRecovery {
    /// A terminal provider response durably completed this snapshot.
    CompletedThrough(tau_proto::AgentHead),
    /// A canonical no-output rejection durably authorized one compaction start,
    /// but no transaction has claimed it yet.
    ContextRecoveryRequired(tau_proto::AgentInferenceDispatchStarted),
    /// Dispatch may have reached the provider but has no durable terminal
    /// response.
    DispatchUncertain(tau_proto::AgentInferenceDispatchStarted),
}

/// Latest durable value for one per-agent metadata key.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentMetadataEntry {
    /// Arbitrary extension-visible CBOR value.
    pub value: tau_proto::CborValue,
    /// Whether this entry is copied to child agents at creation time.
    pub inheritable: bool,
}
fn normalize_display_name(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

impl AgentTree {
    /// Returns the latest validated standalone transaction's recovery
    /// projection, following `SPEC-compaction-and-context-recovery`.
    #[must_use]
    pub fn standalone_compaction_recovery(&self) -> Option<StandaloneCompactionRecovery> {
        if let Some(decision) = self
            .automatic_compaction_decision_order
            .iter()
            .rev()
            .filter_map(|id| self.automatic_compaction_decisions.get(id))
            .find(|decision| {
                !decision.claimed && !decision.closed && decision.decision.evidence.is_some()
            })
        {
            return Some(StandaloneCompactionRecovery::AwaitingAutomaticStart {
                decision: decision.decision.clone(),
                cut: decision.cut,
                finish_committed: decision.finish_committed,
            });
        }
        let id = self.compaction_transaction_order.last()?;
        let transaction = self.compaction_transactions.get(id)?;
        match (&transaction.outcome, &transaction.checkpoint) {
            (None, _) => transaction
                .standalone_context_rejection
                .clone()
                .map_or_else(
                    || {
                        Some(StandaloneCompactionRecovery::Interrupted(
                            transaction.started.clone(),
                        ))
                    },
                    |response| {
                        Some(StandaloneCompactionRecovery::RejectedAwaitingFailure {
                            started: transaction.started.clone(),
                            response: Box::new(response),
                        })
                    },
                ),
            (Some(CompactionTransactionOutcome::Failed(failed)), _) => {
                if let Some(plan) = failed.context_retreat.clone() {
                    Some(StandaloneCompactionRecovery::AwaitingContextRetreat {
                        failed: failed.clone(),
                        plan,
                    })
                } else {
                    Some(StandaloneCompactionRecovery::Blocked {
                        failed: failed.clone(),
                        compact_prompt_id: transaction.started.compact_prompt_id.clone(),
                    })
                }
            }
            (Some(CompactionTransactionOutcome::Succeeded(_)), None) => {
                transaction.started.resume_through.map(|resume| {
                    let current = self
                        .head
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                    StandaloneCompactionRecovery::AwaitingCheckpoint {
                        transaction_id: id.clone(),
                        cut: transaction.started.cut,
                        model: transaction.started.model.clone(),
                        through: if self.is_ancestor_head(resume, current) {
                            current
                        } else {
                            resume
                        },
                        automatic: matches!(
                            transaction.started.trigger,
                            tau_proto::StandaloneCompactionTrigger::AutomaticThreshold
                                | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                                | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                                    ..
                                }
                        ),
                    }
                })
            }
            (Some(CompactionTransactionOutcome::Succeeded(_)), Some(checkpoint))
                if !transaction.inference_finished =>
            {
                Some(StandaloneCompactionRecovery::DispatchUncertain(
                    checkpoint.clone(),
                ))
            }
            (Some(CompactionTransactionOutcome::Succeeded(_)), Some(_)) => None,
        }
    }

    /// Returns rolling progress when the given successful compaction belongs
    /// to a chain rooted at a rejected inference.
    ///
    /// Automatic continuations retain only their immediately preceding
    /// transaction id, so this follows the validated durable chain back to its
    /// reactive context-overflow origin. Recovery stops when the latest cut has
    /// consumed the history preceding the original activation cut; the rejected
    /// activating input remains in the suffix for inference.
    ///
    /// Returns `None` when the transaction is unknown, unfinished, failed,
    /// checkpointed, not rooted at reactive recovery, or has an invalid durable
    /// predecessor/activation relation.
    #[must_use]
    pub fn reactive_compaction_target(
        &self,
        failed_agent_prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<tau_proto::AgentHead> {
        let activation_cut = self
            .inference_dispatches
            .get(failed_agent_prompt_id)?
            .checkpoint
            .activation_cut?;
        let activation_cut = self.closed_provider_prefix_at_or_before(activation_cut);
        Some(
            self.active_provider_window_latest(activation_cut.as_option())
                .map(|(node_id, _)| tau_proto::AgentHead::Node(node_id))
                .or_else(|| {
                    self.active_provider_window_replacement(activation_cut.as_option())
                        .map(|(boundary, _)| boundary)
                        .map(tau_proto::AgentHead::Node)
                })
                .unwrap_or(tau_proto::AgentHead::Root),
        )
    }

    /// Returns the immediately preceding closed transcript cut without crossing
    /// the current logical replacement boundary.
    #[must_use]
    pub fn previous_provider_closed_cut_in_active_window(
        &self,
        active_head: tau_proto::AgentHead,
        rejected_cut: tau_proto::AgentHead,
    ) -> Option<tau_proto::AgentHead> {
        let window = self.active_provider_window(active_head.as_option());
        let rejected = window
            .transcript
            .iter()
            .position(|(node_id, _)| tau_proto::AgentHead::Node(*node_id) == rejected_cut)?;
        window.transcript[..rejected]
            .iter()
            .rev()
            .map(|(node_id, _)| tau_proto::AgentHead::Node(*node_id))
            .find(|candidate| self.closed_provider_prefix_at_or_before(*candidate) == *candidate)
    }

    #[must_use]
    pub fn reactive_compaction_progress(
        &self,
        transaction_id: &tau_proto::CompactionTransactionId,
    ) -> Option<ReactiveCompactionProgress> {
        let mut transaction = self.compaction_transactions.get(transaction_id)?;
        if !matches!(
            transaction.outcome,
            Some(CompactionTransactionOutcome::Succeeded(_))
        ) || transaction.checkpoint.is_some()
        {
            return None;
        }
        let latest_cut = transaction.started.cut;
        loop {
            match &transaction.started.trigger {
                tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                    failed_agent_prompt_id,
                } => {
                    return self
                        .reactive_compaction_target(failed_agent_prompt_id)
                        .and_then(|target_cut| {
                            self.is_ancestor_head(latest_cut, target_cut).then_some(
                                if latest_cut == target_cut {
                                    ReactiveCompactionProgress::ReachedTargetCut
                                } else {
                                    ReactiveCompactionProgress::NeedsContinuation { target_cut }
                                },
                            )
                        });
                }
                tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
                    roll_through,
                    ..
                } => {
                    return self.is_ancestor_head(latest_cut, *roll_through).then_some(
                        if latest_cut == *roll_through {
                            ReactiveCompactionProgress::ReachedTargetCut
                        } else {
                            ReactiveCompactionProgress::NeedsContinuation {
                                target_cut: *roll_through,
                            }
                        },
                    );
                }
                tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                    previous_transaction_id,
                }
                | tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                    previous_transaction_id: Some(previous_transaction_id),
                    ..
                } => {
                    let previous = self.compaction_transactions.get(previous_transaction_id)?;
                    transaction = previous;
                }
                _ => return None,
            }
        }
    }

    /// Returns the latest unresolved terminal failure for one exact model and
    /// selected branch.
    ///
    /// Unrelated later transactions do not clear this authority. A successful
    /// explicit successor clears the failed transaction and its superseded
    /// failure chain.
    #[must_use]
    pub fn unresolved_standalone_compaction_failure(
        &self,
        model: &tau_proto::ModelId,
        current_head: tau_proto::AgentHead,
    ) -> Option<&tau_proto::AgentStandaloneCompactionFailed> {
        self.compaction_transaction_order
            .iter()
            .rev()
            .filter(|id| !self.resolved_compaction_failures.contains(*id))
            .filter_map(|id| {
                let transaction = self.compaction_transactions.get(id)?;
                let CompactionTransactionOutcome::Failed(failed) = transaction.outcome.as_ref()?
                else {
                    return None;
                };
                (transaction.started.model == *model
                    && self.is_ancestor_head(failed.cut, current_head)
                    && failed
                        .resume_through
                        .is_none_or(|resume| self.is_ancestor_head(resume, current_head)))
                .then_some(failed)
            })
            .next()
    }

    /// Returns model-requested compaction state in durable acceptance order.
    #[must_use]
    pub fn manual_compaction_recoveries(&self) -> Vec<ManualCompactionRecovery> {
        self.manual_compaction_request_order
            .iter()
            .filter_map(|id| {
                let request = self.manual_compaction_requests.get(id)?;
                if let ManualCompactionRequestState::Failed(failed) = &request.state {
                    return Some(ManualCompactionRecovery::Failed {
                        requested: request.requested.clone(),
                        failed: failed.clone(),
                    });
                }
                if let ManualCompactionRequestState::Started(transaction_id) = &request.state {
                    let transaction = self.compaction_transactions.get(transaction_id)?;
                    return Some(ManualCompactionRecovery::Started {
                        requested: request.requested.clone(),
                        started: Box::new(transaction.started.clone()),
                        outcome: transaction.outcome.as_ref().map(|outcome| {
                            Box::new(match outcome {
                                CompactionTransactionOutcome::Succeeded(compacted) => {
                                    ManualCompactionOutcome::Succeeded(compacted.clone())
                                }
                                CompactionTransactionOutcome::Failed(failed) => {
                                    ManualCompactionOutcome::Failed(failed.clone())
                                }
                            })
                        }),
                    });
                }
                matches!(request.state, ManualCompactionRequestState::Waiting)
                    .then(|| ManualCompactionRecovery::Waiting(request.requested.clone()))
            })
            .collect()
    }

    /// Returns every durable manual request, including already satisfied UI
    /// intents, for collision-free request-id allocation.
    #[must_use]
    pub fn manual_compaction_request_count(&self) -> usize {
        self.manual_compaction_request_order.len()
    }

    /// Return the typed terminal already delivered for an accepted
    /// self-compaction request.
    #[must_use]
    pub fn self_compaction_delivery(
        &self,
        request_id: &tau_proto::CompactionRequestId,
    ) -> Option<&tau_proto::SelfCompactionTerminal> {
        self.self_compaction_deliveries
            .get(request_id)
            .map(|delivery| &delivery.terminal)
    }

    /// Return whether a typed self-compaction terminal activation still lacks
    /// an ordinary inference checkpoint covering its exact transcript node.
    #[must_use]
    pub fn self_compaction_delivery_needs_checkpoint(
        &self,
        request_id: &tau_proto::CompactionRequestId,
    ) -> bool {
        let Some(delivery) = self.self_compaction_deliveries.get(request_id) else {
            return false;
        };
        !self.inference_dispatches.values().any(|dispatch| {
            self.is_ancestor_head(
                tau_proto::AgentHead::Node(delivery.node_id),
                dispatch.checkpoint.through,
            )
        })
    }

    /// Returns whether the branch contains the complete tool-results node for a
    /// call.
    #[must_use]
    pub fn has_complete_tool_round_for(&self, head: Option<NodeId>, call_id: &ToolCallId) -> bool {
        self.branch_node_ids_from(head).into_iter().any(|node_id| {
            self.node(node_id).is_some_and(|node| {
                matches!(
                    &node.entry,
                    AgentEntry::ToolResults { items }
                        if items.iter().any(|item| item.call_id == *call_id)
                )
            })
        })
    }

    /// Returns whether `ancestor` is on the path to `descendant`.
    #[must_use]
    pub fn contains_head_ancestry(&self, ancestor: AgentHead, descendant: AgentHead) -> bool {
        self.is_ancestor_head(ancestor, descendant)
    }

    /// Returns the nearest provider-valid closed prefix at or before `cut`.
    ///
    /// Validated transcript branches materialize a complete tool round as one
    /// tool-calling assistant node followed by one whole results node. A cut at
    /// that assistant node would expose calls without their outputs, so it
    /// retreats to the assistant node's parent and keeps the complete round in
    /// the exact suffix. Callers must supply a root or node from this tree; an
    /// unknown node is returned unchanged so durable validation remains the
    /// authority rather than silently substituting another branch.
    #[must_use]
    pub fn closed_provider_prefix_at_or_before(&self, cut: AgentHead) -> AgentHead {
        let AgentHead::Node(node_id) = cut else {
            return cut;
        };
        let Some(node) = self.node(node_id) else {
            return cut;
        };
        let has_tool_call = matches!(
            &node.entry,
            AgentEntry::AssistantResponse { output_items, .. }
                if output_items
                    .iter()
                    .any(|item| matches!(item, ContextItem::ToolCall(_)))
        );
        if has_tool_call {
            node.parent_id.map_or(AgentHead::Root, AgentHead::Node)
        } else {
            cut
        }
    }

    /// Reconstruct the logical provider-visible window ending at `head`.
    ///
    /// Physical ancestry around a suffix-preserving boundary is
    /// `cut < preserved suffix < boundary`, while provider order is
    /// `replacement + preserved suffix`. Each boundary therefore consumes its
    /// cut from the window accumulated immediately before the boundary rather
    /// than slicing physical ancestry directly.
    #[must_use]
    pub fn active_provider_window(&self, head: Option<NodeId>) -> ActiveProviderWindow<'_> {
        let Some(position) = head.and_then(|node_id| self.provider_window_position(node_id)) else {
            return ActiveProviderWindow {
                replacement: None,
                replacement_boundary: None,
                transcript: Vec::new(),
            };
        };
        let replacement = self
            .active_provider_window_replacement(head)
            .map(|(_, replacement)| replacement);
        let mut transcript = Vec::new();
        let mut current = self.provider_window_tail(head);
        let replacement_boundary = position.replacement_boundary.get();
        let start_exclusive = self.provider_window_start_exclusive(replacement_boundary);
        while let Some(node_id) = current {
            if Some(node_id) == start_exclusive {
                break;
            }
            let Some(node) = self.node(node_id) else {
                break;
            };
            transcript.push((node_id, &node.entry));
            current = self
                .provider_window_position(node_id)
                .and_then(|item| item.predecessor.get());
        }
        transcript.reverse();
        ActiveProviderWindow {
            replacement,
            replacement_boundary,
            transcript,
        }
    }

    /// Return the latest surviving transcript occurrence without materializing
    /// the complete active provider window.
    #[must_use]
    pub fn active_provider_window_latest(
        &self,
        head: Option<NodeId>,
    ) -> Option<(NodeId, &AgentEntry)> {
        let node_id = self.provider_window_tail(head)?;
        if Some(node_id)
            == self.provider_window_start_exclusive(
                self.provider_window_position(head?)?
                    .replacement_boundary
                    .get(),
            )
        {
            return None;
        }
        self.node(node_id).map(|node| (node_id, &node.entry))
    }

    /// Return the latest compacted replacement and its boundary without
    /// materializing the surviving transcript.
    #[must_use]
    pub fn active_provider_window_replacement(
        &self,
        head: Option<NodeId>,
    ) -> Option<(NodeId, &[ContextItem])> {
        let boundary = self
            .provider_window_position(head?)?
            .replacement_boundary
            .get()?;
        let AgentEntry::Compaction {
            replacement_window, ..
        } = &self.node(boundary)?.entry
        else {
            return None;
        };
        Some((boundary, replacement_window))
    }

    /// Count surviving transcript occurrences without materializing the active
    /// provider window.
    #[must_use]
    pub fn active_provider_window_transcript_count(&self, head: Option<NodeId>) -> usize {
        let Some(position) = head.and_then(|node_id| self.provider_window_position(node_id)) else {
            return 0;
        };
        let start_exclusive =
            self.provider_window_start_exclusive(position.replacement_boundary.get());
        let mut count = 0;
        let mut current = self.provider_window_tail(head);
        while let Some(node_id) = current {
            if Some(node_id) == start_exclusive {
                break;
            }
            let Some(position) = self.provider_window_position(node_id) else {
                break;
            };
            count += 1;
            current = position.predecessor.get();
        }
        count
    }

    /// Return the successful compaction that owns the active replacement
    /// boundary, when that boundary retains qualified token measurements.
    #[must_use]
    pub fn active_provider_compaction(
        &self,
        head: Option<NodeId>,
    ) -> Option<&tau_proto::AgentCompacted> {
        let boundary = self
            .provider_window_position(head?)
            .and_then(|position| position.replacement_boundary.get())?;
        let AgentEntry::Compaction {
            transaction_id: Some(transaction_id),
            ..
        } = &self.node(boundary)?.entry
        else {
            return None;
        };
        let transaction = self.compaction_transactions.get(transaction_id)?;
        match transaction.outcome.as_ref()? {
            CompactionTransactionOutcome::Succeeded(compacted) => Some(compacted),
            CompactionTransactionOutcome::Failed(_) => None,
        }
    }

    /// Return whether `cut` names one occurrence in the logical active window
    /// ending at `through`.
    #[must_use]
    pub fn active_provider_window_contains(&self, through: AgentHead, cut: AgentHead) -> bool {
        match cut {
            AgentHead::Root => through.as_option().is_none_or(|head| {
                self.provider_window_position(head)
                    .is_none_or(|position| position.replacement_boundary.get().is_none())
            }),
            AgentHead::Node(node_id) => {
                self.provider_window_contains_node(through.as_option(), node_id)
            }
        }
    }

    /// Returns whether the branch ending at `head` contains an exact user-input
    /// text item.
    #[must_use]
    pub fn has_user_input_text_on_branch(&self, head: Option<NodeId>, text: &str) -> bool {
        self.branch_node_ids_from(head).into_iter().any(|node_id| {
            self.node(node_id).is_some_and(|node| {
                matches!(
                    &node.entry,
                    AgentEntry::UserInput { items, .. }
                        if items.iter().any(|item| matches!(
                            item,
                            ContextItem::Message(message)
                                if message.content.iter().any(|part| matches!(
                                    part,
                                    ContentPart::Text { text: item_text }
                                        | ContentPart::SyntheticCompactionSummary { text: item_text }
                                        | ContentPart::HarnessInternalText { text: item_text }
                                        if item_text == text
                                ))
                        ))
                )
            })
        })
    }

    /// Returns recovery state for the latest durable inference checkpoint.
    #[must_use]
    pub fn inference_dispatch_recovery(&self) -> Option<InferenceDispatchRecovery> {
        let prompt_id = self.inference_dispatch_order.last()?;
        let dispatch = self.inference_dispatches.get(prompt_id)?;
        let checkpoint = &dispatch.checkpoint;
        match (dispatch.finished, &dispatch.recovery_disposition) {
            (true, tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned) => {
                let claimed = self.compaction_transactions.values().any(|transaction| {
                    matches!(
                        &transaction.started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                            failed_agent_prompt_id
                        } if failed_agent_prompt_id == prompt_id
                    )
                });
                if claimed {
                    Some(InferenceDispatchRecovery::CompletedThrough(
                        checkpoint.through,
                    ))
                } else {
                    Some(InferenceDispatchRecovery::ContextRecoveryRequired(
                        checkpoint.clone(),
                    ))
                }
            }
            (true, tau_proto::ContextRecoveryDisposition::None) => Some(
                InferenceDispatchRecovery::CompletedThrough(checkpoint.through),
            ),
            (false, _) => Some(InferenceDispatchRecovery::DispatchUncertain(
                checkpoint.clone(),
            )),
        }
    }

    /// Returns the agent identifier.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Returns the current head node id, if any.
    ///
    /// This is the *write cursor* — where the next append from a
    /// folded event will be parented. To enumerate the tips of every
    /// existing branch, use [`AgentTree::leaves`] instead.
    #[must_use]
    pub fn head(&self) -> Option<NodeId> {
        self.head
    }

    /// Returns the current human-friendly display name, if one was set.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Returns the latest durable initialization replacement without treating
    /// it as compactable transcript history.
    #[must_use]
    pub fn initialization_context(&self) -> Option<&tau_proto::AgentInitializationContextSet> {
        self.initialization_context.as_ref()
    }

    /// Returns latest committed durable metadata entries for this agent.
    #[must_use]
    pub fn metadata(&self) -> &BTreeMap<tau_proto::AgentMetadataKey, AgentMetadataEntry> {
        &self.metadata
    }

    /// Returns metadata entries marked inheritable for child-agent creation.
    #[must_use]
    pub fn inheritable_metadata(
        &self,
    ) -> BTreeMap<tau_proto::AgentMetadataKey, AgentMetadataEntry> {
        self.metadata
            .iter()
            .filter(|(_, entry)| entry.inheritable)
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect()
    }

    /// Returns a node by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&AgentNode> {
        self.nodes.get(id.get() as usize)
    }

    /// Returns all nodes.
    #[must_use]
    pub fn nodes(&self) -> &[AgentNode] {
        &self.nodes
    }

    /// Returns the entries along the current branch (root to head).
    #[must_use]
    pub fn current_branch(&self) -> Vec<&AgentEntry> {
        self.branch_from(self.head)
    }

    /// Returns every materialized transcript entry in durable append order,
    /// independent of the currently selected branch.
    pub fn all_entries(&self) -> impl Iterator<Item = &AgentEntry> {
        self.nodes.iter().map(|node| &node.entry)
    }

    /// Returns the entries along the branch ending at `head` (root to
    /// `head`). When `head` is `None` or unknown, returns an empty
    /// slice. Use this to assemble a prompt for a *specific*
    /// conversation that may not coincide with the tree's write
    /// cursor — multiple side conversations can interleave their
    /// tree mutations, so `tree.head()` is unreliable for that
    /// purpose.
    #[must_use]
    pub fn branch_from(&self, head: Option<NodeId>) -> Vec<&AgentEntry> {
        self.branch_node_ids_from(head)
            .into_iter()
            .filter_map(|id| self.node(id).map(|node| &node.entry))
            .collect()
    }

    /// Returns foreground tool calls on `head`'s branch that still lack a
    /// terminal provider result.
    ///
    /// Results are ordered by assistant response and by the model's original
    /// tool-call order. Calls that already have a terminal result in a
    /// partially completed parallel round are omitted. Backgrounded tool
    /// calls are out of scope because their foreground is already closed by
    /// a synthetic provider result.
    #[must_use]
    pub fn unresolved_foreground_tool_calls_from(
        &self,
        head: Option<NodeId>,
    ) -> Vec<&ToolCallItem> {
        let mut calls = Vec::new();
        for node_id in self.branch_node_ids_from(head) {
            let Some(round) = self.pending_tool_rounds.get(&node_id) else {
                continue;
            };
            let Some(AgentEntry::AssistantResponse { output_items, .. }) =
                self.node(node_id).map(|node| &node.entry)
            else {
                continue;
            };
            for call_id in &round.call_order {
                if !round.terminal_results.contains_key(call_id)
                    && let Some(call) = output_items.iter().find_map(|item| match item {
                        ContextItem::ToolCall(call) if &call.call_id == call_id => Some(call),
                        _ => None,
                    })
                {
                    calls.push(call);
                }
            }
        }
        calls
    }

    /// Returns whether this agent tree has an unfinished foreground provider
    /// tool round on any branch.
    ///
    /// The tree admits at most one such round globally. Runtime dispatch must
    /// remain blocked on every branch until that round terminalizes.
    #[must_use]
    pub fn has_open_foreground_tool_round(&self) -> bool {
        !self.pending_tool_rounds.is_empty()
    }

    /// Returns a terminal tool result by call id, including results retained in
    /// an incomplete parallel round that has not materialized its aggregate
    /// transcript node yet.
    #[must_use]
    pub fn pending_terminal_tool_result(&self, call_id: &ToolCallId) -> Option<&ToolResultItem> {
        self.pending_tool_rounds
            .values()
            .find_map(|round| round.terminal_results.get(call_id))
    }

    /// Returns the aggregate node containing a materialized terminal result.
    #[must_use]
    pub fn terminal_tool_result_node_id(&self, call_id: &ToolCallId) -> Option<NodeId> {
        self.terminal_tool_result_nodes.get(call_id).copied()
    }

    /// Returns one materialized terminal result from its exact aggregate node.
    #[must_use]
    pub fn terminal_tool_result_at(
        &self,
        node_id: NodeId,
        call_id: &ToolCallId,
    ) -> Option<&ToolResultItem> {
        let AgentEntry::ToolResults { items } = &self.node(node_id)?.entry else {
            return None;
        };
        items.iter().find(|item| &item.call_id == call_id)
    }

    /// Returns unfinished calls from the sole open foreground provider round.
    ///
    /// Unlike [`Self::unresolved_foreground_tool_calls_from`], this query is
    /// branch-independent and is intended for cold recovery of the tree-global
    /// round invariant.
    #[must_use]
    pub fn unresolved_foreground_tool_calls(&self) -> Vec<&ToolCallItem> {
        let Some((&assistant_node_id, round)) = self.pending_tool_rounds.iter().next() else {
            return Vec::new();
        };
        let Some(AgentEntry::AssistantResponse { output_items, .. }) =
            self.node(assistant_node_id).map(|node| &node.entry)
        else {
            return Vec::new();
        };
        round
            .call_order
            .iter()
            .filter(|call_id| !round.terminal_results.contains_key(*call_id))
            .filter_map(|call_id| {
                output_items.iter().find_map(|item| match item {
                    ContextItem::ToolCall(call) if &call.call_id == call_id => Some(call),
                    _ => None,
                })
            })
            .collect()
    }

    /// Returns backgrounded tool calls on `head`'s branch and any durable
    /// background completion recorded for them.
    ///
    /// The provider-visible placeholder is stored as a `ProviderToolResult`
    /// with [`ToolResultKind::BackgroundPlaceholder`]. The later real outcome
    /// is stored separately as `ToolBackgroundResult` or
    /// `ToolBackgroundError` and does not fold into the prompt tree, so
    /// callers must pass the durable event log alongside the tree. Completed
    /// calls are returned in durable completion-event order; unfinished
    /// placeholders follow in provider-placeholder order.
    #[must_use]
    pub fn background_tool_calls_from(
        &self,
        head: Option<NodeId>,
        events: &[PersistedAgentEvent],
    ) -> Vec<BackgroundToolCallState> {
        let branch_call_ids = self.tool_call_ids_from_branch(head);
        if branch_call_ids.is_empty() {
            return Vec::new();
        }

        let mut placeholder_order = Vec::new();
        let mut completion_order = Vec::new();
        let mut completion_order_seen = HashSet::new();
        let mut states = HashMap::new();
        let mut completions = HashMap::new();
        let mut call_refs = HashMap::new();
        let mut terminal_observations = HashMap::new();
        for entry in events {
            match &entry.event {
                Event::ProviderResponseFinished(response) => {
                    for (item_index, item) in response.output_items.iter().enumerate() {
                        let ContextItem::ToolCall(call) = item else {
                            continue;
                        };
                        if !branch_call_ids.contains(&call.call_id) {
                            continue;
                        }
                        if let Ok(item_index) = u32::try_from(item_index) {
                            call_refs.insert(
                                call.call_id.clone(),
                                tau_proto::ToolCallRef {
                                    declaration: entry.observation_id,
                                    item_index,
                                },
                            );
                        }
                    }
                }
                Event::ProviderToolResult(result) => {
                    if result.kind != ToolResultKind::BackgroundPlaceholder
                        || !branch_call_ids.contains(&result.call_id)
                    {
                        continue;
                    }
                    if states.contains_key(&result.call_id) {
                        continue;
                    }
                    placeholder_order.push(result.call_id.clone());
                    states.insert(
                        result.call_id.clone(),
                        BackgroundToolCallState {
                            placeholder: BackgroundToolPlaceholder {
                                call_id: result.call_id.clone(),
                                tool_name: result.tool_name.clone(),
                                tool_type: result.tool_type,
                                originator: result.originator.clone(),
                            },
                            completion: completions.get(&result.call_id).cloned(),
                            call_ref: call_refs.get(&result.call_id).copied(),
                            terminal_observation: terminal_observations
                                .get(&result.call_id)
                                .copied(),
                        },
                    );
                }
                Event::ToolBackgroundResult(result) => {
                    if !branch_call_ids.contains(&result.call_id) {
                        continue;
                    }
                    let completion = BackgroundToolCompletion::Result(result.clone());
                    terminal_observations.insert(result.call_id.clone(), entry.observation_id);
                    completions.insert(result.call_id.clone(), completion.clone());
                    if completion_order_seen.insert(result.call_id.clone()) {
                        completion_order.push(result.call_id.clone());
                    }
                    if let Some(state) = states.get_mut(&result.call_id) {
                        state.completion = Some(completion);
                        state.terminal_observation = Some(entry.observation_id);
                    }
                }
                Event::ToolBackgroundError(error) => {
                    if !branch_call_ids.contains(&error.call_id) {
                        continue;
                    }
                    let completion = BackgroundToolCompletion::Error(error.clone());
                    terminal_observations.insert(error.call_id.clone(), entry.observation_id);
                    completions.insert(error.call_id.clone(), completion.clone());
                    if completion_order_seen.insert(error.call_id.clone()) {
                        completion_order.push(error.call_id.clone());
                    }
                    if let Some(state) = states.get_mut(&error.call_id) {
                        state.completion = Some(completion);
                        state.terminal_observation = Some(entry.observation_id);
                    }
                }
                _ => {}
            }
        }

        let mut ordered = Vec::new();
        for call_id in completion_order {
            if let Some(state) = states.remove(&call_id) {
                ordered.push(state);
            }
        }
        for call_id in placeholder_order {
            if let Some(state) = states.remove(&call_id) {
                ordered.push(state);
            }
        }
        ordered
    }

    /// Returns background placeholders on `head`'s branch that lack a real
    /// background result or error in the durable event log.
    #[must_use]
    pub fn unresolved_background_tool_calls_from(
        &self,
        head: Option<NodeId>,
        events: &[PersistedAgentEvent],
    ) -> Vec<BackgroundToolPlaceholder> {
        self.background_tool_calls_from(head, events)
            .into_iter()
            .filter(|state| state.completion.is_none())
            .map(|state| state.placeholder)
            .collect()
    }

    fn tool_call_ids_from_branch(&self, head: Option<NodeId>) -> HashSet<ToolCallId> {
        let mut call_ids = HashSet::new();
        for node_id in self.branch_node_ids_from(head) {
            let Some(AgentEntry::AssistantResponse { output_items, .. }) =
                self.node(node_id).map(|node| &node.entry)
            else {
                continue;
            };
            for item in output_items {
                if let ContextItem::ToolCall(call) = item {
                    call_ids.insert(call.call_id.clone());
                }
            }
        }
        call_ids
    }

    /// Returns node identifiers on the selected branch in root-to-head order.
    #[must_use]
    pub fn branch_node_ids_from(&self, head: Option<NodeId>) -> Vec<NodeId> {
        #[cfg(test)]
        BRANCH_PATH_MATERIALIZATIONS.set(BRANCH_PATH_MATERIALIZATIONS.get() + 1);

        let mut path = Vec::new();
        let mut current = head;
        while let Some(id) = current {
            if let Some(node) = self.nodes.get(id.get() as usize) {
                path.push(id);
                current = node.parent_id;
            } else {
                break;
            }
        }
        path.reverse();
        path
    }

    /// Returns whether `ancestor` is on the branch ending at `descendant`.
    #[must_use]
    pub fn is_ancestor_head(
        &self,
        ancestor: tau_proto::AgentHead,
        descendant: tau_proto::AgentHead,
    ) -> bool {
        match ancestor {
            tau_proto::AgentHead::Root => true,
            tau_proto::AgentHead::Node(ancestor) => {
                if descendant == tau_proto::AgentHead::Node(ancestor) {
                    return self.node(ancestor).is_some();
                }
                let mut current = descendant.as_option();
                while let Some(node_id) = current {
                    let Some(node) = self.node(node_id) else {
                        return false;
                    };
                    if node_id == ancestor {
                        return true;
                    }
                    current = node.parent_id;
                }
                false
            }
        }
    }

    /// Returns the direct children of a node.
    #[must_use]
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.parent_id == Some(id))
            .map(|n| n.id)
            .collect()
    }

    /// Returns the leaves of the tree — every node that has no
    /// children. Each leaf is the tip of one branch the user can
    /// resume by setting the head to it. Order matches insertion
    /// order (NodeId-ascending).
    #[must_use]
    pub fn leaves(&self) -> Vec<NodeId> {
        use std::collections::HashSet;
        let parents: HashSet<NodeId> = self.nodes.iter().filter_map(|n| n.parent_id).collect();
        self.nodes
            .iter()
            .map(|n| n.id)
            .filter(|id| !parents.contains(id))
            .collect()
    }

    fn provider_window_position(&self, node_id: NodeId) -> Option<ProviderWindowPosition> {
        self.indexes
            .provider_window
            .positions
            .get(node_id.get() as usize)
            .copied()
    }

    fn provider_window_tail(&self, head: Option<NodeId>) -> Option<NodeId> {
        let head = head?;
        let position = self.provider_window_position(head)?;
        if matches!(
            self.node(head).map(|node| &node.entry),
            Some(AgentEntry::Compaction { .. })
        ) {
            position.predecessor.get()
        } else {
            Some(head)
        }
    }

    fn provider_window_start_exclusive(
        &self,
        replacement_boundary: Option<NodeId>,
    ) -> Option<NodeId> {
        replacement_boundary
            .and_then(|boundary| self.indexes.provider_window.boundary_anchors.get(&boundary))
            .and_then(|anchor| anchor.start_exclusive.get())
    }

    fn provider_window_chain_contains(
        &self,
        mut current: Option<NodeId>,
        start_exclusive: Option<NodeId>,
        candidate: NodeId,
    ) -> bool {
        while let Some(node_id) = current {
            if Some(node_id) == start_exclusive {
                return false;
            }
            let Some(position) = self.provider_window_position(node_id) else {
                return false;
            };
            if node_id == candidate {
                return true;
            }
            current = position.predecessor.get();
        }
        false
    }

    fn provider_window_contains_node(&self, head: Option<NodeId>, candidate: NodeId) -> bool {
        let Some(position) = head.and_then(|node_id| self.provider_window_position(node_id)) else {
            return false;
        };
        position.replacement_boundary.get() == Some(candidate)
            || self.provider_window_chain_contains(
                self.provider_window_tail(head),
                self.provider_window_start_exclusive(position.replacement_boundary.get()),
                candidate,
            )
    }

    fn append_node_at(&mut self, parent: Option<NodeId>, entry: AgentEntry) -> NodeId {
        let id = NodeId::new(self.nodes.len() as u64);
        let parent_position = parent.and_then(|node_id| self.provider_window_position(node_id));
        let mut predecessor = parent.and_then(|parent| {
            if matches!(
                self.node(parent).map(|node| &node.entry),
                Some(AgentEntry::Compaction { .. })
            ) {
                parent_position.and_then(|position| position.predecessor.get())
            } else {
                Some(parent)
            }
        });
        let mut replacement_boundary =
            parent_position.and_then(|position| position.replacement_boundary.get());
        let mut boundary_anchor = None;
        if let AgentEntry::Compaction {
            transaction_id,
            cut,
            suffix_end,
            ..
        } = &entry
        {
            let previous_start = self.provider_window_start_exclusive(replacement_boundary);
            let start_exclusive = if transaction_id.is_some() && suffix_end.is_some() {
                match cut {
                    Some(AgentHead::Root) => previous_start,
                    Some(AgentHead::Node(cut)) if replacement_boundary == Some(*cut) => {
                        predecessor = None;
                        None
                    }
                    Some(AgentHead::Node(cut))
                        if self.provider_window_chain_contains(
                            predecessor,
                            previous_start,
                            *cut,
                        ) =>
                    {
                        Some(*cut)
                    }
                    Some(AgentHead::Node(_)) | None => {
                        predecessor = None;
                        None
                    }
                }
            } else {
                predecessor = None;
                None
            };
            replacement_boundary = Some(id);
            boundary_anchor = Some(ProviderWindowBoundaryAnchor {
                start_exclusive: ProviderWindowNodeId::new(start_exclusive),
            });
        }
        self.nodes.push(AgentNode {
            id,
            parent_id: parent,
            entry,
        });
        self.indexes
            .provider_window
            .positions
            .push(ProviderWindowPosition {
                predecessor: ProviderWindowNodeId::new(predecessor),
                replacement_boundary: ProviderWindowNodeId::new(replacement_boundary),
            });
        if let Some(anchor) = boundary_anchor {
            assert!(
                self.indexes
                    .provider_window
                    .boundary_anchors
                    .insert(id, anchor)
                    .is_none(),
                "provider-window boundary indexed more than once"
            );
        }
        self.head = Some(id);
        id
    }

    fn append_context_node_at(
        &mut self,
        parent: Option<NodeId>,
        durable_event_seq: PersistedAgentEventSeq,
        entry: AgentEntry,
    ) -> NodeId {
        let node = self.append_node_at(parent, entry);
        assert!(
            self.indexes
                .context_nodes_by_event_seq
                .insert(durable_event_seq, node)
                .is_none(),
            "context occurrence materialized more than once"
        );
        node
    }

    /// Return the single node materialized for a durable context occurrence.
    #[must_use]
    pub fn node_for_durable_event_seq(
        &self,
        durable_event_seq: PersistedAgentEventSeq,
    ) -> Option<NodeId> {
        self.indexes
            .context_nodes_by_event_seq
            .get(&durable_event_seq)
            .copied()
    }

    /// Return whether an unresolved marked owner holds an activating prompt
    /// fact.
    #[must_use]
    pub fn marked_inference_has_deferred_prompt_activation(
        &self,
        owner: &tau_proto::AgentPromptId,
    ) -> bool {
        self.pending_inference_inputs.iter().any(|input| {
            &input.owner_prompt_id == owner
                && matches!(
                    input.entry.as_ref(),
                    AgentEntry::UserInput {
                        inference_activation: true,
                        ..
                    }
                )
        })
    }

    /// Folds a slice of durable agent events into a fresh tree.
    ///
    /// Replay is purely positional: NodeIds are assigned by insertion
    /// order, so the same event slice always yields the same tree.
    /// Events that don't directly produce an agent entry (lifecycle
    /// chatter, harness notice, etc.) are ignored.
    ///
    /// # Panics
    ///
    /// Panics if `events` contains a record that violates the same semantic
    /// event or parent invariants enforced by [`crate::AgentStore`] during
    /// durable replay. Store callers should use the fallible replay path
    /// instead of this convenience constructor.
    #[must_use]
    pub fn from_events(agent_id: AgentId, events: &[PersistedAgentEvent]) -> Self {
        Self::try_from_events(agent_id, events).expect("validated agent events")
    }

    /// Fallibly folds durable agent events into a fresh deterministic tree.
    ///
    /// Returns the first semantic or parent validation error instead of
    /// panicking, allowing recovery classifiers to fail closed on invalid
    /// input.
    pub fn try_from_events(
        agent_id: AgentId,
        events: &[PersistedAgentEvent],
    ) -> Result<Self, AgentEventValidationError> {
        let mut tree = Self {
            agent_id,
            metadata: BTreeMap::new(),
            nodes: Vec::new(),
            indexes: AgentTreeIndexes::default(),
            head: None,
            display_name: None,
            initialization_context: None,
            next_event_seq: PersistedAgentEventSeq::new(0),
            ordinary_inference_generation: InferenceGeneration::default(),
            prompt_starts: HashMap::new(),
            outer_turns: HashMap::new(),
            active_outer_turn: None,
            retained_provider_image_bytes: 0,
            pending_tool_rounds: HashMap::new(),
            terminal_tool_result_nodes: HashMap::new(),
            tool_call_rounds: HashMap::new(),
            pending_context_inputs: Vec::new(),
            background_completed_tool_calls: HashSet::new(),
            compaction_transactions: HashMap::new(),
            compaction_transaction_order: Vec::new(),
            resolved_compaction_failures: HashSet::new(),
            automatic_compaction_decisions: HashMap::new(),
            automatic_compaction_decision_order: Vec::new(),
            manual_compaction_requests: HashMap::new(),
            manual_compaction_request_order: Vec::new(),
            self_compaction_deliveries: HashMap::new(),
            inference_dispatches: HashMap::new(),
            inference_dispatch_order: Vec::new(),
            pending_inference_inputs: Vec::new(),
            head_move_generation: HeadMoveGeneration::default(),
        };
        for record in events {
            tree.apply_persisted_record(record)?;
        }
        Ok(tree)
    }

    /// Returns the sequence the next durable event appended to this
    /// agent's log should receive. Maintained incrementally by
    /// `AgentStore::append_agent_event_at`; on replay,
    /// initialised from the highest persisted event sequence.
    #[must_use]
    pub fn next_event_seq(&self) -> PersistedAgentEventSeq {
        self.next_event_seq
    }

    /// Returns target-owned ordinary inference progress for manual rate guards.
    #[must_use]
    pub fn ordinary_inference_generation(&self) -> tau_proto::MaterializedPromptGeneration {
        self.ordinary_inference_generation
            .materialized_prompt_generation()
    }

    /// Return the exact branch checkpoint for one unresolved V1 inference
    /// owner.
    #[must_use]
    pub fn marked_inference_through(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<AgentHead> {
        self.inference_dispatches
            .get(prompt_id)
            .and_then(|dispatch| {
                (!dispatch.finished
                    && dispatch.fold_semantics
                        == AgentJournalFoldSemantics::InferenceDeferredInputV1)
                    .then_some(dispatch.checkpoint.through)
            })
    }

    /// Return the exact unresolved V1 ordinary inference checkpoint.
    #[must_use]
    pub fn marked_inference_checkpoint(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<&tau_proto::AgentInferenceDispatchStarted> {
        self.inference_dispatches
            .get(prompt_id)
            .filter(|dispatch| {
                !dispatch.finished
                    && dispatch.fold_semantics
                        == AgentJournalFoldSemantics::InferenceDeferredInputV1
            })
            .map(|dispatch| &dispatch.checkpoint)
    }

    /// Return the unique unresolved V1 ordinary inference checkpoint.
    #[must_use]
    pub fn unresolved_marked_inference_checkpoint(
        &self,
    ) -> Option<&tau_proto::AgentInferenceDispatchStarted> {
        let mut unresolved = self.inference_dispatches.values().filter(|dispatch| {
            !dispatch.finished
                && dispatch.fold_semantics == AgentJournalFoldSemantics::InferenceDeferredInputV1
        });
        let checkpoint = &unresolved.next()?.checkpoint;
        unresolved.next().is_none().then_some(checkpoint)
    }

    /// Bumps the cached next-event sequence after one synthetic or persisted
    /// fold.
    fn advance_next_event_seq(&mut self) {
        self.next_event_seq = self.next_event_seq.next();
    }

    /// Incrementally applies one synthetic event and allocates its next
    /// sequence.
    ///
    /// This convenience path mirrors transcript folding but bypasses
    /// durable-record validation and cannot project raw `message.*` facts.
    /// Tree-folding events are parented at the current `head`; for callers
    /// that need to fold an event onto a *specific* branch (without first
    /// emitting an [`AgentHeadMoved`] to bounce `head` there), use
    /// [`AgentTree::apply_event_at`].
    pub fn apply_event(&mut self, event: &Event) {
        self.apply_event_at(AgentEventParent::InheritHead, event);
    }

    /// Like [`AgentTree::apply_event`] but uses an explicit fold-parent policy.
    ///
    /// This synthetic path allocates and advances one sequence while bypassing
    /// durable-record validation and raw-fact projection.
    ///
    /// [`AgentEventParent::InheritHead`] keeps the current single-cursor
    /// behavior. [`AgentEventParent::Root`] starts a fresh branch at the
    /// transcript root without inheriting the current cursor.
    /// [`AgentEventParent::Under`] folds under a specific existing node.
    ///
    /// Returns the id of the node this event produced, or `None` for
    /// events that don't fold (transient lifecycle chatter, an
    /// [`AgentHeadMoved`], etc.). Callers tracking a per-conversation
    /// branch cursor must advance it only when this returns `Some` —
    /// `tree.head()` is the *global* write cursor, so syncing blindly
    /// to it after a non-folding event would steal whichever other
    /// conversation's node the cursor last visited.
    pub fn apply_event_at(&mut self, parent: AgentEventParent, event: &Event) -> Option<NodeId> {
        let node_id = self.apply_persisted_event_at(
            parent,
            event,
            self.next_event_seq,
            AgentJournalFoldSemantics::Legacy,
        );
        self.advance_next_event_seq();
        node_id
    }

    /// Applies one exact durable record through the canonical incremental fold.
    ///
    /// This is the sequence-aware counterpart to [`Self::apply_event_at`].
    /// It validates contiguous journal sequence, handles canonical raw
    /// `message.*` facts, applies ordinary agent events, and advances
    /// [`Self::next_event_seq`] exactly once. Replay and incremental consumers
    /// should use this method instead of discarding
    /// [`PersistedAgentEvent::seq`].
    pub fn apply_persisted_record(
        &mut self,
        record: &PersistedAgentEvent,
    ) -> Result<Option<NodeId>, AgentEventValidationError> {
        self.prevalidate_persisted_record(record)?;
        self.apply_validated_record(record)
    }

    /// Validates one exact durable record and returns a crate-private
    /// capability bound to that same borrowed value and exact tree state.
    pub(crate) fn prevalidate_persisted_record<'tree, 'record>(
        &'tree self,
        record: &'record PersistedAgentEvent,
    ) -> Result<PrevalidatedPersistedRecord<'tree, 'record>, AgentEventValidationError> {
        if record.seq != self.next_event_seq {
            return Err(AgentEventValidationError::new(format!(
                "agent event sequence mismatch: expected {}, got {}",
                self.next_event_seq.get(),
                record.seq.get()
            )));
        }
        if matches!(
            &record.event,
            Event::AgentPromptStarted(_)
                | Event::AgentOuterTurnStarted(_)
                | Event::AgentOuterTurnFinished(_)
        ) && record.source.is_some()
        {
            return Err(AgentEventValidationError::new(
                "agent accounting lifecycle facts must be harness-authored source-free records",
            ));
        }
        if record.fold_semantics != AgentJournalFoldSemantics::Legacy
            && !matches!(record.event, Event::AgentInferenceDispatchStarted(_))
        {
            return Err(AgentEventValidationError::new(
                "non-checkpoint record carries inference-deferred fold semantics",
            ));
        }
        if record.event.name().category() == &tau_proto::EventCategory::Message {
            if record.parent != AgentEventParent::InheritHead {
                return Err(AgentEventValidationError::new(
                    "raw message fact record has a noncanonical fold parent",
                ));
            }
            let target = record.event.message_agent_target().ok_or_else(|| {
                AgentEventValidationError::new("message category record has no agent target")
            })?;
            if target.as_str() != self.agent_id.as_str() {
                return Err(AgentEventValidationError::new(
                    "raw message fact target does not match agent journal owner",
                ));
            }
        } else {
            self.validate_persisted_event(record)?;
        }
        Ok(PrevalidatedPersistedRecord { tree: self, record })
    }

    /// Applies the exact record proven by a crate-private prevalidation token.
    ///
    /// Token construction enforces sequence, source, parent, fold-shape, and
    /// current-tree semantic invariants. Application clones and advances the
    /// token's bound tree, so a caller cannot substitute another tree or
    /// record; a token never authorizes a later or merely
    /// sequence-equivalent state.
    pub(crate) fn apply_prevalidated(
        validated: PrevalidatedPersistedRecord<'_, '_>,
    ) -> Result<(Self, Option<NodeId>), AgentEventValidationError> {
        let mut tree = validated.tree.clone();
        let record = validated.record;
        let node_id = tree.apply_validated_record(record)?;
        Ok((tree, node_id))
    }

    /// Applies one record after validation against this exact tree state.
    fn apply_validated_record(
        &mut self,
        record: &PersistedAgentEvent,
    ) -> Result<Option<NodeId>, AgentEventValidationError> {
        if record.seq != self.next_event_seq {
            return Err(AgentEventValidationError::new(format!(
                "agent event sequence mismatch: expected {}, got {}",
                self.next_event_seq.get(),
                record.seq.get()
            )));
        }
        let node_id = if record.event.name().category() == &tau_proto::EventCategory::Message {
            // Message facts are canonical raw journal records. Their transcript
            // projection is a post-commit consumer and cannot veto the fact.
            tau_proto::project_message_fact(&record.event)
                .and_then(Result::ok)
                .and_then(|projection| {
                    self.record_message_fact(self.head, Box::new(projection.item), record.seq)
                })
        } else {
            self.apply_persisted_event_at(
                record.parent,
                &record.event,
                record.seq,
                record.fold_semantics,
            )
        };
        self.advance_next_event_seq();
        Ok(node_id)
    }

    /// Validate record-local semantics and current-tree append constraints.
    pub(crate) fn validate_persisted_event(
        &self,
        record: &PersistedAgentEvent,
    ) -> Result<(), AgentEventValidationError> {
        self.validate_event_at(record.parent, &record.event)?;
        if !record.fold_semantics.validates(&record.event) {
            return Err(AgentEventValidationError::new(
                "inference-deferred fold semantics require one marked ordinary inference",
            ));
        }
        if record.fold_semantics == AgentJournalFoldSemantics::InferenceDeferredInputV1
            && self.inference_dispatches.values().any(|dispatch| {
                !dispatch.finished
                    && dispatch.fold_semantics
                        == AgentJournalFoldSemantics::InferenceDeferredInputV1
            })
        {
            return Err(AgentEventValidationError::new(
                "overlapping inference-deferred owners are unsupported",
            ));
        }
        match &record.event {
            Event::ProviderResponseFinished(response) => {
                if self
                    .inference_dispatches
                    .get(&response.agent_prompt_id)
                    .is_some_and(|dispatch| {
                        dispatch.fold_semantics
                            == AgentJournalFoldSemantics::InferenceDeferredInputV1
                            && (dispatch.finished
                                || record.parent
                                    != AgentEventParent::from_head(dispatch.checkpoint.through))
                    })
                {
                    return Err(AgentEventValidationError::new(
                        "marked inference response mismatches its exact unresolved owner",
                    ));
                }
            }
            Event::AgentPromptTerminated(terminated) => {
                let Some(dispatch) = self
                    .inference_dispatches
                    .get(&terminated.agent_prompt_id)
                    .filter(|dispatch| {
                        dispatch.fold_semantics
                            == AgentJournalFoldSemantics::InferenceDeferredInputV1
                    })
                else {
                    return Err(AgentEventValidationError::new(
                        "durable prompt termination requires a marked ordinary owner",
                    ));
                };
                if dispatch.finished {
                    return Err(AgentEventValidationError::new(
                        "durable prompt termination duplicated a closed owner",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Applies one already-validated event with its owning journal sequence.
    ///
    /// This low-level helper does not validate the event or sequence and does
    /// not advance `next_event_seq`; canonical record consumers use
    /// [`Self::apply_persisted_record`].
    fn apply_persisted_event_at(
        &mut self,
        parent: AgentEventParent,
        event: &Event,
        durable_event_seq: PersistedAgentEventSeq,
        fold_semantics: AgentJournalFoldSemantics,
    ) -> Option<NodeId> {
        let selected_before = self.head;
        let preserve_selected =
            self.output_length_append_preserves_selection(parent, event, selected_before);
        let resolved_parent = parent.resolve(self.head);
        let output_length_steer_source = match event {
            Event::AgentPromptSteered(steered)
                if steered.internal_kind
                    == Some(tau_proto::InternalPromptKind::OutputLengthContinuation) =>
            {
                self.active_output_length_plan()
                    .filter(|(_, dispatch)| dispatch.output_length_plan_node == resolved_parent)
                    .map(|(prompt_id, _)| prompt_id.clone())
            }
            _ => None,
        };
        self.retained_provider_image_bytes = self
            .retained_provider_image_bytes
            .saturating_add(durable_event_provider_image_bytes(event));
        self.apply_compaction_control_event(event, fold_semantics);
        if let Event::AgentPromptTerminated(terminated) = event
            && let Some(decision) = &terminated.automatic_compaction_decision
        {
            self.automatic_compaction_decision_order
                .push(decision.transaction_id.clone());
            self.automatic_compaction_decisions.insert(
                decision.transaction_id.clone(),
                AutomaticCompactionDecisionFold {
                    decision: decision.clone(),
                    cut: resolved_parent
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                    finish_committed: false,
                    claimed: false,
                    closed: false,
                },
            );
        }
        if self.apply_side_state_event(event) {
            return None;
        }
        let automatic_terminal_node = matches!(
            event,
            Event::ProviderResponseFinished(response)
                if response.automatic_compaction_decision.is_some()
        )
        .then(|| NodeId::new(u64::try_from(self.nodes.len()).expect("agent node count fits u64")));
        let standalone_context_rejection = matches!(
            event,
            Event::ProviderResponseFinished(response)
                 if response.failure_kind
                     == Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
                     && response.stop_reason == tau_proto::ProviderStopReason::Error
                     && response.output_items.is_empty()
                    && self.prompt_starts.get(&response.agent_prompt_id).is_some_and(|started| {
                        started.operation == tau_proto::PromptOperation::StandaloneCompaction
                    })
        );
        let node = (!standalone_context_rejection)
            .then(|| self.apply_transcript_event(resolved_parent, event, durable_event_seq))
            .flatten();
        if let Some(node_id) = node {
            if let Event::ProviderResponseFinished(response) = event
                && let Some(decision) = &response.automatic_compaction_decision
            {
                self.automatic_compaction_decision_order
                    .push(decision.transaction_id.clone());
                self.automatic_compaction_decisions.insert(
                    decision.transaction_id.clone(),
                    AutomaticCompactionDecisionFold {
                        decision: decision.clone(),
                        cut: tau_proto::AgentHead::Node(
                            automatic_terminal_node
                                .expect("decision response appends its assistant node first"),
                        ),
                        finish_committed: false,
                        claimed: false,
                        closed: false,
                    },
                );
            }
            if let Event::ProviderResponseFinished(response) = event
                && let Some(dispatch) = self.inference_dispatches.get_mut(&response.agent_prompt_id)
            {
                dispatch.response_node = Some(node_id);
                dispatch.rearms_output_length =
                    output_length_response_rearms_budget(&dispatch.checkpoint, response);
            }
            match event {
                Event::ProviderResponseFinished(response)
                    if matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                    ) =>
                {
                    if let Some(dispatch) =
                        self.inference_dispatches.get_mut(&response.agent_prompt_id)
                    {
                        dispatch.output_length_plan_node = Some(node_id);
                    }
                }
                Event::AgentPromptSteered(steered)
                    if steered.internal_kind
                        == Some(tau_proto::InternalPromptKind::OutputLengthContinuation) =>
                {
                    if let Some(source_prompt_id) = output_length_steer_source
                        && let Some(dispatch) = self.inference_dispatches.get_mut(&source_prompt_id)
                    {
                        dispatch.output_length_steer_node = Some(node_id);
                    }
                }
                _ => {}
            }
        }
        if let (Some(node_id), Event::AgentPromptSteered(steered)) = (node, event)
            && let Some(terminal) = &steered.self_compaction_terminal
        {
            self.self_compaction_deliveries.insert(
                terminal.request_id.clone(),
                SelfCompactionDelivery {
                    terminal: terminal.clone(),
                    node_id,
                },
            );
        }
        if preserve_selected {
            self.head = selected_before;
        }
        node
    }

    /// Exact off-selected output-length repair and already-dispatched terminal
    /// facts extend their dormant parent without changing branch selection.
    fn output_length_append_preserves_selection(
        &self,
        parent: AgentEventParent,
        event: &Event,
        selected: Option<NodeId>,
    ) -> bool {
        if parent == AgentEventParent::InheritHead {
            return false;
        }
        let parent_head = match parent {
            AgentEventParent::InheritHead => unreachable!("returned above"),
            AgentEventParent::Root => tau_proto::AgentHead::Root,
            AgentEventParent::Under(node) => tau_proto::AgentHead::Node(node),
        };
        let selected_head = selected
            .map(tau_proto::AgentHead::Node)
            .unwrap_or(tau_proto::AgentHead::Root);
        if self.is_ancestor_head(parent_head, selected_head) {
            return false;
        }
        let matches_repair = match (self.output_length_dormant_repair(), event) {
            (
                Some(OutputLengthDormantRepair::Steer {
                    parent: expected, ..
                }),
                Event::AgentPromptSteered(steered),
            ) => {
                expected == parent_head
                    && steered.internal_kind
                        == Some(tau_proto::InternalPromptKind::OutputLengthContinuation)
            }
            (
                Some(OutputLengthDormantRepair::Owner {
                    through: expected, ..
                }),
                Event::AgentInferenceDispatchStarted(owner),
            ) => expected == parent_head && owner.output_length_continuation.is_some(),
            (
                Some(OutputLengthDormantRepair::Terminal {
                    parent: expected, ..
                }),
                Event::ProviderResponseFinished(response),
            ) => {
                expected == parent_head
                    && matches!(
                        response.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationTerminal {
                            outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                            ..
                        }
                    )
            }
            (
                Some(OutputLengthDormantRepair::Finish {
                    parent: expected, ..
                }),
                Event::AgentOuterTurnFinished(_),
            ) => expected == parent_head,
            _ => false,
        };
        matches_repair
            || matches!(
                event,
                Event::ProviderResponseFinished(response)
                    if (matches!(
                            response.output_length_disposition,
                            tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
                        )
                        || response.recovery_disposition
                            == tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned)
                        && self
                            .output_length_lineage_owner_for_prompt(&response.agent_prompt_id)
                            .is_some()
            )
    }

    fn apply_compaction_control_event(
        &mut self,
        event: &Event,
        fold_semantics: AgentJournalFoldSemantics,
    ) {
        match event {
            Event::AgentManualCompactionRequested(requested) => {
                self.manual_compaction_request_order
                    .push(requested.request_id.clone());
                self.manual_compaction_requests.insert(
                    requested.request_id.clone(),
                    ManualCompactionRequestFold {
                        requested: requested.clone(),
                        state: ManualCompactionRequestState::Waiting,
                    },
                );
            }
            Event::AgentManualCompactionRequestFailed(failed) => {
                if let Some(request) = self.manual_compaction_requests.get_mut(&failed.request_id) {
                    request.state = ManualCompactionRequestState::Failed(failed.clone());
                }
            }
            Event::AgentManualCompactionRequestSatisfied(satisfied) => {
                if let Some(request) = self
                    .manual_compaction_requests
                    .get_mut(&satisfied.request_id)
                {
                    request.state =
                        ManualCompactionRequestState::Satisfied(satisfied.transaction_id.clone());
                }
            }
            Event::AgentStandaloneCompactionStarted(started) => {
                self.compaction_transaction_order
                    .push(started.transaction_id.clone());
                self.compaction_transactions.insert(
                    started.transaction_id.clone(),
                    CompactionTransactionFold {
                        started: started.clone(),
                        outcome: None,
                        checkpoint: None,
                        inference_finished: false,
                        standalone_context_rejection: None,
                    },
                );
                let decision_id = match &started.trigger {
                    tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { decision_id } => {
                        Some(decision_id)
                    }
                    tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                        decision_id: Some(decision_id),
                        ..
                    } => Some(decision_id),
                    _ => None,
                };
                if let Some(decision_id) = decision_id
                    && let Some(decision) = self.automatic_compaction_decisions.get_mut(decision_id)
                {
                    decision.claimed = true;
                }
                if let tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                    request_id, ..
                } = &started.trigger
                    && let Some(request) = self.manual_compaction_requests.get_mut(request_id)
                {
                    request.state =
                        ManualCompactionRequestState::Started(started.transaction_id.clone());
                }
                if let tau_proto::StandaloneCompactionTrigger::ManualUi { request_id } =
                    &started.trigger
                    && let Some(request) = self.manual_compaction_requests.get_mut(request_id)
                {
                    request.state =
                        ManualCompactionRequestState::Started(started.transaction_id.clone());
                }
            }
            Event::AgentStandaloneCompactionFailed(failed) => {
                if let Some(transaction) =
                    self.compaction_transactions.get_mut(&failed.transaction_id)
                {
                    transaction.outcome =
                        Some(CompactionTransactionOutcome::Failed(failed.clone()));
                } else if let Some(decision) = self
                    .automatic_compaction_decisions
                    .get_mut(&failed.transaction_id)
                {
                    decision.closed = true;
                }
            }
            Event::AgentCompacted(compacted) => {
                if let Some(transaction_id) = &compacted.transaction_id {
                    let Some(transaction) = self.compaction_transactions.get_mut(transaction_id)
                    else {
                        return;
                    };
                    transaction.outcome =
                        Some(CompactionTransactionOutcome::Succeeded(compacted.clone()));
                    let mut superseded = transaction.started.supersedes.clone();
                    let clear_matching = superseded.as_ref().map(|_| {
                        (
                            transaction.started.model.clone(),
                            transaction
                                .started
                                .resume_through
                                .unwrap_or(transaction.started.cut),
                        )
                    });
                    while let Some(failed_id) = superseded {
                        if !self.resolved_compaction_failures.insert(failed_id.clone()) {
                            break;
                        }
                        superseded = self
                            .compaction_transactions
                            .get(&failed_id)
                            .and_then(|failed| failed.started.supersedes.clone());
                    }
                    if let Some((model, through)) = clear_matching {
                        while let Some(failed_id) = self
                            .unresolved_standalone_compaction_failure(&model, through)
                            .map(|failed| failed.transaction_id.clone())
                        {
                            self.resolved_compaction_failures.insert(failed_id);
                        }
                    }
                }
            }
            Event::AgentInferenceDispatchStarted(checkpoint) => {
                self.inference_dispatch_order
                    .push(checkpoint.agent_prompt_id.clone());
                self.inference_dispatches.insert(
                    checkpoint.agent_prompt_id.clone(),
                    InferenceDispatchFold {
                        checkpoint: checkpoint.clone(),
                        fold_semantics,
                        head_move_generation: self.head_move_generation,
                        finished: false,
                        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                        output_length_disposition: tau_proto::OutputLengthDisposition::None,
                        provider_attempt: None,
                        provider_stop_reason: None,
                        provider_input_tokens: None,
                        rearms_output_length: false,
                        output_length_plan_node: None,
                        output_length_steer_node: None,
                        response_node: None,
                    },
                );
                if let Some(transaction_id) = &checkpoint.transaction_id
                    && let Some(transaction) = self.compaction_transactions.get_mut(transaction_id)
                {
                    transaction.checkpoint = Some(checkpoint.clone());
                }
            }
            Event::AgentPromptStarted(started) => {
                self.prompt_starts
                    .insert(started.agent_prompt_id.clone(), started.clone());
                if started.operation == tau_proto::PromptOperation::Inference {
                    self.ordinary_inference_generation =
                        self.ordinary_inference_generation.saturating_next();
                }
            }
            Event::ProviderResponseFinished(response) => {
                if let Some(dispatch) = self.inference_dispatches.get_mut(&response.agent_prompt_id)
                {
                    dispatch.finished = true;
                    dispatch.recovery_disposition = response.recovery_disposition;
                    dispatch.output_length_disposition = response.output_length_disposition.clone();
                    dispatch.provider_attempt = Some(response.provider_attempt);
                    dispatch.provider_stop_reason = Some(response.stop_reason);
                    dispatch.provider_input_tokens = response.usage.as_ref().and_then(|usage| {
                        (usage.prompt_sent_tokens > 0)
                            .then(|| tau_proto::TokenCount::new(usage.prompt_sent_tokens))
                    });
                }
                for transaction in self.compaction_transactions.values_mut() {
                    if transaction.started.compact_prompt_id == response.agent_prompt_id
                        && response.failure_kind
                            == Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
                        && response.stop_reason == tau_proto::ProviderStopReason::Error
                        && response.output_items.is_empty()
                    {
                        transaction.standalone_context_rejection = Some(response.clone());
                    }
                    if transaction.checkpoint.as_ref().is_some_and(|checkpoint| {
                        checkpoint.agent_prompt_id == response.agent_prompt_id
                    }) {
                        transaction.inference_finished = true;
                    }
                }
            }
            Event::AgentPromptTerminated(terminated) => {
                if let Some(dispatch) = self
                    .inference_dispatches
                    .get_mut(&terminated.agent_prompt_id)
                {
                    dispatch.finished = true;
                }
            }
            _ => {}
        }
    }

    fn apply_side_state_event(&mut self, event: &Event) -> bool {
        match event {
            Event::AgentStarted(started) => self.apply_agent_started(started),
            Event::AgentUserInteractionRecorded(_) => {}
            Event::AgentOuterTurnStarted(started) => {
                self.outer_turns.insert(
                    started.outer_turn_id.clone(),
                    OuterTurnFold {
                        session_id: started.session_id.clone(),
                        finished: false,
                        runtime_id: started.runtime_id.clone(),
                        agent_prompt_id: started.agent_prompt_id.clone(),
                    },
                );
                self.active_outer_turn = Some(started.outer_turn_id.clone());
            }
            Event::AgentOuterTurnFinished(finished) => {
                if let Some(turn) = self.outer_turns.get_mut(&finished.outer_turn_id) {
                    turn.finished = true;
                }
                if let Some(id) = &finished.automatic_compaction_decision
                    && let Some(decision) = self.automatic_compaction_decisions.get_mut(id)
                {
                    decision.finish_committed = true;
                }
                self.active_outer_turn = None;
            }
            Event::AgentDisplayNameSet(name) => self.update_display_name(&name.display_name),
            Event::AgentInitializationContextSet(context) => {
                self.initialization_context = Some(context.clone());
            }
            Event::AgentMetadataSet(set) => {
                self.metadata.insert(
                    set.key.clone(),
                    AgentMetadataEntry {
                        value: set.value.clone(),
                        inheritable: set.inheritable,
                    },
                );
            }
            Event::AgentMetadataUnset(unset) => {
                self.metadata.remove(&unset.key);
            }
            Event::AgentHeadMoved(moved) => self.apply_head_moved(moved),
            Event::ToolRequest(_)
            | Event::ToolStarted(_)
            | Event::ToolRejected(_)
            | Event::ToolResult(_)
            | Event::ToolError(_) => {}
            Event::ToolBackgroundResult(result) => {
                self.background_completed_tool_calls
                    .insert(result.call_id.clone());
            }
            Event::ToolBackgroundError(error) => {
                self.background_completed_tool_calls
                    .insert(error.call_id.clone());
            }
            _ => return false,
        }
        true
    }

    fn apply_agent_started(&mut self, started: &tau_proto::AgentStarted) {
        if let Some(display_name) = started.display_name.as_deref() {
            self.update_display_name(display_name);
        }
        for item in &started.metadata {
            self.metadata.insert(
                item.key.clone(),
                AgentMetadataEntry {
                    value: item.value.clone(),
                    inheritable: item.inheritable,
                },
            );
        }
    }

    fn update_display_name(&mut self, display_name: &str) {
        if let Some(display_name) = normalize_display_name(Some(display_name)) {
            self.display_name = Some(display_name);
        }
    }

    fn apply_head_moved(&mut self, moved: &AgentHeadMoved) {
        if moved.agent_id == self.agent_id && self.validate_head_moved(moved).is_ok() {
            self.head = moved.head.as_option();
            self.head_move_generation = self.head_move_generation.saturating_next();
        }
    }

    fn apply_transcript_event(
        &mut self,
        parent: Option<NodeId>,
        event: &Event,
        durable_event_seq: PersistedAgentEventSeq,
    ) -> Option<NodeId> {
        match event {
            Event::AgentPromptSubmitted(prompt) => self.record_context_entry(
                parent,
                Self::user_text_input_entry(
                    prompt.text.clone(),
                    &prompt.trusted_internal_spans,
                    Some(prompt.submission_source.clone()),
                    prompt.inference_activation,
                ),
                durable_event_seq,
            ),
            Event::AgentUserMessageInjected(injected) => self.record_context_entry(
                parent,
                Self::user_text_input_entry(
                    injected.text.clone(),
                    &[],
                    None,
                    injected.inference_activation,
                ),
                durable_event_seq,
            ),
            Event::AgentPromptSteered(steered) => self.record_context_entry(
                parent,
                Self::user_text_input_entry(
                    steered.text.clone(),
                    &steered.trusted_internal_spans,
                    Some(steered.submission_source.clone()),
                    steered.inference_activation,
                ),
                durable_event_seq,
            ),
            Event::AgentCompactionTriggered(triggered) => Some(self.append_node_at(
                parent,
                AgentEntry::CompactionTrigger {
                    resume_inference: triggered.resume_inference,
                },
            )),
            Event::AgentCompacted(compacted) => Some(self.append_node_at(
                parent,
                AgentEntry::Compaction {
                    replacement_window: compacted.replacement_window.clone(),
                    transaction_id: compacted.transaction_id.clone(),
                    cut: compacted.cut,
                    suffix_end: compacted.suffix_end,
                },
            )),
            Event::AgentMessageSent(message) => self
                .agent_message_entry_from_sent(message, durable_event_seq)
                .and_then(|entry| self.record_context_entry(parent, entry, durable_event_seq)),
            Event::AgentMessageReceived(message) => self
                .agent_message_entry_from_received(message, durable_event_seq)
                .and_then(|entry| self.record_context_entry(parent, entry, durable_event_seq)),
            Event::ProviderResponseFinished(response) => {
                Some(self.apply_provider_response_finished(parent, response))
            }
            Event::AgentPromptTerminated(terminated) => {
                self.close_terminated_inference(&terminated.agent_prompt_id)
            }
            Event::ProviderToolResult(result) => self.record_provider_tool_result(result),
            Event::ProviderToolError(error) => self.record_provider_tool_error(error),
            Event::ToolCancelled(cancelled) => self.record_cancelled_tool_result(cancelled),
            _ => None,
        }
    }

    fn user_text_input_entry(
        text: String,
        trusted_internal_spans: &[tau_proto::TrustedInternalSpan],
        submission_source: Option<tau_proto::PromptSubmissionSource>,
        inference_activation: bool,
    ) -> AgentEntry {
        AgentEntry::UserInput {
            items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::User,
                content: Self::prompt_content_parts(text, trusted_internal_spans),
                phase: None,
                responses_raw_json: None,
            })],
            submission_source,
            inference_activation,
        }
    }

    /// Split trusted harness spans from ordinary prompt bytes without allowing
    /// a malformed durable range to manufacture authority.
    fn prompt_content_parts(
        text: String,
        spans: &[tau_proto::TrustedInternalSpan],
    ) -> Vec<ContentPart> {
        if check_trusted_internal_spans(&text, spans).is_err() || spans.is_empty() {
            return vec![ContentPart::Text { text }];
        }
        let mut parts = Vec::new();
        let mut offset = 0_usize;
        for span in spans {
            let start = span.start as usize;
            let end = span.end as usize;
            if offset < start {
                parts.push(ContentPart::Text {
                    text: text[offset..start].to_owned(),
                });
            }
            if start < end {
                parts.push(ContentPart::HarnessInternalText {
                    text: text[start..end].to_owned(),
                });
            }
            offset = end;
        }
        if offset < text.len() || parts.is_empty() {
            parts.push(ContentPart::Text {
                text: text[offset..].to_owned(),
            });
        }
        parts
    }

    /// Fold one canonical context occurrence or defer it behind its exact
    /// owner.
    fn record_context_entry(
        &mut self,
        parent: Option<NodeId>,
        entry: AgentEntry,
        durable_event_seq: PersistedAgentEventSeq,
    ) -> Option<NodeId> {
        if self.open_tool_round_applies_to(parent) {
            self.pending_context_inputs.push(PendingToolContextInput {
                durable_event_seq,
                entry: Box::new(entry),
            });
            return None;
        }
        if let Some(owner_prompt_id) = self.marked_inference_owner_for(parent) {
            let virtual_predecessor_seq = self
                .pending_inference_inputs
                .iter()
                .rev()
                .find(|input| {
                    input.owner_prompt_id == owner_prompt_id && input.accepted_real_parent == parent
                })
                .map(|input| input.durable_event_seq);
            self.pending_inference_inputs.push(PendingInferenceInput {
                owner_prompt_id,
                durable_event_seq,
                accepted_real_parent: parent,
                virtual_predecessor_seq,
                entry: Box::new(entry),
            });
            return None;
        }
        Some(self.append_context_node_at(parent, durable_event_seq, entry))
    }

    /// Fold one valid committed message fact behind tool adjacency when needed.
    fn record_message_fact(
        &mut self,
        parent: Option<NodeId>,
        item: Box<tau_proto::MessageItem>,
        durable_event_seq: PersistedAgentEventSeq,
    ) -> Option<NodeId> {
        self.record_context_entry(
            parent,
            AgentEntry::MessageFact {
                item,
                durable_event_seq,
            },
            durable_event_seq,
        )
    }

    /// Return the unique unresolved V1 ordinary inference applicable to
    /// `parent`.
    fn marked_inference_owner_for(
        &self,
        parent: Option<NodeId>,
    ) -> Option<tau_proto::AgentPromptId> {
        let parent = parent?;
        let mut owners = self.inference_dispatches.values().filter(|dispatch| {
            !dispatch.finished
                && dispatch.fold_semantics == AgentJournalFoldSemantics::InferenceDeferredInputV1
                && dispatch.head_move_generation == self.head_move_generation
                && self.is_ancestor_head(
                    dispatch.checkpoint.through,
                    tau_proto::AgentHead::Node(parent),
                )
        });
        let owner = owners.next()?;
        owners
            .next()
            .is_none()
            .then(|| owner.checkpoint.agent_prompt_id.clone())
    }

    /// Returns whether the sole open foreground round owns context accepted
    /// under `parent`.
    ///
    /// Root inputs, inputs above the tool-calling assistant, and sibling-branch
    /// inputs materialize immediately. Only the assistant itself and its
    /// descendants defer until the aggregate tool result closes the round.
    fn open_tool_round_applies_to(&self, parent: Option<NodeId>) -> bool {
        let Some(parent) = parent else {
            return false;
        };
        let Some(assistant_node_id) = self.pending_tool_rounds.keys().next().copied() else {
            return false;
        };
        self.is_ancestor_head(AgentHead::Node(assistant_node_id), AgentHead::Node(parent))
    }

    fn apply_provider_response_finished(
        &mut self,
        parent: Option<NodeId>,
        response: &tau_proto::ProviderResponseFinished,
    ) -> NodeId {
        let selected_head = self.head;
        let node_id = self.append_node_at(
            parent,
            AgentEntry::AssistantResponse {
                provider_response_id: response.provider_response_id.clone(),
                backend: response.backend.clone(),
                output_items: response.output_items.clone(),
                usage: response.usage.clone(),
            },
        );
        let call_order = self.provider_response_tool_call_order(&response.output_items);
        let mut owned = self.take_pending_inference_inputs(&response.agent_prompt_id);
        if call_order.is_empty() {
            let tail = self.materialize_after(node_id, &mut owned);
            if selected_head != parent {
                self.head = selected_head;
            }
            return tail;
        }
        self.open_pending_tool_round(node_id, call_order);
        self.pending_context_inputs
            .extend(owned.drain(..).map(PendingToolContextInput::from));
        if selected_head != parent {
            self.head = selected_head;
        }
        node_id
    }

    /// Remove one inference owner's pending inputs in durable order.
    fn take_pending_inference_inputs(
        &mut self,
        owner: &tau_proto::AgentPromptId,
    ) -> Vec<PendingInferenceInput> {
        let (mut owned, retained): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.pending_inference_inputs)
                .into_iter()
                .partition(|input| &input.owner_prompt_id == owner);
        self.pending_inference_inputs = retained;
        owned.sort_by_key(|input| input.durable_event_seq.get());
        owned
    }

    /// Append pending typed entries as one sequence-ordered chain.
    fn materialize_after(
        &mut self,
        mut head: NodeId,
        inputs: &mut Vec<PendingInferenceInput>,
    ) -> NodeId {
        inputs.sort_by_key(|input| input.durable_event_seq.get());
        for input in inputs.drain(..) {
            head = self.append_context_node_at(Some(head), input.durable_event_seq, *input.entry);
        }
        head
    }

    /// Close a no-response owner and materialize each pending virtual branch.
    fn close_terminated_inference(&mut self, owner: &tau_proto::AgentPromptId) -> Option<NodeId> {
        let selected_head = self.head;
        let mut owned = self.take_pending_inference_inputs(owner);
        let mut materialized = HashMap::new();
        let mut last = None;
        let mut selected_seq = None;
        let mut selected_tail = None;
        for input in owned.drain(..) {
            let advances_selected = input.accepted_real_parent == selected_head
                || input.virtual_predecessor_seq == selected_seq;
            let parent = input
                .virtual_predecessor_seq
                .and_then(|seq| materialized.get(&seq).copied())
                .or(input.accepted_real_parent);
            let node = self.append_context_node_at(parent, input.durable_event_seq, *input.entry);
            materialized.insert(input.durable_event_seq, node);
            if advances_selected {
                selected_seq = Some(input.durable_event_seq);
                selected_tail = Some(node);
            }
            last = Some(node);
        }
        self.head = selected_tail.or(selected_head);
        last
    }

    fn provider_response_tool_call_order(&self, output_items: &[ContextItem]) -> Vec<ToolCallId> {
        let mut call_order = Vec::new();
        let mut seen = HashSet::new();
        for item in output_items {
            let ContextItem::ToolCall(call) = item else {
                continue;
            };
            assert!(
                seen.insert(call.call_id.clone()),
                "duplicate tool call id in agent response: {}",
                call.call_id
            );
            assert!(
                !self.tool_call_rounds.contains_key(&call.call_id),
                "tool call id reused while a round is open: {}",
                call.call_id
            );
            call_order.push(call.call_id.clone());
        }
        call_order
    }

    fn open_pending_tool_round(&mut self, node_id: NodeId, call_order: Vec<ToolCallId>) {
        if call_order.is_empty() {
            return;
        }
        assert!(
            self.pending_tool_rounds.is_empty(),
            "cannot open a second foreground provider tool round"
        );
        for call_id in &call_order {
            self.tool_call_rounds.insert(call_id.clone(), node_id);
        }
        self.pending_tool_rounds.insert(
            node_id,
            PendingToolRound {
                assistant_node_id: node_id,
                call_order,
                terminal_results: HashMap::new(),
            },
        );
    }

    fn record_provider_tool_result(&mut self, result: &tau_proto::ToolResult) -> Option<NodeId> {
        self.record_terminal_tool_result(ToolResultItem {
            call_id: result.call_id.clone(),
            tool_type: result.tool_type,
            status: ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&result.result),
            presentation: result.presentation,
            provider_content: result.provider_content.clone(),
        })
    }

    fn record_provider_tool_error(&mut self, error: &tau_proto::ToolError) -> Option<NodeId> {
        self.record_terminal_tool_result(ToolResultItem {
            call_id: error.call_id.clone(),
            tool_type: error.tool_type,
            status: ToolResultStatus::Error {
                message: error.message.clone(),
            },
            output: tau_proto::ToolResponse::from_cbor(
                error
                    .details
                    .as_ref()
                    .unwrap_or(&tau_proto::CborValue::Null),
            ),
            presentation: error.presentation,
            provider_content: Vec::new(),
        })
    }

    fn record_cancelled_tool_result(
        &mut self,
        cancelled: &tau_proto::ToolCancelled,
    ) -> Option<NodeId> {
        self.record_terminal_tool_result(ToolResultItem {
            call_id: cancelled.call_id.clone(),
            tool_type: cancelled.tool_type,
            status: ToolResultStatus::Cancelled {
                reason: "cancelled".to_owned(),
            },
            output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Null),
            presentation: cancelled.presentation,
            provider_content: Vec::new(),
        })
    }

    fn agent_message_entry_from_sent(
        &self,
        message: &AgentMessageSent,
        durable_event_seq: PersistedAgentEventSeq,
    ) -> Option<AgentEntry> {
        (message.sender_id == self.agent_id).then(|| AgentEntry::AgentMessage {
            durable_event_seq,
            message_id: message.message_id.clone(),
            direction: AgentMessageDirection::Outbound,
            sender_id: message.sender_id.clone(),
            sender_session_id: None,
            recipient: message.recipient.clone(),
            kind: message.kind,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            watch_lifecycle: None,
            message: message.message.clone(),
        })
    }

    fn agent_message_entry_from_received(
        &self,
        message: &AgentMessageReceived,
        durable_event_seq: PersistedAgentEventSeq,
    ) -> Option<AgentEntry> {
        (message.recipient_id == self.agent_id).then(|| AgentEntry::AgentMessage {
            durable_event_seq,
            message_id: message.message_id.clone(),
            direction: AgentMessageDirection::Inbound,
            sender_id: message.sender_id.clone(),
            sender_session_id: message.sender_session_id.clone(),
            recipient: AgentMessageRecipient::Agent {
                agent_id: message.recipient_id.clone(),
            },
            kind: message.kind,
            watch_provider_status: message.watch_provider_status.clone(),
            watch_work_status: message.watch_work_status.clone().map(Box::new),
            watch_long_wait: message.watch_long_wait.clone().map(Box::new),
            watch_lifecycle: message.watch_lifecycle.clone().map(Box::new),
            message: message.message.clone(),
        })
    }

    /// Validate an explicit fold parent before persisting an event.
    ///
    /// [`AgentEventParent::Under`] must reference an existing node in this
    /// agent tree; otherwise replay would preserve a dangling parent pointer
    /// and later branch assembly would silently truncate history.
    pub fn validate_event_parent(
        &self,
        parent: AgentEventParent,
    ) -> Result<(), AgentEventValidationError> {
        if let AgentEventParent::Under(node_id) = parent
            && self.node(node_id).is_none()
        {
            return Err(AgentEventValidationError::new(format!(
                "agent event parent referenced unknown node_id: {node_id}"
            )));
        }
        Ok(())
    }

    /// Validate an event against the current transcript fold state before
    /// appending it to the durable log.
    pub fn validate_event(&self, event: &Event) -> Result<(), AgentEventValidationError> {
        self.validate_event_for_head(self.head, event)
    }

    /// Validate an event and explicit fold parent against the current
    /// transcript fold state before appending it to the durable log.
    pub fn validate_event_at(
        &self,
        parent: AgentEventParent,
        event: &Event,
    ) -> Result<(), AgentEventValidationError> {
        self.validate_event_parent(parent)?;
        self.validate_event_for_head(parent.resolve(self.head), event)
    }

    fn validate_event_for_head(
        &self,
        head: Option<NodeId>,
        event: &Event,
    ) -> Result<(), AgentEventValidationError> {
        let retained_provider_image_bytes = self
            .retained_provider_image_bytes
            .checked_add(durable_event_provider_image_bytes(event))
            .ok_or_else(|| {
                AgentEventValidationError::new("retained provider image byte count overflow")
            })?;
        if MAX_RETAINED_PROVIDER_IMAGE_BYTES_PER_AGENT < retained_provider_image_bytes {
            return Err(AgentEventValidationError::new(format!(
                "retained provider image bytes exceed per-agent limit of \
                 {MAX_RETAINED_PROVIDER_IMAGE_BYTES_PER_AGENT}"
            )));
        }
        if let Some(result) = self.validate_agent_state_event(event) {
            return result;
        }
        if let Some(result) = self.validate_agent_message_event(event) {
            return result;
        }
        if let Some(result) = self.validate_agent_fold_event(head, event) {
            return result;
        }
        if let Some(result) = self.validate_tool_completion_event(head, event) {
            return result;
        }
        if Self::is_tool_observation_event(event) {
            return Ok(());
        }
        if Self::is_agent_id_mismatch_event(event) {
            return Err(AgentEventValidationError::new(
                "agent event agent_id did not match target agent",
            ));
        }
        Err(AgentEventValidationError::new(
            "agent store only persists agent transcript events",
        ))
    }

    fn validate_agent_state_event(
        &self,
        event: &Event,
    ) -> Option<Result<(), AgentEventValidationError>> {
        match event {
            Event::AgentStarted(started) if started.agent_id == self.agent_id => {
                Some(validate_optional_display_name(&started.display_name))
            }
            Event::AgentUserInteractionRecorded(interaction)
                if interaction.agent_id == self.agent_id =>
            {
                Some(Ok(()))
            }
            Event::AgentDisplayNameSet(name) if name.agent_id == self.agent_id => {
                Some(validate_display_name(&name.display_name))
            }
            Event::AgentMetadataSet(set) if set.agent_id == self.agent_id => Some(Ok(())),
            Event::AgentMetadataUnset(unset) if unset.agent_id == self.agent_id => Some(Ok(())),
            Event::AgentOuterTurnStarted(turn) if turn.agent_id == self.agent_id => {
                Some(if self.outer_turns.contains_key(&turn.outer_turn_id) {
                    Err(AgentEventValidationError::new(
                        "outer turn start duplicates an existing turn identity",
                    ))
                } else if self
                    .active_outer_turn
                    .as_ref()
                    .and_then(|active| self.outer_turns.get(active))
                    .is_some_and(|active| active.runtime_id == turn.runtime_id)
                {
                    Err(AgentEventValidationError::new(
                        "outer turn start overlaps another turn in this runtime",
                    ))
                } else if turn.outer_turn_id
                    != tau_proto::AgentOuterTurnId::for_prompt(&turn.agent_prompt_id)
                {
                    Err(AgentEventValidationError::new(
                        "outer turn identity does not match its inference prompt",
                    ))
                } else if !matches!(
                    self.inference_dispatches.get(&turn.agent_prompt_id),
                    Some(dispatch)
                        if dispatch.checkpoint.operation
                            == Some(tau_proto::PromptOperation::Inference)
                            && !dispatch.finished
                ) {
                    Err(AgentEventValidationError::new(
                        "outer turn has no matching unresolved inference checkpoint",
                    ))
                } else if let tau_proto::AgentOuterTurnActivation::Journal {
                    occurrence: claimed,
                } = turn.activation
                    && self
                        .inference_dispatches
                        .get(&turn.agent_prompt_id)
                        .and_then(|dispatch| {
                            let checkpoint = &dispatch.checkpoint;
                            (checkpoint.operation == Some(tau_proto::PromptOperation::Inference))
                                .then_some(checkpoint)
                        })
                        .and_then(|checkpoint| {
                            let path = self.branch_node_ids_from(match checkpoint.through {
                                tau_proto::AgentHead::Root => None,
                                tau_proto::AgentHead::Node(node) => Some(node),
                            });
                            match checkpoint.activation_cut? {
                                tau_proto::AgentHead::Root => path.first().copied(),
                                tau_proto::AgentHead::Node(cut) => path
                                    .iter()
                                    .position(|candidate| *candidate == cut)
                                    .and_then(|index| path.get(index.saturating_add(1)).copied()),
                            }
                        })
                        .map(tau_proto::AgentHead::Node)
                        != Some(claimed)
                {
                    Err(AgentEventValidationError::new(
                        "outer turn activation does not match its inference checkpoint",
                    ))
                } else if matches!(
                    turn.activation,
                    tau_proto::AgentOuterTurnActivation::Journal {
                        occurrence: tau_proto::AgentHead::Root
                    }
                ) {
                    Err(AgentEventValidationError::new(
                        "journal activation correlation must identify an occurrence",
                    ))
                } else if let tau_proto::AgentOuterTurnActivation::Journal {
                    occurrence: tau_proto::AgentHead::Node(node),
                } = turn.activation
                    && self.node(node).is_none()
                {
                    Err(AgentEventValidationError::new(
                        "outer turn activation occurrence is absent from the journal",
                    ))
                } else {
                    // A prior unmatched start is an allowed crash cut. A new
                    // unique identity starts the next runtime without rewriting
                    // that explicitly unterminated historical fact.
                    Ok(())
                })
            }
            Event::AgentOuterTurnFinished(turn) if turn.agent_id == self.agent_id => Some(
                match (
                    self.active_outer_turn.as_ref(),
                    self.outer_turns.get(&turn.outer_turn_id),
                ) {
                    (Some(active), Some(fold))
                        if active == &turn.outer_turn_id
                            && fold.session_id == turn.session_id
                            && !fold.finished
                            && self
                                .automatic_compaction_decisions
                                .values()
                                .find(|decision| {
                                    !decision.finish_committed
                                        && decision.decision.outer_turn_id == turn.outer_turn_id
                                })
                                .map(|decision| &decision.decision.transaction_id)
                                == turn.automatic_compaction_decision.as_ref() =>
                    {
                        Ok(())
                    }
                    _ => Err(AgentEventValidationError::new(
                        "outer turn finish has no matching open start",
                    )),
                },
            ),
            _ => None,
        }
    }

    fn validate_agent_message_event(
        &self,
        event: &Event,
    ) -> Option<Result<(), AgentEventValidationError>> {
        match event {
            Event::AgentMessageSent(message)
                if self
                    .agent_message_entry_from_sent(message, self.next_event_seq)
                    .is_some() =>
            {
                Some(Ok(()))
            }
            Event::AgentMessageReceived(message)
                if self
                    .agent_message_entry_from_received(message, self.next_event_seq)
                    .is_some() =>
            {
                let payload_matches_kind = ((message.kind
                    == AgentMessageKind::WatchProviderStatus)
                    == message.watch_provider_status.is_some())
                    && ((message.kind == AgentMessageKind::WatchWorkStatus)
                        == message.watch_work_status.is_some())
                    && ((message.kind == AgentMessageKind::WatchLongWait)
                        == message.watch_long_wait.is_some())
                    && ((message.kind == AgentMessageKind::WatchLifecycle)
                        == message.watch_lifecycle.is_some());
                let work_status_shape_valid =
                    message.watch_work_status.as_ref().is_none_or(|status| {
                        (status.phase == tau_proto::AgentWorkStatusPhase::Unreported)
                            == status.title.is_none()
                    });
                let work_status_title_valid =
                    message.watch_work_status.as_ref().is_none_or(|status| {
                        status.title.as_ref().is_none_or(|title| {
                            !title.is_empty()
                                && title.len() <= 160
                                && !title.chars().any(|character| {
                                    character.is_control()
                                        || matches!(character, '\u{2028}' | '\u{2029}')
                                })
                                && title.trim() == title
                        })
                    });
                let lifecycle_body_valid =
                    message.kind != AgentMessageKind::WatchLifecycle || message.message.is_empty();
                Some(if !payload_matches_kind {
                    Err(AgentEventValidationError::new(
                        "watch payload must be present exactly for its matching watch message kind",
                    ))
                } else if !lifecycle_body_valid {
                    Err(AgentEventValidationError::new(
                        "watch lifecycle messages must be content-free",
                    ))
                } else if !work_status_shape_valid {
                    Err(AgentEventValidationError::new(
                        "work-status title must be absent for unreported and present for every reported phase",
                    ))
                } else if !work_status_title_valid {
                    Err(AgentEventValidationError::new(
                        "work-status title must be nonempty, trimmed, one line, control-free, and at most 160 UTF-8 bytes",
                    ))
                } else {
                    Ok(())
                })
            }
            _ => None,
        }
    }

    fn validate_agent_fold_event(
        &self,
        head: Option<NodeId>,
        event: &Event,
    ) -> Option<Result<(), AgentEventValidationError>> {
        match event {
            Event::AgentInitializationContextSet(context) if context.agent_id == self.agent_id => {
                Some(Ok(()))
            }
            Event::AgentPromptSubmitted(prompt) if prompt.agent_id == self.agent_id => Some(
                check_trusted_internal_spans(&prompt.text, &prompt.trusted_internal_spans),
            ),
            Event::AgentPromptStarted(started) if started.agent_id == self.agent_id => {
                Some(self.validate_prompt_started(started))
            }
            Event::AgentPromptCreated(prompt) if prompt.agent_id == self.agent_id => {
                Some(Err(AgentEventValidationError::new(
                    "persisted agent.prompt_created is unsupported; discard or reset this agent journal",
                )))
            }
            Event::AgentUserMessageInjected(injected) if injected.agent_id == self.agent_id => {
                Some(Ok(()))
            }
            Event::AgentPromptSteered(steered) if steered.agent_id == self.agent_id => Some(
                check_trusted_internal_spans(&steered.text, &steered.trusted_internal_spans)
                    .and_then(|()| self.validate_self_compaction_delivery(steered))
                    .and_then(|()| self.validate_output_length_steer(head, steered)),
            ),
            Event::AgentCompactionTriggered(triggered) if triggered.agent_id == self.agent_id => {
                Some(Ok(()))
            }
            Event::AgentStandaloneCompactionStarted(started)
                if started.agent_id == self.agent_id =>
            {
                Some(self.validate_compaction_started(started))
            }
            Event::AgentManualCompactionRequested(requested)
                if requested.target_agent_id == self.agent_id =>
            {
                Some(self.validate_manual_compaction_requested(requested))
            }
            Event::AgentManualCompactionRequestFailed(failed)
                if failed.target_agent_id == self.agent_id =>
            {
                Some(self.validate_manual_compaction_request_failed(failed))
            }
            Event::AgentManualCompactionRequestSatisfied(satisfied)
                if satisfied.target_agent_id == self.agent_id =>
            {
                Some(self.validate_manual_compaction_request_satisfied(satisfied))
            }
            Event::AgentStandaloneCompactionFailed(failed) if failed.agent_id == self.agent_id => {
                Some(self.validate_compaction_failed(failed))
            }
            Event::AgentInferenceDispatchStarted(started) if started.agent_id == self.agent_id => {
                Some(self.validate_inference_checkpoint(started))
            }
            Event::AgentPromptTerminated(terminated) if terminated.agent_id == self.agent_id => {
                Some(self.validate_prompt_terminated(head, terminated))
            }
            Event::AgentCompacted(compacted) if compacted.agent_id == self.agent_id => Some(
                tau_proto::validate_compaction_window(&compacted.replacement_window)
                    .map_err(|error| {
                        AgentEventValidationError::new(format!(
                            "invalid compaction replacement window: {error}"
                        ))
                    })
                    .and_then(|()| {
                        validate_context_items_provider_content(&compacted.replacement_window)
                    })
                    .and_then(|()| self.validate_compaction_boundary(head, compacted)),
            ),
            Event::AgentHeadMoved(moved) if moved.agent_id == self.agent_id => {
                Some(self.validate_head_moved(moved))
            }
            Event::ProviderResponseFinished(response) if response.agent_id == self.agent_id => {
                Some(self.validate_provider_response(response))
            }
            Event::ShellCommandFinished(finished)
                if finished.target_agent_id.as_ref() == Some(&self.agent_id) =>
            {
                Some(Ok(()))
            }
            _ => None,
        }
    }

    fn proactive_threshold_source_is_valid(source: &tau_proto::CompactionThresholdSource) -> bool {
        match source {
            tau_proto::CompactionThresholdSource::ProviderDefault
            | tau_proto::CompactionThresholdSource::RoleThreshold => true,
            tau_proto::CompactionThresholdSource::NamedPolicy { name } => !name.trim().is_empty(),
            tau_proto::CompactionThresholdSource::NamedPolicies { names } => {
                !names.is_empty()
                    && names.iter().all(|name| !name.trim().is_empty())
                    && names.iter().collect::<HashSet<_>>().len() == names.len()
            }
        }
    }

    fn proactive_evidence_is_unclaimed(
        &self,
        evidence: &tau_proto::ProactiveCompactionEvidence,
    ) -> bool {
        !self.compaction_transactions.values().any(|transaction| {
            matches!(
                &transaction.started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence {
                    evidence: claimed
                } if claimed.provider_prompt_id == evidence.provider_prompt_id
            )
        }) && !self
            .automatic_compaction_decisions
            .values()
            .any(|decision| {
                decision.decision.evidence.as_ref().is_some_and(|claimed| {
                    claimed.provider_prompt_id == evidence.provider_prompt_id
                })
            })
    }

    /// Returns whether exact provider evidence is the newest unclaimed
    /// observation on the selected uncompacted branch.
    #[must_use]
    pub fn historical_proactive_evidence_is_valid(
        &self,
        evidence: &tau_proto::ProactiveCompactionEvidence,
        model: &tau_proto::ModelId,
    ) -> bool {
        let current = self
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let active_nodes = self
            .active_provider_window(current.as_option())
            .transcript
            .iter()
            .map(|(node_id, _)| *node_id)
            .collect::<HashSet<_>>();
        let Some(order) = self
            .inference_dispatch_order
            .iter()
            .position(|prompt_id| prompt_id == &evidence.provider_prompt_id)
        else {
            return false;
        };
        evidence.threshold > tau_proto::TokenCount::ZERO
            && evidence.provider_input_tokens >= evidence.threshold
            && Self::proactive_threshold_source_is_valid(&evidence.threshold_source)
            && self.proactive_evidence_is_unclaimed(evidence)
            && self
                .inference_dispatches
                .get(&evidence.provider_prompt_id)
                .is_some_and(|dispatch| {
                    dispatch.finished
                        && dispatch.checkpoint.operation
                            == Some(tau_proto::PromptOperation::Inference)
                        && dispatch.checkpoint.model.as_ref() == Some(model)
                        && dispatch.provider_input_tokens == Some(evidence.provider_input_tokens)
                        && dispatch
                            .response_node
                            .is_some_and(|node_id| active_nodes.contains(&node_id))
                })
            && !self.inference_dispatch_order[order.saturating_add(1)..]
                .iter()
                .filter_map(|prompt_id| self.inference_dispatches.get(prompt_id))
                .any(|dispatch| {
                    dispatch.finished
                        && dispatch.checkpoint.model.as_ref() == Some(model)
                        && dispatch
                            .response_node
                            .is_some_and(|node_id| active_nodes.contains(&node_id))
                })
    }

    fn validate_compaction_started(
        &self,
        started: &tau_proto::AgentStandaloneCompactionStarted,
    ) -> Result<(), AgentEventValidationError> {
        if started.operation != tau_proto::PromptOperation::StandaloneCompaction {
            return Err(AgentEventValidationError::new(
                "standalone compaction start has non-standalone operation",
            ));
        }
        if self
            .compaction_transactions
            .contains_key(&started.transaction_id)
        {
            return Err(AgentEventValidationError::new(
                "duplicate standalone compaction transaction id",
            ));
        }
        if self
            .automatic_compaction_decisions
            .contains_key(&started.transaction_id)
            && !matches!(
                &started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { decision_id }
                    if decision_id == &started.transaction_id
            )
            && !matches!(
                &started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                    decision_id: Some(decision_id),
                    ..
                } if decision_id == &started.transaction_id
            )
        {
            return Err(AgentEventValidationError::new(
                "standalone compaction transaction id is reserved by an automatic decision",
            ));
        }
        if let tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
            decision_id,
            previous_transaction_id,
            reason,
        } = &started.trigger
        {
            let valid_authority = match (decision_id, previous_transaction_id) {
                (None, None) | (Some(_), None) | (None, Some(_)) => true,
                (Some(_), Some(_)) => false,
            };
            let valid_reason = *reason
                == tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
                || (*reason == tau_proto::StandaloneCompactionFailureReason::RouteFailed
                    && decision_id.is_none()
                    && previous_transaction_id
                        .as_ref()
                        .is_some_and(|previous_transaction_id| {
                            matches!(
                                self.reactive_compaction_progress(previous_transaction_id),
                                Some(ReactiveCompactionProgress::NeedsContinuation { .. })
                            )
                        }));
            if !valid_authority || !valid_reason {
                return Err(AgentEventValidationError::new(
                    "automatic preflight failure has invalid authority or reason",
                ));
            }
        }
        if let Some(previous_id) = self.compaction_transaction_order.last()
            && let Some(previous) = self.compaction_transactions.get(previous_id)
            && matches!(
                previous.outcome,
                Some(CompactionTransactionOutcome::Succeeded(_))
            )
            && previous.checkpoint.is_none()
            && previous.started.resume_through.is_some()
        {
            let claimed = match &started.trigger {
                tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                    previous_transaction_id,
                }
                | tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                    previous_transaction_id: Some(previous_transaction_id),
                    ..
                } => previous_transaction_id == previous_id,
                _ => false,
            };
            if !claimed {
                return Err(AgentEventValidationError::new(
                    "successful standalone compaction requires an explicitly linked continuation or checkpoint",
                ));
            }
        }
        if let tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
            request_id,
            caller_agent_id,
            initiating_tool_call_id,
        } = &started.trigger
        {
            let Some(request) = self.manual_compaction_requests.get(request_id) else {
                return Err(AgentEventValidationError::new(
                    "manual-agent transaction references unknown request",
                ));
            };
            let tau_proto::ManualCompactionSource::Tool(source) = &request.requested.source else {
                return Err(AgentEventValidationError::new(
                    "manual-agent transaction references a non-tool request",
                ));
            };
            if !matches!(request.state, ManualCompactionRequestState::Waiting)
                || request.requested.target_agent_id != started.agent_id
                || source.caller_agent_id != *caller_agent_id
                || source.initiating_tool_call_id != *initiating_tool_call_id
                || request.requested.model != started.model
            {
                return Err(AgentEventValidationError::new(
                    "manual-agent transaction does not uniquely match its request",
                ));
            }
            let accepted = &request.requested;
            let valid_cut = match source.initiating_tool_name {
                tau_proto::ManualCompactionTool::Compact => {
                    started.resume_through.is_some_and(|resume| {
                        let valid_boundary = if started.supersedes.is_some() {
                            self.is_ancestor_head(started.cut, accepted.requested_target_head)
                        } else {
                            resume == started.cut
                                && self
                                    .is_ancestor_head(accepted.requested_target_head, started.cut)
                        };
                        valid_boundary
                            && self.is_ancestor_head(accepted.requested_target_head, resume)
                            && self.has_complete_tool_round_for(
                                resume.as_option(),
                                &source.initiating_tool_call_id,
                            )
                    })
                }
                tau_proto::ManualCompactionTool::AgentCompact => {
                    if started.supersedes.is_some() {
                        self.is_ancestor_head(started.cut, accepted.requested_target_head)
                            && started
                                .resume_through
                                .is_none_or(|resume| resume == accepted.requested_target_head)
                    } else {
                        started.cut == accepted.requested_target_head
                            && started.resume_through.is_none()
                    }
                }
            };
            if !valid_cut {
                return Err(AgentEventValidationError::new(
                    "manual-agent transaction does not honor its accepted target boundary",
                ));
            }
        }
        if let tau_proto::StandaloneCompactionTrigger::ManualUi { request_id } = &started.trigger {
            let Some(request) = self.manual_compaction_requests.get(request_id) else {
                return Err(AgentEventValidationError::new(
                    "manual UI transaction references unknown request",
                ));
            };
            let accepted = &request.requested;
            let valid_cut = if started.supersedes.is_some() {
                self.is_ancestor_head(started.cut, accepted.requested_target_head)
                    && started
                        .resume_through
                        .is_none_or(|resume| resume == accepted.requested_target_head)
            } else {
                self.is_ancestor_head(accepted.requested_target_head, started.cut)
                    && started
                        .resume_through
                        .is_none_or(|resume| resume == started.cut)
            };
            let eligible_automatic_finished = accepted
                .ui_source()
                .and_then(|source| source.eligible_automatic_transaction_id.as_ref())
                .is_none_or(|id| {
                    self.compaction_transactions.get(id).map_or_else(
                        || {
                            self.automatic_compaction_decisions
                                .get(id)
                                .is_some_and(|decision| decision.closed)
                        },
                        |transaction| {
                            matches!(
                                transaction.outcome,
                                Some(CompactionTransactionOutcome::Failed(_))
                            )
                        },
                    )
                });
            if !matches!(request.state, ManualCompactionRequestState::Waiting)
                || !matches!(
                    accepted.source,
                    tau_proto::ManualCompactionSource::UiCompact { .. }
                )
                || accepted.target_agent_id != started.agent_id
                || accepted.model != started.model
                || accepted.target_generation
                    != self
                        .ordinary_inference_generation
                        .materialized_prompt_generation()
                || !eligible_automatic_finished
                || !valid_cut
            {
                return Err(AgentEventValidationError::new(
                    "manual UI transaction does not uniquely match its queued request",
                ));
            }
        }
        let reactive_prompt_id = match &started.trigger {
            tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                failed_agent_prompt_id,
            }
            | tau_proto::StandaloneCompactionTrigger::ReactivePreflightFailure {
                failed_agent_prompt_id,
                ..
            } => Some(failed_agent_prompt_id),
            _ => None,
        };
        if let Some(failed_agent_prompt_id) = reactive_prompt_id {
            if matches!(
                started.trigger,
                tau_proto::StandaloneCompactionTrigger::ReactivePreflightFailure {
                    reason,
                    ..
                } if reason != tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge
            ) {
                return Err(AgentEventValidationError::new(
                    "reactive preflight failure has invalid reason",
                ));
            }
            let Some(dispatch) = self.inference_dispatches.get(failed_agent_prompt_id) else {
                return Err(AgentEventValidationError::new(
                    "reactive compaction references unknown inference checkpoint",
                ));
            };
            let checkpoint = &dispatch.checkpoint;
            if !dispatch.finished
                || dispatch.recovery_disposition
                    != tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
                || checkpoint.transaction_id.is_some()
                || checkpoint.operation != Some(tau_proto::PromptOperation::Inference)
                || checkpoint.model.as_ref() != Some(&started.model)
                || checkpoint
                    .activation_cut
                    .is_none_or(|cut| !self.active_provider_window_contains(cut, started.cut))
                || started.resume_through != Some(checkpoint.through)
                || self.compaction_transactions.values().any(|transaction| {
                    matches!(
                        &transaction.started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                            failed_agent_prompt_id: claimed
                        } | tau_proto::StandaloneCompactionTrigger::ReactivePreflightFailure {
                            failed_agent_prompt_id: claimed,
                            ..
                        } if claimed == failed_agent_prompt_id
                    )
                })
            {
                return Err(AgentEventValidationError::new(
                    "reactive compaction does not uniquely match its planned inference recovery",
                ));
            }
        }
        if let tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { evidence } =
            &started.trigger
            && !self.historical_proactive_evidence_is_valid(evidence, &started.model)
        {
            return Err(AgentEventValidationError::new(
                "automatic threshold start lacks unique exact provider evidence",
            ));
        }
        let automatic_decision_id = match &started.trigger {
            tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { decision_id } => {
                Some(decision_id)
            }
            tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                decision_id: Some(decision_id),
                ..
            } => Some(decision_id),
            _ => None,
        };
        if let Some(decision_id) = automatic_decision_id {
            let Some(decision) = self.automatic_compaction_decisions.get(decision_id) else {
                return Err(AgentEventValidationError::new(
                    "automatic compaction references unknown terminal decision",
                ));
            };
            if decision.claimed
                || decision.closed
                || !decision.finish_committed
                || started.transaction_id != *decision_id
                || !self.is_ancestor_head(started.cut, decision.cut)
                || started.model != decision.decision.model
                || started.resume_through
                    != Some(
                        self.head
                            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                    )
                || started.supersedes.is_some()
            {
                return Err(AgentEventValidationError::new(
                    "automatic compaction does not uniquely claim its terminal decision",
                ));
            }
        }
        let automatic_previous_id = match &started.trigger {
            tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                previous_transaction_id,
            } => Some(previous_transaction_id),
            tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                previous_transaction_id: Some(previous_transaction_id),
                ..
            } => Some(previous_transaction_id),
            _ => None,
        };
        if let Some(previous_transaction_id) = automatic_previous_id {
            let Some(previous) = self.compaction_transactions.get(previous_transaction_id) else {
                return Err(AgentEventValidationError::new(
                    "automatic continuation references unknown transaction",
                ));
            };
            let current = self
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
            let previous_boundary = self
                .active_provider_window_replacement(current.as_option())
                .map(|(node_id, _)| tau_proto::AgentHead::Node(node_id));
            let previous_boundary_matches = previous_boundary
                .and_then(AgentHead::as_option)
                .and_then(|node_id| self.node(node_id))
                .is_some_and(|node| {
                    matches!(
                        &node.entry,
                        AgentEntry::Compaction {
                            transaction_id: Some(transaction_id),
                            ..
                        } if transaction_id == previous_transaction_id
                    )
                });
            let previous_is_rolling = matches!(
                previous.started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticThreshold
                    | tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat { .. }
                    | tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
            );
            let reactive_progress = self.reactive_compaction_progress(previous_transaction_id);
            let reactive_target_cut = match reactive_progress {
                Some(ReactiveCompactionProgress::NeedsContinuation { target_cut }) => {
                    Some(target_cut)
                }
                _ => None,
            };
            if self.compaction_transaction_order.last() != Some(previous_transaction_id)
                || !matches!(
                    previous.outcome,
                    Some(CompactionTransactionOutcome::Succeeded(_))
                )
                || previous.checkpoint.is_some()
                || !previous_boundary_matches
                || !previous_is_rolling
                || previous.started.model != started.model
                || started.resume_through != Some(current)
                || (matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                ) && Some(started.cut) == previous_boundary)
                || reactive_progress == Some(ReactiveCompactionProgress::ReachedTargetCut)
                || reactive_target_cut
                    .is_some_and(|target_cut| !self.is_ancestor_head(started.cut, target_cut))
                || started.supersedes.is_some()
            {
                return Err(AgentEventValidationError::new(
                    "automatic continuation does not uniquely claim the preceding successful pass",
                ));
            }
        }
        if !self.active_provider_window_contains(
            started.resume_through.unwrap_or(started.cut),
            started.cut,
        ) {
            return Err(AgentEventValidationError::new(
                "standalone compaction cut must occur in its selected logical active window",
            ));
        }
        if self
            .compaction_transactions
            .values()
            .any(|transaction| transaction.outcome.is_none())
        {
            return Err(AgentEventValidationError::new(
                "another standalone compaction transaction is active",
            ));
        }
        if let Some(id) = &started.supersedes {
            let automatic_retreat = matches!(
                &started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
                    failed_transaction_id,
                    ..
                } if failed_transaction_id == id
            );
            if !automatic_retreat
                && !matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::Manual
                        | tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                        | tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
                )
            {
                return Err(AgentEventValidationError::new(
                    "only an explicit manual compaction may supersede a failed transaction",
                ));
            }
            let Some(previous) = self.compaction_transactions.get(id) else {
                return Err(AgentEventValidationError::new(
                    "standalone compaction supersedes unknown transaction",
                ));
            };
            if !matches!(
                previous.outcome,
                Some(CompactionTransactionOutcome::Failed(_))
            ) {
                return Err(AgentEventValidationError::new(
                    "standalone compaction may supersede only a failed transaction",
                ));
            }
            if automatic_retreat {
                let Some(CompactionTransactionOutcome::Failed(failed)) = previous.outcome.as_ref()
                else {
                    unreachable!("failed outcome checked above")
                };
                let plan_matches = failed.context_retreat.as_ref().is_some_and(|plan| {
                    plan.transaction_id == started.transaction_id
                        && plan.compact_prompt_id == started.compact_prompt_id
                        && plan.cut == started.cut
                        && plan.model == started.model
                        && plan.originator == started.originator
                        && plan.resume_through == started.resume_through
                        && matches!(
                            &started.trigger,
                            tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
                                roll_through,
                                ..
                            } if *roll_through == plan.roll_through
                        )
                });
                if failed.reason
                    != tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
                    || !plan_matches
                    || started.cut == previous.started.cut
                {
                    return Err(AgentEventValidationError::new(
                        "automatic context retreat does not exactly claim its committed strict-predecessor plan",
                    ));
                }
            }
            let current = self
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
            if self
                .unresolved_standalone_compaction_failure(&started.model, current)
                .as_ref()
                .map(|failed| &failed.transaction_id)
                != Some(id)
                || previous.started.model != started.model
            {
                return Err(AgentEventValidationError::new(
                    "standalone compaction must supersede the latest matching unresolved failure",
                ));
            }
            let preserves_resume = previous
                .started
                .resume_through
                .is_none_or(|previous_resume| {
                    started
                        .resume_through
                        .is_some_and(|resume| self.is_ancestor_head(previous_resume, resume))
                });
            if !self.is_ancestor_head(started.cut, previous.started.cut) || !preserves_resume {
                return Err(AgentEventValidationError::new(
                    "superseding compaction must preserve or retreat the cut and preserve the owed resume branch",
                ));
            }
        }
        Ok(())
    }

    fn validate_prompt_started(
        &self,
        started: &tau_proto::AgentPromptStarted,
    ) -> Result<(), AgentEventValidationError> {
        if self.prompt_starts.contains_key(&started.agent_prompt_id) {
            return Err(AgentEventValidationError::new(
                "duplicate agent prompt materialization fact",
            ));
        }
        match (&started.outer_turn_id, started.operation) {
            (Some(turn_id), tau_proto::PromptOperation::Inference) => {
                if self.active_outer_turn.as_ref() != Some(turn_id)
                    || !matches!(
                        self.outer_turns.get(turn_id),
                        Some(fold)
                            if fold.session_id == started.session_id && !fold.finished
                    )
                {
                    return Err(AgentEventValidationError::new(
                        "agent prompt outer turn is absent, closed, or in another session",
                    ));
                }
            }
            (Some(_), tau_proto::PromptOperation::StandaloneCompaction) => {
                return Err(AgentEventValidationError::new(
                    "standalone compaction cannot belong to an outer turn",
                ));
            }
            (None, _) => {
                // Missing correlation is accepted only as explicit legacy data;
                // forward writers always populate ordinary inference ownership.
            }
        }

        match self.prompt_started_owner_matches(started) {
            None => {
                return Err(AgentEventValidationError::new(
                    "agent prompt materialization must uniquely match one unresolved dispatch owner",
                ));
            }
            Some(false) => {
                return Err(AgentEventValidationError::new(
                    "agent prompt materialization mismatches its unresolved dispatch owner",
                ));
            }
            Some(true) => {}
        }
        Ok(())
    }

    /// Returns the exact durable materialization fact for one provider prompt.
    #[must_use]
    pub fn prompt_started(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<&tau_proto::AgentPromptStarted> {
        self.prompt_starts.get(prompt_id)
    }

    /// Return whether this exact outer turn remains durably open.
    #[must_use]
    pub fn outer_turn_is_open(&self, turn_id: &tau_proto::AgentOuterTurnId) -> bool {
        self.active_outer_turn.as_ref() == Some(turn_id)
            && self
                .outer_turns
                .get(turn_id)
                .is_some_and(|turn| !turn.finished)
    }

    /// Returns the selected-branch output-length repair authorized by durable
    /// typed lineage.
    #[must_use]
    pub fn output_length_continuation_recovery(&self) -> Option<OutputLengthContinuationRecovery> {
        let (source_prompt_id, source) = self.active_output_length_plan()?;
        let tau_proto::OutputLengthDisposition::ContinuationPlanned {
            outer_turn_id,
            successor_agent_prompt_id,
            ordinal: 1,
            limit: 1,
        } = &source.output_length_disposition
        else {
            return None;
        };
        let selected = self.head;
        if source.output_length_steer_node.is_none() {
            if selected != source.output_length_plan_node {
                return Some(OutputLengthContinuationRecovery::BranchInvalid {
                    source: source.checkpoint.clone(),
                    successor_agent_prompt_id: successor_agent_prompt_id.clone(),
                    outer_turn_id: outer_turn_id.clone(),
                });
            }
            return Some(OutputLengthContinuationRecovery::SteerNeeded {
                source: source.checkpoint.clone(),
                successor_agent_prompt_id: successor_agent_prompt_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
            });
        }
        let steer_node = source.output_length_steer_node?;
        if self.inference_dispatches.values().any(|dispatch| {
            dispatch
                .checkpoint
                .output_length_continuation
                .as_ref()
                .is_some_and(|owner| owner.source_agent_prompt_id == *source_prompt_id)
        }) {
            return None;
        }
        if selected != Some(steer_node) {
            return Some(OutputLengthContinuationRecovery::BranchInvalid {
                source: source.checkpoint.clone(),
                successor_agent_prompt_id: successor_agent_prompt_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
            });
        }
        Some(OutputLengthContinuationRecovery::OwnerNeeded {
            source: source.checkpoint.clone(),
            successor_agent_prompt_id: successor_agent_prompt_id.clone(),
            outer_turn_id: outer_turn_id.clone(),
            through: tau_proto::AgentHead::Node(steer_node),
        })
    }

    /// Returns the next exact repair step for one active-turn output-length
    /// plan that is no longer an ancestor of the selected transcript head.
    #[must_use]
    pub fn output_length_dormant_repair(&self) -> Option<OutputLengthDormantRepair> {
        let active = self.active_outer_turn.as_ref()?;
        let selected = self
            .head
            .map(tau_proto::AgentHead::Node)
            .unwrap_or(tau_proto::AgentHead::Root);
        let (source_prompt_id, source) =
            self.inference_dispatch_order
                .iter()
                .rev()
                .find_map(|prompt_id| {
                    let dispatch = self.inference_dispatches.get(prompt_id)?;
                    matches!(
                        &dispatch.output_length_disposition,
                        tau_proto::OutputLengthDisposition::ContinuationPlanned {
                            outer_turn_id,
                            ordinal: 1,
                            limit: 1,
                            ..
                        } if outer_turn_id == active
                    )
                    .then_some((prompt_id, dispatch))
                })?;
        let tau_proto::OutputLengthDisposition::ContinuationPlanned {
            outer_turn_id,
            successor_agent_prompt_id,
            ..
        } = &source.output_length_disposition
        else {
            return None;
        };
        let plan_node = source.output_length_plan_node?;
        if self.is_ancestor_head(tau_proto::AgentHead::Node(plan_node), selected) {
            return None;
        }
        let Some(steer_node) = source.output_length_steer_node else {
            return Some(OutputLengthDormantRepair::Steer {
                source: source.checkpoint.clone(),
                successor_agent_prompt_id: successor_agent_prompt_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                parent: tau_proto::AgentHead::Node(plan_node),
            });
        };
        let owner = self.inference_dispatches.values().find(|dispatch| {
            dispatch
                .checkpoint
                .output_length_continuation
                .as_ref()
                .is_some_and(|owner| owner.source_agent_prompt_id == *source_prompt_id)
        });
        let Some(owner) = owner else {
            return Some(OutputLengthDormantRepair::Owner {
                source: source.checkpoint.clone(),
                successor_agent_prompt_id: successor_agent_prompt_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                through: tau_proto::AgentHead::Node(steer_node),
                plan_parent: tau_proto::AgentHead::Node(plan_node),
            });
        };
        if !owner.finished {
            if self
                .prompt_starts
                .contains_key(&owner.checkpoint.agent_prompt_id)
            {
                return None;
            }
            return Some(OutputLengthDormantRepair::Terminal {
                owner: owner.checkpoint.clone(),
                parent: tau_proto::AgentHead::Node(steer_node),
            });
        }
        let tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outer_turn_id: terminal_turn,
            outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
            outer_turn_finish_owed: true,
            ..
        } = &owner.output_length_disposition
        else {
            return None;
        };
        if terminal_turn != outer_turn_id || !self.outer_turn_is_open(outer_turn_id) {
            return None;
        }
        Some(OutputLengthDormantRepair::Finish {
            outer_turn_id: outer_turn_id.clone(),
            parent: tau_proto::AgentHead::Node(owner.response_node?),
        })
    }

    /// Returns the active selected-branch turn whose current reasoning-only run
    /// has already spent its output-length continuation.
    #[must_use]
    pub fn output_length_budget_spent_outer_turn(&self) -> Option<tau_proto::AgentOuterTurnId> {
        let active = self.active_outer_turn.as_ref()?;
        let selected_head = self
            .head
            .map(tau_proto::AgentHead::Node)
            .unwrap_or(tau_proto::AgentHead::Root);
        for prompt_id in self.inference_dispatch_order.iter().rev() {
            let Some(dispatch) = self.inference_dispatches.get(prompt_id) else {
                continue;
            };
            let selected = dispatch.response_node.is_some_and(|response_node| {
                self.is_ancestor_head(tau_proto::AgentHead::Node(response_node), selected_head)
            });
            if !selected {
                continue;
            }
            if dispatch.rearms_output_length {
                return None;
            }
            if matches!(
                &dispatch.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationPlanned {
                    outer_turn_id,
                    ordinal: 1,
                    limit: 1,
                    ..
                } if outer_turn_id == active
            ) {
                return Some(active.clone());
            }
        }
        None
    }

    /// Returns whether this committed selected-branch response durably rearms
    /// output-length recovery for its current reasoning-only run.
    #[must_use]
    pub fn output_length_response_rearms_budget(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> bool {
        let Some(dispatch) = self.inference_dispatches.get(prompt_id) else {
            return false;
        };
        let selected_head = self
            .head
            .map(tau_proto::AgentHead::Node)
            .unwrap_or(tau_proto::AgentHead::Root);
        dispatch.rearms_output_length
            && dispatch.response_node.is_some_and(|response_node| {
                self.is_ancestor_head(tau_proto::AgentHead::Node(response_node), selected_head)
            })
    }

    /// Resolves the exact output-length lineage owner for either its reserved
    /// successor or the one post-compaction inference descendant durably owned
    /// by that successor's reactive recovery transaction.
    #[must_use]
    pub fn output_length_lineage_owner_for_prompt(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<tau_proto::OutputLengthContinuationOwner> {
        let dispatch = self.inference_dispatches.get(prompt_id)?;
        if let Some(owner) = &dispatch.checkpoint.output_length_continuation {
            return Some(owner.clone());
        }
        let transaction_id = dispatch.checkpoint.transaction_id.as_ref()?;
        let transaction = self.compaction_transactions.get(transaction_id)?;
        if transaction.checkpoint.as_ref() != Some(&dispatch.checkpoint)
            || !matches!(
                transaction.outcome,
                Some(CompactionTransactionOutcome::Succeeded(_))
            )
        {
            return None;
        }
        let tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
            failed_agent_prompt_id,
        } = &transaction.started.trigger
        else {
            return None;
        };
        let failed = self.inference_dispatches.get(failed_agent_prompt_id)?;
        if failed.recovery_disposition
            != tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
        {
            return None;
        }
        failed.checkpoint.output_length_continuation.clone()
    }

    /// Resolves the original output-length owner for one successful reactive
    /// transaction before its post-compaction inference checkpoint exists.
    #[must_use]
    pub fn output_length_lineage_owner_for_transaction(
        &self,
        transaction_id: &tau_proto::CompactionTransactionId,
    ) -> Option<tau_proto::OutputLengthContinuationOwner> {
        let transaction = self.compaction_transactions.get(transaction_id)?;
        if !matches!(
            transaction.outcome,
            Some(CompactionTransactionOutcome::Succeeded(_))
        ) {
            return None;
        }
        let tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
            failed_agent_prompt_id,
        } = &transaction.started.trigger
        else {
            return None;
        };
        let failed = self.inference_dispatches.get(failed_agent_prompt_id)?;
        failed.checkpoint.output_length_continuation.clone()
    }

    /// Returns the durable harness-authored attempt for one finished inference.
    #[must_use]
    pub fn provider_attempt_for_prompt(
        &self,
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<tau_proto::ProviderAttempt> {
        let dispatch = self.inference_dispatches.get(prompt_id)?;
        dispatch
            .finished
            .then_some(dispatch.provider_attempt)
            .flatten()
    }

    /// Returns the canonical ordinary provider prompt that committed one
    /// transcript response node.
    #[must_use]
    pub fn provider_prompt_for_response_node(
        &self,
        node_id: NodeId,
    ) -> Option<tau_proto::AgentPromptId> {
        self.inference_dispatch_order
            .iter()
            .rev()
            .find(|prompt_id| {
                self.inference_dispatches
                    .get(*prompt_id)
                    .is_some_and(|dispatch| dispatch.response_node == Some(node_id))
            })
            .cloned()
    }

    /// Finds exactly one active-turn plan not yet claimed by a successor owner.
    fn active_output_length_plan(
        &self,
    ) -> Option<(&tau_proto::AgentPromptId, &InferenceDispatchFold)> {
        let active = self.active_outer_turn.as_ref()?;
        let mut plans = self
            .inference_dispatches
            .iter()
            .filter(|(source_id, dispatch)| {
                matches!(
                    &dispatch.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationPlanned {
                        outer_turn_id, ..
                    } if outer_turn_id == active
                ) && !self.inference_dispatches.values().any(|candidate| {
                    candidate
                        .checkpoint
                        .output_length_continuation
                        .as_ref()
                        .is_some_and(|owner| &owner.source_agent_prompt_id == *source_id)
                })
            });
        let plan = plans.next()?;
        plans.next().is_none().then_some(plan)
    }

    /// Projects the latest selected-lineage terminal output-limit status for
    /// sticky watcher restoration after a cold restart.
    #[must_use]
    pub fn output_length_terminal_incomplete(&self) -> Option<OutputLengthTerminalIncomplete> {
        let selected_head = self
            .head
            .map(tau_proto::AgentHead::Node)
            .unwrap_or(tau_proto::AgentHead::Root);
        let (prompt_id, dispatch) =
            self.inference_dispatch_order
                .iter()
                .rev()
                .find_map(|prompt_id| {
                    let dispatch = self.inference_dispatches.get(prompt_id)?;
                    let selected = if dispatch.finished {
                        dispatch.response_node.is_some_and(|response_node| {
                            self.is_ancestor_head(
                                tau_proto::AgentHead::Node(response_node),
                                selected_head,
                            )
                        })
                    } else {
                        dispatch.head_move_generation == self.head_move_generation
                            && self.is_ancestor_head(dispatch.checkpoint.through, selected_head)
                    };
                    selected.then_some((prompt_id, dispatch))
                })?;
        if !dispatch.finished {
            return None;
        }
        (matches!(
            dispatch.output_length_disposition,
            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                outcome: tau_proto::OutputLengthContinuationOutcome::Incomplete,
                ..
            }
        ) || (dispatch.output_length_disposition == tau_proto::OutputLengthDisposition::None
            && dispatch.provider_stop_reason == Some(tau_proto::ProviderStopReason::Length)))
        .then(|| {
            dispatch
                .provider_attempt
                .map(|provider_attempt| OutputLengthTerminalIncomplete {
                    agent_prompt_id: prompt_id.clone(),
                    provider_attempt,
                })
        })
        .flatten()
    }

    /// Returns the sole explicit authority to repair a missing outer-turn
    /// finish after an output-length successor terminal committed.
    #[must_use]
    pub fn output_length_outer_turn_finish_repair(&self) -> Option<tau_proto::AgentOuterTurnId> {
        let active = self.active_outer_turn.as_ref()?;
        let outer_turn_id = self
            .inference_dispatch_order
            .iter()
            .rev()
            .find_map(|prompt_id| {
                let dispatch = self.inference_dispatches.get(prompt_id)?;
                match &dispatch.output_length_disposition {
                    tau_proto::OutputLengthDisposition::ContinuationTerminal {
                        outer_turn_id,
                        outer_turn_finish_owed: true,
                        ..
                    } if outer_turn_id == active => Some(outer_turn_id),
                    tau_proto::OutputLengthDisposition::None
                    | tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                    | tau_proto::OutputLengthDisposition::ContinuationTerminal { .. } => None,
                }
            })?;
        self.outer_turn_is_open(outer_turn_id)
            .then(|| outer_turn_id.clone())
    }

    /// Returns whether a compact materialization fact may acquire its sole live
    /// continuation before prompt construction mutates runtime bookkeeping.
    #[must_use]
    pub fn prompt_started_can_materialize(&self, started: &tau_proto::AgentPromptStarted) -> bool {
        !self.prompt_starts.contains_key(&started.agent_prompt_id)
            && self.prompt_started_owner_matches(started) == Some(true)
    }

    /// Returns whether a folded materialization fact still has its exact
    /// unresolved durable dispatch owner.
    #[must_use]
    pub fn prompt_started_is_dispatchable(&self, started: &tau_proto::AgentPromptStarted) -> bool {
        self.prompt_starts.get(&started.agent_prompt_id) == Some(started)
            && self.prompt_started_owner_matches(started) == Some(true)
    }

    /// Returns `None` unless exactly one owner has this prompt id, otherwise
    /// whether that owner is unresolved and matches every durable correlation.
    fn prompt_started_owner_matches(
        &self,
        started: &tau_proto::AgentPromptStarted,
    ) -> Option<bool> {
        let inference_owner = self.inference_dispatches.get(&started.agent_prompt_id);
        let mut compaction_owners = self
            .compaction_transactions
            .values()
            .filter(|transaction| transaction.started.compact_prompt_id == started.agent_prompt_id);
        let compaction_owner = compaction_owners.next();
        if usize::from(inference_owner.is_some())
            + usize::from(compaction_owner.is_some())
            + usize::from(compaction_owners.next().is_some())
            != 1
        {
            return None;
        }
        Some(match (inference_owner, compaction_owner) {
            (Some(dispatch), None) => {
                !dispatch.finished
                    && dispatch.checkpoint.model.as_ref() == Some(&started.model)
                    && dispatch.checkpoint.operation == Some(started.operation)
                    && started.operation == tau_proto::PromptOperation::Inference
            }
            (None, Some(transaction)) => {
                transaction.outcome.is_none()
                    && transaction.started.model == started.model
                    && transaction.started.operation == started.operation
                    && started.operation == tau_proto::PromptOperation::StandaloneCompaction
            }
            _ => false,
        })
    }

    fn validate_manual_compaction_requested(
        &self,
        requested: &tau_proto::AgentManualCompactionRequested,
    ) -> Result<(), AgentEventValidationError> {
        let ui_request = matches!(
            requested.source,
            tau_proto::ManualCompactionSource::UiCompact { .. }
        );
        if let Some(source) = requested.ui_source() {
            if source.target_role.is_empty() {
                return Err(AgentEventValidationError::new(
                    "UI compaction request has no effective role identity",
                ));
            }
            let eligible_transaction = source.eligible_automatic_transaction_id.as_ref();
            let active_transaction =
                eligible_transaction.is_some_and(|id| {
                    self.compaction_transactions.get(id).is_some_and(|transaction| {
                    transaction.outcome.is_none()
                        && !matches!(
                            transaction.started.trigger,
                            tau_proto::StandaloneCompactionTrigger::Manual
                                | tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                                | tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
                        )
                })
                });
            let active_decision = eligible_transaction.is_some_and(|id| {
                self.automatic_compaction_decisions
                    .get(id)
                    .is_some_and(|decision| !decision.claimed && !decision.closed)
            });
            let any_active_automatic = self.compaction_transactions.values().any(|transaction| {
                transaction.outcome.is_none()
                    && !matches!(
                        transaction.started.trigger,
                        tau_proto::StandaloneCompactionTrigger::Manual
                            | tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                            | tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
                    )
            }) || self
                .automatic_compaction_decisions
                .values()
                .any(|decision| !decision.claimed && !decision.closed);
            if eligible_transaction.is_some() != (active_transaction || active_decision)
                || eligible_transaction.is_none() && any_active_automatic
            {
                return Err(AgentEventValidationError::new(
                    "UI compaction request has invalid automatic precedence authority",
                ));
            }
        }
        if self
            .manual_compaction_requests
            .contains_key(&requested.request_id)
        {
            return Err(AgentEventValidationError::new(
                "duplicate manual compaction request id",
            ));
        }
        if self
            .manual_compaction_requests
            .values()
            .any(|request| matches!(request.state, ManualCompactionRequestState::Waiting))
            || self.compaction_transactions.values().any(|transaction| {
                transaction.outcome.is_none()
                    && (!ui_request
                        || matches!(
                            transaction.started.trigger,
                            tau_proto::StandaloneCompactionTrigger::Manual
                                | tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                                | tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
                        ))
            })
        {
            return Err(AgentEventValidationError::new(
                "another compaction request or transaction is pending",
            ));
        }
        if let tau_proto::ManualCompactionSource::Tool(source) = &requested.source {
            if source.initiating_tool_name == tau_proto::ManualCompactionTool::Compact
                && (source.caller_agent_id != requested.target_agent_id || !source.resume_inference)
            {
                return Err(AgentEventValidationError::new(
                    "self compaction request has different caller and target",
                ));
            }
            if source.initiating_tool_name == tau_proto::ManualCompactionTool::AgentCompact
                && (source.caller_agent_id == requested.target_agent_id || source.resume_inference)
            {
                return Err(AgentEventValidationError::new(
                    "cross-agent compaction request targets its caller",
                ));
            }
        }
        if !self.is_ancestor_head(
            requested.requested_target_head,
            requested.requested_target_head,
        ) {
            return Err(AgentEventValidationError::new(
                "manual compaction request references an unknown target head",
            ));
        }
        if requested.target_generation
            != self
                .ordinary_inference_generation
                .materialized_prompt_generation()
        {
            return Err(AgentEventValidationError::new(
                "manual compaction request has a stale target generation",
            ));
        }
        Ok(())
    }

    fn validate_self_compaction_delivery(
        &self,
        steered: &tau_proto::AgentPromptSteered,
    ) -> Result<(), AgentEventValidationError> {
        let Some(terminal) = &steered.self_compaction_terminal else {
            return Ok(());
        };
        if self
            .self_compaction_deliveries
            .contains_key(&terminal.request_id)
        {
            return Err(AgentEventValidationError::new(
                "duplicate self-compaction terminal delivery",
            ));
        }
        let Some(request) = self.manual_compaction_requests.get(&terminal.request_id) else {
            return Err(AgentEventValidationError::new(
                "self-compaction delivery references unknown request",
            ));
        };
        let tau_proto::ManualCompactionSource::Tool(source) = &request.requested.source else {
            return Err(AgentEventValidationError::new(
                "self-compaction delivery references a non-tool request",
            ));
        };
        if !source.resume_inference
            || source.caller_agent_id != request.requested.target_agent_id
            || source.initiating_tool_call_id != terminal.tool_call_id
            || !steered.inference_activation
            || !steered.message_class.is_internal()
        {
            return Err(AgentEventValidationError::new(
                "self-compaction delivery correlation does not match request",
            ));
        }
        let matches_outcome = match (&terminal.outcome, &request.state) {
            (
                tau_proto::SelfCompactionTerminalOutcome::RequestFailed { reason },
                ManualCompactionRequestState::Failed(failed),
            ) => terminal.transaction_id.is_none() && *reason == failed.reason,
            (_, ManualCompactionRequestState::Started(transaction_id))
                if terminal.transaction_id.as_ref() == Some(transaction_id) =>
            {
                self.compaction_transactions
                    .get(transaction_id)
                    .and_then(|transaction| transaction.outcome.as_ref())
                    .is_some_and(|outcome| match (&terminal.outcome, outcome) {
                        (
                            tau_proto::SelfCompactionTerminalOutcome::Compacted,
                            CompactionTransactionOutcome::Succeeded(_),
                        ) => true,
                        (
                            tau_proto::SelfCompactionTerminalOutcome::Failed { reason },
                            CompactionTransactionOutcome::Failed(failed),
                        ) => *reason == failed.reason,
                        _ => false,
                    })
            }
            _ => false,
        };
        if !matches_outcome {
            return Err(AgentEventValidationError::new(
                "self-compaction delivery does not match durable terminal outcome",
            ));
        }
        Ok(())
    }

    fn validate_output_length_steer(
        &self,
        head: Option<NodeId>,
        steered: &tau_proto::AgentPromptSteered,
    ) -> Result<(), AgentEventValidationError> {
        if steered.internal_kind != Some(tau_proto::InternalPromptKind::OutputLengthContinuation) {
            return Ok(());
        }
        let exact_span = (steered.text == tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION)
            .then_some(tau_proto::OUTPUT_LENGTH_CONTINUATION_INSTRUCTION)
            .and_then(|instruction| u32::try_from(instruction.len()).ok())
            .map(|end| vec![tau_proto::TrustedInternalSpan { start: 0, end }]);
        if self
            .active_output_length_plan()
            .filter(|(_, dispatch)| dispatch.output_length_plan_node == head)
            .is_none_or(|(_, dispatch)| dispatch.output_length_steer_node.is_some())
            || !steered.inference_activation
            || steered.message_class != tau_proto::PromptMessageClass::Internal
            || steered.submission_source != tau_proto::PromptSubmissionSource::HarnessInternal
            || exact_span.is_none()
            || Some(&steered.trusted_internal_spans) != exact_span.as_ref()
            || steered.self_compaction_terminal.is_some()
            || steered.ctx_id.is_some()
        {
            return Err(AgentEventValidationError::new(
                "output-length continuation steer mismatches its durable plan",
            ));
        }
        Ok(())
    }

    fn validate_manual_compaction_request_failed(
        &self,
        failed: &tau_proto::AgentManualCompactionRequestFailed,
    ) -> Result<(), AgentEventValidationError> {
        let Some(request) = self.manual_compaction_requests.get(&failed.request_id) else {
            return Err(AgentEventValidationError::new(
                "manual compaction failure references unknown request",
            ));
        };
        if request.requested.target_agent_id != failed.target_agent_id
            || !matches!(request.state, ManualCompactionRequestState::Waiting)
        {
            return Err(AgentEventValidationError::new(
                "manual compaction request already has a terminal pre-start outcome",
            ));
        }
        Ok(())
    }

    fn validate_manual_compaction_request_satisfied(
        &self,
        satisfied: &tau_proto::AgentManualCompactionRequestSatisfied,
    ) -> Result<(), AgentEventValidationError> {
        let Some(request) = self.manual_compaction_requests.get(&satisfied.request_id) else {
            return Err(AgentEventValidationError::new(
                "manual compaction satisfaction references unknown request",
            ));
        };
        let successful_automatic = self
            .compaction_transactions
            .get(&satisfied.transaction_id)
            .is_some_and(|transaction| {
                matches!(
                    transaction.outcome,
                    Some(CompactionTransactionOutcome::Succeeded(_))
                ) && !matches!(
                    transaction.started.trigger,
                    tau_proto::StandaloneCompactionTrigger::Manual
                        | tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                        | tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
                )
            });
        if request.requested.target_agent_id != satisfied.target_agent_id
            || !matches!(
                request.requested.source,
                tau_proto::ManualCompactionSource::UiCompact { .. }
            )
            || request
                .requested
                .ui_source()
                .and_then(|source| source.eligible_automatic_transaction_id.as_ref())
                != Some(&satisfied.transaction_id)
            || !matches!(request.state, ManualCompactionRequestState::Waiting)
            || !successful_automatic
        {
            return Err(AgentEventValidationError::new(
                "queued UI compaction is not satisfiable by this transaction",
            ));
        }
        Ok(())
    }

    fn validate_compaction_failed(
        &self,
        failed: &tau_proto::AgentStandaloneCompactionFailed,
    ) -> Result<(), AgentEventValidationError> {
        if let Some(incomplete) = &failed.incomplete_response
            && (failed.reason != tau_proto::StandaloneCompactionFailureReason::OutputLengthExceeded
                || incomplete.backend.kind != tau_proto::ProviderBackendKind::PublicResponses)
        {
            return Err(AgentEventValidationError::new(
                "standalone incomplete response mismatched reason or backend",
            ));
        }
        if failed.reason == tau_proto::StandaloneCompactionFailureReason::OutputLengthExceeded
            && failed.incomplete_response.is_none()
        {
            return Err(AgentEventValidationError::new(
                "standalone output-length failure requires its incomplete response",
            ));
        }
        let Some(transaction) = self.compaction_transactions.get(&failed.transaction_id) else {
            let valid_stale_closure = self
                .automatic_compaction_decisions
                .get(&failed.transaction_id)
                .is_some_and(|decision| {
                    decision.finish_committed
                        && !decision.claimed
                        && !decision.closed
                        && !self.is_ancestor_head(
                            decision.cut,
                            self.head
                                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                        )
                        && failed.reason
                            == tau_proto::StandaloneCompactionFailureReason::StaleBranch
                        && failed.incomplete_response.is_none()
                        && failed.cut == decision.cut
                        && failed.resume_through.is_none()
                });
            return valid_stale_closure.then_some(()).ok_or_else(|| {
                AgentEventValidationError::new(
                    "standalone compaction failure references unknown transaction or invalid eager closure",
                )
            });
        };
        if transaction.outcome.is_some()
            || transaction.started.cut != failed.cut
            || transaction.started.resume_through != failed.resume_through
        {
            return Err(AgentEventValidationError::new(
                "standalone compaction failure mismatched transaction or duplicate outcome",
            ));
        }
        if let Some(incomplete) = &failed.incomplete_response
            && incomplete.agent_prompt_id != transaction.started.compact_prompt_id
        {
            return Err(AgentEventValidationError::new(
                "standalone incomplete response mismatched transaction",
            ));
        }
        if matches!(
            failed.reason,
            tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
                | tau_proto::StandaloneCompactionFailureReason::ContextIrreducible
        ) && transaction.standalone_context_rejection.is_none()
        {
            return Err(AgentEventValidationError::new(
                "standalone context rejection requires its canonical provider terminal first",
            ));
        }
        if let Some(plan) = &failed.context_retreat {
            let automatic = matches!(
                transaction.started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat { .. }
                    | tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
            );
            let current = self
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
            let immediate_predecessor =
                self.previous_provider_closed_cut_in_active_window(current, failed.cut);
            let expected_target = match &transaction.started.trigger {
                tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. } => {
                    Some(transaction.started.cut)
                }
                tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
                    roll_through,
                    ..
                } => Some(*roll_through),
                tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                    previous_transaction_id,
                } => match self.reactive_compaction_progress(previous_transaction_id) {
                    Some(ReactiveCompactionProgress::NeedsContinuation { target_cut }) => {
                        Some(target_cut)
                    }
                    Some(ReactiveCompactionProgress::ReachedTargetCut) => self
                        .compaction_transactions
                        .get(previous_transaction_id)
                        .map(|previous| previous.started.cut),
                    None => None,
                },
                tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                    failed_agent_prompt_id,
                } => self.reactive_compaction_target(failed_agent_prompt_id),
                _ => None,
            };
            if failed.reason != tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
                || !automatic
                || immediate_predecessor != Some(plan.cut)
                || plan.cut == tau_proto::AgentHead::Root
                || expected_target != Some(plan.roll_through)
                || plan.model != transaction.started.model
                || plan.originator != transaction.started.originator
                || plan.resume_through != transaction.started.resume_through
                || self
                    .compaction_transactions
                    .contains_key(&plan.transaction_id)
                || self.prompt_starts.contains_key(&plan.compact_prompt_id)
            {
                return Err(AgentEventValidationError::new(
                    "standalone context-retreat plan is not an exact automatic strict predecessor",
                ));
            }
        } else if failed.reason
            == tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
            && matches!(
                transaction.started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat { .. }
                    | tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
            )
        {
            return Err(AgentEventValidationError::new(
                "reducible standalone context rejection requires a durable successor plan",
            ));
        } else if failed.reason == tau_proto::StandaloneCompactionFailureReason::ContextIrreducible
        {
            let current = self
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
            let automatic = matches!(
                transaction.started.trigger,
                tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                    | tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat { .. }
                    | tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
            );
            if !automatic
                || self
                    .previous_provider_closed_cut_in_active_window(current, failed.cut)
                    .is_some()
            {
                return Err(AgentEventValidationError::new(
                    "context irreducible requires an automatic rejection at the oldest logical cut",
                ));
            }
        }
        if let tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure { reason, .. } =
            transaction.started.trigger
            && failed.reason != reason
        {
            return Err(AgentEventValidationError::new(
                "automatic preflight terminal reason mismatches its durable start",
            ));
        }
        if let tau_proto::StandaloneCompactionTrigger::ReactivePreflightFailure { reason, .. } =
            transaction.started.trigger
            && failed.reason != reason
        {
            return Err(AgentEventValidationError::new(
                "reactive preflight terminal reason mismatches its durable start",
            ));
        }
        Ok(())
    }

    fn validate_inference_checkpoint(
        &self,
        checkpoint: &tau_proto::AgentInferenceDispatchStarted,
    ) -> Result<(), AgentEventValidationError> {
        let Some(transaction_id) = &checkpoint.transaction_id else {
            self.validate_inference_checkpoint_correlations(checkpoint)?;
            return self.validate_inference_prompt_id_unique(checkpoint);
        };
        let transaction = self
            .compaction_transactions
            .get(transaction_id)
            .ok_or_else(|| {
                AgentEventValidationError::new(
                    "inference checkpoint references unknown compaction transaction",
                )
            })?;
        if !matches!(
            transaction.outcome,
            Some(CompactionTransactionOutcome::Succeeded(_))
        ) || transaction.checkpoint.is_some()
        {
            return Err(AgentEventValidationError::new(
                "inference checkpoint requires one successful uncheckpointed transaction",
            ));
        }
        if checkpoint.model.as_ref() != Some(&transaction.started.model)
            || checkpoint.operation != Some(tau_proto::PromptOperation::Inference)
            || checkpoint.activation_cut != Some(transaction.started.cut)
        {
            return Err(AgentEventValidationError::new(
                "inference checkpoint model, operation, or activation cut mismatches its transaction",
            ));
        }
        if !self.is_ancestor_head(transaction.started.cut, checkpoint.through) {
            return Err(AgentEventValidationError::new(
                "inference checkpoint is not on the compacted branch",
            ));
        }
        if transaction
            .started
            .resume_through
            .is_some_and(|resume| !self.is_ancestor_head(resume, checkpoint.through))
        {
            return Err(AgentEventValidationError::new(
                "inference checkpoint does not cover the owed activation",
            ));
        }
        self.validate_inference_prompt_id_unique(checkpoint)
    }

    fn validate_inference_checkpoint_correlations(
        &self,
        checkpoint: &tau_proto::AgentInferenceDispatchStarted,
    ) -> Result<(), AgentEventValidationError> {
        if let Some(owner) = &checkpoint.output_length_continuation {
            let source = self
                .inference_dispatches
                .get(&owner.source_agent_prompt_id)
                .ok_or_else(|| {
                    AgentEventValidationError::new(
                        "output-length continuation references an unknown source inference",
                    )
                })?;
            if owner.ordinal != 1
                || checkpoint.transaction_id.is_some()
                || source.output_length_steer_node.is_none()
                || checkpoint.through
                    != tau_proto::AgentHead::Node(
                        source
                            .output_length_steer_node
                            .expect("checked output-length steer"),
                    )
                || checkpoint.model != source.checkpoint.model
                || checkpoint.operation != source.checkpoint.operation
                || checkpoint.activation_cut != source.checkpoint.activation_cut
                || self.inference_dispatches.values().any(|dispatch| {
                    dispatch
                        .checkpoint
                        .output_length_continuation
                        .as_ref()
                        .is_some_and(|claimed| {
                            claimed.source_agent_prompt_id == owner.source_agent_prompt_id
                        })
                })
                || !matches!(
                    &source.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationPlanned {
                        outer_turn_id,
                        successor_agent_prompt_id,
                        ordinal: 1,
                        limit: 1,
                    } if outer_turn_id == &owner.outer_turn_id
                        && successor_agent_prompt_id == &checkpoint.agent_prompt_id
                )
            {
                return Err(AgentEventValidationError::new(
                    "output-length continuation mismatches its durable plan",
                ));
            }
        }
        match (
            checkpoint.model.as_ref(),
            checkpoint.operation,
            checkpoint.activation_cut,
        ) {
            (None, None, None) => Ok(()),
            (Some(_), Some(tau_proto::PromptOperation::Inference), Some(cut))
                if self.is_ancestor_head(cut, checkpoint.through) =>
            {
                Ok(())
            }
            _ => Err(AgentEventValidationError::new(
                "inference checkpoint must have one complete inference model/operation/cut correlation",
            )),
        }
    }

    fn validate_inference_prompt_id_unique(
        &self,
        checkpoint: &tau_proto::AgentInferenceDispatchStarted,
    ) -> Result<(), AgentEventValidationError> {
        if self
            .inference_dispatches
            .contains_key(&checkpoint.agent_prompt_id)
        {
            return Err(AgentEventValidationError::new(
                "inference checkpoint reused an agent prompt id",
            ));
        }
        Ok(())
    }

    fn validate_compaction_boundary(
        &self,
        parent: Option<NodeId>,
        compacted: &tau_proto::AgentCompacted,
    ) -> Result<(), AgentEventValidationError> {
        match (
            &compacted.transaction_id,
            compacted.cut,
            compacted.suffix_end,
            &compacted.compact_prompt_id,
            &compacted.model,
            compacted.operation,
        ) {
            (None, None, None, None, None, None) => Ok(()),
            (
                Some(transaction_id),
                Some(cut),
                Some(suffix_end),
                Some(compact_prompt_id),
                Some(model),
                Some(operation),
            ) => {
                if operation != tau_proto::PromptOperation::StandaloneCompaction {
                    return Err(AgentEventValidationError::new(
                        "compaction boundary has non-standalone operation",
                    ));
                }
                if suffix_end.as_option() != parent {
                    return Err(AgentEventValidationError::new(
                        "compaction suffix_end must equal the boundary parent",
                    ));
                }
                if !self.active_provider_window_contains(suffix_end, cut) {
                    return Err(AgentEventValidationError::new(
                        "compaction cut must occur in the source logical active window",
                    ));
                }
                let transaction = self
                    .compaction_transactions
                    .get(transaction_id)
                    .ok_or_else(|| {
                        AgentEventValidationError::new(
                            "compaction boundary references unknown transaction",
                        )
                    })?;
                if transaction.outcome.is_some()
                    || transaction.started.cut != cut
                    || &transaction.started.compact_prompt_id != compact_prompt_id
                    || &transaction.started.model != model
                    || transaction.started.operation != operation
                {
                    return Err(AgentEventValidationError::new(
                        "compaction boundary mismatched transaction or duplicate outcome",
                    ));
                }
                Ok(())
            }
            _ => Err(AgentEventValidationError::new(
                "new compaction boundary metadata must be complete",
            )),
        }
    }

    fn validate_tool_completion_event(
        &self,
        head: Option<NodeId>,
        event: &Event,
    ) -> Option<Result<(), AgentEventValidationError>> {
        match event {
            Event::ProviderToolResult(result) => Some(
                self.validate_terminal_tool_result(&result.call_id)
                    .and_then(|()| validate_tool_result_provider_content(result)),
            ),
            Event::ProviderToolError(error) | Event::ToolError(error) => {
                Some(self.validate_terminal_tool_result(&error.call_id))
            }
            Event::ToolCancelled(cancelled) => {
                Some(self.validate_terminal_tool_result(&cancelled.call_id))
            }
            Event::ToolBackgroundResult(result) => {
                Some(self.validate_background_tool_completion(head, &result.call_id))
            }
            Event::ToolBackgroundError(error) => {
                Some(self.validate_background_tool_completion(head, &error.call_id))
            }
            _ => None,
        }
    }

    /// Returns whether `event` is a content-free tool-correlation observation.
    ///
    /// These records deliberately do not affect transcript fold state. The
    /// containing per-agent journal supplies their agent identity.
    fn is_tool_observation_event(event: &Event) -> bool {
        matches!(
            event,
            Event::AgentToolDispatchObserved(_)
                | Event::AgentToolBackgroundedObserved(_)
                | Event::AgentToolWaitObserved(_)
                | Event::AgentToolWaitRegistered(_)
                | Event::AgentActivationQueued(_)
                | Event::AgentToolWaitSettled(_)
                | Event::AgentToolCancellationRequested(_)
                | Event::AgentToolTerminalClassified(_)
        )
    }

    fn is_agent_id_mismatch_event(event: &Event) -> bool {
        // Keep this list aligned with the historical mismatch diagnostics.
        // Agent metadata events are intentionally excluded: metadata for a
        // different agent still falls through to the generic non-transcript
        // rejection instead of the agent-id mismatch diagnostic.
        matches!(
            event,
            Event::AgentStarted(_)
                | Event::AgentInitializationContextSet(_)
                | Event::AgentUserInteractionRecorded(_)
                | Event::AgentDisplayNameSet(_)
                | Event::AgentPromptSubmitted(_)
                | Event::AgentPromptCreated(_)
                | Event::AgentPromptStarted(_)
                | Event::AgentUserMessageInjected(_)
                | Event::AgentPromptSteered(_)
                | Event::AgentPromptTerminated(_)
                | Event::AgentCompactionTriggered(_)
                | Event::AgentCompacted(_)
                | Event::AgentMessageSent(_)
                | Event::AgentMessageReceived(_)
                | Event::AgentHeadMoved(_)
                | Event::ProviderResponseFinished(_)
        )
    }

    fn validate_provider_response(
        &self,
        response: &tau_proto::ProviderResponseFinished,
    ) -> Result<(), AgentEventValidationError> {
        if let Some(decision) = &response.automatic_compaction_decision {
            let prompt_owns_outer_turn = self
                .prompt_starts
                .get(&response.agent_prompt_id)
                .is_some_and(|started| {
                    started.outer_turn_id.as_ref() == Some(&decision.outer_turn_id)
                });
            let valid = decision.threshold > tau_proto::TokenCount::ZERO
                && decision.evidence.as_ref().is_none_or(|evidence| {
                    evidence.provider_prompt_id == response.agent_prompt_id
                        && evidence.threshold == decision.threshold
                        && evidence.provider_input_tokens >= evidence.threshold
                        && Self::proactive_threshold_source_is_valid(&evidence.threshold_source)
                        && self.proactive_evidence_is_unclaimed(evidence)
                        && response.usage.as_ref().is_some_and(|usage| {
                            tau_proto::TokenCount::new(usage.prompt_sent_tokens)
                                == evidence.provider_input_tokens
                        })
                })
                && !self
                    .automatic_compaction_decisions
                    .contains_key(&decision.transaction_id)
                && !self
                    .compaction_transactions
                    .contains_key(&decision.transaction_id)
                && !self
                    .automatic_compaction_decisions
                    .values()
                    .any(|existing| {
                        existing.decision.outer_turn_id == decision.outer_turn_id
                            && !existing.claimed
                            && !existing.closed
                    })
                && self.active_outer_turn.as_ref() == Some(&decision.outer_turn_id)
                && self
                    .inference_dispatches
                    .get(&response.agent_prompt_id)
                    .is_some_and(|dispatch| {
                        !dispatch.finished
                            && dispatch.checkpoint.operation
                                == Some(tau_proto::PromptOperation::Inference)
                            && dispatch.checkpoint.model.as_ref() == Some(&decision.model)
                            && prompt_owns_outer_turn
                    })
                && self.pending_tool_rounds.is_empty()
                && self
                    .provider_response_tool_call_order(&response.output_items)
                    .is_empty()
                && response.stop_reason != tau_proto::ProviderStopReason::ToolCalls
                && response.recovery_disposition == tau_proto::ContextRecoveryDisposition::None
                && !matches!(
                    response.output_length_disposition,
                    tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
                )
                && !response
                    .output_items
                    .iter()
                    .any(|item| matches!(item, ContextItem::Compaction(_)));
            if !valid {
                return Err(AgentEventValidationError::new(
                    "automatic compaction decision is not owned by this canonical terminal",
                ));
            }
        }
        // Context recovery and output-length dispositions are mutually
        // exclusive on one response: a context-rejected reserved successor
        // carries recovery authority only, and an output-length terminal never
        // also plans reactive recovery. A durable fact combining both is
        // ambiguous and must fail closed.
        if response.recovery_disposition != tau_proto::ContextRecoveryDisposition::None
            && response.output_length_disposition != tau_proto::OutputLengthDisposition::None
        {
            return Err(AgentEventValidationError::new(
                "provider response cannot combine context recovery and output-length dispositions",
            ));
        }
        match &response.output_length_disposition {
            tau_proto::OutputLengthDisposition::None => {}
            tau_proto::OutputLengthDisposition::ContinuationPlanned {
                outer_turn_id,
                successor_agent_prompt_id,
                ordinal,
                limit,
            } => {
                let eligible = *ordinal == 1
                    && *limit == 1
                    && response.stop_reason == tau_proto::ProviderStopReason::Length
                    && response.originator.is_user()
                    && response.backend.as_ref().is_some_and(|backend| {
                        matches!(
                            backend.kind,
                            tau_proto::ProviderBackendKind::ChatCompletions
                                | tau_proto::ProviderBackendKind::PublicResponses
                        )
                    })
                    && response.error.is_none()
                    && response.failure_kind.is_none()
                    && response.has_replay_safe_reasoning_only_output()
                    && self
                        .inference_dispatches
                        .get(&response.agent_prompt_id)
                        .is_some_and(|dispatch| {
                            !dispatch.finished
                                && dispatch.fold_semantics
                                    == AgentJournalFoldSemantics::InferenceDeferredInputV1
                                && dispatch.checkpoint.transaction_id.is_none()
                                && dispatch.checkpoint.operation
                                    == Some(tau_proto::PromptOperation::Inference)
                                && dispatch.checkpoint.model.is_some()
                                && dispatch.checkpoint.activation_cut.is_some()
                                && self
                                    .prompt_starts
                                    .get(&response.agent_prompt_id)
                                    .is_some_and(|started| {
                                        started.outer_turn_id.as_ref() == Some(outer_turn_id)
                                            && started.originator == response.originator
                                            && started.originator.is_user()
                                            && Some(&started.model)
                                                == dispatch.checkpoint.model.as_ref()
                                            && Some(started.operation)
                                                == dispatch.checkpoint.operation
                                            && self.outer_turns.get(outer_turn_id).is_some_and(
                                                |turn| turn.session_id == started.session_id,
                                            )
                                    })
                        })
                    && self
                        .outer_turns
                        .get(outer_turn_id)
                        .is_some_and(|turn| !turn.finished)
                    && self.active_outer_turn.as_ref() == Some(outer_turn_id)
                    && self.output_length_budget_spent_outer_turn().is_none()
                    && !self
                        .inference_dispatches
                        .contains_key(successor_agent_prompt_id);
                if !eligible {
                    return Err(AgentEventValidationError::new(
                        "output-length plan requires one replay-safe reasoning-only inference",
                    ));
                }
            }
            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                outer_turn_id,
                source_agent_prompt_id,
                ordinal,
                outcome,
                outer_turn_finish_owed,
            } => {
                let matches_owner = *ordinal == 1
                    && self
                        .output_length_lineage_owner_for_prompt(&response.agent_prompt_id)
                        .is_some_and(|owner| {
                            owner.source_agent_prompt_id == *source_agent_prompt_id
                                && owner.outer_turn_id == *outer_turn_id
                                && owner.ordinal == *ordinal
                        })
                    && self
                        .inference_dispatches
                        .get(&response.agent_prompt_id)
                        .is_some_and(|dispatch| {
                            let harness_pre_start_terminal = matches!(
                                outcome,
                                tau_proto::OutputLengthContinuationOutcome::Cancelled
                            ) || (matches!(
                                outcome,
                                tau_proto::OutputLengthContinuationOutcome::Failed
                            ) && response
                                .output_items
                                .is_empty()
                                && response.error.is_some()
                                && response.failure_kind
                                    == Some(tau_proto::ProviderFailureKind::Unknown)
                                && response.backend.is_none()
                                && response.usage.is_none()
                                && response.provider_attempt == tau_proto::ProviderAttempt::ONE);
                            harness_pre_start_terminal
                                || self
                                    .prompt_starts
                                    .get(&response.agent_prompt_id)
                                    .is_some_and(|started| {
                                        started.outer_turn_id.as_ref() == Some(outer_turn_id)
                                            && Some(&started.model)
                                                == dispatch.checkpoint.model.as_ref()
                                            && Some(started.operation)
                                                == dispatch.checkpoint.operation
                                            && self.outer_turns.get(outer_turn_id).is_some_and(
                                                |turn| turn.session_id == started.session_id,
                                            )
                                    })
                        });
                let matches_outcome = match outcome {
                    tau_proto::OutputLengthContinuationOutcome::Completed => {
                        matches!(
                            response.stop_reason,
                            tau_proto::ProviderStopReason::EndTurn
                                | tau_proto::ProviderStopReason::ToolCalls
                        ) && response.error.is_none()
                            && response.failure_kind.is_none()
                    }
                    tau_proto::OutputLengthContinuationOutcome::Incomplete => {
                        response.stop_reason == tau_proto::ProviderStopReason::Length
                    }
                    tau_proto::OutputLengthContinuationOutcome::Failed => {
                        response.error.is_some()
                            || response.failure_kind.is_some()
                            || matches!(
                                response.stop_reason,
                                tau_proto::ProviderStopReason::Error
                                    | tau_proto::ProviderStopReason::RepetitionDetected
                            )
                    }
                    tau_proto::OutputLengthContinuationOutcome::Cancelled => {
                        response.stop_reason == tau_proto::ProviderStopReason::Error
                            && response.error.as_deref() == Some("cancelled")
                            && response.failure_kind.is_none()
                            && response.output_items.is_empty()
                    }
                };
                let finish_bit_valid = !*outer_turn_finish_owed
                    || (response.recovery_disposition
                        == tau_proto::ContextRecoveryDisposition::None
                        && (response.stop_reason != tau_proto::ProviderStopReason::ToolCalls
                            || !response
                                .output_items
                                .iter()
                                .any(|item| matches!(item, ContextItem::ToolCall(_))))
                        && !(response.stop_reason == tau_proto::ProviderStopReason::EndTurn
                            && response
                                .output_items
                                .iter()
                                .any(|item| matches!(item, ContextItem::ToolCall(_)))));
                if !matches_owner || !matches_outcome || !finish_bit_valid {
                    return Err(AgentEventValidationError::new(
                        "output-length terminal mismatches its durable continuation owner",
                    ));
                }
            }
        }
        if response.recovery_disposition
            == tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
        {
            let eligible_checkpoint = self
                .inference_dispatches
                .get(&response.agent_prompt_id)
                .is_some_and(|dispatch| {
                    !dispatch.finished
                        && dispatch.checkpoint.transaction_id.is_none()
                        && dispatch.checkpoint.operation
                            == Some(tau_proto::PromptOperation::Inference)
                        && dispatch.checkpoint.model.is_some()
                        && dispatch.checkpoint.activation_cut.is_some()
                });
            if !eligible_checkpoint
                || response.failure_kind
                    != Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
                || response.stop_reason != tau_proto::ProviderStopReason::Error
                || !response.output_items.is_empty()
            {
                return Err(AgentEventValidationError::new(
                    "planned reactive compaction requires one canonical no-output inference rejection",
                ));
            }
        }
        let mut seen = HashSet::new();
        let mut requests_tool_calls = false;
        for item in &response.output_items {
            if matches!(item, ContextItem::ToolResult(_)) {
                return Err(AgentEventValidationError::new(
                    "provider response cannot contain input-side tool result items",
                ));
            }
            let ContextItem::ToolCall(call) = item else {
                continue;
            };
            requests_tool_calls = true;
            if call.call_id.as_str().is_empty() {
                return Err(AgentEventValidationError::new(
                    "agent response contained an empty tool call id",
                ));
            }
            if !seen.insert(call.call_id.clone()) {
                return Err(AgentEventValidationError::new(format!(
                    "agent response contained duplicate tool call id: {}",
                    call.call_id
                )));
            }
            if self.tool_call_rounds.contains_key(&call.call_id) {
                return Err(AgentEventValidationError::new(format!(
                    "agent response reused open tool call id: {}",
                    call.call_id
                )));
            }
        }
        if requests_tool_calls && !self.pending_tool_rounds.is_empty() {
            return Err(AgentEventValidationError::new(
                "agent tree already has an open foreground provider tool round",
            ));
        }
        Ok(())
    }

    fn validate_prompt_terminated(
        &self,
        _head: Option<NodeId>,
        terminated: &tau_proto::AgentPromptTerminated,
    ) -> Result<(), AgentEventValidationError> {
        let Some(decision) = &terminated.automatic_compaction_decision else {
            return Ok(());
        };
        let prompt_owns_outer_turn = self
            .prompt_starts
            .get(&terminated.agent_prompt_id)
            .is_some_and(|started| started.outer_turn_id.as_ref() == Some(&decision.outer_turn_id));
        let valid = terminated.reason == tau_proto::AgentPromptTerminationReason::Canceled
            && decision.threshold > tau_proto::TokenCount::ZERO
            && decision.evidence.as_ref().is_none_or(|evidence| {
                evidence.threshold == decision.threshold
                    && self.historical_proactive_evidence_is_valid(evidence, &decision.model)
            })
            && !self
                .automatic_compaction_decisions
                .contains_key(&decision.transaction_id)
            && !self
                .compaction_transactions
                .contains_key(&decision.transaction_id)
            && self.active_outer_turn.as_ref() == Some(&decision.outer_turn_id)
            && self.pending_tool_rounds.is_empty()
            && prompt_owns_outer_turn
            && self
                .inference_dispatches
                .get(&terminated.agent_prompt_id)
                .is_some_and(|dispatch| {
                    !dispatch.finished
                        && dispatch.checkpoint.operation
                            == Some(tau_proto::PromptOperation::Inference)
                        && dispatch.checkpoint.model.as_ref() == Some(&decision.model)
                });
        if valid {
            Ok(())
        } else {
            Err(AgentEventValidationError::new(
                "automatic compaction decision is not owned by this prompt termination",
            ))
        }
    }

    fn validate_head_moved(&self, moved: &AgentHeadMoved) -> Result<(), AgentEventValidationError> {
        if let AgentHead::Node(node_id) = moved.head
            && self.node(node_id).is_none()
        {
            return Err(AgentEventValidationError::new(format!(
                "head move referenced unknown node_id: {node_id}",
            )));
        }
        Ok(())
    }

    fn record_terminal_tool_result(&mut self, item: ToolResultItem) -> Option<NodeId> {
        let Some(assistant_node_id) = self.tool_call_rounds.get(&item.call_id).copied() else {
            panic!(
                "terminal tool result for unknown or already-closed call_id: {}",
                item.call_id
            );
        };
        let Some(round) = self.pending_tool_rounds.get_mut(&assistant_node_id) else {
            panic!(
                "tool call mapped to missing pending round: {}",
                item.call_id
            );
        };
        round.terminal_results.insert(item.call_id.clone(), item);
        if round.terminal_results.len() != round.call_order.len() {
            return None;
        }
        let selected_head = self.head;
        let round_is_selected = selected_head.is_some_and(|selected| {
            self.is_ancestor_head(
                AgentHead::Node(assistant_node_id),
                AgentHead::Node(selected),
            )
        });

        let round = self
            .pending_tool_rounds
            .remove(&assistant_node_id)
            .expect("pending round should exist when terminal");
        for call_id in &round.call_order {
            self.tool_call_rounds.remove(call_id);
        }
        let items = round
            .call_order
            .iter()
            .map(|call_id| {
                round
                    .terminal_results
                    .get(call_id)
                    .cloned()
                    .expect("terminal round missing tool result")
            })
            .collect();
        let result_node_id = self.append_node_at(
            Some(round.assistant_node_id),
            AgentEntry::ToolResults { items },
        );
        for call_id in &round.call_order {
            self.terminal_tool_result_nodes
                .insert(call_id.clone(), result_node_id);
        }
        let mut head = result_node_id;
        let mut pending = std::mem::take(&mut self.pending_context_inputs);
        pending.sort_by_key(|input| input.durable_event_seq.get());
        for input in pending {
            head = self.append_context_node_at(Some(head), input.durable_event_seq, *input.entry);
        }
        if !round_is_selected {
            self.head = selected_head;
        }
        Some(head)
    }

    fn validate_background_tool_completion(
        &self,
        head: Option<NodeId>,
        call_id: &ToolCallId,
    ) -> Result<(), AgentEventValidationError> {
        if self.background_completed_tool_calls.contains(call_id) {
            return Err(AgentEventValidationError::new(format!(
                "duplicate background tool completion for call_id: {call_id}"
            )));
        }
        if !self.tool_call_ids_from_branch(head).contains(call_id)
            && !self.tool_call_rounds.contains_key(call_id)
        {
            return Err(AgentEventValidationError::new(format!(
                "background tool completion for unknown call_id: {call_id}"
            )));
        }
        Ok(())
    }

    fn validate_terminal_tool_result(
        &self,
        call_id: &ToolCallId,
    ) -> Result<(), AgentEventValidationError> {
        let Some(assistant_node_id) = self.tool_call_rounds.get(call_id) else {
            return Err(AgentEventValidationError::new(format!(
                "terminal tool result for unknown or already-closed call_id: {call_id}"
            )));
        };
        let Some(round) = self.pending_tool_rounds.get(assistant_node_id) else {
            return Err(AgentEventValidationError::new(format!(
                "tool call mapped to missing pending round: {call_id}"
            )));
        };
        if round.terminal_results.contains_key(call_id) {
            return Err(AgentEventValidationError::new(format!(
                "duplicate terminal tool result for call_id: {call_id}"
            )));
        }
        Ok(())
    }
}

fn durable_event_provider_image_bytes(event: &Event) -> u64 {
    match event {
        Event::ProviderToolResult(result) => tool_result_provider_image_bytes(result),
        Event::AgentCompacted(compacted) => compacted
            .replacement_window
            .iter()
            .map(context_item_provider_image_bytes)
            .sum(),
        _ => 0,
    }
}

fn context_item_provider_image_bytes(item: &ContextItem) -> u64 {
    match item {
        ContextItem::ToolResult(result) => result
            .provider_content
            .iter()
            .map(|part| {
                let tau_proto::ToolResultContentPart::Image(image) = part;
                image.data.len() as u64
            })
            .sum(),
        _ => 0,
    }
}

fn tool_result_provider_image_bytes(result: &tau_proto::ToolResult) -> u64 {
    result
        .provider_content
        .iter()
        .map(|part| {
            let tau_proto::ToolResultContentPart::Image(image) = part;
            image.data.len() as u64
        })
        .sum()
}

fn validate_tool_result_provider_content(
    result: &tau_proto::ToolResult,
) -> Result<(), AgentEventValidationError> {
    validate_provider_content_parts(result.tool_type, &result.provider_content)
}

fn validate_context_items_provider_content(
    items: &[ContextItem],
) -> Result<(), AgentEventValidationError> {
    for item in items {
        let ContextItem::ToolResult(result) = item else {
            continue;
        };
        if !matches!(result.status, tau_proto::ToolResultStatus::Success)
            && !result.provider_content.is_empty()
        {
            return Err(AgentEventValidationError::new(
                "non-successful tool result contains provider image content",
            ));
        }
        validate_provider_content_parts(result.tool_type, &result.provider_content)?;
    }
    Ok(())
}

fn validate_provider_content_parts(
    tool_type: tau_proto::ToolType,
    provider_content: &[tau_proto::ToolResultContentPart],
) -> Result<(), AgentEventValidationError> {
    const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
    const MAX_IMAGE_SIDE: u32 = 8192;
    const MAX_IMAGE_PIXELS: u64 = 16_777_216;
    const MAX_WEBP_IMAGE_PIXELS: u64 = 4_194_304;
    const MAX_IMAGE_DECODE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;

    if 1 < provider_content.len() {
        return Err(AgentEventValidationError::new(
            "tool result contains more than one provider image",
        ));
    }
    for part in provider_content {
        let tau_proto::ToolResultContentPart::Image(image) = part;
        if tool_type != tau_proto::ToolType::Function {
            return Err(AgentEventValidationError::new(
                "typed image output requires a function tool result",
            ));
        }
        let pixels = u64::from(image.width)
            .checked_mul(u64::from(image.height))
            .ok_or_else(|| AgentEventValidationError::new("image dimensions overflow"))?;
        if image.width == 0
            || image.height == 0
            || MAX_IMAGE_SIDE < image.width
            || MAX_IMAGE_SIDE < image.height
            || MAX_IMAGE_PIXELS < pixels
        {
            return Err(AgentEventValidationError::new(
                "image dimensions exceed provider-content limits",
            ));
        }
        if image.media_type == tau_proto::ImageMediaType::Webp && MAX_WEBP_IMAGE_PIXELS < pixels {
            return Err(AgentEventValidationError::new(
                "WebP image dimensions exceed provider-content limits",
            ));
        }
        if image.data.is_empty() || MAX_IMAGE_BYTES < image.data.len() {
            return Err(AgentEventValidationError::new(
                "image bytes exceed provider-content limits",
            ));
        }
        let magic_matches = match image.media_type {
            tau_proto::ImageMediaType::Png => image.data.starts_with(b"\x89PNG\r\n\x1a\n"),
            tau_proto::ImageMediaType::Jpeg => image.data.starts_with(b"\xff\xd8\xff"),
            tau_proto::ImageMediaType::Webp => {
                image.data.starts_with(b"RIFF")
                    && image.data.get(8..12).is_some_and(|kind| kind == b"WEBP")
            }
        };
        if !magic_matches {
            return Err(AgentEventValidationError::new(
                "image media type does not match encoded bytes",
            ));
        }
        if provider_image_is_animated(&image.data, image.media_type) {
            return Err(AgentEventValidationError::new(
                "animated provider image content is not supported",
            ));
        }
        let format = match image.media_type {
            tau_proto::ImageMediaType::Png => image::ImageFormat::Png,
            tau_proto::ImageMediaType::Jpeg => image::ImageFormat::Jpeg,
            tau_proto::ImageMediaType::Webp => image::ImageFormat::WebP,
        };
        let mut reader =
            image::ImageReader::with_format(path_std_io::Cursor::new(&image.data), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_SIDE);
        limits.max_image_height = Some(MAX_IMAGE_SIDE);
        limits.max_alloc = Some(MAX_IMAGE_DECODE_ALLOC_BYTES);
        reader.limits(limits);
        use image::ImageDecoder as _;
        let decoder = reader.into_decoder().map_err(|error| {
            AgentEventValidationError::new(format!("cannot decode provider image header: {error}"))
        })?;
        if decoder.dimensions() != (image.width, image.height) {
            return Err(AgentEventValidationError::new(
                "provider image dimensions do not match encoded bytes",
            ));
        }
        let decoded_byte_limit = if image.media_type == tau_proto::ImageMediaType::Webp {
            MAX_IMAGE_DECODE_ALLOC_BYTES / 2
        } else {
            MAX_IMAGE_DECODE_ALLOC_BYTES
        };
        if decoded_byte_limit < decoder.total_bytes() {
            return Err(AgentEventValidationError::new(
                "decoded provider image exceeds allocation limit",
            ));
        }
        image::DynamicImage::from_decoder(decoder).map_err(|error| {
            AgentEventValidationError::new(format!("cannot fully decode provider image: {error}"))
        })?;
    }
    Ok(())
}

fn provider_image_is_animated(bytes: &[u8], media_type: tau_proto::ImageMediaType) -> bool {
    match media_type {
        tau_proto::ImageMediaType::Png => {
            let mut offset = 8_usize;
            while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
                let length = u32::from_be_bytes([
                    bytes[offset],
                    bytes[offset + 1],
                    bytes[offset + 2],
                    bytes[offset + 3],
                ]) as usize;
                let kind = &bytes[offset + 4..offset + 8];
                if kind == b"acTL" {
                    return true;
                }
                if kind == b"IDAT" || kind == b"IEND" {
                    return false;
                }
                let Some(next) = offset
                    .checked_add(12)
                    .and_then(|offset| offset.checked_add(length))
                else {
                    return false;
                };
                if bytes.len() < next {
                    return false;
                }
                offset = next;
            }
            false
        }
        tau_proto::ImageMediaType::Webp => {
            let mut offset = 12_usize;
            while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
                let kind = &bytes[offset..offset + 4];
                if kind == b"ANIM" || kind == b"ANMF" {
                    return true;
                }
                let length = u32::from_le_bytes([
                    bytes[offset + 4],
                    bytes[offset + 5],
                    bytes[offset + 6],
                    bytes[offset + 7],
                ]) as usize;
                let Some(next) = offset
                    .checked_add(8)
                    .and_then(|offset| offset.checked_add(length))
                    .and_then(|offset| offset.checked_add(length % 2))
                else {
                    return false;
                };
                if bytes.len() < next {
                    return false;
                }
                offset = next;
            }
            false
        }
        tau_proto::ImageMediaType::Jpeg => false,
    }
}

/// Typed publishing provenance retained with a persisted semantic event.
///
/// JSON and CBOR encode this externally tagged enum as a single-entry
/// `{"connection": id}` or `{"extension": name}` map.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedEventSource {
    /// Run-local connection provenance captured when the event was published.
    Connection(ConnectionId),
    /// Stable configured extension publisher retained across replay.
    Extension(ExtensionName),
}

impl PersistedEventSource {
    /// Return the captured connection identity when this provenance names one.
    #[must_use]
    pub const fn connection_id(&self) -> Option<&ConnectionId> {
        match self {
            Self::Connection(connection_id) => Some(connection_id),
            Self::Extension(_) => None,
        }
    }
}

/// Private agent-journal transcript-fold semantics.
///
/// This discriminator never crosses the harness-extension protocol. Missing
/// fields decode as [`Self::Legacy`] so historical node allocation remains
/// byte-for-byte positional.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentJournalFoldSemantics {
    /// Preserve historical commit-order placement.
    #[default]
    Legacy,
    /// Let one marked ordinary inference own later same-branch input placement.
    InferenceDeferredInputV1,
}

impl AgentJournalFoldSemantics {
    /// Select semantics for one newly authored event.
    pub(crate) fn for_new_event(event: &Event) -> Self {
        if Self::marked_checkpoint(event).is_some() {
            Self::InferenceDeferredInputV1
        } else {
            Self::Legacy
        }
    }

    /// Return the ordinary inference checkpoint eligible for V1 ownership.
    fn marked_checkpoint(event: &Event) -> Option<&tau_proto::AgentInferenceDispatchStarted> {
        let Event::AgentInferenceDispatchStarted(checkpoint) = event else {
            return None;
        };
        (checkpoint.transaction_id.is_none()
            && checkpoint.operation == Some(tau_proto::PromptOperation::Inference)
            && checkpoint.model.is_some()
            && checkpoint.activation_cut.is_some())
        .then_some(checkpoint)
    }

    /// Validate that this marker is legal for the event it decorates.
    fn validates(self, event: &Event) -> bool {
        self == Self::Legacy || Self::marked_checkpoint(event).is_some()
    }
}

/// One durable agent-scoped protocol event.
///
/// `parent` is the explicit fold parent that was passed to
/// `AgentStore::append_agent_event_at` at write time. Carrying it on the
/// persisted record (rather than on the wire) preserves cross-conversation
/// branching across replay without requiring the publisher-side
/// `UiNavigateTree` head-bouncing dance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedAgentEvent {
    /// Opaque random identity used only for explicit observation references.
    ///
    /// This identity carries no ordering or causality semantics.
    pub observation_id: tau_proto::ObservationId,
    /// Sequence within this agent's durable `events.cbor` stream.
    ///
    /// This is persisted to catch reordered, duplicated, or spliced logs during
    /// load. The implied sequence from file order is still authoritative for
    /// replay; load rejects records where this stored value disagrees with the
    /// record's zero-based position.
    pub seq: PersistedAgentEventSeq,
    /// Typed publisher provenance, when known.
    pub source: Option<PersistedEventSource>,
    /// Agent-scoped protocol event.
    pub event: Event,
    /// Explicit fold parent used when replaying this record into the agent
    /// tree.
    pub parent: AgentEventParent,
    /// Private projection semantics selected for this journal occurrence.
    #[serde(default, skip_serializing_if = "AgentJournalFoldSemantics::is_legacy")]
    pub fold_semantics: AgentJournalFoldSemantics,
    /// Wall-clock micros since UNIX epoch when the event was
    /// appended, matching the value carried by the harness event delivery and
    /// stamped in [`crate::AgentStore::append_agent_event_at`]. `UnixMicros(0)`
    /// on records written before this field existed (deserialized via
    /// `#[serde(default)]`). Used for offline inspection — inter-turn
    /// timing, RPM bursts, cache-miss correlation — never for replay
    /// semantics.
    #[serde(default)]
    pub recorded_at: UnixMicros,
}

/// Crate-private proof that one exact borrowed record passed live validation.
pub(crate) struct PrevalidatedPersistedRecord<'tree, 'record> {
    /// Exact tree state against which the record was validated.
    tree: &'tree AgentTree,
    /// Exact record whose immutable fields were validated.
    record: &'record PersistedAgentEvent,
}

impl AgentJournalFoldSemantics {
    /// Return whether this record uses historical placement.
    #[must_use]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }
}

/// Per-agent sidecar metadata at `<agents_dir>/<agent_id>/meta.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentMeta {
    /// Unix epoch seconds when the agent was first created.
    pub created_at: u64,
    /// Unix epoch seconds of the most recent append.
    pub last_touched: u64,
    /// Unix epoch seconds of the most recent human-authored interaction.
    pub last_user_interaction_time: u64,
    /// Optional human-friendly name shown in UIs. Falls back to the agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Preview of the latest user-authored prompt, used by the resume picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_user_prompt_preview: Option<String>,
}

/// Canonical durable-session manifest at
/// `<sessions_dir>/<session_id>/meta.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Canonical Unix epoch seconds when the durable session was first created.
    pub created_at: u64,
    /// Derived Unix epoch seconds used for ordering and retention.
    pub last_touched: u64,
}

#[cfg(test)]
mod tests;

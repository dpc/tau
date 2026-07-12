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

use serde::{Deserialize, Serialize};
use tau_proto::{
    AgentHead, AgentHeadMoved, AgentId, AgentMessageId, AgentMessageKind, AgentMessageReceived,
    AgentMessageRecipient, AgentMessageSent, ConnectionId, ContentPart, ContextItem, ContextRole,
    Event, MessageItem, PromptOriginator, ProviderBackend, ProviderTokenUsage, ToolBackgroundError,
    ToolBackgroundResult, ToolCallId, ToolCallItem, ToolName, ToolResultItem, ToolResultKind,
    ToolResultStatus, ToolType, UnixMicros,
};

fn message_envelope_item(
    direction: tau_proto::MessageDirection,
    envelope: &tau_proto::MessageEnvelope,
) -> tau_proto::MessageEnvelopeItem {
    let source_label = match &envelope.source {
        tau_proto::MessageEndpoint::Agent {
            agent_id,
            display_name,
            ..
        } => display_name.clone().unwrap_or_else(|| agent_id.to_string()),
        tau_proto::MessageEndpoint::User => "user".to_owned(),
        tau_proto::MessageEndpoint::External {
            stable_id,
            display_name,
            ..
        } => match envelope.trust.identity {
            tau_proto::SenderIdentityAssurance::VerifiedAccount => {
                match (display_name, stable_id) {
                    (Some(display), Some(stable)) if display != stable => {
                        format!("{display} ({stable})")
                    }
                    (_, Some(stable)) => stable.clone(),
                    (Some(display), None) => format!("unverified {display}"),
                    (None, None) => "unverified external sender".to_owned(),
                }
            }
            tau_proto::SenderIdentityAssurance::RoomMembership => format!(
                "room occupant {}",
                display_name
                    .clone()
                    .or_else(|| stable_id.clone())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
            tau_proto::SenderIdentityAssurance::DisplayOnly
            | tau_proto::SenderIdentityAssurance::Unknown => format!(
                "unverified {}",
                display_name
                    .clone()
                    .or_else(|| stable_id.clone())
                    .unwrap_or_else(|| "external sender".to_owned())
            ),
            tau_proto::SenderIdentityAssurance::AuthenticatedTauAgent => display_name
                .clone()
                .or_else(|| stable_id.clone())
                .unwrap_or_else(|| "external sender".to_owned()),
        },
    };
    tau_proto::MessageEnvelopeItem {
        direction,
        envelope: envelope.clone(),
        model_presentation: tau_proto::MessageModelPresentation {
            transport_label: envelope.transport.name.clone(),
            source_label,
            live_send_tool: None,
            conversation_label: envelope.conversation.as_ref().and_then(|conversation| {
                conversation
                    .display_name
                    .clone()
                    .or_else(|| conversation.stable_id.clone())
            }),
        },
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEventValidationError {
    message: String,
}

impl AgentEventValidationError {
    fn new(message: impl Into<String>) -> Self {
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
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
        /// Context items appended by the user or harness.
        items: Vec<ContextItem>,
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
        /// Recipient agent or user.
        recipient: AgentMessageRecipient,
        /// Delivery source semantics.
        kind: AgentMessageKind,
        /// Typed watched-turn state for receiver-only lifecycle projections.
        watch_turn_state: Option<tau_proto::AgentWatchTurnStateNotification>,
        /// Typed provider status for receiver-only watch projections.
        watch_provider_status: Option<tau_proto::AgentWatchProviderStatusNotification>,
        /// Message body.
        message: String,
    },
    /// Canonical v2 transport message preserved as a typed provider item.
    MessageEnvelope {
        /// Direction, envelope, and harness-derived presentation policy.
        item: Box<tau_proto::MessageEnvelopeItem>,
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

#[derive(Clone, Debug, Default, PartialEq)]
struct PendingToolRound {
    assistant_node_id: NodeId,
    call_order: Vec<ToolCallId>,
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
    pub(crate) head: Option<NodeId>,
    pub(crate) display_name: Option<String>,
    /// Sequence the next durable event appended to this agent's log
    /// should receive. Cached here so that
    /// [`AgentStore::append_agent_event_at`] doesn't have to
    /// re-decode the entire on-disk log on every write to look at
    /// the last sequence (the previous behaviour was O(N) per append,
    /// quadratic over a long agent).
    pub(crate) next_event_seq: PersistedAgentEventSeq,
    /// Number of materialized agent prompts already present in this
    /// derived tree. Maintained while replaying/applying events so callers can
    /// mint the next per-agent prompt id without rescanning the event log.
    materialized_prompt_count: u64,
    /// Number of ordinary inference prompts, excluding compaction operations.
    ordinary_inference_generation: u64,
    pending_tool_rounds: HashMap<NodeId, PendingToolRound>,
    tool_call_rounds: HashMap<ToolCallId, NodeId>,
    /// Message facts committed while provider tool adjacency is open. They are
    /// materialized immediately after the terminal tool-result node.
    pending_message_envelopes: Vec<tau_proto::MessageEnvelopeItem>,
    /// Globally unique tool calls that already have one real background
    /// completion event.
    background_completed_tool_calls: HashSet<ToolCallId>,
    /// Durable standalone-compaction transactions folded from control facts.
    compaction_transactions: HashMap<tau_proto::CompactionTransactionId, CompactionTransactionFold>,
    /// Durable insertion order for deterministic recovery projection.
    compaction_transaction_order: Vec<tau_proto::CompactionTransactionId>,
    /// Durable model-requested compactions, including requests not started yet.
    manual_compaction_requests:
        HashMap<tau_proto::CompactionRequestId, ManualCompactionRequestFold>,
    /// Durable request insertion order for deterministic recovery.
    manual_compaction_request_order: Vec<tau_proto::CompactionRequestId>,
    /// All durable inference checkpoints keyed by their provider prompt id.
    inference_dispatches: HashMap<tau_proto::AgentPromptId, InferenceDispatchFold>,
    /// Durable inference checkpoint insertion order.
    inference_dispatch_order: Vec<tau_proto::AgentPromptId>,
}

/// Folded state for one durable inference checkpoint.
#[derive(Clone, Debug, PartialEq)]
struct InferenceDispatchFold {
    checkpoint: tau_proto::AgentInferenceDispatchStarted,
    finished: bool,
    recovery_disposition: tau_proto::ContextRecoveryDisposition,
}

/// Validated durable state for one standalone compaction transaction.
#[derive(Clone, Debug, PartialEq)]
struct CompactionTransactionFold {
    started: tau_proto::AgentStandaloneCompactionStarted,
    outcome: Option<CompactionTransactionOutcome>,
    checkpoint: Option<tau_proto::AgentInferenceDispatchStarted>,
    inference_finished: bool,
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
    /// Transaction that uniquely claimed this request, if started.
    transaction_id: Option<tau_proto::CompactionTransactionId>,
    /// Terminal pre-start failure, if starting became impossible.
    failed: Option<tau_proto::AgentManualCompactionRequestFailed>,
}

/// Durable state of an accepted model-requested compaction.
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
    /// Compaction failed and left the target blocked.
    Failed(tau_proto::AgentStandaloneCompactionFailed),
}

/// Durable standalone-compaction state reconstructed by the core fold.
#[derive(Clone, Debug, PartialEq)]
pub enum StandaloneCompactionRecovery {
    /// A start has no terminal outcome and must be repaired as interrupted.
    Interrupted(tau_proto::AgentStandaloneCompactionStarted),
    /// A terminal failure retains an explicit recovery obligation.
    Blocked {
        /// Durable terminal failure.
        failed: tau_proto::AgentStandaloneCompactionFailed,
        /// Actual provider prompt reserved by the matching durable start.
        compact_prompt_id: tau_proto::AgentPromptId,
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
    },
    /// A checkpoint exists without a matching durable provider terminal
    /// response.
    DispatchUncertain(tau_proto::AgentInferenceDispatchStarted),
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
    /// projection.
    #[must_use]
    pub fn standalone_compaction_recovery(&self) -> Option<StandaloneCompactionRecovery> {
        let id = self.compaction_transaction_order.last()?;
        let transaction = self.compaction_transactions.get(id)?;
        match (&transaction.outcome, &transaction.checkpoint) {
            (None, _) => Some(StandaloneCompactionRecovery::Interrupted(
                transaction.started.clone(),
            )),
            (Some(CompactionTransactionOutcome::Failed(failed)), _) => {
                Some(StandaloneCompactionRecovery::Blocked {
                    failed: failed.clone(),
                    compact_prompt_id: transaction.started.compact_prompt_id.clone(),
                })
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

    /// Returns model-requested compaction state in durable acceptance order.
    #[must_use]
    pub fn manual_compaction_recoveries(&self) -> Vec<ManualCompactionRecovery> {
        self.manual_compaction_request_order
            .iter()
            .filter_map(|id| {
                let request = self.manual_compaction_requests.get(id)?;
                if let Some(failed) = &request.failed {
                    return Some(ManualCompactionRecovery::Failed {
                        requested: request.requested.clone(),
                        failed: failed.clone(),
                    });
                }
                if let Some(transaction_id) = &request.transaction_id {
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
                Some(ManualCompactionRecovery::Waiting(request.requested.clone()))
            })
            .collect()
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

    /// Returns canonical message items from materialized nodes and durable
    /// facts deferred behind an open provider tool round.
    pub fn all_message_envelopes(&self) -> impl Iterator<Item = &tau_proto::MessageEnvelopeItem> {
        self.nodes
            .iter()
            .filter_map(|node| match &node.entry {
                AgentEntry::MessageEnvelope { item } => Some(item.as_ref()),
                _ => None,
            })
            .chain(self.pending_message_envelopes.iter())
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
                if round.terminal_results.contains_key(call_id) {
                    continue;
                }
                if let Some(call) = output_items.iter().find_map(|item| match item {
                    ContextItem::ToolCall(call) if &call.call_id == call_id => Some(call),
                    _ => None,
                }) {
                    calls.push(call);
                }
            }
        }
        calls
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
        for entry in events {
            match &entry.event {
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
                        },
                    );
                }
                Event::ToolBackgroundResult(result) => {
                    if !branch_call_ids.contains(&result.call_id) {
                        continue;
                    }
                    let completion = BackgroundToolCompletion::Result(result.clone());
                    completions.insert(result.call_id.clone(), completion.clone());
                    if completion_order_seen.insert(result.call_id.clone()) {
                        completion_order.push(result.call_id.clone());
                    }
                    if let Some(state) = states.get_mut(&result.call_id) {
                        state.completion = Some(completion);
                    }
                }
                Event::ToolBackgroundError(error) => {
                    if !branch_call_ids.contains(&error.call_id) {
                        continue;
                    }
                    let completion = BackgroundToolCompletion::Error(error.clone());
                    completions.insert(error.call_id.clone(), completion.clone());
                    if completion_order_seen.insert(error.call_id.clone()) {
                        completion_order.push(error.call_id.clone());
                    }
                    if let Some(state) = states.get_mut(&error.call_id) {
                        state.completion = Some(completion);
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
            tau_proto::AgentHead::Node(ancestor) => self
                .branch_node_ids_from(descendant.as_option())
                .contains(&ancestor),
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

    fn append_node_at(&mut self, parent: Option<NodeId>, entry: AgentEntry) -> NodeId {
        let id = NodeId::new(self.nodes.len() as u64);
        self.nodes.push(AgentNode {
            id,
            parent_id: parent,
            entry,
        });
        self.head = Some(id);
        id
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

    pub(crate) fn try_from_events(
        agent_id: AgentId,
        events: &[PersistedAgentEvent],
    ) -> Result<Self, AgentEventValidationError> {
        let mut tree = Self {
            agent_id,
            metadata: BTreeMap::new(),
            nodes: Vec::new(),
            head: None,
            display_name: None,
            next_event_seq: PersistedAgentEventSeq::new(0),
            materialized_prompt_count: 0,
            ordinary_inference_generation: 0,
            pending_tool_rounds: HashMap::new(),
            tool_call_rounds: HashMap::new(),
            pending_message_envelopes: Vec::new(),
            background_completed_tool_calls: HashSet::new(),
            compaction_transactions: HashMap::new(),
            compaction_transaction_order: Vec::new(),
            manual_compaction_requests: HashMap::new(),
            manual_compaction_request_order: Vec::new(),
            inference_dispatches: HashMap::new(),
            inference_dispatch_order: Vec::new(),
        };
        for entry in events {
            tree.validate_event_at(entry.parent, &entry.event)?;
            tree.apply_event_at(entry.parent, &entry.event);
            tree.next_event_seq = entry.seq.next();
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

    /// Returns the number of materialized agent prompts folded into this tree.
    #[must_use]
    pub fn materialized_prompt_count(&self) -> u64 {
        self.materialized_prompt_count
    }

    /// Returns target-owned ordinary inference progress for manual rate guards.
    #[must_use]
    pub fn ordinary_inference_generation(&self) -> u64 {
        self.ordinary_inference_generation
    }

    /// Bumps the cached next-event-seq after a successful append.
    /// Crate-internal — only the agent store mutates this.
    pub(crate) fn advance_next_event_seq(&mut self) {
        self.next_event_seq = self.next_event_seq.next();
    }

    /// Incrementally apply one durable event to the tree. Mirrors the
    /// fold rules of [`AgentTree::from_events`]. Tree-folding events
    /// are parented at the current `head`; for callers that need to
    /// fold an event onto a *specific* branch (without first emitting
    /// an [`AgentHeadMoved`] to bounce `head` there), use
    /// [`AgentTree::apply_event_at`].
    pub fn apply_event(&mut self, event: &Event) {
        self.apply_event_at(AgentEventParent::InheritHead, event);
    }

    /// Like [`AgentTree::apply_event`] but parents the produced node using an
    /// explicit fold-parent policy.
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
        self.count_materialized_prompt(event);
        self.apply_compaction_control_event(event);
        if self.apply_side_state_event(event) {
            return None;
        }
        self.apply_transcript_event(parent.resolve(self.head), event)
    }

    fn apply_compaction_control_event(&mut self, event: &Event) {
        match event {
            Event::AgentManualCompactionRequested(requested) => {
                self.manual_compaction_request_order
                    .push(requested.request_id.clone());
                self.manual_compaction_requests.insert(
                    requested.request_id.clone(),
                    ManualCompactionRequestFold {
                        requested: requested.clone(),
                        transaction_id: None,
                        failed: None,
                    },
                );
            }
            Event::AgentManualCompactionRequestFailed(failed) => {
                if let Some(request) = self.manual_compaction_requests.get_mut(&failed.request_id) {
                    request.failed = Some(failed.clone());
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
                    },
                );
                if let tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                    request_id, ..
                } = &started.trigger
                    && let Some(request) = self.manual_compaction_requests.get_mut(request_id)
                {
                    request.transaction_id = Some(started.transaction_id.clone());
                }
            }
            Event::AgentStandaloneCompactionFailed(failed) => {
                if let Some(transaction) =
                    self.compaction_transactions.get_mut(&failed.transaction_id)
                {
                    transaction.outcome =
                        Some(CompactionTransactionOutcome::Failed(failed.clone()));
                }
            }
            Event::AgentCompacted(compacted) => {
                if let Some(transaction_id) = &compacted.transaction_id
                    && let Some(transaction) = self.compaction_transactions.get_mut(transaction_id)
                {
                    transaction.outcome =
                        Some(CompactionTransactionOutcome::Succeeded(compacted.clone()));
                }
            }
            Event::AgentInferenceDispatchStarted(checkpoint) => {
                self.inference_dispatch_order
                    .push(checkpoint.agent_prompt_id.clone());
                self.inference_dispatches.insert(
                    checkpoint.agent_prompt_id.clone(),
                    InferenceDispatchFold {
                        checkpoint: checkpoint.clone(),
                        finished: false,
                        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                    },
                );
                if let Some(transaction_id) = &checkpoint.transaction_id
                    && let Some(transaction) = self.compaction_transactions.get_mut(transaction_id)
                {
                    transaction.checkpoint = Some(checkpoint.clone());
                }
            }
            Event::ProviderResponseFinished(response) => {
                if let Some(dispatch) = self.inference_dispatches.get_mut(&response.agent_prompt_id)
                {
                    dispatch.finished = true;
                    dispatch.recovery_disposition = response.recovery_disposition;
                }
                for transaction in self.compaction_transactions.values_mut() {
                    if transaction.checkpoint.as_ref().is_some_and(|checkpoint| {
                        checkpoint.agent_prompt_id == response.agent_prompt_id
                    }) {
                        transaction.inference_finished = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn count_materialized_prompt(&mut self, event: &Event) {
        if let Event::AgentPromptCreated(created) = event {
            self.materialized_prompt_count += 1;
            if created.operation == tau_proto::PromptOperation::Inference {
                self.ordinary_inference_generation =
                    self.ordinary_inference_generation.saturating_add(1);
            }
        }
    }

    fn apply_side_state_event(&mut self, event: &Event) -> bool {
        match event {
            Event::AgentStarted(started) => self.apply_agent_started(started),
            Event::AgentDisplayNameSet(name) => self.update_display_name(&name.display_name),
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
        }
    }

    fn apply_transcript_event(&mut self, parent: Option<NodeId>, event: &Event) -> Option<NodeId> {
        match event {
            Event::AgentPromptSubmitted(prompt) => Some(self.append_user_text_input(
                parent,
                prompt.text.clone(),
                prompt.inference_activation,
            )),
            Event::AgentUserMessageInjected(injected) => Some(self.append_user_text_input(
                parent,
                injected.text.clone(),
                injected.inference_activation,
            )),
            Event::AgentPromptSteered(steered) => Some(self.append_user_text_input(
                parent,
                steered.text.clone(),
                steered.inference_activation,
            )),
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
                .agent_message_entry_from_sent(message)
                .map(|entry| self.append_node_at(parent, entry)),
            Event::AgentMessageReceived(message) => self
                .agent_message_entry_from_received(message)
                .map(|entry| self.append_node_at(parent, entry)),
            Event::AgentMessageIncoming(message) if message.recipient_id == self.agent_id => self
                .record_message_envelope(
                    parent,
                    Box::new(message_envelope_item(
                        tau_proto::MessageDirection::Incoming,
                        &message.envelope,
                    )),
                ),
            Event::AgentMessageOutgoing(message) if message.sender_id == self.agent_id => self
                .record_message_envelope(
                    parent,
                    Box::new(message_envelope_item(
                        tau_proto::MessageDirection::Outgoing,
                        &message.envelope,
                    )),
                ),
            Event::ProviderResponseFinished(response) => {
                Some(self.apply_provider_response_finished(parent, response))
            }
            Event::ProviderToolResult(result) => self.record_provider_tool_result(result),
            Event::ProviderToolError(error) => self.record_provider_tool_error(error),
            Event::ToolCancelled(cancelled) => self.record_cancelled_tool_result(cancelled),
            _ => None,
        }
    }

    fn append_user_text_input(
        &mut self,
        parent: Option<NodeId>,
        text: String,
        inference_activation: bool,
    ) -> NodeId {
        self.append_node_at(
            parent,
            AgentEntry::UserInput {
                items: vec![ContextItem::Message(MessageItem {
                    role: ContextRole::User,
                    content: vec![ContentPart::Text { text }],
                    phase: None,
                    responses_raw_json: None,
                })],
                inference_activation,
            },
        )
    }

    fn record_message_envelope(
        &mut self,
        parent: Option<NodeId>,
        item: Box<tau_proto::MessageEnvelopeItem>,
    ) -> Option<NodeId> {
        if !self.pending_tool_rounds.is_empty() {
            self.pending_message_envelopes.push(*item);
            return None;
        }
        Some(self.append_node_at(parent, AgentEntry::MessageEnvelope { item }))
    }

    fn apply_provider_response_finished(
        &mut self,
        parent: Option<NodeId>,
        response: &tau_proto::ProviderResponseFinished,
    ) -> NodeId {
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
        self.open_pending_tool_round(node_id, call_order);
        node_id
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
        })
    }

    fn agent_message_entry_from_sent(&self, message: &AgentMessageSent) -> Option<AgentEntry> {
        (message.sender_id == self.agent_id).then(|| AgentEntry::AgentMessage {
            message_id: message.message_id.clone(),
            direction: AgentMessageDirection::Outbound,
            sender_id: message.sender_id.clone(),
            sender_session_id: None,
            recipient: message.recipient.clone(),
            kind: message.kind,
            watch_turn_state: None,
            watch_provider_status: None,
            message: message.message.clone(),
        })
    }

    fn agent_message_entry_from_received(
        &self,
        message: &AgentMessageReceived,
    ) -> Option<AgentEntry> {
        (message.recipient_id == self.agent_id).then(|| AgentEntry::AgentMessage {
            message_id: message.message_id.clone(),
            direction: AgentMessageDirection::Inbound,
            sender_id: message.sender_id.clone(),
            sender_session_id: message.sender_session_id.clone(),
            recipient: AgentMessageRecipient::Agent {
                agent_id: message.recipient_id.clone(),
            },
            kind: message.kind,
            watch_turn_state: message.watch_turn_state.clone(),
            watch_provider_status: message.watch_provider_status.clone(),
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
            Event::AgentDisplayNameSet(name) if name.agent_id == self.agent_id => {
                Some(validate_display_name(&name.display_name))
            }
            Event::AgentMetadataSet(set) if set.agent_id == self.agent_id => Some(Ok(())),
            Event::AgentMetadataUnset(unset) if unset.agent_id == self.agent_id => Some(Ok(())),
            _ => None,
        }
    }

    fn validate_agent_message_event(
        &self,
        event: &Event,
    ) -> Option<Result<(), AgentEventValidationError>> {
        match event {
            Event::AgentMessageSent(message)
                if self.agent_message_entry_from_sent(message).is_some() =>
            {
                Some(Ok(()))
            }
            Event::AgentMessageReceived(message)
                if self.agent_message_entry_from_received(message).is_some() =>
            {
                let payload_matches_kind = ((message.kind == AgentMessageKind::WatchTurnState)
                    == message.watch_turn_state.is_some())
                    && ((message.kind == AgentMessageKind::WatchProviderStatus)
                        == message.watch_provider_status.is_some());
                Some(if payload_matches_kind {
                    Ok(())
                } else {
                    Err(AgentEventValidationError::new(
                        "watch payload must be present exactly for its matching watch message kind",
                    ))
                })
            }
            Event::AgentMessageIncoming(message) if message.recipient_id == self.agent_id => {
                Some(Ok(()))
            }
            Event::AgentMessageOutgoing(message) if message.sender_id == self.agent_id => {
                Some(Ok(()))
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
            Event::AgentPromptSubmitted(prompt) if prompt.agent_id == self.agent_id => Some(Ok(())),
            Event::AgentUserMessageInjected(injected) if injected.agent_id == self.agent_id => {
                Some(Ok(()))
            }
            Event::AgentPromptSteered(steered) if steered.agent_id == self.agent_id => Some(Ok(())),
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
            Event::AgentStandaloneCompactionFailed(failed) if failed.agent_id == self.agent_id => {
                Some(self.validate_compaction_failed(failed))
            }
            Event::AgentInferenceDispatchStarted(started) if started.agent_id == self.agent_id => {
                Some(self.validate_inference_checkpoint(started))
            }
            Event::AgentCompacted(compacted) if compacted.agent_id == self.agent_id => Some(
                tau_proto::validate_compaction_window(&compacted.replacement_window)
                    .map_err(|error| {
                        AgentEventValidationError::new(format!(
                            "invalid compaction replacement window: {error}"
                        ))
                    })
                    .and_then(|()| self.validate_compaction_boundary(head, compacted)),
            ),
            Event::AgentHeadMoved(moved) if moved.agent_id == self.agent_id => {
                Some(self.validate_head_moved(moved))
            }
            Event::ProviderResponseFinished(response) if response.agent_id == self.agent_id => {
                Some(self.validate_provider_response(response))
            }
            _ => None,
        }
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
            if request.failed.is_some()
                || request.transaction_id.is_some()
                || request.requested.target_agent_id != started.agent_id
                || request.requested.caller_agent_id != *caller_agent_id
                || request.requested.initiating_tool_call_id != *initiating_tool_call_id
                || request.requested.model != started.model
            {
                return Err(AgentEventValidationError::new(
                    "manual-agent transaction does not uniquely match its request",
                ));
            }
            let accepted = &request.requested;
            let valid_cut = match accepted.initiating_tool_name {
                tau_proto::ManualCompactionTool::Compact => {
                    started.supersedes.is_none()
                        && started.resume_through == Some(started.cut)
                        && self.is_ancestor_head(accepted.requested_target_head, started.cut)
                        && self.has_complete_tool_round_for(
                            started.cut.as_option(),
                            &accepted.initiating_tool_call_id,
                        )
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
        if let tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
            failed_agent_prompt_id,
        } = &started.trigger
        {
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
                || checkpoint.activation_cut != Some(started.cut)
                || started.resume_through != Some(checkpoint.through)
                || self.compaction_transactions.values().any(|transaction| {
                    matches!(
                        &transaction.started.trigger,
                        tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                            failed_agent_prompt_id: claimed
                        } if claimed == failed_agent_prompt_id
                    )
                })
            {
                return Err(AgentEventValidationError::new(
                    "reactive compaction does not uniquely match its planned inference recovery",
                ));
            }
        }
        if !self.is_ancestor_head(started.cut, started.resume_through.unwrap_or(started.cut)) {
            return Err(AgentEventValidationError::new(
                "standalone compaction cut must be an ancestor of resume_through",
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
            if previous.started.cut != started.cut
                || previous.started.resume_through.is_some() && started.resume_through.is_none()
            {
                return Err(AgentEventValidationError::new(
                    "superseding compaction must preserve cut and resume obligation",
                ));
            }
        }
        Ok(())
    }

    fn validate_manual_compaction_requested(
        &self,
        requested: &tau_proto::AgentManualCompactionRequested,
    ) -> Result<(), AgentEventValidationError> {
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
            .any(|request| request.failed.is_none() && request.transaction_id.is_none())
            || self
                .compaction_transactions
                .values()
                .any(|transaction| transaction.outcome.is_none())
        {
            return Err(AgentEventValidationError::new(
                "another compaction request or transaction is pending",
            ));
        }
        if requested.initiating_tool_name == tau_proto::ManualCompactionTool::Compact
            && (requested.caller_agent_id != requested.target_agent_id
                || !requested.resume_inference)
        {
            return Err(AgentEventValidationError::new(
                "self compaction request has different caller and target",
            ));
        }
        if requested.initiating_tool_name == tau_proto::ManualCompactionTool::AgentCompact
            && (requested.caller_agent_id == requested.target_agent_id
                || requested.resume_inference)
        {
            return Err(AgentEventValidationError::new(
                "cross-agent compaction request targets its caller",
            ));
        }
        if !self.is_ancestor_head(
            requested.requested_target_head,
            requested.requested_target_head,
        ) {
            return Err(AgentEventValidationError::new(
                "manual compaction request references an unknown target head",
            ));
        }
        if requested.target_generation != self.ordinary_inference_generation {
            return Err(AgentEventValidationError::new(
                "manual compaction request has a stale target generation",
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
            || request.failed.is_some()
            || request.transaction_id.is_some()
        {
            return Err(AgentEventValidationError::new(
                "manual compaction request already has a terminal pre-start outcome",
            ));
        }
        Ok(())
    }

    fn validate_compaction_failed(
        &self,
        failed: &tau_proto::AgentStandaloneCompactionFailed,
    ) -> Result<(), AgentEventValidationError> {
        let transaction = self
            .compaction_transactions
            .get(&failed.transaction_id)
            .ok_or_else(|| {
                AgentEventValidationError::new(
                    "standalone compaction failure references unknown transaction",
                )
            })?;
        if transaction.outcome.is_some()
            || transaction.started.cut != failed.cut
            || transaction.started.resume_through != failed.resume_through
        {
            return Err(AgentEventValidationError::new(
                "standalone compaction failure mismatched transaction or duplicate outcome",
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
                if !self.is_ancestor_head(cut, suffix_end) {
                    return Err(AgentEventValidationError::new(
                        "compaction cut must be an ancestor of suffix_end",
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
            Event::ProviderToolResult(result) => {
                Some(self.validate_terminal_tool_result(&result.call_id))
            }
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

    fn is_agent_id_mismatch_event(event: &Event) -> bool {
        // Keep this list aligned with the historical mismatch diagnostics.
        // Agent metadata events are intentionally excluded: metadata for a
        // different agent still falls through to the generic non-transcript
        // rejection instead of the agent-id mismatch diagnostic.
        matches!(
            event,
            Event::AgentStarted(_)
                | Event::AgentDisplayNameSet(_)
                | Event::AgentPromptSubmitted(_)
                | Event::AgentUserMessageInjected(_)
                | Event::AgentPromptSteered(_)
                | Event::AgentCompactionTriggered(_)
                | Event::AgentCompacted(_)
                | Event::AgentMessageSent(_)
                | Event::AgentMessageReceived(_)
                | Event::AgentMessageIncoming(_)
                | Event::AgentMessageOutgoing(_)
                | Event::AgentHeadMoved(_)
                | Event::ProviderResponseFinished(_)
        )
    }

    fn validate_provider_response(
        &self,
        response: &tau_proto::ProviderResponseFinished,
    ) -> Result<(), AgentEventValidationError> {
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
        for item in &response.output_items {
            let ContextItem::ToolCall(call) = item else {
                continue;
            };
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
        Ok(())
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
        let mut head = self.append_node_at(
            Some(round.assistant_node_id),
            AgentEntry::ToolResults { items },
        );
        for item in std::mem::take(&mut self.pending_message_envelopes) {
            head = self.append_node_at(
                Some(head),
                AgentEntry::MessageEnvelope {
                    item: Box::new(item),
                },
            );
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

/// One durable agent-scoped protocol event.
///
/// `parent` is the explicit fold parent that was passed to
/// `AgentStore::append_agent_event_at` at write time. Carrying it on the
/// persisted record (rather than on the wire) preserves cross-conversation
/// branching across replay without requiring the publisher-side
/// `UiNavigateTree` head-bouncing dance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedAgentEvent {
    /// Sequence within this agent's durable `events.cbor` stream.
    ///
    /// This is persisted to catch reordered, duplicated, or spliced logs during
    /// load. The implied sequence from file order is still authoritative for
    /// replay; load rejects records where this stored value disagrees with the
    /// record's zero-based position.
    pub seq: PersistedAgentEventSeq,
    /// Connection that published the fact, when known.
    pub source: Option<ConnectionId>,
    /// Agent-scoped protocol event.
    pub event: Event,
    /// Explicit fold parent used when replaying this record into the agent
    /// tree.
    pub parent: AgentEventParent,
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

/// Per-session sidecar metadata at `<sessions_dir>/<session_id>/meta.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Unix epoch seconds when the session was first created.
    pub created_at: u64,
    /// Unix epoch seconds of the most recent membership append.
    pub last_touched: u64,
}

#[cfg(test)]
mod tests;

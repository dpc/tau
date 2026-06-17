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
    AgentHeadMoved, AgentId, AgentMessageId, AgentMessageKind, AgentMessageReceived,
    AgentMessageRecipient, AgentMessageSent, ConnectionId, ContentPart, ContextItem, ContextRole,
    Event, MessageItem, PromptOriginator, ProviderBackend, ProviderTokenUsage, ToolBackgroundError,
    ToolBackgroundResult, ToolCallId, ToolCallItem, ToolName, ToolResultItem, ToolResultKind,
    ToolResultStatus, ToolType, UnixMicros,
};

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
        /// Recipient agent or user.
        recipient: AgentMessageRecipient,
        /// Delivery source semantics.
        kind: AgentMessageKind,
        /// Message body.
        message: String,
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
    pending_tool_rounds: HashMap<NodeId, PendingToolRound>,
    tool_call_rounds: HashMap<ToolCallId, NodeId>,
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

    fn branch_node_ids_from(&self, head: Option<NodeId>) -> Vec<NodeId> {
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
    #[must_use]
    pub fn from_events(agent_id: AgentId, events: &[PersistedAgentEvent]) -> Self {
        let mut tree = Self {
            agent_id,
            metadata: BTreeMap::new(),
            nodes: Vec::new(),
            head: None,
            display_name: None,
            next_event_seq: PersistedAgentEventSeq::new(0),
            materialized_prompt_count: 0,
            pending_tool_rounds: HashMap::new(),
            tool_call_rounds: HashMap::new(),
        };
        for entry in events {
            tree.apply_event_at(entry.parent, &entry.event);
            tree.next_event_seq = entry.seq.next();
        }
        tree
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
        if matches!(event, Event::AgentPromptCreated(_)) {
            self.materialized_prompt_count += 1;
        }
        let parent = parent.resolve(self.head);
        match event {
            Event::AgentStarted(started) => {
                if let Some(display_name) = normalize_display_name(started.display_name.as_deref())
                {
                    self.display_name = Some(display_name);
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
                None
            }
            Event::AgentDisplayNameSet(name) => {
                if let Some(display_name) = normalize_display_name(Some(&name.display_name)) {
                    self.display_name = Some(display_name);
                }
                None
            }
            Event::AgentMetadataSet(set) => {
                self.metadata.insert(
                    set.key.clone(),
                    AgentMetadataEntry {
                        value: set.value.clone(),
                        inheritable: set.inheritable,
                    },
                );
                None
            }
            Event::AgentMetadataUnset(unset) => {
                self.metadata.remove(&unset.key);
                None
            }
            Event::AgentPromptSubmitted(prompt) => Some(self.append_node_at(
                parent,
                AgentEntry::UserInput {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: prompt.text.clone(),
                        }],
                        phase: None,
                    })],
                },
            )),
            Event::AgentUserMessageInjected(injected) => Some(self.append_node_at(
                parent,
                AgentEntry::UserInput {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: injected.text.clone(),
                        }],
                        phase: None,
                    })],
                },
            )),
            Event::AgentPromptSteered(steered) => Some(self.append_node_at(
                parent,
                AgentEntry::UserInput {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: steered.text.clone(),
                        }],
                        phase: None,
                    })],
                },
            )),
            Event::AgentCompactionTriggered(_) => Some(self.append_node_at(
                parent,
                AgentEntry::UserInput {
                    items: vec![ContextItem::CompactionTrigger],
                },
            )),
            Event::AgentMessageSent(message) => self
                .agent_message_entry_from_sent(message)
                .map(|entry| self.append_node_at(parent, entry)),
            Event::AgentMessageReceived(message) => self
                .agent_message_entry_from_received(message)
                .map(|entry| self.append_node_at(parent, entry)),
            Event::ProviderResponseFinished(response) => {
                let node_id = self.append_node_at(
                    parent,
                    AgentEntry::AssistantResponse {
                        provider_response_id: response.provider_response_id.clone(),
                        backend: response.backend.clone(),
                        output_items: response.output_items.clone(),
                        usage: response.usage.clone(),
                    },
                );
                let mut call_order = Vec::new();
                let mut seen = HashSet::new();
                for item in &response.output_items {
                    if let ContextItem::ToolCall(call) = item {
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
                }
                if !call_order.is_empty() {
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
                Some(node_id)
            }
            Event::ToolRequest(_)
            | Event::ToolStarted(_)
            | Event::ToolRejected(_)
            | Event::ToolResult(_)
            | Event::ToolError(_) => None,
            Event::ProviderToolResult(result) => self.record_terminal_tool_result(ToolResultItem {
                call_id: result.call_id.clone(),
                tool_type: result.tool_type,
                status: ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&result.result),
            }),
            Event::ProviderToolError(error) => self.record_terminal_tool_result(ToolResultItem {
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
            }),
            Event::ToolCancelled(cancelled) => self.record_terminal_tool_result(ToolResultItem {
                call_id: cancelled.call_id.clone(),
                tool_type: cancelled.tool_type,
                status: ToolResultStatus::Cancelled {
                    reason: "cancelled".to_owned(),
                },
                output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Null),
            }),
            Event::AgentHeadMoved(moved) => {
                if moved.agent_id == self.agent_id && self.node(moved.node_id).is_some() {
                    self.head = Some(moved.node_id);
                }
                None
            }
            _ => None,
        }
    }

    fn agent_message_entry_from_sent(&self, message: &AgentMessageSent) -> Option<AgentEntry> {
        (message.sender_id == self.agent_id).then(|| AgentEntry::AgentMessage {
            message_id: message.message_id.clone(),
            direction: AgentMessageDirection::Outbound,
            sender_id: message.sender_id.clone(),
            recipient: message.recipient.clone(),
            kind: message.kind,
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
            recipient: AgentMessageRecipient::Agent {
                agent_id: message.recipient_id.clone(),
            },
            kind: message.kind,
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
        match event {
            Event::AgentStarted(started) if started.agent_id == self.agent_id => {
                if started
                    .display_name
                    .as_deref()
                    .is_some_and(|name| name.trim().is_empty())
                {
                    Err(AgentEventValidationError::new(
                        "agent display name must not be empty",
                    ))
                } else {
                    Ok(())
                }
            }
            Event::AgentDisplayNameSet(name) if name.agent_id == self.agent_id => {
                if name.display_name.trim().is_empty() {
                    Err(AgentEventValidationError::new(
                        "agent display name must not be empty",
                    ))
                } else {
                    Ok(())
                }
            }
            Event::AgentMetadataSet(set) if set.agent_id == self.agent_id => Ok(()),
            Event::AgentMetadataUnset(unset) if unset.agent_id == self.agent_id => Ok(()),
            Event::AgentPromptSubmitted(prompt) if prompt.agent_id == self.agent_id => Ok(()),
            Event::AgentUserMessageInjected(injected) if injected.agent_id == self.agent_id => {
                Ok(())
            }
            Event::AgentPromptSteered(steered) if steered.agent_id == self.agent_id => Ok(()),
            Event::AgentCompactionTriggered(triggered) if triggered.agent_id == self.agent_id => {
                Ok(())
            }
            Event::AgentMessageSent(message)
                if self.agent_message_entry_from_sent(message).is_some() =>
            {
                Ok(())
            }
            Event::AgentMessageReceived(message)
                if self.agent_message_entry_from_received(message).is_some() =>
            {
                Ok(())
            }
            Event::AgentHeadMoved(moved) if moved.agent_id == self.agent_id => {
                self.validate_head_moved(moved)
            }
            Event::ProviderResponseFinished(response) if response.agent_id == self.agent_id => {
                self.validate_provider_response(response)
            }
            Event::ProviderToolResult(result) => {
                self.validate_terminal_tool_result(&result.call_id)
            }
            Event::ProviderToolError(error) | Event::ToolError(error) => {
                self.validate_terminal_tool_result(&error.call_id)
            }
            Event::ToolCancelled(cancelled) => {
                self.validate_terminal_tool_result(&cancelled.call_id)
            }
            Event::ToolBackgroundResult(result) => self.validate_known_tool_call(&result.call_id),
            Event::ToolBackgroundError(error) => self.validate_known_tool_call(&error.call_id),
            Event::AgentStarted(_)
            | Event::AgentDisplayNameSet(_)
            | Event::AgentPromptSubmitted(_)
            | Event::AgentUserMessageInjected(_)
            | Event::AgentPromptSteered(_)
            | Event::AgentCompactionTriggered(_)
            | Event::AgentMessageSent(_)
            | Event::AgentMessageReceived(_)
            | Event::AgentHeadMoved(_)
            | Event::ProviderResponseFinished(_) => Err(AgentEventValidationError::new(
                "agent event agent_id did not match target agent",
            )),
            _ => Err(AgentEventValidationError::new(
                "agent store only persists agent transcript events",
            )),
        }
    }

    fn validate_provider_response(
        &self,
        response: &tau_proto::ProviderResponseFinished,
    ) -> Result<(), AgentEventValidationError> {
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
        if self.node(moved.node_id).is_none() {
            return Err(AgentEventValidationError::new(format!(
                "head move referenced unknown node_id: {}",
                moved.node_id
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
        Some(self.append_node_at(
            Some(round.assistant_node_id),
            AgentEntry::ToolResults { items },
        ))
    }

    fn validate_known_tool_call(
        &self,
        call_id: &ToolCallId,
    ) -> Result<(), AgentEventValidationError> {
        if self.tool_call_ids_from_branch(self.head).contains(call_id)
            || self.tool_call_rounds.contains_key(call_id)
        {
            return Ok(());
        }
        Err(AgentEventValidationError::new(format!(
            "background tool completion for unknown call_id: {call_id}"
        )))
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

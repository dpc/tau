//! Load-scoped transcript ownership, routing, and conversation configuration.

use tau_core::{AgentPersistenceMode, NodeId};
use tau_proto::{
    AgentId, ConnectionId, ModelId, PromptOriginator, SessionId, ToolCallId, ToolUseStats,
};

/// Load-scoped identity, mutable transcript position, routing, and metadata.
#[derive(Debug)]
pub(crate) struct AgentIdentityState {
    /// Owning agent id. Duplicates the key in the harness's agent map, but
    /// pinning it on the struct itself
    /// lets future code carry a `&Agent` without also threading
    /// the id through every call site.
    #[allow(dead_code)]
    pub(crate) id: AgentId,
    /// Monotonic identity for this loaded runtime instance.
    pub(crate) runtime_incarnation: u64,
    /// Session whose loaded-agent membership owns this runtime.
    pub(crate) session_id: SessionId,
    /// Current prompt authority, initially inherited from agent creation and
    /// replaced after an authenticated UI prompt commits.
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
    /// Runtime startup correlation retained for duplicate acceptance replay.
    pub(crate) start_operation_id: Option<tau_proto::StartOperationId>,
    /// For side agents spawned by a tool-implementing extension
    /// (currently just `agent_start`): the parent agent's tool call id
    /// that this conversation is fulfilling. Kept for teardown/routing of
    /// tool-backed side agents. `None` for user agents and for non-tool
    /// ext-queries (e.g. notifications' idle summary).
    pub(crate) parent_tool_call_id: Option<ToolCallId>,
    /// Direct parent resolved from a tool owner or an explicit-parent typed
    /// start. Teardown uses it after tool routing disappears, and completed
    /// parented starts use its presence to retain and detach the worker.
    pub(crate) parent_agent_id: Option<AgentId>,
    /// Cold restore proved that this extension-owned request came from the
    /// built-in `agent_start` tool even though its transient tool-call id is
    /// gone.
    pub(crate) restored_tool_backed_start: bool,
    /// Human-friendly name shown in UIs. Falls back to the stable agent id.
    pub(crate) display_name: Option<String>,
    /// Optional request task label surfaced in the UI for a side agent.
    /// Populated independently of whether the request is tool-backed.
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
    /// Stable id assigned when this conversation first starts a turn.
    pub(crate) agent_id: Option<AgentId>,
    /// Whether this agent's semantic transcript is durable or memory-only.
    pub(crate) persistence: AgentPersistenceMode,
    /// Durable semantic lifecycle marker for a peer-created entrypoint
    /// endpoint.
    pub(crate) peer_entrypoint_endpoint: bool,
}

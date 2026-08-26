//! Branch-local execution counters, telemetry, deduplication, and guards.

use std::collections::HashSet;

use tau_core::NodeId;
use tau_proto::ModelId;

use super::LoopGuardState;
use crate::dedup::ResultDedupMap;
/// Per-branch execution counters, context telemetry, and safety guards.
#[derive(Debug)]
pub(crate) struct AgentExecutionState {
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
    /// [`super::AgentIdentityState::head`] whenever the cursor moves
    /// non-linearly. See `crate::dedup` for the full rationale.
    pub(crate) result_dedup: ResultDedupMap,
    /// Runtime-only conservative loop guard state for this agent branch.
    pub(crate) loop_guard: LoopGuardState,
}

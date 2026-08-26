//! Queued activation and provider-prompt dispatch state.

use std::collections::VecDeque;

use tau_proto::AgentPromptId;

use super::{ActivationDispatchState, PendingCancel, PendingMessageWake, PendingPrompt};

/// Queued work and provider-prompt dispatch bookkeeping for one agent.
#[derive(Debug)]
pub(crate) struct AgentDispatchState {
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
}

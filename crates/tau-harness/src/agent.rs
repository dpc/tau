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

use std::collections::VecDeque;

pub(crate) use loop_guard::{LoopCycleState, LoopGuardState, LoopGuardTrigger, LoopTurnSignature};
use tau_core::{AgentPersistenceMode, NodeId};
use tau_proto::{
    AgentId, AgentPromptId, ConnectionId, ModelId, PromptMessageClass, PromptOriginator, SessionId,
    ToolCallId, ToolUseStats,
};

use crate::dedup::ResultDedupMap;

/// Per-agent turn state. There is no global execution slot — each loaded agent
/// tracks whether its next prompt can be dispatched.
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
    /// Number of tool calls currently in flight on this conversation.
    pub(crate) tools_in_flight: u32,
    /// Cumulative tool calls this conversation has started (in-flight
    /// + completed). Used in generic agent stats snapshots.
    pub(crate) tools_total: u32,
    /// Most recent input-token count this agent's agent
    /// reported on a finished response. Used for generic agent stats snapshots.
    pub(crate) context_input_tokens: Option<u64>,
    /// Most recent cached input-token count this agent's provider reported on
    /// a finished response.
    pub(crate) context_cached_tokens: Option<u64>,
    /// Most recent percent-of-context-window this conversation's
    /// agent has used. Computed from `context_input_tokens` and the
    /// model's window size; `None` when the window is unknown.
    pub(crate) context_percent_used: Option<u8>,

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingPromptSource {
    /// A normal user or harness steering prompt.
    General,
    /// A prompt created from an `agent.message_received` delivery.
    AgentMessageReceived,
    /// Internal loop-guard pivot prompt.
    LoopGuard,
    /// A passive background-completion notice that should be folded into the
    /// next real user prompt, but must not make an idle agent runnable by
    /// itself.
    PassiveBackgroundCompletion,
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
    /// Optional caller correlation id carried with this exact prompt.
    pub(crate) ctx_id: Option<String>,
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
            ctx_id: None,
        }
    }

    /// Create a hidden internal queued prompt.
    pub(crate) fn internal(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::General,
            ctx_id: None,
        }
    }

    /// Create a hidden queued prompt from an `agent.message_received` delivery.
    pub(crate) fn agent_message_received(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::AgentMessageReceived,
            ctx_id: None,
        }
    }

    /// Create an internal loop-guard pivot prompt.
    pub(crate) fn loop_guard(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::LoopGuard,
            ctx_id: None,
        }
    }

    /// Create a hidden background-completion notice that waits for the next
    /// user-driven continuation instead of starting a standalone model turn.
    pub(crate) fn passive_background_completion(text: String) -> Self {
        Self {
            text,
            message_class: PromptMessageClass::Internal,
            source: PendingPromptSource::PassiveBackgroundCompletion,
            ctx_id: None,
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
        self.source == PendingPromptSource::AgentMessageReceived
    }

    /// Whether this queued prompt is a loop-guard pivot.
    #[must_use]
    pub(crate) fn is_loop_guard(&self) -> bool {
        self.source == PendingPromptSource::LoopGuard
    }

    /// Whether this prompt is a passive background-completion notice.
    #[must_use]
    pub(crate) fn is_passive_background_completion(&self) -> bool {
        self.source == PendingPromptSource::PassiveBackgroundCompletion
    }
}

impl Agent {
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
            source_connection,
            in_flight_prompt: None,
            pending_prompts: VecDeque::new(),
            pending_cancel: None,
            last_prompt_id: None,
            next_prompt_index: 0,
            prompt_index_initialized: false,
            next_ctx_id: None,
            turn_state: AgentTurnState::Idle,
            parent_tool_call_id: None,
            parent_agent_id: None,
            display_name: None,
            task_name: None,
            delegate_input_stats: ToolUseStats::default(),
            role: None,
            model_override: None,
            agent_id: None,
            persistence: AgentPersistenceMode::Durable,
            tools_in_flight: 0,
            tools_total: 0,
            context_input_tokens: None,
            context_cached_tokens: None,
            context_percent_used: None,
            result_dedup: ResultDedupMap::new(),
            loop_guard: LoopGuardState::default(),
        }
    }
}

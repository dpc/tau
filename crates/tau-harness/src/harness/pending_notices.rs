//! Pending model-visible notices owned by the harness.
//!
//! These notices are state-machine edges that cannot be published immediately:
//! they must be folded into the next real user prompt so the model sees them in
//! context without an extra standalone turn.

use std::collections::{BTreeMap, HashMap, HashSet};

use tau_proto::{AgentId, SessionId, ToolName};

use super::{tool_available_again_notice_prompt, tool_unavailable_notice_prompt};

/// Model-visible notice state that is deliberately held until the next real
/// user prompt can fold it into context.
#[derive(Debug, Default)]
pub(crate) struct PendingPromptNoticeState {
    /// Resumed sessions that still need a one-shot internal restore notice
    /// folded immediately before the next real user prompt, with the last
    /// durable event timestamp seen before resume when available.
    pub(crate) restore_sessions: HashMap<SessionId, Option<tau_proto::UnixMicros>>,
    /// Per-background-tool restore notes that should be folded immediately
    /// before the owning agent's next real user prompt, not dispatched as
    /// standalone turns.
    pub(crate) restore_background_notices: HashMap<(SessionId, AgentId), Vec<String>>,
    /// Existing agents newly loaded into another session that still need a
    /// one-shot warning about session-scoped tool and extension state.
    pub(crate) changed_session_agents: HashSet<(SessionId, AgentId)>,
    /// Tool availability notices waiting to be folded before the next real user
    /// prompt on the target user agent, keyed by internal tool name for
    /// deterministic delivery.
    pub(super) tool_availability: BTreeMap<String, PendingToolAvailabilityNotice>,
    /// Tools whose unavailable notice has already been delivered and that are
    /// still absent from the registry. A later registration uses this to queue
    /// the matching available-again notice.
    pub(super) unavailable_tools_delivered: BTreeMap<String, ToolName>,
}

/// Pending tool availability notification to inject before a user prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PendingToolAvailabilityNotice {
    /// The model has seen this tool before, but it is no longer registered.
    Unavailable {
        /// Model-visible tool name to mention in the notice.
        visible_name: ToolName,
    },
    /// A tool that was previously announced as unavailable is registered again.
    AvailableAgain {
        /// Model-visible tool name to mention in the notice.
        visible_name: ToolName,
    },
}

impl PendingToolAvailabilityNotice {
    /// Returns the internal prompt text corresponding to this notice.
    pub(super) fn prompt_text(&self) -> String {
        match self {
            Self::Unavailable { visible_name } => tool_unavailable_notice_prompt(visible_name),
            Self::AvailableAgain { visible_name } => {
                tool_available_again_notice_prompt(visible_name)
            }
        }
    }
}

//! Small, argument-free identity retained around one owned `tool.started` call.

use tau_proto::{
    AgentId, CborValue, PromptOriginator, ToolCallId, ToolInvocationPolicy, ToolName, ToolStarted,
};

/// Stable wire and local identity split from a potentially large argument
/// value.
#[derive(Debug)]
pub(crate) struct ToolStartedIdentity {
    /// Stable accepted call id.
    pub(crate) call_id: ToolCallId,
    /// Harness-visible tool name used for terminal correlation.
    pub(crate) wire_tool_name: ToolName,
    /// Extension-local canonical handler name.
    pub(crate) local_tool_name: ToolName,
    /// Agent that owns the call.
    pub(crate) agent_id: AgentId,
    /// Prompt source copied into result and error events.
    pub(crate) originator: PromptOriginator,
    /// Hidden harness policy preserved for the executing handler.
    invocation_policy: ToolInvocationPolicy,
}

impl Clone for ToolStartedIdentity {
    fn clone(&self) -> Self {
        #[cfg(test)]
        ownership_probe::record_identity_clone(&self.call_id);
        Self {
            call_id: self.call_id.clone(),
            wire_tool_name: self.wire_tool_name.clone(),
            local_tool_name: self.local_tool_name.clone(),
            agent_id: self.agent_id.clone(),
            originator: self.originator.clone(),
            invocation_policy: self.invocation_policy.clone(),
        }
    }
}

impl From<ToolStarted> for ToolStartedIdentity {
    fn from(started: ToolStarted) -> Self {
        let local_tool_name = started.tool_name.clone();
        Self::split(started, local_tool_name).0
    }
}

impl ToolStartedIdentity {
    /// Split an accepted invocation without cloning its argument tree.
    pub(crate) fn split(started: ToolStarted, local_tool_name: ToolName) -> (Self, CborValue) {
        let ToolStarted {
            call_id,
            tool_name,
            arguments,
            agent_id,
            originator,
            invocation_policy,
        } = started;
        #[cfg(test)]
        ownership_probe::record_split(&call_id, &arguments);
        (
            Self {
                call_id,
                wire_tool_name: tool_name,
                local_tool_name,
                agent_id,
                originator,
                invocation_policy,
            },
            arguments,
        )
    }

    /// Reassemble the canonical local invocation immediately before queue
    /// ownership.
    pub(crate) fn into_local_started(self, arguments: CborValue) -> ToolStarted {
        #[cfg(test)]
        ownership_probe::record_reassembly(&self.call_id, &arguments);
        ToolStarted {
            call_id: self.call_id,
            tool_name: self.local_tool_name,
            arguments,
            agent_id: self.agent_id,
            originator: self.originator,
            invocation_policy: self.invocation_policy,
        }
    }
}

#[cfg(test)]
pub(crate) mod ownership_probe;

#[cfg(test)]
mod tests;

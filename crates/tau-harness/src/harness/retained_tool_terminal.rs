use super::interception::{OwnedPublication, OwnedPublicationBranch, OwnedPublicationRetryPolicy};
use super::*;

/// One interceptor-approved terminal awaiting semantic admission, independently
/// owned by its call rather than the agent's single prompt-completion slot.
pub(super) struct RetainedToolTerminal {
    /// Runtime agent whose journal owns the terminal.
    cid: AgentId,
    /// Exact classified terminal identity; a competing terminal can supersede
    /// it.
    terminal: tau_proto::ObservationId,
    /// Approved payload and its original semantic parent.
    publication: OwnedPublication,
    /// Original canonical publisher, preserved on retry.
    source: Option<tau_proto::ConnectionId>,
}

impl Harness {
    /// Retain ordinary canonical terminals independently for every routed call.
    /// Specialized continuations retain their own publication ownership
    /// instead.
    pub(super) fn retain_rejected_tool_terminal(
        &mut self,
        sync: Option<&ConversationHeadSync>,
        event: &Event,
        semantic_parent: tau_core::AgentEventParent,
        source: Option<&tau_proto::ConnectionId>,
    ) -> bool {
        if self.retain_rejected_tool_output(event, semantic_parent) {
            return true;
        }
        let Some(sync) = sync else {
            return false;
        };
        if !matches!(
            sync.completion(),
            None | Some(AgentPublishCompletion::ToolTerminal { .. })
        ) {
            return false;
        }
        let Some(call_id) = canonical_tool_terminal_call_id(event) else {
            return false;
        };
        let Some(terminal) = self
            .tool_routing
            .tool_runtime
            .pending_terminal_observations
            .get(call_id)
        else {
            return false;
        };
        self.tool_routing.tool_runtime.retained_terminals.insert(
            call_id.clone(),
            RetainedToolTerminal {
                cid: sync.cid.clone(),
                terminal: terminal.observation_id,
                publication: OwnedPublication {
                    approved_event: Box::new(event.clone()),
                    semantic_parent,
                    owning_branch: OwnedPublicationBranch::SemanticParent,
                    retry_policy: OwnedPublicationRetryPolicy::ApprovedEventWithoutInterception,
                },
                source: source.cloned(),
            },
        );
        // Leaving an expired deadline armed would make the central scheduler
        // select it forever while backgrounding is blocked by this owner.
        self.tool_routing
            .tool_runtime
            .tool_turn
            .clear_background_deadline(call_id);
        true
    }

    /// Retry each exact call owner once, without replaying extension work or
    /// interception. A newer classification or committed terminal retires it.
    pub(super) fn retry_retained_tool_terminals(&mut self, cid: &AgentId) {
        let calls = self
            .tool_routing
            .tool_runtime
            .retained_terminals
            .iter()
            .filter(|(_, retained)| &retained.cid == cid)
            .map(|(call_id, _)| call_id.clone())
            .collect::<Vec<_>>();
        for call_id in calls {
            let Some(retained) = self
                .tool_routing
                .tool_runtime
                .retained_terminals
                .remove(&call_id)
            else {
                continue;
            };
            if self
                .tool_routing
                .tool_runtime
                .pending_terminal_observations
                .get(&call_id)
                .is_none_or(|pending| pending.observation_id != retained.terminal)
            {
                continue;
            }
            self.commit_approved_agent_retry(
                cid,
                *retained.publication.approved_event,
                AgentPublishCompletion::ToolTerminal {
                    call_id,
                    owned_publication: None,
                },
                retained.publication.semantic_parent,
                retained.source.as_ref(),
            );
        }
    }

    /// Include per-call terminal owners in the existing capacity/progress wake.
    pub(super) fn retained_tool_terminal_agents(&self) -> HashSet<AgentId> {
        self.tool_routing
            .tool_runtime
            .retained_terminals
            .values()
            .map(|retained| retained.cid.clone())
            .chain(self.retained_tool_output_agents())
            .collect()
    }
}

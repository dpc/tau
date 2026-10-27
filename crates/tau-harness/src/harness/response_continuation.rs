//! Bounded explicit provider continuation using ordinary durable dispatch.

use super::*;

/// Maximum additional samplings authorized by explicit no-tool responses in
/// one outer turn. Durable selected-branch terminals supply the counter.
const RESPONSE_CONTINUATION_LIMIT: usize = 16;

impl Harness {
    /// Enforce the finite response-continuation budget before terminal shaping
    /// and accounting. Accepted output and paid usage remain intact on
    /// exhaustion.
    pub(super) fn normalize_response_continuation(
        &self,
        cid: &AgentId,
        response: &mut ProviderResponseFinished,
        has_tool_calls: bool,
    ) {
        if response.stop_reason != ProviderStopReason::Continue {
            return;
        }
        if response.error.is_some() || response.failure_kind.is_some() {
            response.stop_reason = ProviderStopReason::Error;
        } else if has_tool_calls {
            response.stop_reason = ProviderStopReason::ToolCalls;
        } else if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|id| self.session_runtime.agent_store.agent(id))
            .is_some_and(|tree| tree.response_continuation_count() >= RESPONSE_CONTINUATION_LIMIT)
        {
            response.stop_reason = ProviderStopReason::Error;
            response.error = Some("Provider response continuation limit reached (16 additional samplings per outer turn).".to_owned());
            response.failure_kind = Some(tau_proto::ProviderFailureKind::RequestRejected);
        }
    }

    /// Continue only from a committed selected response. Reuse the ordinary
    /// preflight/compaction/checkpoint pipeline without inventing an input.
    pub(super) fn continue_after_explicit_response(&mut self, cid: &AgentId) {
        let source = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|id| self.session_runtime.agent_store.agent(id))
            .and_then(tau_core::AgentTree::pending_response_continuation)
            .cloned();
        let Some(source) = source else { return };
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            // An inner-round boundary, not the outer-turn Idle transition.
            agent.turn.turn_state = AgentTurnState::Idle;
            agent.turn.terminal_notice_eligible = false;
        }
        self.enqueue_committed_activation_dispatch(
            cid.clone(),
            Some(source.activation_cut),
            self.selected_head_for_agent(cid),
        );
        self.drain_publish_idle_dispatches();
    }

    /// Whether an unclaimed explicit response still owns cancellable work.
    pub(super) fn has_pending_response_continuation(&self, cid: &AgentId) -> bool {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|id| self.session_runtime.agent_store.agent(id))
            .and_then(tau_core::AgentTree::pending_response_continuation)
            .is_some()
    }
}

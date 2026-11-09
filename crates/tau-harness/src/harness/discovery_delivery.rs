//! Discovery user messages join existing inference, never creating activation.

use super::*;

impl Harness {
    /// Compute a delta only from canonical receipts surviving in this branch.
    pub(super) fn discovery_context_delta(
        &self,
        cid: &AgentId,
    ) -> Option<tau_proto::DiscoveryContextDelivery> {
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let tree = self
            .session_runtime
            .agent_store
            .agent(agent.identity.agent_id.as_deref()?)?;
        let current = &tree.initialization_context()?.discovery_items;
        let receipts = tree.discovery_context_receipts(agent.identity.head);
        if current.is_empty() && receipts.is_empty() {
            return None;
        }
        let scope = current
            .iter()
            .map(tau_proto::DiscoveryContextItem::reference)
            .collect::<Vec<_>>();
        let mut items = Vec::new();
        let mut updated = Vec::new();
        for item in current {
            let previous = receipts
                .iter()
                .rev()
                .flat_map(|receipt| receipt.items.iter())
                .find(|seen| seen.identity == item.identity);
            if previous == Some(item) {
                continue;
            }
            if previous.is_some() {
                updated.push(item.identity.clone());
            }
            items.push(item.clone());
        }
        if items.is_empty() {
            return None;
        }
        Some(tau_proto::DiscoveryContextDelivery {
            scope,
            items,
            updated,
        })
    }

    /// Append passive guidance immediately before an already-authorized
    /// inference. The existing activation remains its sole wake and
    /// dispatch owner.
    pub(super) fn deliver_discovery_for_activation(&mut self, cid: &AgentId) -> bool {
        if self
            .runtime_io
            .publication
            .capacity_rejected_activations
            .contains_key(cid)
            || self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .is_some_and(|agent| {
                    !matches!(
                        agent.dispatch.activation_dispatch,
                        path_crate_agent::ActivationDispatchState::None
                    ) || matches!(
                        agent.turn.output_length_continuation,
                        path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                    )
                })
        {
            // A retained checkpoint retry or successor owns an immutable
            // through cut. Its original discovery was admitted
            // before that checkpoint. New discovery can join
            // its next writable continuation, never invalidate that cut.
            return false;
        }
        let Some(delivery) = self.discovery_context_delta(cid) else {
            return false;
        };
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return false;
        };
        self.publish_for_agent(
            cid,
            Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id,
                inference_activation: false,
                text: String::new(),
                discovery_context: Some(delivery),
                message_class: tau_proto::PromptMessageClass::Internal,
            }),
        );
        true
    }
}

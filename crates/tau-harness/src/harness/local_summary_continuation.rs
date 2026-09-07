//! Same-cut local-summary successors advance only after their failure commits.

use super::*;

impl Harness {
    /// Pre-mint an eligible local-summary successor without publishing effects.
    pub(super) fn plan_local_summary_continuation(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
    ) -> Option<tau_proto::LocalSummaryContinuationPlan> {
        if response.stop_reason != ProviderStopReason::Length
            || response.error.is_some()
            || response.failure_kind.is_some()
            || response.backend.as_ref()?.kind != tau_proto::ProviderBackendKind::ChatCompletions
        {
            return None;
        }
        let (text, reasoning) =
            tau_proto::local_summary_output_parts(&response.output_items).ok()?;
        if text.is_empty() && reasoning == 0 {
            return None;
        }
        let agent = self.agent_runtime.agent_registry.agents.get_mut(cid)?;
        let next = agent.dispatch.next_prompt_index;
        let plan = tau_proto::LocalSummaryContinuationPlan {
            transaction_id: tau_proto::CompactionTransactionId::parse(format!("ct-{next}")).ok()?,
            compact_prompt_id: tau_proto::AgentPromptId::parse(format!(
                "ap-{}-{next}",
                response.agent_id
            ))
            .ok()?,
        };
        agent.dispatch.next_prompt_index = next.saturating_add(1);
        Some(plan)
    }

    /// Claim the committed same-cut plan once, preserving the original owner.
    pub(super) fn start_local_summary_continuation(
        &mut self,
        cid: &AgentId,
        failed: &tau_proto::AgentStandaloneCompactionFailed,
        started: &tau_proto::AgentStandaloneCompactionStarted,
        plan: tau_proto::LocalSummaryContinuationPlan,
    ) {
        let event =
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: failed.agent_id.clone(),
                transaction_id: plan.transaction_id,
                compact_prompt_id: plan.compact_prompt_id,
                cut: started.cut,
                resume_through: started.resume_through,
                model: started.model.clone(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: started.originator.clone(),
                supersedes: Some(failed.transaction_id.clone()),
                trigger:
                    tau_proto::StandaloneCompactionTrigger::AutomaticOutputLengthContinuation {
                        failed_transaction_id: failed.transaction_id.clone(),
                    },
            });
        self.publish_event_for_agent_with_completion(
            cid,
            None,
            event,
            Some(AgentPublishCompletion::RollingCompactionStart {
                owned_publication: None,
            }),
            false,
        );
    }
}

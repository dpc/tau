//! Exact standalone accounting facts prepared before exactly-once publication.

use super::*;

/// One output-independent accounting publication, without consuming its owner
/// or recording its attempt as observed.
pub(super) struct PreparedStandaloneAccounting {
    /// Runtime agent that owns publication and any delayed correction.
    pub(super) cid: AgentId,
    /// Exact canonical fact, including response-local accounting enrichment.
    pub(super) fact: StandaloneAccountingFact,
}

/// The initial and correction phases retain their existing separate commit
/// cuts.
pub(super) enum StandaloneAccountingFact {
    /// The sole request-counting observation of this attempt.
    Initial(tau_proto::ProviderStandaloneExecutionAccounted),
    /// A late terminal filling in a cancellation-time unknown observation.
    Correction(tau_proto::ProviderStandaloneExecutionAccountingCorrected),
}

impl Harness {
    /// Build the exact owned accounting fact without touching exactly-once
    /// sets, correction queues, accounting owners, or live totals.
    pub(super) fn prepare_standalone_accounting(
        &self,
        response: &ProviderResponseFinished,
        accepted: bool,
    ) -> Option<PreparedStandaloneAccounting> {
        let state = &self.prompt_coordination.standalone_accounting;
        let owner = state.owners.get(&response.agent_prompt_id)?;
        let identity = (response.agent_prompt_id.clone(), response.provider_attempt);
        let usage = || {
            response.usage.clone().map_or(
                tau_proto::StandaloneExecutionUsage::Unknown,
                tau_proto::StandaloneExecutionUsage::Known,
            )
        };
        let fact = if state.awaiting_corrections.get(&response.agent_prompt_id)
            == Some(&response.provider_attempt)
        {
            if state.observed_corrections.contains(&identity) {
                return None;
            }
            StandaloneAccountingFact::Correction(
                tau_proto::ProviderStandaloneExecutionAccountingCorrected {
                    session_id: owner.session_id.clone(),
                    agent_id: owner.agent_id.clone(),
                    agent_prompt_id: response.agent_prompt_id.clone(),
                    logical_attempt: response.provider_attempt,
                    transaction_id: owner.transaction_id.clone(),
                    model: owner.model.clone(),
                    backend: response.backend.clone(),
                    usage: usage(),
                    estimated_api_cost_rates: Some(owner.estimated_cost_rates),
                    estimated_api_cost_increment: response.estimated_api_cost_increment,
                    output: tau_proto::StandaloneExecutionOutput::Rejected,
                },
            )
        } else {
            if state.observed_attempts.contains(&identity) {
                return None;
            }
            StandaloneAccountingFact::Initial(tau_proto::ProviderStandaloneExecutionAccounted {
                session_id: owner.session_id.clone(),
                agent_id: owner.agent_id.clone(),
                agent_prompt_id: response.agent_prompt_id.clone(),
                logical_attempt: response.provider_attempt,
                transaction_id: owner.transaction_id.clone(),
                model: owner.model.clone(),
                backend: response.backend.clone(),
                usage: usage(),
                estimated_api_cost_rates: Some(owner.estimated_cost_rates),
                estimated_api_cost_increment: response.estimated_api_cost_increment,
                output: if accepted {
                    tau_proto::StandaloneExecutionOutput::Accepted
                } else {
                    tau_proto::StandaloneExecutionOutput::Rejected
                },
                finality: tau_proto::StandaloneExecutionAccountingFinality::Final,
            })
        };
        Some(PreparedStandaloneAccounting {
            cid: owner.cid.clone(),
            fact,
        })
    }

    /// Apply the already-shaped fact at the original accounting publication
    /// cut.
    pub(super) fn apply_standalone_accounting(
        &mut self,
        response: &ProviderResponseFinished,
        prepared: Option<PreparedStandaloneAccounting>,
    ) {
        let Some(PreparedStandaloneAccounting { cid, fact }) = prepared else {
            if !self
                .prompt_coordination
                .standalone_accounting
                .owners
                .contains_key(&response.agent_prompt_id)
            {
                tracing::error!(
                    target: "tau_harness",
                    agent_prompt_id = %response.agent_prompt_id,
                    "standalone backend terminal has no accounting owner"
                );
            }
            return;
        };
        let identity = (response.agent_prompt_id.clone(), response.provider_attempt);
        match fact {
            StandaloneAccountingFact::Initial(accounted) => {
                if !self
                    .prompt_coordination
                    .standalone_accounting
                    .observed_attempts
                    .insert(identity.clone())
                {
                    return;
                }
                self.publish_event_for_agent_with_completion(
                    &cid,
                    Some(harness_connection_id()),
                    Event::ProviderStandaloneExecutionAccounted(accounted),
                    Some(AgentPublishCompletion::StandaloneExecutionAccounting {
                        key: (
                            identity.0,
                            identity.1,
                            StandaloneAccountingPublicationPhase::Initial,
                        ),
                        owned_publication: None,
                    }),
                    false,
                );
                self.prompt_coordination
                    .standalone_accounting
                    .owners
                    .remove(&response.agent_prompt_id);
            }
            StandaloneAccountingFact::Correction(corrected) => {
                if !self
                    .prompt_coordination
                    .standalone_accounting
                    .observed_corrections
                    .insert(identity.clone())
                {
                    return;
                }
                self.prompt_coordination
                    .standalone_accounting
                    .owners
                    .remove(&response.agent_prompt_id);
                if self
                    .prompt_coordination
                    .standalone_accounting
                    .folded
                    .get(&identity)
                    != Some(&FoldedStandaloneAccountingPhase::AwaitingCorrection)
                {
                    self.prompt_coordination
                        .standalone_accounting
                        .pending_corrections
                        .insert(
                            identity,
                            PendingStandaloneAccountingCorrection { cid, corrected },
                        );
                    return;
                }
                self.publish_standalone_execution_accounting_correction_event(&cid, corrected);
            }
        }
    }
}

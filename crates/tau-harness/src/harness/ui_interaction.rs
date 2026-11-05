//! Commit-driven acceptance of visible UI input, separate from prompt
//! execution.

use tau_proto::{AgentId, ConnectionId, Event};

use super::session_runtime_state::SessionGeneration;
use super::ui_create_agent::CreatedInitialPrompt;
use super::{ConversationHeadSync, Harness, PromptSubmission};
use crate::agent::PendingPrompt;

/// One visible input awaiting the ordinary interaction publication outcome.
///
/// This owner lives only in the publication envelope. Replay observes the fact,
/// but cannot reconstruct or execute its dependent prompt.
pub(super) struct PendingUiInteraction {
    /// UI connection receiving an admission failure.
    client_id: ConnectionId,
    /// Exact runtime route that accepted the input for validation.
    pub(super) cid: AgentId,
    /// Durable agent addressed by the input.
    agent_id: AgentId,
    /// Session generation that admitted the publication.
    generation: SessionGeneration,
    /// Explicit navigation epoch captured before publication could park.
    navigation_epoch: u64,
    /// Work released only by a committed interaction.
    admission: UiInteractionAdmission,
}

/// The two UI admission workflows sharing the interaction commit boundary.
pub(super) enum UiInteractionAdmission {
    /// Ordinary input to an existing loaded agent.
    Prompt(Box<PendingPrompt>),
    /// Initial input whose failure must retain the create-request correlation.
    Created(CreatedInitialPrompt),
    /// Canonical-main bootstrap first commits its restart-suppression fact,
    /// then independently records the visible interaction and admits the
    /// prompt.
    Bootstrap {
        /// Exact validated serve generation.
        bootstrap_id: String,
        /// Literal prompt retained only by this live publication owner.
        admission: CreatedInitialPrompt,
    },
}

impl Harness {
    /// Publish one content-free interaction with its dependent admission owner.
    pub(super) fn enqueue_ui_interaction(
        &mut self,
        client_id: &ConnectionId,
        cid: AgentId,
        agent_id: AgentId,
        admission: UiInteractionAdmission,
    ) {
        let event = match &admission {
            UiInteractionAdmission::Bootstrap { bootstrap_id, .. } => {
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: agent_id.clone(),
                    key: tau_proto::AgentMetadataKey::new(format!(
                        "{}{bootstrap_id}",
                        super::ui_create_agent::BOOTSTRAP_ADMISSION_PREFIX,
                    )),
                    value: tau_proto::CborValue::Bool(true),
                    inheritable: false,
                    mutation_id: None,
                })
            }
            _ => Event::AgentUserInteractionRecorded(tau_proto::AgentUserInteractionRecorded {
                agent_id: agent_id.clone(),
            }),
        };
        let owner = PendingUiInteraction {
            client_id: client_id.clone(),
            cid: cid.clone(),
            agent_id: agent_id.clone(),
            generation: self.session_runtime.current_session_generation,
            navigation_epoch: self.ui_runtime.explicit_navigation_epoch,
            admission,
        };
        self.enqueue_publish_with_ui_interaction(
            event,
            ConversationHeadSync {
                user_response_baseline: None,
                cid,
                agent_id: Some(agent_id),
                session_generation: owner.generation,
                fold_parent: None,
                suppress_activation_dispatch: false,
                continuation: None,
                notify_watchers: false,
            },
            owner,
        );
    }

    /// Validate the exact live owner before admitting its semantic interaction.
    pub(super) fn ui_interaction_is_current(
        &self,
        owner: &PendingUiInteraction,
        event: &Event,
    ) -> bool {
        let exact_event = match &owner.admission {
            UiInteractionAdmission::Bootstrap { bootstrap_id, .. } => {
                matches!(event, Event::AgentMetadataSet(set)
                    if set.agent_id == owner.agent_id
                        && set.key.as_str() == format!("{}{bootstrap_id}",
                            super::ui_create_agent::BOOTSTRAP_ADMISSION_PREFIX)
                        && set.value == tau_proto::CborValue::Bool(true)
                        && !set.inheritable && set.mutation_id.is_none())
            }
            _ => matches!(event, Event::AgentUserInteractionRecorded(interaction)
                if interaction.agent_id == owner.agent_id),
        };
        exact_event
            && owner.generation == self.session_runtime.current_session_generation
            && !self.ui_runtime.shutdown_requested
            && !self
                .prompt_coordination
                .standalone_accounting
                .pending_agent_removals
                .contains(&owner.cid)
            && self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(owner.agent_id.as_str())
                == Some(&owner.cid)
            && self
                .agent_runtime
                .agent_registry
                .agents
                .get(&owner.cid)
                .is_some_and(|agent| {
                    !agent.dispatch.terminating
                        && agent.identity.session_id == self.session_runtime.current_session_id
                })
            && self
                .agent_runtime
                .agent_registry
                .session_loaded
                .contains(&owner.agent_id)
            && self
                .agent_runtime
                .agent_registry
                .navigation_modes
                .contains_key(&owner.agent_id)
    }

    /// Reject unaccepted input without changing navigation, ordering, or
    /// prompts.
    pub(super) fn reject_ui_interaction(&mut self, owner: PendingUiInteraction, reason: &str) {
        match owner.admission {
            UiInteractionAdmission::Prompt(_) => {
                self.send_ui_error_response(&owner.client_id, reason);
            }
            UiInteractionAdmission::Created(admission)
            | UiInteractionAdmission::Bootstrap { admission, .. } => {
                self.send_ui_create_agent_rejection(
                    &owner.client_id,
                    admission.request_id,
                    admission.session_id,
                    tau_proto::UiCreateAgentRejection::InitialPromptFailed,
                    reason.to_owned(),
                    Some(admission.agent_id),
                );
            }
        }
    }

    /// Advance acceptance ordering and release the separate prompt lifecycle.
    pub(super) fn commit_ui_interaction(&mut self, owner: PendingUiInteraction) {
        if let UiInteractionAdmission::Bootstrap { admission, .. } = owner.admission {
            // The committed marker suppresses restart even if the process stops
            // before the separate interaction or prompt transition. Replay must
            // never reconstruct this runtime-only continuation.
            self.enqueue_ui_interaction(
                &owner.client_id,
                owner.cid,
                owner.agent_id,
                UiInteractionAdmission::Created(admission),
            );
            return;
        }
        self.session_runtime.user_interaction_order.insert(
            owner.agent_id.to_string(),
            self.session_runtime.next_user_interaction_order,
        );
        self.session_runtime.next_user_interaction_order = self
            .session_runtime
            .next_user_interaction_order
            .saturating_add(1);
        match owner.admission {
            UiInteractionAdmission::Prompt(prompt) => {
                // A newer explicit choice wins over this delayed implicit
                // write.
                if owner.navigation_epoch == self.ui_runtime.explicit_navigation_epoch
                    && self
                        .write_loaded_agent_navigation_mode(
                            &owner.agent_id,
                            tau_proto::AgentNavigationMode::Active,
                        )
                        .is_err()
                {
                    self.send_ui_error_response(
                        &owner.client_id,
                        "accepted UI prompt target lost its navigation mode",
                    );
                    return;
                }
                match self.submit_prompt_to_agent(
                    self.session_runtime.current_session_id.clone(),
                    owner.agent_id.as_str(),
                    *prompt,
                ) {
                    Ok(PromptSubmission::Rejected { reason }) => {
                        self.send_ui_error_response(&owner.client_id, reason);
                    }
                    Err(error) => {
                        self.send_ui_error_response(
                            &owner.client_id,
                            format!("failed to submit accepted UI prompt: {error}"),
                        );
                    }
                    Ok(_) => {}
                }
            }
            UiInteractionAdmission::Created(admission) => {
                self.admit_created_initial_prompt(&owner.client_id, &owner.cid, admission);
            }
            UiInteractionAdmission::Bootstrap { .. } => unreachable!("handled above"),
        }
    }
}

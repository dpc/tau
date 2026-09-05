//! Non-destructive operator workflow for unloading one saved agent.

use super::*;

/// Runtime-only correlation and rollback state for one admitted operator
/// unload.
pub(super) struct PendingOperatorUnload {
    /// Attached socket UI that submitted the request.
    pub(super) requester: tau_proto::ConnectionId,
    /// Caller-generated request correlation.
    pub(super) request_id: String,
    /// Exact loaded runtime selected at admission.
    pub(super) cid: AgentId,
    /// Whether watch teardown was already expected before this request.
    pub(super) watch_was_expected: bool,
}

impl Harness {
    /// Handles one attached-UI request to unload an idle saved agent.
    pub(super) fn handle_unload_session_agent(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request: tau_proto::UnloadSessionAgent,
    ) {
        use tau_proto::UnloadSessionAgentOutcome as Outcome;

        let outcome = if request.session_id != self.session_runtime.current_session_id {
            Some(Outcome::StaleSession)
        } else if self.ui_runtime.shutdown_requested
            || self.session_runtime.shutdown_published
            || !self.agent_runtime.agent_registry.roster_valid
        {
            Some(Outcome::MembershipUnavailable)
        } else if !self
            .agent_runtime
            .agent_registry
            .roster_ever_loaded
            .contains(&request.agent_id)
        {
            Some(Outcome::AgentNotFound)
        } else if !self
            .agent_runtime
            .agent_registry
            .roster_durable_ever_loaded
            .contains(&request.agent_id)
        {
            Some(Outcome::UnsupportedEphemeral)
        } else if !self
            .agent_runtime
            .agent_registry
            .roster_loaded
            .contains(&request.agent_id)
        {
            Some(Outcome::AlreadyUnloaded)
        } else if self
            .agent_runtime
            .agent_registry
            .pending_operator_unloads
            .contains_key(&request.agent_id)
        {
            Some(Outcome::AlreadyUnloading)
        } else {
            let route = self
                .agent_runtime
                .agent_registry
                .agent_routes
                .get(&request.agent_id)
                .cloned();
            let cid = route.filter(|cid| {
                self.agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .is_some_and(|agent| {
                        agent.identity.agent_id.as_ref() == Some(&request.agent_id)
                            && agent.identity.session_id == request.session_id
                            && !agent.identity.persistence.is_ephemeral()
                            && !agent.dispatch.terminating
                    })
            });
            match cid {
                None => Some(Outcome::AgentUnavailable),
                Some(cid) if self.agent_has_accepted_work(&cid, &request.agent_id) => {
                    Some(Outcome::AgentBusy)
                }
                Some(cid) => {
                    let watch_was_expected = !self
                        .agent_runtime
                        .agent_watch
                        .expected_unloads
                        .insert(request.agent_id.to_string());
                    self.agent_runtime
                        .agent_registry
                        .agents
                        .get_mut(&cid)
                        .expect("validated unload runtime remains installed")
                        .dispatch
                        .terminating = true;
                    self.agent_runtime
                        .agent_registry
                        .pending_operator_unloads
                        .insert(
                            request.agent_id.clone(),
                            PendingOperatorUnload {
                                requester: client_id.clone(),
                                request_id: request.request_id.clone(),
                                cid,
                                watch_was_expected,
                            },
                        );
                    self.publish_event(
                        Some(crate::harness::harness_connection_id()),
                        Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                            session_id: request.session_id.clone(),
                            agent_id: request.agent_id.clone(),
                        }),
                    );
                    None
                }
            }
        };
        if let Some(outcome) = outcome {
            self.send_unload_session_agent_result(client_id, request, outcome);
        }
    }

    /// Returns whether teardown would discard accepted work owned by this
    /// agent.
    fn agent_has_accepted_work(&self, cid: &AgentId, agent_id: &tau_proto::AgentId) -> bool {
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return true;
        };
        !matches!(agent.turn.turn_state, AgentTurnState::Idle)
            || agent.dispatch.in_flight_prompt.is_some()
            || !agent.dispatch.pending_prompts.is_empty()
            || !agent.dispatch.pending_message_wakes.is_empty()
            || agent.dispatch.pending_replay_activation
            || !matches!(
                agent.dispatch.activation_dispatch,
                ActivationDispatchState::None
            )
            || agent.dispatch.pending_cancel.is_some()
            || agent.execution.tools_in_flight != 0
            || agent.turn.outer_turn.owned_id().is_some()
            || !matches!(
                agent.turn.output_length_continuation,
                path_crate_agent::OutputLengthContinuationState::None
            )
            || agent.turn.automatic_compaction.transaction_id().is_some()
            || self
                .agent_runtime
                .agent_registry
                .start_coordinator
                .agents
                .contains_key(cid)
            || self
                .prompt_coordination
                .prompt_runtime
                .agents
                .values()
                .any(|owner| owner == cid)
            || self
                .prompt_coordination
                .prompt_runtime
                .pending_replay_activation_occurrences
                .contains_key(cid)
            || self
                .prompt_coordination
                .prompt_runtime
                .pending_replay_uncertain_stale
                .contains_key(cid)
            || self
                .prompt_coordination
                .prompt_runtime
                .pending_uncertain_supersessions
                .contains_key(cid)
            || self
                .prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(cid)
            || self
                .prompt_coordination
                .prompt_runtime
                .pending_initial_correlations
                .contains_key(cid)
            || self
                .tool_routing
                .tool_runtime
                .tool_agents
                .values()
                .any(|owner| owner == cid)
            || self
                .tool_routing
                .tool_runtime
                .peer_internal_tool_agents
                .values()
                .any(|owner| owner == cid)
            || self
                .tool_routing
                .tool_runtime
                .background_completion_targets
                .values()
                .any(|owner| owner == cid)
            || self.input_wait_pending_for(cid)
            || self.has_agent_owned_publication(cid, agent_id)
            || self
                .peer_messaging
                .pending_external_receive_acks
                .values()
                .any(|pending| &pending.recipient_id == agent_id)
            || self
                .runtime_io
                .publication
                .capacity_rejected_activations
                .contains_key(cid)
            || self
                .prompt_coordination
                .context_discovery
                .pending_agents
                .contains_key(agent_id)
            || self
                .prompt_coordination
                .compaction_runtime
                .pending_ui_after_wait
                .contains_key(cid)
            || self
                .prompt_coordination
                .compaction_runtime
                .rejected_ui_starts
                .contains_key(cid)
            || self
                .prompt_coordination
                .compaction_runtime
                .enqueued_inference_checkpoints
                .iter()
                .any(|(target, _)| target == agent_id)
            || self
                .prompt_coordination
                .standalone_accounting
                .owners
                .values()
                .any(|owner| &owner.cid == cid)
            || self.has_unsettled_standalone_accounting_for(cid)
    }

    /// Sends one directed terminal unload result.
    fn send_unload_session_agent_result(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        request: tau_proto::UnloadSessionAgent,
        outcome: tau_proto::UnloadSessionAgentOutcome,
    ) {
        let _ = self.runtime_io.bus.send_to(
            client_id,
            None,
            HarnessOutputMessage::UnloadSessionAgentResult(tau_proto::UnloadSessionAgentResult {
                request_id: request.request_id,
                session_id: request.session_id,
                agent_id: request.agent_id,
                outcome,
            }),
        );
    }

    /// Rolls back one operator reservation after semantic admission rejects it.
    pub(super) fn reject_pending_operator_unload(
        &mut self,
        unloaded: &tau_proto::SessionAgentUnloaded,
    ) {
        let Some(pending) = self
            .agent_runtime
            .agent_registry
            .pending_operator_unloads
            .remove(&unloaded.agent_id)
        else {
            return;
        };
        if let Some(agent) = self
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(&pending.cid)
        {
            agent.dispatch.terminating = false;
        }
        if !pending.watch_was_expected {
            self.agent_runtime
                .agent_watch
                .expected_unloads
                .remove(unloaded.agent_id.as_str());
        }
        self.send_unload_session_agent_result(
            &pending.requester,
            tau_proto::UnloadSessionAgent {
                request_id: pending.request_id,
                session_id: unloaded.session_id.clone(),
                agent_id: unloaded.agent_id.clone(),
            },
            tau_proto::UnloadSessionAgentOutcome::TransitionRejected,
        );
    }

    /// Completes one committed operator unload after synchronous route
    /// retirement.
    pub(super) fn finish_pending_operator_unload(
        &mut self,
        unloaded: &tau_proto::SessionAgentUnloaded,
    ) {
        let Some(pending) = self
            .agent_runtime
            .agent_registry
            .pending_operator_unloads
            .remove(&unloaded.agent_id)
        else {
            return;
        };
        #[cfg(test)]
        {
            self.agent_runtime
                .agent_registry
                .unload_result_after_retirement = !self
                .agent_runtime
                .agent_registry
                .agent_routes
                .contains_key(&unloaded.agent_id)
                && !self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .contains_key(&pending.cid);
        }
        self.send_unload_session_agent_result(
            &pending.requester,
            tau_proto::UnloadSessionAgent {
                request_id: pending.request_id,
                session_id: unloaded.session_id.clone(),
                agent_id: unloaded.agent_id.clone(),
            },
            tau_proto::UnloadSessionAgentOutcome::Unloaded,
        );
    }
}

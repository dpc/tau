//! Owns post-commit publication completion, retained retries, and durability
//! classification.
//!
//! This boundary preserves the publication and recovery contracts governed by
//! `GATE-persistence-and-extension-interface-change-approval`.

use super::*;

impl Harness {
    /// Lets a cancellation accepted before terminal write-complete own the one
    /// canonical continuation terminal, regardless of the response queued
    /// first.
    pub(super) fn arbitrate_output_length_terminal_cancellation(
        &mut self,
        event: &mut Event,
        sync: &mut Option<ConversationHeadSync>,
    ) {
        if let Event::AgentStandaloneCompactionFailed(failed) = event {
            let cancellation_owns_failure = self
                .runtime_agent_id_for_target_agent(Some(failed.agent_id.as_str()))
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(&cid))
                .is_some_and(|agent| {
                    agent.pending_cancel.is_some()
                        && matches!(
                            &agent.activation_dispatch,
                            path_crate_agent::ActivationDispatchState::ContextRecoveryClaimPending {
                                transaction_id,
                                ..
                            } if transaction_id == &failed.transaction_id
                        )
                });
            if cancellation_owns_failure {
                failed.reason = tau_proto::StandaloneCompactionFailureReason::Cancelled;
            }
            return;
        }
        let Event::ProviderResponseFinished(response) = event else {
            return;
        };
        let Some(cid) = self.runtime_agent_id_for_target_agent(Some(response.agent_id.as_str()))
        else {
            return;
        };
        let cancellation_owns_prompt = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .is_some_and(|agent| {
                agent.pending_cancel.is_some()
                    && matches!(
                        &agent.output_length_continuation,
                        path_crate_agent::OutputLengthContinuationState::Active(continuation)
                            if continuation.plan.agent_prompt_id == response.agent_prompt_id
                    )
            });
        if cancellation_owns_prompt
            && response.recovery_disposition
                == tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
            && response.output_length_disposition == tau_proto::OutputLengthDisposition::None
            && let Some(owner) =
                self.agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|agent| match &agent.output_length_continuation {
                        path_crate_agent::OutputLengthContinuationState::Active(continuation)
                            if continuation.plan.agent_prompt_id == response.agent_prompt_id =>
                        {
                            Some(continuation.plan.owner.clone())
                        }
                        _ => None,
                    })
        {
            response.recovery_disposition = tau_proto::ContextRecoveryDisposition::None;
            if let Some(telemetry) = response.context_limit_telemetry.as_mut() {
                telemetry.recovery_eligible = false;
                telemetry.action = tau_proto::ContextLimitAction::Terminal;
            }
            response.output_length_disposition =
                tau_proto::OutputLengthDisposition::ContinuationTerminal {
                    outer_turn_id: owner.outer_turn_id,
                    source_agent_prompt_id: owner.source_agent_prompt_id,
                    ordinal: owner.ordinal,
                    outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                    outer_turn_finish_owed: true,
                };
        }
        if !matches!(
            response.output_length_disposition,
            tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
        ) {
            return;
        }
        if !cancellation_owns_prompt {
            return;
        }
        let tau_proto::OutputLengthDisposition::ContinuationTerminal {
            outcome,
            outer_turn_finish_owed,
            ..
        } = &mut response.output_length_disposition
        else {
            unreachable!("terminal checked above");
        };
        *outcome = tau_proto::OutputLengthContinuationOutcome::Cancelled;
        *outer_turn_finish_owed = true;
        response.stop_reason = ProviderStopReason::Error;
        response.error = Some("cancelled".to_owned());
        response.failure_kind = None;
        response.output_items.clear();
        self.prompt_coordination
            .prompt_runtime
            .local_route_failures
            .remove(&response.agent_prompt_id);
        let batch_parent = sync.as_ref().and_then(|sync| {
            let completion = sync.completion()?;
            match completion {
                AgentPublishCompletion::GatedFinal { batch_parent, .. }
                | AgentPublishCompletion::OutputLengthContinuation { batch_parent, .. }
                | AgentPublishCompletion::OutputLengthPreDeliveryFailure { batch_parent, .. } => {
                    Some(*batch_parent)
                }
                AgentPublishCompletion::ReactiveContextRecovery { checkpoint, .. } => {
                    Some(checkpoint.through)
                }
                AgentPublishCompletion::InitialPromptSubmission { .. }
                | AgentPublishCompletion::OutputLengthSteer { .. }
                | AgentPublishCompletion::OutputLengthDormantRepair { .. }
                | AgentPublishCompletion::ReactiveContextRecoveryStart { .. }
                | AgentPublishCompletion::ReactiveContextRecoveryFailure { .. }
                | AgentPublishCompletion::StandaloneContinuation { .. } => None,
            }
        });
        if let (Some(sync), Some(batch_parent)) = (sync.as_mut(), batch_parent) {
            sync.suppress_activation_dispatch = true;
            self.runtime_io
                .publication
                .idle_dispatches
                .retain(|dispatch| dispatch.cid != sync.cid);
            sync.continuation = Some(PostCommitContinuation::AgentPublish(Box::new(
                AgentPublishCompletion::OutputLengthContinuation {
                    batch_parent,
                    response: Box::new(response.clone()),
                    assistant_text: None,
                    retry_event: None,
                },
            )));
        }
    }

    /// Restore a claimed wait when its canonical preemption terminal did not
    /// cross the semantic append boundary.
    pub(super) fn rollback_failed_wait_compaction_terminal(&mut self, event: &Event) {
        let Event::ToolCancelled(cancelled) = event else {
            return;
        };
        let Some(cid) = self
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(&cancelled.call_id)
            .cloned()
        else {
            return;
        };
        let Some(pending) = self
            .prompt_coordination
            .compaction_runtime
            .pending_ui_after_wait
            .get(&cid)
        else {
            return;
        };
        if pending.wait_call_id != cancelled.call_id {
            return;
        }
        self.reject_pending_ui_compaction(
            &cid,
            "compaction canceled because wait cancellation could not be committed",
        );
        self.tool_routing
            .tool_runtime
            .pending_terminal_observations
            .remove(&cancelled.call_id);
        self.rollback_manual_compaction_wait_claim(&cid, &cancelled.call_id);
        self.process_input_wait_deadlines(Instant::now());
    }

    /// Removes one deferred UI compaction and reports why it cannot continue.
    pub(super) fn reject_pending_ui_compaction(&mut self, cid: &AgentId, message: &'static str) {
        if let Some(pending) = self
            .prompt_coordination
            .compaction_runtime
            .pending_ui_after_wait
            .remove(cid)
        {
            self.send_ui_error_response(&pending.requester_client_id, message);
        }
    }

    /// Require an exact live `AwaitingCheckpoint` owner for every delayed
    /// inference checkpoint before it can append.
    pub(super) fn synchronized_inference_checkpoint_has_live_owner(
        &self,
        event: &Event,
        sync: Option<&ConversationHeadSync>,
    ) -> bool {
        let Event::AgentInferenceDispatchStarted(started) = event else {
            return true;
        };
        let Some(sync) = sync else {
            return true;
        };
        if matches!(
            sync.completion(),
            Some(AgentPublishCompletion::OutputLengthDormantRepair { .. })
        ) {
            return true;
        }
        if sync.session_generation != self.session_runtime.current_session_generation {
            return false;
        }
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(&sync.cid) else {
            return false;
        };
        if agent.terminating
            || agent.session_id != self.session_runtime.current_session_id
            || agent.agent_id.as_deref() != Some(started.agent_id.as_str())
            || sync
                .agent_id
                .as_ref()
                .is_some_and(|agent_id| agent_id != &started.agent_id)
        {
            return false;
        }
        let path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
            owner,
            agent_prompt_id,
            through,
            dispatch,
        } = &agent.activation_dispatch
        else {
            return false;
        };
        owner.transaction_id() == started.transaction_id.as_ref()
            && agent_prompt_id == &started.agent_prompt_id
            && through == &started.through
            && started.model.as_ref() == Some(&dispatch.model)
            && started.operation.as_ref() == Some(&dispatch.operation)
            && started.activation_cut.as_ref() == Some(&dispatch.activation_cut)
    }

    /// Validate a delayed activation successor against the selected branch
    /// immediately before durable persistence.
    pub(super) fn activation_successor_matches_selected_head(&self, event: &Event) -> bool {
        let (agent_id, through) = match event {
            Event::AgentInferenceDispatchStarted(started)
                if self
                    .runtime_agent_id_for_target_agent(Some(started.agent_id.as_str()))
                    .and_then(|cid| self.agent_runtime.agent_registry.agents.get(&cid))
                    .is_some_and(|agent| {
                        matches!(
                            agent.activation_dispatch,
                            crate::agent::ActivationDispatchState::AwaitingCheckpoint { .. }
                        )
                    }) =>
            {
                (&started.agent_id, Some(started.through))
            }
            Event::AgentInferenceDispatchStarted(_) => return true,
            Event::AgentStandaloneCompactionStarted(started) => {
                (&started.agent_id, started.resume_through)
            }
            _ => return true,
        };
        let Some(through) = through else {
            return true;
        };
        self.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(&cid))
            .is_some_and(|agent| {
                let selected = agent
                    .head
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                agent
                    .agent_id
                    .as_deref()
                    .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                    .is_none_or(|tree| tree.is_ancestor_head(through, selected))
            })
    }

    /// Complete the exact post-commit action carried by an agent publication.
    pub(super) fn complete_agent_publish(
        &mut self,
        cid: &AgentId,
        completion: AgentPublishCompletion,
        through: tau_proto::AgentHead,
    ) {
        if let AgentPublishCompletion::InitialPromptSubmission { mut correlation } = completion {
            correlation.activation_through = Some(through);
            self.prompt_coordination
                .prompt_runtime
                .pending_initial_correlations
                .insert(cid.clone(), correlation);
            return;
        }
        if let AgentPublishCompletion::GatedFinal { disposition, .. } = completion {
            match disposition {
                GatedFinalDisposition::Challenge { challenge } => {
                    if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                        agent.work_status.record_final_challenge(&challenge);
                        agent
                            .pending_prompts
                            .push_back(PendingPrompt::internal(final_status_reminder(&challenge)));
                    }
                    self.continue_after_gated_final_challenge(cid);
                }
                GatedFinalDisposition::Accept { terminal } => {
                    if self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get_mut(cid)
                        .is_some_and(|agent| agent.work_status.invalidate_working())
                    {
                        self.notify_work_status_transition(cid);
                    }
                    self.complete_committed_gated_final(cid, *terminal);
                }
            }
            return;
        }
        if let AgentPublishCompletion::OutputLengthContinuation {
            response,
            assistant_text,
            ..
        } = completion
        {
            self.complete_finished_response_without_tool_calls(
                cid,
                &response,
                assistant_text.as_deref(),
            );
            return;
        }
        if let AgentPublishCompletion::OutputLengthSteer { .. } = completion {
            let dormant = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.agent_id.as_deref())
                .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                .and_then(tau_core::AgentTree::output_length_dormant_repair)
                .is_some();
            if dormant {
                self.repair_dormant_output_length_lineage(cid);
            } else {
                self.dispatch_activation_after_publish_idle(cid);
            }
            return;
        }
        if let AgentPublishCompletion::OutputLengthPreDeliveryFailure { response, .. } = completion
        {
            if self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .is_some_and(|agent| agent.pending_cancel.is_some())
            {
                self.prompt_coordination
                    .prompt_runtime
                    .local_route_failures
                    .remove(&response.agent_prompt_id);
                self.finalize_canceled_in_flight_prompt(cid);
                return;
            }
            let completion = Some(AgentPublishCompletion::OutputLengthContinuation {
                batch_parent: self
                    .selected_head_for_agent(cid)
                    .unwrap_or(tau_proto::AgentHead::Root),
                response: response.clone(),
                assistant_text: None,
                retry_event: None,
            });
            self.publish_finished_response_for_agent(cid, None, &response, completion, false);
            return;
        }
        if let AgentPublishCompletion::OutputLengthDormantRepair { step, .. } = completion {
            match step {
                DormantOutputLengthCompletion::Owner {
                    activation_cut,
                    steer,
                    ..
                } => {
                    self.retire_dormant_output_length_activation(cid, activation_cut, steer);
                    self.repair_dormant_output_length_lineage(cid);
                }
                DormantOutputLengthCompletion::Steer { .. }
                | DormantOutputLengthCompletion::Terminal { .. } => {
                    self.repair_dormant_output_length_lineage(cid);
                }
                DormantOutputLengthCompletion::Finish { .. } => {
                    if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                        agent.pending_cancel = None;
                    }
                    self.set_agent_turn_state(cid, AgentTurnState::Idle);
                    self.emit_info_important(
                        "Output-length continuation failed on its dormant original branch after branch selection changed.",
                    );
                    self.drain_publish_idle_dispatches();
                    self.try_advance_queue();
                }
            }
            return;
        }
        if let AgentPublishCompletion::ReactiveContextRecovery {
            checkpoint, source, ..
        } = completion
        {
            let selected = self
                .selected_head_for_agent(cid)
                .unwrap_or(tau_proto::AgentHead::Root);
            let branch_matches = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.agent_id.as_deref())
                .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                .is_some_and(|tree| tree.is_ancestor_head(checkpoint.through, selected));
            let cancelled = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .is_some_and(|agent| agent.pending_cancel.is_some());
            if !branch_matches || cancelled {
                self.terminalize_replay_blocked_context_recovery(
                    cid,
                    &checkpoint,
                    if cancelled {
                        tau_proto::StandaloneCompactionFailureReason::Cancelled
                    } else {
                        tau_proto::StandaloneCompactionFailureReason::StaleBranch
                    },
                );
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    agent.pending_cancel = None;
                }
                return;
            }
            self.start_reactive_compaction_for_checkpoint(cid, &checkpoint, source.as_ref());
            return;
        }
        if let AgentPublishCompletion::ReactiveContextRecoveryStart {
            failure_after_commit,
            ..
        } = completion
        {
            if let Some(mut failure) = failure_after_commit {
                if self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .is_some_and(|agent| agent.pending_cancel.is_some())
                {
                    failure.reason = tau_proto::StandaloneCompactionFailureReason::Cancelled;
                }
                self.publish_event_for_agent_with_completion(
                    cid,
                    None,
                    Event::AgentStandaloneCompactionFailed(*failure),
                    Some(AgentPublishCompletion::ReactiveContextRecoveryFailure {
                        batch_parent: through,
                        retry_event: None,
                    }),
                    false,
                );
            }
            return;
        }
        if let AgentPublishCompletion::ReactiveContextRecoveryFailure { .. } = completion {
            return;
        }
        let AgentPublishCompletion::StandaloneContinuation {
            transaction_id,
            model,
            activation_cut,
            batch_parent: _,
            source,
            retry_prompts: _,
            complete_on_commit,
            ..
        } = completion
        else {
            unreachable!("gated final returned above")
        };
        if !complete_on_commit {
            return;
        }
        let Some((agent_id, agent_prompt_id)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|agent| {
                let durable_agent_id = agent.agent_id.as_deref().unwrap_or(cid.as_ref());
                (
                    crate::parse_agent_id(durable_agent_id),
                    tau_proto::AgentPromptId::parse(format!(
                        "ap-{durable_agent_id}-{}",
                        agent.next_prompt_index
                    ))
                    .expect("known-safe AgentPromptId must be valid"),
                )
            })
        else {
            return;
        };
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.next_prompt_index = agent.next_prompt_index.saturating_add(1);
            agent.activation_dispatch =
                path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                    owner: path_crate_agent::InferenceCheckpointOwner::Standalone {
                        id: transaction_id.clone(),
                    },
                    agent_prompt_id: agent_prompt_id.clone(),
                    through,
                    dispatch: crate::agent::InferenceDispatchOwnership {
                        model: model.clone(),
                        operation: tau_proto::PromptOperation::Inference,
                        activation_cut,
                    },
                };
        }
        self.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .insert((agent_id.clone(), transaction_id.clone()));
        self.publish_for_agent_from(
            cid,
            source.as_ref(),
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id,
                transaction_id: Some(transaction_id),
                agent_prompt_id,
                through,
                model: Some(model),
                operation: Some(tau_proto::PromptOperation::Inference),
                activation_cut: Some(activation_cut),
                output_length_continuation: None,
            }),
        );
    }

    /// Retain a rejected completion-bearing envelope without synthesizing an
    /// activation token or draining its prompt payload.
    pub(super) fn retain_rejected_agent_publish(
        &mut self,
        sync: Option<&ConversationHeadSync>,
        event: &Event,
    ) {
        let Some((cid, mut completion)) = sync.and_then(|sync| {
            sync.completion()
                .cloned()
                .map(|completion| (sync.cid.clone(), completion))
        }) else {
            return;
        };
        if let AgentPublishCompletion::InitialPromptSubmission { correlation } = completion {
            self.runtime_io
                .publication
                .idle_dispatches
                .retain(|dispatch| dispatch.cid != cid);
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.in_flight_prompt = None;
            }
            self.set_agent_turn_state(&cid, AgentTurnState::Idle);
            self.publish_initial_prompt_failed(
                correlation,
                tau_proto::AgentPromptFailureStage::Submission,
                "failed to commit initial prompt",
            );
            return;
        }
        match &mut completion {
            AgentPublishCompletion::StandaloneContinuation {
                approved_retry_event,
                ..
            } => *approved_retry_event = Some(Box::new(event.clone())),
            AgentPublishCompletion::GatedFinal { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::OutputLengthContinuation { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::OutputLengthSteer { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::OutputLengthPreDeliveryFailure { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::OutputLengthDormantRepair { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::ReactiveContextRecovery { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::ReactiveContextRecoveryStart { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::ReactiveContextRecoveryFailure { retry_event, .. } => {
                *retry_event = Some(Box::new(event.clone()));
            }
            AgentPublishCompletion::InitialPromptSubmission { .. } => {
                unreachable!("initial submission returned above")
            }
        }
        if matches!(
            completion,
            AgentPublishCompletion::StandaloneContinuation { .. }
        ) {
            self.discard_deferred_agent_publish_batch(&cid, &completion);
        }
        self.prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .insert(cid, completion);
    }

    /// Releases runtime de-duplication after semantic persistence rejects an
    /// eager start. The durable decision remains authoritative for a later
    /// distinct progress-triggered retry.
    pub(super) fn clear_rejected_eager_compaction_start(&mut self, event: &Event) {
        let (agent_id, decision_id) = match event {
            Event::AgentStandaloneCompactionStarted(started) => {
                let tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { decision_id } =
                    &started.trigger
                else {
                    return;
                };
                (&started.agent_id, decision_id)
            }
            Event::AgentStandaloneCompactionFailed(failed)
                if failed.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch =>
            {
                (&failed.agent_id, &failed.transaction_id)
            }
            _ => return,
        };
        if let Some(cid) = self.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            && agent.pending_automatic_compaction_start.as_ref() == Some(decision_id)
        {
            agent.pending_automatic_compaction_start = None;
        }
    }

    /// Republish one retained completion envelope only on its owning branch.
    pub(super) fn retry_pending_agent_publish_completion(&mut self, cid: &AgentId) {
        let Some(completion) = self
            .prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .remove(cid)
        else {
            return;
        };
        if let AgentPublishCompletion::ReactiveContextRecoveryStart { checkpoint, .. } = &completion
        {
            let selected = self
                .selected_head_for_agent(cid)
                .unwrap_or(tau_proto::AgentHead::Root);
            let branch_matches = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.agent_id.as_deref())
                .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                .is_some_and(|tree| tree.is_ancestor_head(checkpoint.through, selected));
            let cancelled = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .is_some_and(|agent| agent.pending_cancel.is_some());
            if !branch_matches || cancelled {
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    agent.activation_dispatch =
                        path_crate_agent::ActivationDispatchState::ContextRecoveryPending {
                            checkpoint: checkpoint.clone(),
                        };
                }
                self.terminalize_replay_blocked_context_recovery(
                    cid,
                    checkpoint,
                    if cancelled {
                        tau_proto::StandaloneCompactionFailureReason::Cancelled
                    } else {
                        tau_proto::StandaloneCompactionFailureReason::StaleBranch
                    },
                );
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    agent.pending_cancel = None;
                }
                return;
            }
        }
        if matches!(
            completion,
            AgentPublishCompletion::GatedFinal { .. }
                | AgentPublishCompletion::OutputLengthContinuation { .. }
                | AgentPublishCompletion::OutputLengthSteer { .. }
                | AgentPublishCompletion::OutputLengthPreDeliveryFailure { .. }
                | AgentPublishCompletion::OutputLengthDormantRepair { .. }
                | AgentPublishCompletion::ReactiveContextRecovery { .. }
                | AgentPublishCompletion::ReactiveContextRecoveryStart { .. }
                | AgentPublishCompletion::ReactiveContextRecoveryFailure { .. }
        ) {
            if matches!(
                completion,
                AgentPublishCompletion::OutputLengthDormantRepair { .. }
                    | AgentPublishCompletion::ReactiveContextRecovery { .. }
                    | AgentPublishCompletion::ReactiveContextRecoveryStart { .. }
                    | AgentPublishCompletion::ReactiveContextRecoveryFailure { .. }
            ) {
                let retry_event = match &completion {
                    AgentPublishCompletion::OutputLengthDormantRepair { retry_event, .. }
                    | AgentPublishCompletion::ReactiveContextRecovery { retry_event, .. }
                    | AgentPublishCompletion::ReactiveContextRecoveryStart {
                        retry_event, ..
                    }
                    | AgentPublishCompletion::ReactiveContextRecoveryFailure {
                        retry_event, ..
                    } => retry_event,
                    _ => unreachable!("matched direct retry"),
                };
                let Some(event) = retry_event.clone() else {
                    return;
                };
                let mut approved = completion;
                match &mut approved {
                    AgentPublishCompletion::OutputLengthDormantRepair { retry_event, .. }
                    | AgentPublishCompletion::ReactiveContextRecovery { retry_event, .. }
                    | AgentPublishCompletion::ReactiveContextRecoveryStart {
                        retry_event, ..
                    }
                    | AgentPublishCompletion::ReactiveContextRecoveryFailure {
                        retry_event, ..
                    } => *retry_event = None,
                    _ => unreachable!("matched direct retry"),
                };
                self.commit_approved_agent_retry(cid, *event, approved);
                return;
            }
            let (batch_parent, retry_event) = match &completion {
                AgentPublishCompletion::GatedFinal {
                    batch_parent,
                    retry_event,
                    ..
                }
                | AgentPublishCompletion::OutputLengthContinuation {
                    batch_parent,
                    retry_event,
                    ..
                }
                | AgentPublishCompletion::OutputLengthSteer {
                    batch_parent,
                    retry_event,
                }
                | AgentPublishCompletion::OutputLengthPreDeliveryFailure {
                    batch_parent,
                    retry_event,
                    ..
                } => (*batch_parent, retry_event.clone()),
                AgentPublishCompletion::OutputLengthDormantRepair { .. } => {
                    unreachable!("dormant repair returned above")
                }
                AgentPublishCompletion::ReactiveContextRecovery { .. } => {
                    unreachable!("reactive recovery returned above")
                }
                AgentPublishCompletion::ReactiveContextRecoveryStart { .. } => {
                    unreachable!("reactive start returned above")
                }
                _ => unreachable!(),
            };
            if self.selected_head_for_agent(cid) != Some(batch_parent) {
                self.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .insert(cid.clone(), completion);
                return;
            }
            let Some(event) = retry_event else {
                return;
            };
            let mut approved = completion;
            match &mut approved {
                AgentPublishCompletion::GatedFinal { retry_event, .. }
                | AgentPublishCompletion::OutputLengthContinuation { retry_event, .. }
                | AgentPublishCompletion::OutputLengthSteer { retry_event, .. }
                | AgentPublishCompletion::OutputLengthPreDeliveryFailure { retry_event, .. } => {
                    *retry_event = None;
                }
                AgentPublishCompletion::OutputLengthDormantRepair { .. } => {
                    unreachable!("dormant repair returned above")
                }
                AgentPublishCompletion::ReactiveContextRecovery { .. } => {
                    unreachable!("reactive recovery returned above")
                }
                AgentPublishCompletion::ReactiveContextRecoveryStart { .. } => {
                    unreachable!("reactive start returned above")
                }
                _ => unreachable!(),
            }
            self.commit_approved_agent_retry(cid, *event, approved);
            return;
        }
        let (batch_parent, retry_prompts, approved_retry_event) = match &completion {
            AgentPublishCompletion::StandaloneContinuation {
                batch_parent,
                retry_prompts,
                approved_retry_event,
                ..
            } => (
                *batch_parent,
                retry_prompts.clone(),
                approved_retry_event.clone(),
            ),
            AgentPublishCompletion::GatedFinal { .. } => unreachable!("returned above"),
            AgentPublishCompletion::OutputLengthContinuation { .. } => {
                unreachable!("returned above")
            }
            AgentPublishCompletion::OutputLengthSteer { .. } => unreachable!("returned above"),
            AgentPublishCompletion::OutputLengthPreDeliveryFailure { .. } => {
                unreachable!("returned above")
            }
            AgentPublishCompletion::OutputLengthDormantRepair { .. } => {
                unreachable!("returned above")
            }
            AgentPublishCompletion::ReactiveContextRecovery { .. } => {
                unreachable!("returned above")
            }
            AgentPublishCompletion::ReactiveContextRecoveryStart { .. } => {
                unreachable!("returned above")
            }
            AgentPublishCompletion::ReactiveContextRecoveryFailure { .. } => {
                unreachable!("returned above")
            }
            AgentPublishCompletion::InitialPromptSubmission { .. } => {
                unreachable!("initial submissions are never retained for retry")
            }
        };
        if retry_prompts.is_empty() {
            return;
        };
        let selected = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.head)
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let on_owning_branch = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .is_some_and(|tree| tree.is_ancestor_head(batch_parent, selected));
        if !on_owning_branch {
            self.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .insert(cid.clone(), completion);
            return;
        }
        if let Some(approved_event) = approved_retry_event {
            let mut approved_completion = completion.clone();
            let AgentPublishCompletion::StandaloneContinuation {
                approved_retry_event,
                complete_on_commit,
                ..
            } = &mut approved_completion
            else {
                return;
            };
            *approved_retry_event = None;
            *complete_on_commit = retry_prompts.len() == 1;
            self.commit_approved_agent_retry(cid, *approved_event, approved_completion);
            if self
                .prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .contains_key(cid)
            {
                return;
            }
            if retry_prompts.len() == 1 {
                return;
            }
            let mut remaining_completion = completion;
            let AgentPublishCompletion::StandaloneContinuation {
                approved_retry_event,
                ..
            } = &mut remaining_completion
            else {
                return;
            };
            *approved_retry_event = None;
            self.publish_prompts_as_steered(
                cid,
                retry_prompts[1..].to_vec(),
                Some(remaining_completion),
            );
            return;
        }
        self.publish_prompts_as_steered(cid, retry_prompts, Some(completion));
    }

    /// Retry retained append-rejected publications when ordinary runtime input
    /// proves that the harness is making progress again.
    pub(super) fn retry_pending_agent_publications(&mut self) {
        let pending = self
            .prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for cid in pending {
            self.retry_pending_agent_publish_completion(&cid);
        }
        let pending_finishes = self
            .agent_runtime
            .agent_registry
            .agents
            .iter()
            .filter(|(_, agent)| {
                matches!(
                    agent.outer_turn,
                    path_crate_agent::OuterTurnRuntimeState::FinishRetry(_)
                )
            })
            .map(|(cid, _)| cid.clone())
            .collect::<Vec<_>>();
        for cid in pending_finishes {
            self.retry_outer_turn_finish(&cid);
        }
    }

    /// Advance one core-projected dormant output-length repair step, restoring
    /// the selected sibling before deriving the next step.
    pub(super) fn repair_dormant_output_length_lineage(&mut self, cid: &AgentId) {
        if let Some(completion) = self
            .prompt_coordination
            .prompt_runtime
            .pending_publish_completions
            .remove(cid)
        {
            if matches!(
                completion,
                AgentPublishCompletion::OutputLengthSteer { .. }
                    | AgentPublishCompletion::OutputLengthContinuation { .. }
            ) {
                // The exact dormant repair supersedes pre-branch live
                // scheduling.
            } else {
                self.prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .insert(cid.clone(), completion);
                return;
            }
        }
        let Some((agent_id, repair)) =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| {
                    let agent_id = crate::parse_agent_id(agent.agent_id.as_deref()?);
                    let repair = self
                        .session_runtime
                        .agent_store
                        .agent(agent_id.as_str())?
                        .output_length_dormant_repair()?;
                    Some((agent_id, repair))
                })
        else {
            return;
        };
        let (event, step) = match repair {
            tau_core::OutputLengthDormantRepair::Steer { parent, .. } => {
                let prompt = PendingPrompt::output_length_continuation();
                let internal_kind = prompt.internal_kind();
                (
                    Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                        agent_id: agent_id.clone(),
                        inference_activation: true,
                        submission_source: prompt.submission_source,
                        text: prompt.text,
                        trusted_internal_spans: prompt.trusted_internal_spans,
                        message_class: prompt.message_class,
                        self_compaction_terminal: None,
                        internal_kind,
                        ctx_id: None,
                    }),
                    DormantOutputLengthCompletion::Steer {
                        fold_parent: tau_core::AgentEventParent::from_head(parent),
                    },
                )
            }
            tau_core::OutputLengthDormantRepair::Owner {
                source,
                successor_agent_prompt_id,
                outer_turn_id,
                through,
                plan_parent: _,
            } => {
                let activation_cut = source
                    .activation_cut
                    .expect("validated output-length source carries activation cut");
                (
                    Event::AgentInferenceDispatchStarted(
                        tau_proto::AgentInferenceDispatchStarted {
                            agent_id: agent_id.clone(),
                            transaction_id: None,
                            agent_prompt_id: successor_agent_prompt_id,
                            through,
                            model: source.model,
                            operation: source.operation,
                            activation_cut: Some(activation_cut),
                            output_length_continuation: Some(
                                tau_proto::OutputLengthContinuationOwner {
                                    source_agent_prompt_id: source.agent_prompt_id,
                                    outer_turn_id,
                                    ordinal: 1,
                                },
                            ),
                        },
                    ),
                    DormantOutputLengthCompletion::Owner {
                        fold_parent: tau_core::AgentEventParent::from_head(through),
                        activation_cut,
                        steer: through,
                    },
                )
            }
            tau_core::OutputLengthDormantRepair::Terminal { owner, parent } => {
                let continuation = owner
                    .output_length_continuation
                    .expect("dormant terminal owner carries continuation");
                (
                    Event::ProviderResponseFinished(ProviderResponseFinished {
                        automatic_compaction_decision: None,
                        agent_prompt_id: owner.agent_prompt_id,
                        agent_id: agent_id.clone(),
                        output_items: Vec::new(),
                        stop_reason: ProviderStopReason::Error,
                        error: Some("output-length continuation branch was deselected".to_owned()),
                        failure_kind: Some(tau_proto::ProviderFailureKind::Unknown),
                        context_limit_telemetry: None,
                        recovery_disposition: tau_proto::ContextRecoveryDisposition::None,
                        output_length_disposition:
                            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                                outer_turn_id: continuation.outer_turn_id,
                                source_agent_prompt_id: continuation.source_agent_prompt_id,
                                ordinal: continuation.ordinal,
                                outcome: tau_proto::OutputLengthContinuationOutcome::Failed,
                                outer_turn_finish_owed: true,
                            },
                        provider_attempt: Default::default(),
                        originator: PromptOriginator::User,
                        usage: None,
                        estimated_api_cost_rates: None,
                        estimated_api_cost_increment: None,
                        compaction_original_input_tokens: None,
                        compaction_compacted_input_tokens: None,
                        backend: None,
                        provider_response_id: None,
                        ws_pool_delta: None,
                    }),
                    DormantOutputLengthCompletion::Terminal {
                        fold_parent: tau_core::AgentEventParent::from_head(parent),
                    },
                )
            }
            tau_core::OutputLengthDormantRepair::Finish {
                outer_turn_id,
                parent,
            } => {
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                    agent.outer_turn = path_crate_agent::OuterTurnRuntimeState::FinishInFlight(
                        outer_turn_id.clone(),
                    );
                }
                (
                    Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
                        automatic_compaction_decision: None,
                        agent_id: agent_id.clone(),
                        session_id: self.session_runtime.current_session_id.clone(),
                        outer_turn_id,
                        disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                    }),
                    DormantOutputLengthCompletion::Finish {
                        fold_parent: tau_core::AgentEventParent::from_head(parent),
                    },
                )
            }
        };
        let parent = step.fold_parent();
        self.enqueue_publish(
            None,
            event,
            true,
            true,
            Some(ConversationHeadSync {
                cid: cid.clone(),
                agent_id: Some(agent_id),
                session_generation: self.session_runtime.current_session_generation,
                fold_parent: Some(parent),
                suppress_activation_dispatch: true,
                continuation: Some(PostCommitContinuation::AgentPublish(Box::new(
                    AgentPublishCompletion::OutputLengthDormantRepair {
                        step,
                        retry_event: None,
                    },
                ))),
                notify_watchers: false,
            }),
        );
    }

    /// Re-publishes one append-rejected finish while keeping one in-flight
    /// owner.
    pub(super) fn retry_outer_turn_finish(&mut self, cid: &AgentId) {
        let Some(finish) = self
            .agent_runtime
            .agent_registry
            .agents
            .get_mut(cid)
            .and_then(|agent| {
                let path_crate_agent::OuterTurnRuntimeState::FinishRetry(outer_turn_id) =
                    &agent.outer_turn
                else {
                    return None;
                };
                let outer_turn_id = outer_turn_id.clone();
                agent.outer_turn =
                    path_crate_agent::OuterTurnRuntimeState::FinishInFlight(outer_turn_id.clone());
                Some(tau_proto::AgentOuterTurnFinished {
                    automatic_compaction_decision: agent
                        .pending_automatic_compaction_decision
                        .clone(),
                    agent_id: crate::parse_agent_id(agent.agent_id.as_deref()?),
                    session_id: agent.session_id.clone(),
                    outer_turn_id,
                    disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                })
            })
        else {
            return;
        };
        self.publish_for_agent(cid, Event::AgentOuterTurnFinished(finish));
    }

    /// Retains one exact finish only after its durable append is rejected.
    pub(super) fn retain_rejected_outer_turn_finish(&mut self, event: &Event) {
        let Event::AgentOuterTurnFinished(finished) = event else {
            return;
        };
        let Some(cid) = self.runtime_agent_id_for_target_agent(Some(finished.agent_id.as_str()))
        else {
            return;
        };
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            && matches!(
                &agent.outer_turn,
                path_crate_agent::OuterTurnRuntimeState::FinishInFlight(id)
                    if id == &finished.outer_turn_id
            )
        {
            agent.outer_turn = path_crate_agent::OuterTurnRuntimeState::FinishRetry(
                finished.outer_turn_id.clone(),
            );
        }
    }

    /// Retry the exact standalone-owned inference checkpoint after branch
    /// reselection, retaining `AwaitingCheckpoint` until a commit succeeds.
    pub(super) fn retry_standalone_inference_checkpoint(&mut self, cid: &AgentId) {
        let Some((agent_id, transaction_id, agent_prompt_id, through, dispatch)) =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| {
                    let path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                        owner:
                            path_crate_agent::InferenceCheckpointOwner::Standalone {
                                id: transaction_id,
                            },
                        agent_prompt_id,
                        through,
                        dispatch,
                    } = &agent.activation_dispatch
                    else {
                        return None;
                    };
                    Some((
                        crate::parse_agent_id(agent.agent_id.as_deref()?),
                        transaction_id.clone(),
                        agent_prompt_id.clone(),
                        *through,
                        dispatch.clone(),
                    ))
                })
        else {
            return;
        };
        let key = (agent_id.clone(), transaction_id.clone());
        if self
            .prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .contains(&key)
        {
            return;
        }
        let event =
            Event::AgentInferenceDispatchStarted(tau_proto::AgentInferenceDispatchStarted {
                agent_id,
                transaction_id: Some(transaction_id),
                agent_prompt_id,
                through,
                model: Some(dispatch.model),
                operation: Some(dispatch.operation),
                activation_cut: Some(dispatch.activation_cut),
                output_length_continuation: None,
            });
        if !self.activation_successor_matches_selected_head(&event) {
            return;
        }
        self.prompt_coordination
            .compaction_runtime
            .enqueued_inference_checkpoints
            .insert(key);
        self.publish_for_agent(cid, event);
    }

    /// Roll back an ordinary successor that did not commit while retaining its
    /// branch-owned obligation.
    ///
    /// A standalone successor instead retains `AwaitingCheckpoint`: its durable
    /// compaction transaction is the sole continuation owner and is retried on
    /// eligible branch reselection.
    pub(super) fn rollback_rejected_activation_successor(&mut self, event: &Event) {
        let Event::AgentInferenceDispatchStarted(started) = event else {
            return;
        };
        if let Some(transaction_id) = started.transaction_id.as_ref() {
            self.prompt_coordination
                .compaction_runtime
                .enqueued_inference_checkpoints
                .remove(&(started.agent_id.clone(), transaction_id.clone()));
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(Some(started.agent_id.as_str()))
        else {
            return;
        };
        let ordinary_reservation = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .is_some_and(|agent| {
                matches!(
                    &agent.activation_dispatch,
                    crate::agent::ActivationDispatchState::AwaitingCheckpoint {
                        owner: crate::agent::InferenceCheckpointOwner::Inference,
                        agent_prompt_id,
                        through,
                        ..
                    } if agent_prompt_id == &started.agent_prompt_id && through == &started.through
                )
            });
        if !ordinary_reservation {
            // A standalone-owned checkpoint is the sole continuation owner for
            // its durable transaction. Keep AwaitingCheckpoint intact when its
            // successor does not commit; unlike an ordinary activation, it has
            // no deferred branch obligation from which to reconstruct ownership.
            return;
        }
        let mut retained_output_length = false;
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
            agent.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
            if matches!(
                &agent.output_length_continuation,
                path_crate_agent::OutputLengthContinuationState::OwnerPending(continuation)
                    if continuation.plan.agent_prompt_id == started.agent_prompt_id
            ) {
                let path_crate_agent::OutputLengthContinuationState::OwnerPending(continuation) =
                    std::mem::take(&mut agent.output_length_continuation)
                else {
                    unreachable!("matched owner-pending continuation");
                };
                agent.output_length_continuation =
                    path_crate_agent::OutputLengthContinuationState::OwnerReady(continuation);
                agent.pending_replay_activation = true;
                agent.turn_state = AgentTurnState::Idle;
                retained_output_length = true;
            }
        }
        if retained_output_length {
            self.emit_harness_failure(
                "output-length continuation owner did not commit; retaining the durable obligation",
            );
            return;
        }
        self.discard_finished_response_prompt_tracking(&started.agent_prompt_id);
        self.set_agent_turn_state(&cid, AgentTurnState::Idle);
    }

    pub(super) fn pending_external_receive_message_id(
        event: &Event,
    ) -> Option<&tau_proto::AgentMessageId> {
        let Event::AgentMessageReceived(message) = event else {
            return None;
        };
        Some(&message.message_id)
    }

    pub(super) fn validate_pending_external_receive_before_commit(
        &mut self,
        event: &Event,
    ) -> bool {
        let Some(message_id) = Self::pending_external_receive_message_id(event) else {
            return true;
        };
        let Some(pending) = self
            .peer_messaging
            .pending_external_receive_acks
            .get(message_id)
        else {
            return true;
        };
        let completion_live = match &pending.completion {
            PendingPeerReceiveCompletion::Remote { client_id, .. } => self
                .peer_messaging
                .external_message_peers
                .contains(client_id),
            PendingPeerReceiveCompletion::Local {
                conversation_id, ..
            } => self
                .agent_runtime
                .agent_registry
                .agents
                .contains_key(conversation_id),
        };
        let route_valid = match &pending.recipient {
            tau_proto::ExternalAgentMessageRecipient::Exact(agent_id) => {
                agent_id == &pending.recipient_id
            }
            tau_proto::ExternalAgentMessageRecipient::BareEntrypoint => {
                self.peer_entrypoint_recipient_is_eligible(&pending.recipient_id)
            }
        };
        let valid = pending.session_generation == self.session_runtime.current_session_generation
            && !pending.canceled
            && completion_live
            && route_valid
            && self.peer_auto_start_creation_committed(&pending.recipient_id)
            && matches!(
                event,
                Event::AgentMessageReceived(message)
                    if message == &pending.expected_receive
                        && self.agent_message_recipient_status(message.recipient_id.as_str())
                            == AgentMessageRecipientStatus::Live
            );
        if !valid {
            let fallback_failure =
                if pending.session_generation != self.session_runtime.current_session_generation {
                    tau_proto::ExternalAgentMessageFailure::TargetSessionChanged
                } else if matches!(
                    pending.recipient,
                    tau_proto::ExternalAgentMessageRecipient::BareEntrypoint
                ) {
                    if self.config.inter_session_receivers.is_empty() {
                        tau_proto::ExternalAgentMessageFailure::NoInterSessionReceiver
                    } else {
                        tau_proto::ExternalAgentMessageFailure::Rejected
                    }
                } else {
                    match self.agent_message_recipient_status(pending.recipient_id.as_str()) {
                        AgentMessageRecipientStatus::Stopped => {
                            tau_proto::ExternalAgentMessageFailure::RecipientStopped
                        }
                        AgentMessageRecipientStatus::RestoredUnavailable => {
                            tau_proto::ExternalAgentMessageFailure::RecipientRestoredUnavailable
                        }
                        AgentMessageRecipientStatus::Unknown => {
                            tau_proto::ExternalAgentMessageFailure::RecipientUnknown
                        }
                        AgentMessageRecipientStatus::Live => {
                            tau_proto::ExternalAgentMessageFailure::Rejected
                        }
                    }
                };
            let may_reselect = pending.session_generation
                == self.session_runtime.current_session_generation
                && !pending.canceled
                && completion_live
                && matches!(
                    pending.recipient,
                    tau_proto::ExternalAgentMessageRecipient::BareEntrypoint
                )
                && !pending.reselect_attempted;
            if may_reselect {
                match self.reselect_pending_external_receive(message_id, event) {
                    Ok(true) => return false,
                    Ok(false) => {}
                    Err(failure) => {
                        self.fail_pending_external_receive(
                            event,
                            "peer target changed before receive commit",
                            failure,
                        );
                        return valid;
                    }
                }
            }
            self.fail_pending_external_receive(
                event,
                "peer target changed before receive commit",
                fallback_failure,
            );
        }
        valid
    }

    /// Require the immutable creation role and reserved peer-purpose marker to
    /// be durable before the first receive can establish an auto-started
    /// endpoint.
    pub(super) fn peer_auto_start_creation_committed(
        &self,
        recipient_id: &tau_proto::AgentId,
    ) -> bool {
        if !self
            .peer_messaging
            .uncommitted_peer_auto_starts
            .contains(recipient_id)
        {
            return true;
        }
        let runtime_role = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(recipient_id.as_str())
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .and_then(|agent| agent.role.as_deref());
        let Some(runtime_role) = runtime_role else {
            return false;
        };
        self.session_runtime.agent_store
            .agent_events(recipient_id.as_str())
            .ok()
            .into_iter()
            .flatten()
            .any(|record| {
                matches!(
                    record.event,
                    Event::AgentStarted(started)
                        if started.role == runtime_role
                            && started.metadata.iter().any(|metadata| {
                                metadata.key.as_str()
                                    == crate::harness::subagents_tool::PEER_ENTRYPOINT_AGENT_METADATA_KEY
                                    && metadata.value == CborValue::Bool(true)
                                    && !metadata.inheritable
                            })
                )
            })
    }

    /// Rebind one parked bare receive after commit-time authority changed.
    ///
    /// The original projection is discarded and one replacement is published.
    /// A second invalidation is terminal, preventing an unbounded retry loop.
    pub(super) fn reselect_pending_external_receive(
        &mut self,
        message_id: &tau_proto::AgentMessageId,
        event: &Event,
    ) -> Result<bool, tau_proto::ExternalAgentMessageFailure> {
        let Some(mut pending) = self
            .peer_messaging
            .pending_external_receive_acks
            .remove(message_id)
        else {
            return Ok(false);
        };
        let old_recipient = pending.recipient_id.clone();
        let message_bytes = pending.expected_receive.message.len();
        self.release_peer_input_rate(&old_recipient, pending.rate_admitted_at);
        let (recipient_id, started, rate_admitted_at) =
            match self.resolve_peer_entrypoint_recipient(message_id, message_bytes) {
                Ok(replacement) => replacement,
                Err(error) => {
                    self.peer_messaging
                        .pending_external_receive_acks
                        .insert(message_id.clone(), pending);
                    return Err(error.failure());
                }
            };
        pending.recipient_id = recipient_id.clone();
        pending.expected_receive.recipient_id = recipient_id;
        pending.started = started;
        pending.reselect_attempted = true;
        pending.rate_admitted_at = rate_admitted_at;
        let replacement_event = Event::AgentMessageReceived(pending.expected_receive.clone());
        self.peer_messaging
            .pending_external_receive_acks
            .insert(message_id.clone(), pending);
        self.cleanup_uncommitted_peer_auto_start(&old_recipient);
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(matches!(event, Event::AgentMessageReceived(_)));
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            replacement_event,
        );
        Ok(true)
    }

    pub(super) fn fail_pending_external_receive(
        &mut self,
        event: &Event,
        error: &str,
        failure: tau_proto::ExternalAgentMessageFailure,
    ) {
        let Some(message_id) = Self::pending_external_receive_message_id(event) else {
            return;
        };
        let Some(pending) = self
            .peer_messaging
            .pending_external_receive_acks
            .remove(message_id)
        else {
            return;
        };
        self.release_peer_input_rate(&pending.recipient_id, pending.rate_admitted_at);
        self.cleanup_uncommitted_peer_auto_start(&pending.recipient_id);
        if pending.canceled
            || pending.session_generation != self.session_runtime.current_session_generation
        {
            return;
        }
        match pending.completion {
            PendingPeerReceiveCompletion::Remote {
                client_id,
                request_id,
            } => {
                let _ = self.runtime_io.bus.send_to(
                    &client_id,
                    None,
                    HarnessOutputMessage::ExternalAgentMessageResult(
                        tau_proto::ExternalAgentMessageResult {
                            request_id,
                            failure: Some(failure),
                            recipient_id: None,
                            started: false,
                        },
                    ),
                );
            }
            PendingPeerReceiveCompletion::Local {
                conversation_id,
                call_id,
                tool_name,
                tool_type,
                ..
            } => self.finish_harness_owned_tool_with_error(
                &conversation_id,
                call_id,
                tool_name,
                tool_type,
                error.to_owned(),
                None,
            ),
        }
    }

    pub(super) fn complete_pending_external_receive(&mut self, event: &Event) {
        let Some(message_id) = Self::pending_external_receive_message_id(event) else {
            return;
        };
        let Some(pending) = self
            .peer_messaging
            .pending_external_receive_acks
            .remove(message_id)
        else {
            return;
        };
        self.peer_messaging
            .uncommitted_peer_auto_starts
            .remove(&pending.recipient_id);
        self.record_peer_route(&pending.recipient_id);
        match pending.completion {
            PendingPeerReceiveCompletion::Remote {
                client_id,
                request_id,
            } => {
                let _ = self.runtime_io.bus.send_to(
                    &client_id,
                    None,
                    HarnessOutputMessage::ExternalAgentMessageResult(
                        tau_proto::ExternalAgentMessageResult {
                            request_id,
                            failure: None,
                            recipient_id: Some(pending.recipient_id),
                            started: pending.started,
                        },
                    ),
                );
            }
            PendingPeerReceiveCompletion::Local {
                conversation_id,
                call_id,
                tool_name,
                tool_type,
                sender_id,
                message,
            } => {
                self.publish_for_agent_from(
                    &conversation_id,
                    Some(crate::harness::harness_connection_id()),
                    Event::AgentMessageSent(tau_proto::AgentMessageSent {
                        message_id: message_id.clone(),
                        sender_id,
                        recipient: tau_proto::AgentMessageRecipient::Agent {
                            agent_id: pending.recipient_id.clone(),
                        },
                        kind: tau_proto::AgentMessageKind::Message,
                        message,
                    }),
                );
                self.finish_harness_owned_tool_with_cbor_result(
                    &conversation_id,
                    call_id,
                    tool_name,
                    tool_type,
                    tau_proto::CborValue::Map(vec![
                        (
                            tau_proto::CborValue::Text("status".to_owned()),
                            tau_proto::CborValue::Text(format!(
                                "Message committed: {message_id}; recipient was live; response not guaranteed"
                            )),
                        ),
                        (
                            tau_proto::CborValue::Text("message_id".to_owned()),
                            tau_proto::CborValue::Text(message_id.to_string()),
                        ),
                        (
                            tau_proto::CborValue::Text("recipient".to_owned()),
                            tau_proto::CborValue::Text(format!(
                                "{}/{}",
                                self.session_runtime.current_session_id, pending.recipient_id
                            )),
                        ),
                        (
                            tau_proto::CborValue::Text("started".to_owned()),
                            tau_proto::CborValue::Bool(pending.started),
                        ),
                    ]),
                    None,
                );
            }
        }
    }

    /// Remove a freshly auto-started endpoint when every precommit delivery
    /// that could establish it has failed. Coalesced deliveries keep the
    /// endpoint.
    pub(super) fn cleanup_uncommitted_peer_auto_start(
        &mut self,
        recipient_id: &tau_proto::AgentId,
    ) {
        if !self
            .peer_messaging
            .uncommitted_peer_auto_starts
            .contains(recipient_id)
            || self
                .peer_messaging
                .pending_external_receive_acks
                .values()
                .any(|pending| &pending.recipient_id == recipient_id && !pending.canceled)
        {
            return;
        }
        self.peer_messaging
            .uncommitted_peer_auto_starts
            .remove(recipient_id);
        if let Some(cid) = self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(recipient_id.as_str())
            .cloned()
        {
            self.remove_agent_expected(&cid);
        }
    }

    /// Derive renderer output and settle runtime state from one committed
    /// authoritative tool terminal.
    pub(super) fn react_to_committed_tool_terminal(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        event: &Event,
        append_outcome: Option<&tau_core::AgentAppendOutcome>,
    ) {
        let call_id = match event {
            Event::ProviderToolResult(result) => &result.call_id,
            Event::ProviderToolError(error) => &error.call_id,
            Event::ToolCancelled(cancelled) => &cancelled.call_id,
            Event::ToolBackgroundResult(result) => &result.call_id,
            Event::ToolBackgroundError(error) => &error.call_id,
            _ => return,
        };
        let runtime_only_cid = self.take_post_commit_runtime_only_tool_cid(call_id);
        if let Event::ProviderToolError(error) = event {
            let projection_cid = runtime_only_cid.clone().or_else(|| {
                self.tool_routing
                    .tool_runtime
                    .tool_agents
                    .get(call_id)
                    .or_else(|| {
                        self.tool_routing
                            .tool_runtime
                            .peer_internal_tool_agents
                            .get(call_id)
                    })
                    .cloned()
            });
            match projection_cid.as_ref() {
                Some(cid) => {
                    self.publish_for_agent_from(cid, source, Event::ToolError(error.clone()));
                }
                None => self.publish_event(source, Event::ToolError(error.clone())),
            }
        }
        if let Event::ProviderToolResult(result) = event {
            let projection_cid = self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(call_id)
                .or_else(|| {
                    self.tool_routing
                        .tool_runtime
                        .peer_internal_tool_agents
                        .get(call_id)
                })
                .cloned();
            self.publish_tool_result_projections(projection_cid.as_ref(), source, result);
        }
        match event {
            Event::ProviderToolResult(result)
                if result.kind == ToolResultKind::BackgroundPlaceholder =>
            {
                if !self
                    .tool_routing
                    .tool_runtime
                    .tool_agents
                    .contains_key(call_id)
                    && !self
                        .tool_routing
                        .tool_runtime
                        .peer_internal_tool_agents
                        .contains_key(call_id)
                {
                    return;
                }
                let newly_backgrounded = self
                    .tool_routing
                    .tool_runtime
                    .tool_turn
                    .mark_backgrounded(call_id);
                if !newly_backgrounded
                    && !self
                        .tool_routing
                        .tool_runtime
                        .tool_turn
                        .is_backgrounded(call_id)
                {
                    return;
                }
                self.record_wait_tool_result(result.clone(), None);
                if newly_backgrounded {
                    self.on_tool_call_foreground_complete(call_id.as_str());
                }
                return;
            }
            _ => {}
        }
        let Some(append_outcome) = append_outcome else {
            let disconnect_batch_pending = self
                .runtime_io
                .publication
                .disconnect_terminal_batch_pending
                .contains(call_id);
            let runtime_cid = runtime_only_cid.clone().or_else(|| {
                self.tool_routing
                    .tool_runtime
                    .tool_agents
                    .get(call_id)
                    .or_else(|| {
                        self.tool_routing
                            .tool_runtime
                            .peer_internal_tool_agents
                            .get(call_id)
                    })
                    .cloned()
            });
            match event {
                Event::ProviderToolResult(result) => {
                    self.record_wait_tool_result(result.clone(), None);
                    if let Some(cid) = self
                        .tool_routing
                        .tool_runtime
                        .tool_agents
                        .get(call_id)
                        .or_else(|| {
                            self.tool_routing
                                .tool_runtime
                                .peer_internal_tool_agents
                                .get(call_id)
                        })
                        .cloned()
                    {
                        self.reset_loop_guard_for_progress(&cid);
                    }
                    self.finish_non_durable_tool_tracking_after_terminal(call_id);
                }
                Event::ProviderToolError(error) => {
                    if let Some(cid) = runtime_only_cid.clone().or_else(|| {
                        self.tool_routing
                            .tool_runtime
                            .tool_agents
                            .get(call_id)
                            .or_else(|| {
                                self.tool_routing
                                    .tool_runtime
                                    .peer_internal_tool_agents
                                    .get(call_id)
                            })
                            .cloned()
                    }) {
                        self.record_tool_failure_loop_signature(&cid, error);
                    }
                    self.record_wait_tool_error(error.clone(), None);
                    if runtime_only_cid.is_none() {
                        if disconnect_batch_pending {
                            self.finish_non_durable_disconnect_tool_tracking(call_id);
                        } else {
                            self.finish_non_durable_tool_tracking_after_terminal(call_id);
                        }
                    }
                }
                Event::ToolCancelled(_) => {
                    self.record_wait_tool_cancelled(&HashSet::from([call_id.clone()]), None);
                    self.finish_harness_owned_tool_tracking(call_id);
                }
                _ => {}
            }
            self.release_disconnect_terminal_batch_after_commit(call_id, runtime_cid);
            return;
        };
        let Some(cid) = runtime_only_cid.clone().or_else(|| {
            self.tool_routing
                .tool_runtime
                .tool_agents
                .get(call_id)
                .or_else(|| {
                    self.tool_routing
                        .tool_runtime
                        .peer_internal_tool_agents
                        .get(call_id)
                })
                .cloned()
        }) else {
            return;
        };
        if let Event::ToolBackgroundResult(result) = event {
            let Some(mode) = self
                .tool_routing
                .tool_runtime
                .pending_background_completion_modes
                .remove(call_id)
            else {
                return;
            };
            self.finish_tool_call_runtime_state(call_id.as_str());
            self.publish_for_agent_from(
                &cid,
                source,
                Event::ToolBackgroundResultDisplay(tau_proto::ToolBackgroundResultDisplay::from(
                    result,
                )),
            );
            self.record_wait_background_result(result.clone(), Some(append_outcome.observation_id));
            self.finish_committed_background_completion(&cid, call_id, mode);
            return;
        }
        if let Event::ToolBackgroundError(error) = event {
            let Some(mode) = self
                .tool_routing
                .tool_runtime
                .pending_background_completion_modes
                .remove(call_id)
            else {
                return;
            };
            self.finish_tool_call_runtime_state(call_id.as_str());
            self.record_wait_background_error(error.clone(), Some(append_outcome.observation_id));
            self.finish_committed_background_completion(&cid, call_id, mode);
            return;
        }
        self.tool_routing
            .tool_runtime
            .pending_cancellation_observations
            .remove(call_id);
        if let Some(settlement) = self
            .tool_routing
            .tool_runtime
            .pending_wait_settlements
            .remove(call_id)
        {
            self.append_best_effort_observation(
                &cid,
                tau_proto::ObservationId::random(),
                Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                    wait_observation: settlement.wait_observation,
                    wait_call: settlement.wait_call,
                    registration: settlement.registration,
                    wait_terminal: append_outcome.observation_id,
                    outcome: settlement.outcome,
                }),
            );
        }
        match event {
            Event::ProviderToolResult(result) => {
                self.reset_loop_guard_for_progress(&cid);
                self.record_wait_tool_result(result.clone(), Some(append_outcome.observation_id));
            }
            Event::ProviderToolError(error) => {
                self.record_tool_failure_loop_signature(&cid, error);
                self.record_wait_tool_error(error.clone(), Some(append_outcome.observation_id));
            }
            Event::ToolCancelled(_) => {
                self.record_wait_tool_cancelled(
                    &HashSet::from([call_id.clone()]),
                    Some((call_id, append_outcome.observation_id)),
                );
            }
            _ => unreachable!("terminal variants handled above"),
        }

        if runtime_only_cid.is_some() {
            return;
        }

        if self
            .runtime_io
            .publication
            .disconnect_terminal_batch_pending
            .contains(call_id)
        {
            self.finish_tool_call_runtime_state(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
            self.release_disconnect_terminal_batch_after_commit(call_id, Some(cid));
            return;
        }

        let deferred_teardown = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .is_some_and(|agent| agent.terminating || agent.pending_cancel.is_some());
        if deferred_teardown {
            self.finish_tool_call_runtime_state(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
            let foreground_remains =
                self.tool_routing
                    .tool_runtime
                    .tool_agents
                    .iter()
                    .any(|(pending, owner)| {
                        owner == &cid
                            && !self
                                .tool_routing
                                .tool_runtime
                                .tool_turn
                                .is_backgrounded(pending)
                    });
            if !foreground_remains {
                if self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .is_some_and(|agent| agent.terminating)
                {
                    self.finish_cancel_delegate_side_conversation(&cid);
                } else {
                    self.finalize_cancelled_tool_turn(&cid);
                }
            }
        } else {
            if self
                .tool_routing
                .tool_runtime
                .peer_internal_tool_agents
                .contains_key(call_id)
            {
                self.finish_harness_owned_tool_tracking(call_id);
            } else {
                self.on_tool_call_complete(call_id.as_str());
                self.clear_tool_call_tracking(call_id.as_str());
                self.repair_closed_foreground_tool_turn(&cid, call_id);
            }
        }
    }

    /// Release scheduler advancement after one disconnect-synthesized canonical
    /// foreground terminal commits.
    pub(super) fn release_disconnect_terminal_batch_after_commit(
        &mut self,
        call_id: &ToolCallId,
        cid: Option<AgentId>,
    ) {
        if !self
            .runtime_io
            .publication
            .disconnect_terminal_batch_pending
            .remove(call_id)
        {
            return;
        }
        if let Some(cid) = cid {
            self.runtime_io
                .publication
                .disconnect_terminal_batch_completed
                .push((call_id.clone(), cid));
        }
        if !self
            .runtime_io
            .publication
            .disconnect_terminal_batch_pending
            .is_empty()
        {
            return;
        }
        let completed = std::mem::take(
            &mut self
                .runtime_io
                .publication
                .disconnect_terminal_batch_completed,
        );
        self.drain_pending_tool_invocations_or_report();
        for (completed_call_id, completed_cid) in completed {
            self.maybe_complete_agent_turn_for(&completed_cid, completed_call_id.as_str());
            self.repair_closed_foreground_tool_turn(&completed_cid, &completed_call_id);
        }
        self.drain_publish_idle_dispatches();
        self.try_advance_queue();
    }

    /// Clear one non-durable disconnect terminal without draining scheduler
    /// work before the complete disconnect batch commits.
    pub(super) fn finish_non_durable_disconnect_tool_tracking(&mut self, call_id: &ToolCallId) {
        if let Some(cid) = self
            .tool_routing
            .tool_runtime
            .peer_internal_tool_agents
            .get(call_id)
            .cloned()
        {
            self.tool_routing
                .tool_runtime
                .tool_turn
                .mark_complete(call_id);
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.tools_in_flight = agent.tools_in_flight.saturating_sub(1);
            }
            self.emit_agent_stats_updated(&cid);
        } else {
            self.finish_tool_call_runtime_state(call_id.as_str());
        }
        self.clear_tool_call_tracking(call_id.as_str());
    }

    /// Settle and retain attribution for a runtime-only terminal mode before
    /// deriving its transient projection from the committed canonical event.
    pub(super) fn take_post_commit_runtime_only_tool_cid(
        &mut self,
        call_id: &ToolCallId,
    ) -> Option<AgentId> {
        if !self
            .tool_routing
            .tool_runtime
            .post_commit_runtime_only_tool_terminals
            .remove(call_id)
        {
            return None;
        }
        let cid = self
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(call_id)
            .or_else(|| {
                self.tool_routing
                    .tool_runtime
                    .peer_internal_tool_agents
                    .get(call_id)
            })
            .cloned()?;
        self.finish_tool_call_runtime_state(call_id.as_str());
        self.clear_tool_call_tracking(call_id.as_str());
        Some(cid)
    }

    /// Settle one non-journal terminal after its canonical event commits.
    pub(super) fn finish_non_durable_tool_tracking_after_terminal(&mut self, call_id: &ToolCallId) {
        if self
            .tool_routing
            .tool_runtime
            .post_commit_runtime_only_tool_terminals
            .remove(call_id)
        {
            self.finish_tool_call_runtime_state(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
        } else {
            self.finish_harness_owned_tool_tracking(call_id);
        }
    }

    /// Publish raw non-UI and payload-free UI views of one committed provider
    /// result.
    pub(super) fn publish_tool_result_projections(
        &mut self,
        cid: Option<&AgentId>,
        source: Option<&tau_proto::ConnectionId>,
        result: &ToolResult,
    ) {
        let mut generic_result = result.clone();
        generic_result.provider_content.clear();
        let generic = Event::ToolResult(generic_result);
        let display = Event::ToolResultDisplay(tau_proto::ToolResultDisplay::from(result));
        match cid {
            Some(cid) => {
                self.publish_for_agent_from(cid, source, generic);
                self.publish_for_agent_from(cid, source, display);
            }
            None => {
                self.publish_event(source, generic);
                self.publish_event(source, display);
            }
        }
    }

    /// Run post-commit reactions after semantic persistence and observer
    /// delivery. Agent dispatch therefore sees the just-folded semantic state.
    pub(super) fn react_to_committed_event(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        event: &Event,
        persist: bool,
        append_outcome: Option<&tau_core::AgentAppendOutcome>,
    ) {
        self.react_to_committed_tool_terminal(source, event, append_outcome);
        if let Event::ProviderResponseFinished(response) = event
            && let Some(decision) = &response.automatic_compaction_decision
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(response.agent_id.as_str()))
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
        {
            agent.pending_automatic_compaction_decision = Some(decision.transaction_id.clone());
        }
        if let Event::AgentPromptTerminated(terminated) = event
            && let Some(decision) = &terminated.automatic_compaction_decision
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(terminated.agent_id.as_str()))
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
        {
            agent.pending_automatic_compaction_decision = Some(decision.transaction_id.clone());
        }
        if let Event::AgentOuterTurnFinished(finished) = event
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(finished.agent_id.as_str()))
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            && agent.outer_turn.owned_id() == Some(&finished.outer_turn_id)
        {
            agent.outer_turn = path_crate_agent::OuterTurnRuntimeState::None;
            agent.pending_automatic_compaction_decision = None;
            if agent.output_length_continuation.outer_turn_id() == Some(&finished.outer_turn_id) {
                agent.output_length_continuation =
                    path_crate_agent::OutputLengthContinuationState::None;
            }
        }
        if let Event::AgentOuterTurnFinished(finished) = event
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(finished.agent_id.as_str()))
        {
            self.queue_outer_turn_finished_context_size_alerts(&cid, &finished.outer_turn_id);
            let eager = self
                .session_runtime
                .agent_store
                .agent(finished.agent_id.as_str())
                .and_then(tau_core::AgentTree::standalone_compaction_recovery);
            if let Some(tau_core::StandaloneCompactionRecovery::AwaitingAutomaticStart {
                decision,
                cut,
                finish_committed: true,
            }) = eager
            {
                self.start_eager_automatic_compaction(&cid, decision, cut);
            }
        }
        if let Event::AgentStandaloneCompactionStarted(started) = event {
            if let tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { decision_id } =
                &started.trigger
                && let Some(cid) =
                    self.runtime_agent_id_for_target_agent(Some(started.agent_id.as_str()))
                && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
                && agent.pending_automatic_compaction_start.as_ref() == Some(decision_id)
            {
                agent.pending_automatic_compaction_start = None;
            }
            if let tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                request_id,
                caller_agent_id,
                initiating_tool_call_id,
            } = &started.trigger
            {
                let accepted = self
                    .prompt_coordination
                    .compaction_runtime
                    .accepted_manual_tools
                    .remove(request_id);
                self.prompt_coordination
                    .compaction_runtime
                    .pending_manual_tools
                    .entry(started.transaction_id.clone())
                    .or_insert_with(|| PendingManualCompactionTool {
                        request_id: request_id.clone(),
                        caller_agent_id: caller_agent_id.clone(),
                        call_id: initiating_tool_call_id.clone(),
                        tool_name: accepted.as_ref().map_or_else(
                            || ToolName::new("compact"),
                            |entry| entry.visible_tool_name.clone(),
                        ),
                        target_agent_id: started.agent_id.clone(),
                    });
            }
            let suppression_key = (started.agent_id.clone(), started.transaction_id.clone());
            let suppressed = self
                .prompt_coordination
                .compaction_runtime
                .suppressed_dispatches
                .remove(&suppression_key);
            let cancelled = suppressed
                && self
                    .prompt_coordination
                    .compaction_runtime
                    .cancelled_claims
                    .remove(&suppression_key);
            let cid = self.runtime_agent_id_for_target_agent(Some(started.agent_id.as_str()));
            if let Some(cid) = cid {
                if suppressed {
                    if cancelled {
                        self.publish_event_for_agent_with_completion(
                            &cid,
                            None,
                            Event::AgentStandaloneCompactionFailed(
                                tau_proto::AgentStandaloneCompactionFailed {
                                    agent_id: started.agent_id.clone(),
                                    transaction_id: started.transaction_id.clone(),
                                    cut: started.cut,
                                    reason: tau_proto::StandaloneCompactionFailureReason::Cancelled,
                                    resume_through: started.resume_through,
                                },
                            ),
                            Some(AgentPublishCompletion::ReactiveContextRecoveryFailure {
                                batch_parent: append_outcome
                                    .and_then(|outcome| outcome.folded_node_id)
                                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                                retry_event: None,
                            }),
                            false,
                        );
                    }
                    return;
                }
                let reactive_off_branch = matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                ) && started.resume_through.is_some_and(|through| {
                    self.session_runtime
                        .agent_store
                        .agent(started.agent_id.as_str())
                        .is_some_and(|tree| {
                            !tree.is_ancestor_head(
                                through,
                                self.selected_head_for_agent(&cid)
                                    .unwrap_or(tau_proto::AgentHead::Root),
                            )
                        })
                });
                if reactive_off_branch {
                    self.publish_for_agent(
                        &cid,
                        Event::AgentStandaloneCompactionFailed(
                            tau_proto::AgentStandaloneCompactionFailed {
                                agent_id: started.agent_id.clone(),
                                transaction_id: started.transaction_id.clone(),
                                cut: started.cut,
                                reason: tau_proto::StandaloneCompactionFailureReason::StaleBranch,
                                resume_through: started.resume_through,
                            },
                        ),
                    );
                    return;
                }
                if let Some(resume_through) = started.resume_through {
                    self.acknowledge_deferred_activations_through(&cid, resume_through);
                }
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                    agent.activation_dispatch =
                        path_crate_agent::ActivationDispatchState::Running {
                            id: started.transaction_id.clone(),
                            cut: started.cut,
                            resume_through: started.resume_through,
                            model: started.model.clone(),
                            branch_generation: agent.branch_generation,
                            compact_prompt_id: started.compact_prompt_id.clone(),
                        };
                    agent.in_flight_prompt = Some(started.compact_prompt_id.clone());
                }
                self.set_agent_turn_state(
                    &cid,
                    AgentTurnState::AgentThinking {
                        agent_prompt_id: started.compact_prompt_id.clone(),
                    },
                );
                self.dispatch_prompt_after_publish_idle(&cid);
                if let tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                    failed_agent_prompt_id,
                } = &started.trigger
                {
                    let attempt = self
                        .session_runtime
                        .agent_store
                        .agent(started.agent_id.as_str())
                        .and_then(|tree| tree.provider_attempt_for_prompt(failed_agent_prompt_id))
                        .map(tau_proto::ProviderAttempt::get)
                        .unwrap_or(1);
                    self.project_agent_watch_provider_state(
                        &cid,
                        failed_agent_prompt_id.clone(),
                        tau_proto::AgentWatchProviderState::RecoveringContext { attempt },
                    );
                }
                if matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::ManualAgentTool { .. }
                ) {
                    self.project_agent_watch_provider_state(
                        &cid,
                        started.compact_prompt_id.clone(),
                        tau_proto::AgentWatchProviderState::RecoveringContext { attempt: 1 },
                    );
                }
            }
        }
        if let Event::AgentStandaloneCompactionFailed(failed) = event
            && failed.reason == tau_proto::StandaloneCompactionFailureReason::StaleBranch
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(failed.agent_id.as_str()))
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            && agent.pending_automatic_compaction_start.as_ref() == Some(&failed.transaction_id)
        {
            agent.pending_automatic_compaction_start = None;
        }
        if let Event::AgentManualCompactionRequestFailed(failed) = event
            && let Some(pending) = self
                .prompt_coordination
                .compaction_runtime
                .accepted_manual_tools
                .remove(&failed.request_id)
        {
            if pending.request.resume_inference {
                let call_id = pending.request.initiating_tool_call_id.clone();
                let prompt =
                    self_compaction_terminal_pending_prompt(tau_proto::SelfCompactionTerminal {
                        request_id: pending.request.request_id.clone(),
                        tool_call_id: call_id.clone(),
                        transaction_id: None,
                        outcome: tau_proto::SelfCompactionTerminalOutcome::RequestFailed {
                            reason: failed.reason,
                        },
                    });
                self.finish_prebuilt_internal_tool_error_with_mode(
                    ToolError {
                        presentation: Default::default(),
                        call_id: call_id.clone(),
                        tool_name: pending.visible_tool_name,
                        tool_type: tau_proto::ToolType::Function,
                        message: manual_request_failure_message(failed.reason).to_owned(),
                        details: None,
                        display: None,
                        originator: PromptOriginator::User,
                    },
                    BackgroundCompletionPromptMode::DoNotQueue,
                );
                self.consume_wait_background_completion(&call_id);
                if let Some(cid) = self.runtime_agent_id_for_target_agent(Some(
                    pending.request.caller_agent_id.as_str(),
                )) && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
                {
                    agent.pending_prompts.push_back(prompt);
                }
            } else {
                self.finish_manual_compaction_tool_with_error(
                    pending.request.initiating_tool_call_id,
                    pending.visible_tool_name,
                    manual_request_failure_message(failed.reason),
                    false,
                );
            }
        }
        if let Event::AgentStandaloneCompactionFailed(failed) = event {
            if let Some(pending) = self
                .prompt_coordination
                .compaction_runtime
                .pending_manual_tools
                .remove(&failed.transaction_id)
            {
                let self_request = pending.caller_agent_id == pending.target_agent_id;
                if self_request {
                    let prompt = self_compaction_terminal_pending_prompt(
                        tau_proto::SelfCompactionTerminal {
                            request_id: pending.request_id.clone(),
                            tool_call_id: pending.call_id.clone(),
                            transaction_id: Some(failed.transaction_id.clone()),
                            outcome: tau_proto::SelfCompactionTerminalOutcome::Failed {
                                reason: failed.reason,
                            },
                        },
                    );
                    let call_id = pending.call_id.clone();
                    self.finish_prebuilt_internal_tool_error_with_mode(
                        ToolError {
                            presentation: Default::default(),
                            call_id: pending.call_id,
                            tool_name: pending.tool_name,
                            tool_type: tau_proto::ToolType::Function,
                            message: standalone_compaction_failure_message(failed.reason)
                                .to_owned(),
                            details: None,
                            display: None,
                            originator: PromptOriginator::User,
                        },
                        BackgroundCompletionPromptMode::DoNotQueue,
                    );
                    self.consume_wait_background_completion(&call_id);
                    if let Some(cid) = self
                        .runtime_agent_id_for_target_agent(Some(pending.caller_agent_id.as_str()))
                        && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
                    {
                        agent.pending_prompts.push_back(prompt);
                    }
                } else {
                    self.finish_manual_compaction_tool_with_error(
                        pending.call_id,
                        pending.tool_name,
                        standalone_compaction_failure_message(failed.reason),
                        false,
                    );
                }
            }
            let key = (failed.agent_id.clone(), failed.transaction_id.clone());
            self.prompt_coordination
                .compaction_runtime
                .suppressed_dispatches
                .remove(&key);
            self.prompt_coordination
                .compaction_runtime
                .cancelled_claims
                .remove(&key);
        }
        if let Event::AgentStandaloneCompactionFailed(failed) = event
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(failed.agent_id.as_str()))
        {
            let failed_prompt_id =
                self.agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|agent| {
                        agent.in_flight_prompt.clone().or_else(|| {
                            match &agent.activation_dispatch {
                        path_crate_agent::ActivationDispatchState::ContextRecoveryClaimPending {
                            checkpoint,
                            ..
                        } => Some(checkpoint.agent_prompt_id.clone()),
                        _ => None,
                    }
                        })
                    });
            let suppress_provider_watch = failed_prompt_id.as_ref().is_some_and(|prompt_id| {
                self.prompt_coordination
                    .compaction_runtime
                    .silent_failure_prompts
                    .remove(prompt_id)
            });
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.activation_dispatch = path_crate_agent::ActivationDispatchState::Blocked {
                    failed_id: failed.transaction_id.clone(),
                    cut: failed.cut,
                    resume_through: failed.resume_through,
                };
                agent.in_flight_prompt = None;
                if failed.reason == tau_proto::StandaloneCompactionFailureReason::Cancelled {
                    agent.pending_cancel = None;
                }
            }
            if !suppress_provider_watch && let Some(failed_prompt_id) = failed_prompt_id {
                self.project_agent_watch_provider_state(
                    &cid,
                    failed_prompt_id,
                    tau_proto::AgentWatchProviderState::Blocked {
                        category: tau_proto::AgentWatchProviderCategory::Compaction,
                    },
                );
            }
            if failed.reason != tau_proto::StandaloneCompactionFailureReason::Cancelled
                && self.complete_failed_compaction_side_conversation(&cid, source)
            {
                return;
            }
            if suppress_provider_watch {
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                    agent.turn_state = AgentTurnState::Idle;
                    agent.published_runtime_state = tau_proto::AgentRuntimeState::Idle;
                }
            } else {
                self.set_agent_turn_state(&cid, AgentTurnState::Idle);
            }
            let has_terminal_continuation = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| {
                    agent
                        .pending_prompts
                        .iter()
                        .any(PendingPrompt::is_self_compaction_terminal)
                });
            if has_terminal_continuation {
                self.fold_pending_prompts_as_steered(&cid);
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                    agent.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
                }
                self.dispatch_prompt_after_publish_idle(&cid);
            }
            self.try_advance_queue();
        }
        if let Event::AgentCompacted(compacted) = event
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(compacted.agent_id.as_str()))
        {
            self.clear_agent_context_usage(&cid);
        }
        if let Event::AgentCompacted(compacted) = event
            && let Some(transaction_id) = compacted.transaction_id.as_ref()
            && let Some(pending) = self
                .prompt_coordination
                .compaction_runtime
                .pending_manual_tools
                .remove(transaction_id)
        {
            let self_request = pending.caller_agent_id == pending.target_agent_id;
            let call_id = pending.call_id.clone();
            let direct_prompt = self_request.then(|| {
                self_compaction_terminal_pending_prompt(tau_proto::SelfCompactionTerminal {
                    request_id: pending.request_id.clone(),
                    tool_call_id: pending.call_id.clone(),
                    transaction_id: Some(transaction_id.clone()),
                    outcome: tau_proto::SelfCompactionTerminalOutcome::Compacted,
                })
            });
            self.finish_prebuilt_internal_tool_result_with_mode(
                ToolResult {
                    presentation: Default::default(),
                    call_id: pending.call_id,
                    tool_name: pending.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    result: tau_proto::CborValue::Map(vec![
                        (
                            tau_proto::CborValue::Text("request_id".into()),
                            tau_proto::CborValue::Text(pending.request_id.to_string()),
                        ),
                        (
                            tau_proto::CborValue::Text("status".into()),
                            tau_proto::CborValue::Text("compacted".into()),
                        ),
                        (
                            tau_proto::CborValue::Text("target_agent_id".into()),
                            tau_proto::CborValue::Text(pending.target_agent_id.to_string()),
                        ),
                        (
                            tau_proto::CborValue::Text("transaction_id".into()),
                            tau_proto::CborValue::Text(transaction_id.to_string()),
                        ),
                    ]),
                    provider_content: Vec::new(),
                    kind: ToolResultKind::Final,
                    display: None,
                    originator: PromptOriginator::User,
                },
                if self_request {
                    BackgroundCompletionPromptMode::DoNotQueue
                } else {
                    BackgroundCompletionPromptMode::QueueAndAdvance
                },
            );
            if let Some(prompt) = direct_prompt {
                self.consume_wait_background_completion(&call_id);
                if let Some(cid) =
                    self.runtime_agent_id_for_target_agent(Some(pending.caller_agent_id.as_str()))
                    && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
                {
                    agent.pending_prompts.push_back(prompt);
                }
            }
        }
        if let Event::AgentCompacted(compacted) = event
            && let Some(transaction_id) = compacted.transaction_id.as_ref()
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(compacted.agent_id.as_str()))
        {
            let resume = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .and_then(|agent| match &agent.activation_dispatch {
                    path_crate_agent::ActivationDispatchState::Running {
                        id,
                        cut,
                        resume_through,
                        ..
                    } if id == transaction_id && Some(*cut) == compacted.cut => {
                        Some(*resume_through)
                    }
                    _ => None,
                });
            let Some(resume) = resume else {
                self.emit_info(
                    "ignoring compaction boundary that does not own the runtime transaction",
                );
                return;
            };
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.in_flight_prompt = None;
            }
            if resume.is_some() {
                let completion = AgentPublishCompletion::StandaloneContinuation {
                    transaction_id: transaction_id.clone(),
                    model: compacted.model.clone().expect("qualified compaction model"),
                    activation_cut: compacted.cut.unwrap_or_else(|| {
                        self.agent_runtime
                            .agent_registry
                            .agents
                            .get(&cid)
                            .and_then(|agent| agent.head)
                            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
                    }),
                    batch_parent: self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(&cid)
                        .and_then(|agent| agent.head)
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                    source: source.cloned(),
                    retry_prompts: Vec::new(),
                    complete_on_commit: true,
                    approved_retry_event: None,
                };
                if !self
                    .fold_pending_prompts_as_steered_with_completion(&cid, Some(completion.clone()))
                {
                    let through = self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(&cid)
                        .and_then(|agent| agent.head)
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                    self.complete_agent_publish(&cid, completion, through);
                }
            } else {
                self.set_agent_turn_state(&cid, AgentTurnState::Idle);
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                    agent.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
                }
                self.try_advance_queue();
            }
        }
        if let Event::AgentInferenceDispatchStarted(started) = event
            && let Some(transaction_id) = started.transaction_id.as_ref()
        {
            self.prompt_coordination
                .compaction_runtime
                .enqueued_inference_checkpoints
                .remove(&(started.agent_id.clone(), transaction_id.clone()));
        }
        if let Event::AgentInferenceDispatchStarted(started) = event
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(started.agent_id.as_str()))
        {
            let checkpoint_matches = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| {
                    matches!(
                        &agent.activation_dispatch,
                        crate::agent::ActivationDispatchState::AwaitingCheckpoint {
                            owner,
                            agent_prompt_id,
                            through,
                            dispatch,
                        } if owner.transaction_id() == started.transaction_id.as_ref()
                            && agent_prompt_id == &started.agent_prompt_id
                            && through == &started.through
                            && started.model.as_ref() == Some(&dispatch.model)
                            && started.operation == Some(dispatch.operation)
                            && started.activation_cut == Some(dispatch.activation_cut)
                    )
                });
            if checkpoint_matches {
                self.acknowledge_deferred_activations_through(&cid, started.through);
                self.acknowledge_message_wakes_through(&cid, started.through);
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                    let owner = match &agent.activation_dispatch {
                        path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                            owner,
                            ..
                        } => owner.clone(),
                        _ => unreachable!("matched awaiting checkpoint"),
                    };
                    tracing::debug!(
                        target: "tau_harness",
                        transaction_id = ?owner.transaction_id(),
                        agent_prompt_id = %started.agent_prompt_id,
                        "inference checkpoint committed"
                    );
                    agent.activation_dispatch =
                        path_crate_agent::ActivationDispatchState::DispatchUncertain {
                            owner,
                            agent_prompt_id: started.agent_prompt_id.clone(),
                            through: started.through,
                            model: started.model.clone(),
                            operation: started.operation,
                            activation_cut: started.activation_cut,
                        };
                    if matches!(
                        &agent.output_length_continuation,
                        path_crate_agent::OutputLengthContinuationState::OwnerPending(continuation)
                            if continuation.plan.agent_prompt_id == started.agent_prompt_id
                    ) {
                        let path_crate_agent::OutputLengthContinuationState::OwnerPending(
                            continuation,
                        ) = std::mem::take(&mut agent.output_length_continuation)
                        else {
                            unreachable!("matched owner-pending continuation");
                        };
                        agent.output_length_continuation =
                            path_crate_agent::OutputLengthContinuationState::Active(continuation);
                    }
                }
                if self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .is_some_and(|agent| agent.pending_cancel.is_some())
                {
                    self.finalize_canceled_in_flight_prompt(&cid);
                    return;
                }
                let _ = self.send_prompt_to_agent_for(&cid);
            }
        }
        if let Event::ProviderResponseFinished(response) = event
            && let tau_proto::OutputLengthDisposition::ContinuationTerminal {
                outer_turn_id, ..
            } = &response.output_length_disposition
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(response.agent_id.as_str()))
            && let Some(lineage_owner) = self
                .session_runtime
                .agent_store
                .agent(response.agent_id.as_str())
                .and_then(|tree| {
                    tree.output_length_lineage_owner_for_prompt(&response.agent_prompt_id)
                })
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            && matches!(
                &agent.output_length_continuation,
                path_crate_agent::OutputLengthContinuationState::Active(continuation)
                    if continuation.plan.owner == lineage_owner
            )
        {
            agent.output_length_continuation =
                path_crate_agent::OutputLengthContinuationState::Spent {
                    outer_turn_id: outer_turn_id.clone(),
                };
            agent.pending_cancel = None;
            if matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationTerminal {
                    outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                    ..
                }
            ) {
                agent.pending_prompts.clear();
                agent.pending_replay_activation = false;
            }
        }
        if let Event::ProviderResponseFinished(response) = event
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(response.agent_id.as_str()))
            && self
                .session_runtime
                .agent_store
                .agent(response.agent_id.as_str())
                .is_some_and(|tree| {
                    tree.output_length_response_rearms_budget(&response.agent_prompt_id)
                })
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
            && agent.output_length_continuation.outer_turn_id() == agent.outer_turn.owned_id()
        {
            agent.output_length_continuation =
                path_crate_agent::OutputLengthContinuationState::None;
        }
        if let Event::ProviderResponseFinished(response) = event
            && matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationTerminal {
                    outcome: tau_proto::OutputLengthContinuationOutcome::Failed
                        | tau_proto::OutputLengthContinuationOutcome::Cancelled
                        | tau_proto::OutputLengthContinuationOutcome::Incomplete,
                    ..
                }
            )
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(response.agent_id.as_str()))
        {
            self.invalidate_working_status_after_unsuccessful_terminal(&cid);
        }
        if let Event::ProviderResponseFinished(response) = event
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(response.agent_id.as_str()))
            && self.agent_runtime.agent_registry.agents.get(&cid).is_some_and(|agent| {
                matches!(
                    &agent.activation_dispatch,
                    crate::agent::ActivationDispatchState::DispatchUncertain { agent_prompt_id, .. }
                        if agent_prompt_id == &response.agent_prompt_id
                )
            })
        {
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
            }
            let local_route_failure = self
                .prompt_coordination.prompt_runtime
                .local_route_failures
                .remove(&response.agent_prompt_id);
            if local_route_failure {
                self.project_agent_watch_provider_state(
                    &cid,
                    response.agent_prompt_id.clone(),
                    tau_proto::AgentWatchProviderState::TerminalError {
                        failure_kind: tau_proto::ProviderFailureKind::Unknown,
                        attempt: 1,
                    },
                );
                let mut normalized_tool_calls = NormalizedFinishedToolCalls::default();
                let is_non_tool_ext_query = self.is_non_tool_extension_query(&cid);
                if self.handle_finished_response_side_conversation(
                    &cid,
                    FinishedSideConversation {
                        response,
                        requested_tool_calls: false,
                        is_non_tool_ext_query,
                        assistant_text: None,
                        tool_call_count: 0,
                    },
                    &mut normalized_tool_calls,
                    None,
                ) {
                    return;
                }
                self.set_agent_turn_state(&cid, AgentTurnState::Idle);
                self.try_advance_queue();
            }
        }
        if let Event::AgentPromptTerminated(terminated) = event
            && append_outcome.is_some()
            && let Some(cid) = self
                .runtime_agent_id_for_target_agent(Some(terminated.agent_id.as_str()))
                .or_else(|| {
                    self.agent_runtime
                        .agent_registry
                        .agents
                        .iter()
                        .find_map(|(cid, agent)| {
                            (agent.agent_id.as_deref() == Some(terminated.agent_id.as_str()))
                                .then(|| cid.clone())
                        })
                })
        {
            let finish_unload = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| agent.terminating);
            self.prompt_coordination
                .prompt_runtime
                .operations
                .remove(&terminated.agent_prompt_id);
            self.prompt_coordination
                .prompt_runtime
                .context_limits
                .remove(&terminated.agent_prompt_id);
            self.prompt_coordination
                .prompt_runtime
                .context_size_alerts
                .remove(&terminated.agent_prompt_id);
            self.prompt_coordination
                .prompt_runtime
                .compaction_policies
                .remove(&terminated.agent_prompt_id);
            self.prompt_coordination
                .prompt_runtime
                .compaction_projected_tokens
                .remove(&terminated.agent_prompt_id);
            self.prompt_coordination
                .prompt_runtime
                .semantic_output
                .remove(&terminated.agent_prompt_id);
            if terminated.reason == AgentPromptTerminationReason::Canceled {
                self.prompt_coordination
                    .canceled_prompts
                    .insert(terminated.agent_prompt_id.clone());
                self.fail_pending_initial_prompts(
                    &cid,
                    tau_proto::AgentPromptFailureStage::Canceled,
                    "initial prompt was canceled",
                );
            }
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
                agent.in_flight_prompt = None;
                if agent.last_prompt_id.as_ref() == Some(&terminated.agent_prompt_id) {
                    agent.last_prompt_id = None;
                }
                agent.pending_cancel = None;
                agent.work_status.clear_working_reminder();
                if terminated.reason == AgentPromptTerminationReason::Canceled {
                    agent.pending_prompts.clear();
                }
            }
            self.set_agent_turn_state(&cid, AgentTurnState::Idle);
            self.resolve_materialized_message_wakes(&cid);
            self.cancel_pending_context_claim(&cid);
            self.release_start_agent_request(&cid);
            self.remember_ephemeral_provider_prompt(&terminated.agent_prompt_id);
            if let Some(pending) = self
                .prompt_coordination
                .prompt_runtime
                .pending_stale_provider_responses
                .remove(&terminated.agent_prompt_id)
            {
                debug_assert_eq!(pending.response.agent_prompt_id, terminated.agent_prompt_id);
            }
            self.discard_finished_response_prompt_tracking(&terminated.agent_prompt_id);
            if finish_unload {
                self.remove_agent_after_prompt_closure(&cid);
                return;
            }
            self.try_advance_queue();
        }
        if matches!(
            event,
            Event::ProviderResponseFinished(_)
                | Event::ProviderToolResult(_)
                | Event::ProviderToolError(_)
                | Event::ToolCancelled(_)
        ) && let Some(agent_id) = self.agent_id_for_event(event)
            && let Some(cid) = self.runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
        {
            self.resolve_materialized_message_wakes(&cid);
            self.try_advance_queue();
        }
        if let Event::SessionAgentUnloaded(unloaded) = event
            && unloaded.session_id == self.session_runtime.current_session_id
        {
            let reason = self
                .agent_runtime
                .agent_watch
                .pending_unload_reasons
                .remove(unloaded.agent_id.as_str())
                .or_else(|| {
                    (!self
                        .agent_runtime
                        .agent_watch
                        .expected_unloads
                        .remove(unloaded.agent_id.as_str()))
                    .then_some(tau_proto::AgentWatchLifecycleReason::UnexpectedUnload)
                });
            self.retire_agent_watch_endpoint(unloaded.agent_id.as_str(), reason);
            self.agent_runtime
                .agent_registry
                .navigation_modes
                .remove(&unloaded.agent_id);
        }
        if let Event::SessionAgentUnloaded(unloaded) = event
            && let Some(cid) = self
                .agent_runtime
                .agent_registry
                .agents
                .iter()
                .find(|(_, agent)| {
                    agent.agent_id.as_deref() == Some(unloaded.agent_id.as_str())
                        && agent.session_id == unloaded.session_id
                })
                .map(|(cid, _)| cid.clone())
        {
            self.agent_runtime
                .agent_registry
                .session_loaded
                .remove(&unloaded.agent_id);
            self.prompt_coordination
                .context_discovery
                .pending_agents
                .remove(&unloaded.agent_id);
            self.prompt_coordination
                .context_discovery
                .frozen_agents
                .remove(&unloaded.agent_id);
            self.prompt_coordination
                .context_discovery
                .initialized_agent_context
                .remove(&unloaded.agent_id);
            self.prompt_coordination
                .prompt_runtime
                .shown_tool_failure_examples
                .retain(|(agent_id, _, _)| agent_id != &cid);
            self.agent_runtime
                .agent_registry
                .agent_routes
                .remove(unloaded.agent_id.as_str());
            self.agent_runtime
                .agent_registry
                .stopped_ids
                .insert(unloaded.agent_id.to_string());
            self.discard_input_wait_for(&cid);
            self.prompt_coordination
                .prompt_runtime
                .pending_publish_completions
                .remove(&cid);
            self.runtime_io
                .publication
                .idle_dispatches
                .retain(|dispatch| dispatch.cid != cid);
            self.prompt_coordination
                .compaction_runtime
                .enqueued_inference_checkpoints
                .retain(|(agent_id, _)| agent_id != &unloaded.agent_id);
            self.tombstone_ephemeral_provider_prompts_for_agent(&cid);
            self.agent_runtime.agent_registry.agents.remove(&cid);
            self.cancel_agent_synchronized_publications(&cid);
        }
        if let Event::StartAgentResult(result) = event {
            self.notify_watchers_about_start_agent_result(result);
        }
        if let Event::AgentMessageReceived(message) = event {
            self.activate_received_agent_message(message, append_outcome);
        }
        if let Event::SessionAgentLoaded(loaded) = event {
            if persist {
                self.replay_loaded_agent_history_to_subscribers(&loaded.agent_id);
            }
            let is_current_initialization = self
                .prompt_coordination
                .context_discovery
                .pending_agents
                .get(&loaded.agent_id)
                .is_some_and(|pending| pending.initialization_id == loaded.agent_initialization_id);
            if is_current_initialization
                && self
                    .prompt_coordination
                    .context_discovery
                    .pending_agents
                    .get(&loaded.agent_id)
                    .is_some_and(|pending| pending.waiting_on.is_empty())
                && let Err(error) = self.finalize_agent_discovery(&loaded.agent_id)
            {
                self.emit_harness_failure(&format!("failed to finalize agent discovery: {error}"));
            }
        }
        if let Event::AgentInitializationContextSet(context) = event {
            self.apply_finalized_agent_initialization_context(context);
        }
        if let Event::AgentHeadMoved(moved) = event
            && let Some(cid) = self.runtime_agent_id_for_target_agent(Some(moved.agent_id.as_str()))
        {
            self.reconcile_agent_context_usage_for_selected_branch(&cid);
            self.resolve_materialized_message_wakes(&cid);
            self.reproject_idle_output_length_budget(&cid);
            let dormant_repair = self
                .session_runtime
                .agent_store
                .agent(moved.agent_id.as_str())
                .is_some_and(|tree| tree.output_length_dormant_repair().is_some());
            if dormant_repair {
                if let Some(completion) = self
                    .prompt_coordination
                    .prompt_runtime
                    .pending_publish_completions
                    .remove(&cid)
                    && !matches!(
                        completion,
                        AgentPublishCompletion::OutputLengthSteer { .. }
                            | AgentPublishCompletion::OutputLengthContinuation { .. }
                    )
                {
                    self.prompt_coordination
                        .prompt_runtime
                        .pending_publish_completions
                        .insert(cid.clone(), completion);
                }
                self.repair_dormant_output_length_lineage(&cid);
                return;
            }
            self.retry_pending_agent_publish_completion(&cid);
            self.retry_standalone_inference_checkpoint(&cid);
            self.drain_publish_idle_dispatches();
            self.try_advance_queue();
        }
    }

    /// Synchronize idle continuation budget state after selected ancestry
    /// moves.
    ///
    /// Planned, owner-pending, and active lineage states retain their exact
    /// publication or terminal authority and are reconciled by their dedicated
    /// repair paths instead.
    pub(super) fn reproject_idle_output_length_budget(&mut self, cid: &AgentId) {
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.agent_id.clone())
        else {
            return;
        };
        let projected = self
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .and_then(tau_core::AgentTree::output_length_budget_spent_outer_turn);
        let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) else {
            return;
        };
        if !matches!(
            agent.output_length_continuation,
            path_crate_agent::OutputLengthContinuationState::None
                | path_crate_agent::OutputLengthContinuationState::Spent { .. }
        ) {
            return;
        }
        agent.output_length_continuation = projected.map_or(
            path_crate_agent::OutputLengthContinuationState::None,
            |outer_turn_id| path_crate_agent::OutputLengthContinuationState::Spent {
                outer_turn_id,
            },
        );
    }

    pub(super) fn finish_manual_compaction_tool_with_error(
        &mut self,
        call_id: ToolCallId,
        tool_name: ToolName,
        message: &str,
        passive: bool,
    ) {
        if passive
            && self
                .tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(&call_id)
        {
            self.handle_background_tool_error_inner(
                Some(crate::harness::harness_connection_id()),
                ToolError {
                    presentation: Default::default(),
                    call_id,
                    tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    message: message.to_owned(),
                    details: None,
                    display: None,
                    originator: PromptOriginator::User,
                },
                BackgroundCompletionPromptMode::QueuePassive,
                tau_proto::ToolTerminalCause::ToolError,
            );
        } else {
            self.finish_prebuilt_internal_tool_error(ToolError {
                presentation: Default::default(),
                call_id,
                tool_name,
                tool_type: tau_proto::ToolType::Function,
                message: message.to_owned(),
                details: None,
                display: None,
                originator: PromptOriginator::User,
            });
        }
    }

    /// Merges a captured activation cut with the earliest selected message
    /// wake.
    ///
    /// Comparable selected-branch cuts choose the ancestor so every owed
    /// activation remains in the exact suffix. A cut outside the selected
    /// branch, or two incomparable cuts, returns `None`; branch-owned callers
    /// must keep that activation dormant rather than scalarizing it to root.
    pub(crate) fn earliest_activation_cut(
        &self,
        cid: &AgentId,
        captured: Option<tau_proto::AgentHead>,
    ) -> Option<tau_proto::AgentHead> {
        let message = self.selected_message_activation_cut(cid);
        let (Some(captured), Some(message)) = (captured, message) else {
            let selected = captured.or(message)?;
            let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
            let tree = self
                .session_runtime
                .agent_store
                .agent(agent.agent_id.as_deref()?)?;
            let through = agent
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
            return tree.is_ancestor_head(selected, through).then_some(selected);
        };
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let tree = self
            .session_runtime
            .agent_store
            .agent(agent.agent_id.as_deref()?)?;
        let through = agent
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        if !tree.is_ancestor_head(captured, through) || !tree.is_ancestor_head(message, through) {
            None
        } else if tree.is_ancestor_head(captured, message) {
            Some(captured)
        } else if tree.is_ancestor_head(message, captured) {
            Some(message)
        } else {
            None
        }
    }

    /// Select one exact inference checkpoint for both ordinary and intercepted
    /// dispatch paths without claiming its runtime state.
    pub(crate) fn select_inference_dispatch(
        &self,
        cid: &AgentId,
        captured_activation_cut: Option<tau_proto::AgentHead>,
    ) -> Result<InferenceDispatchSelection, InferenceDispatchSelectionError> {
        let agent = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .ok_or(InferenceDispatchSelectionError::MissingModel)?;
        let output_length = match &agent.output_length_continuation {
            path_crate_agent::OutputLengthContinuationState::OwnerReady(dispatch) => {
                let selected = agent
                    .head
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                if dispatch.through != selected {
                    return Err(InferenceDispatchSelectionError::OutputLengthBranchInvalid);
                }
                Some(dispatch.clone())
            }
            _ => None,
        };
        let model = output_length
            .as_ref()
            .map(|continuation| continuation.plan.dispatch.model.clone())
            .or_else(|| self.model_for_agent_role(agent))
            .ok_or(InferenceDispatchSelectionError::MissingModel)?;
        let operation = output_length
            .as_ref()
            .map_or(tau_proto::PromptOperation::Inference, |continuation| {
                continuation.plan.dispatch.operation
            });
        let activation_cut = output_length
            .as_ref()
            .map(|continuation| continuation.plan.dispatch.activation_cut)
            .or_else(|| {
                self.earliest_activation_cut(
                    cid,
                    captured_activation_cut
                        .or_else(|| self.activation_cut_before_current_head(cid))
                        .or(Some(tau_proto::AgentHead::Root)),
                )
            })
            .ok_or(InferenceDispatchSelectionError::MissingActivationCut)?;
        Ok(InferenceDispatchSelection {
            model,
            operation,
            activation_cut,
        })
    }

    /// Claims one selected inference and installs its write-pending checkpoint
    /// state for either direct or interception-delayed dispatch.
    pub(super) fn claim_inference_checkpoint(
        &mut self,
        cid: &AgentId,
        selection: InferenceDispatchSelection,
    ) -> Option<InferenceCheckpointInput> {
        let agent = self.agent_runtime.agent_registry.agents.get_mut(cid)?;
        let durable_agent_id = crate::parse_agent_id(agent.agent_id.as_deref()?);
        let (agent_prompt_id, through, output_length_continuation) =
            if let Some(continuation) = agent.output_length_continuation.claim_pending() {
                (
                    continuation.plan.agent_prompt_id,
                    continuation.through,
                    Some(continuation.plan.owner),
                )
            } else {
                let agent_prompt_id = tau_proto::AgentPromptId::parse(format!(
                    "ap-{durable_agent_id}-{}",
                    agent.next_prompt_index
                ))
                .expect("known-safe AgentPromptId must be valid");
                agent.next_prompt_index = agent.next_prompt_index.saturating_add(1);
                let through = agent
                    .head
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                (agent_prompt_id, through, None)
            };
        agent.activation_dispatch = path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
            owner: path_crate_agent::InferenceCheckpointOwner::Inference,
            agent_prompt_id: agent_prompt_id.clone(),
            through,
            dispatch: crate::agent::InferenceDispatchOwnership {
                model: selection.model.clone(),
                operation: selection.operation,
                activation_cut: selection.activation_cut,
            },
        };
        Some(InferenceCheckpointInput {
            durable_agent_id,
            agent_prompt_id,
            through,
            selection,
            output_length_continuation,
        })
    }

    /// Writes or retains `event` in its semantic store and folds it into the
    /// corresponding in-memory view. Session membership facts go to the session
    /// store; agent transcript facts go to the owning agent store. Either store
    /// may choose a durable or memory-only path based on session/agent
    /// persistence. Returns the owning journal sequence and last folded
    /// transcript node, when applicable. A context input accepted under an
    /// open tool-calling assistant may commit with no node until the round
    /// terminalizes.
    pub(super) fn persist_semantic_event(
        &mut self,
        source: Option<tau_core::PersistedEventSource>,
        event: &Event,
        persist: bool,
        parent: tau_core::AgentEventParent,
        sync_head_for: Option<&ConversationHeadSync>,
        recorded_at: tau_proto::UnixMicros,
    ) -> Result<Option<tau_core::AgentAppendOutcome>, HarnessError> {
        if let Event::AgentStarted(started) = event
            && self
                .agent_runtime
                .agent_registry
                .precommitted_starts
                .remove(started.agent_id.as_str())
        {
            return Ok(None);
        }
        if let Event::AgentUserInteractionRecorded(interaction) = event
            && let Some(count) = self
                .session_runtime
                .precommitted_user_interactions
                .get_mut(interaction.agent_id.as_str())
            && *count != 0
        {
            *count -= 1;
            if *count == 0 {
                self.session_runtime
                    .precommitted_user_interactions
                    .remove(interaction.agent_id.as_str());
            }
            return Ok(None);
        }
        if let Some(call_id) = match event {
            Event::ProviderToolResult(result) => Some(&result.call_id),
            Event::ProviderToolError(error) => Some(&error.call_id),
            _ => None,
        } && !persist
            && self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(call_id)
                .or_else(|| {
                    self.tool_routing
                        .tool_runtime
                        .peer_internal_tool_agents
                        .get(call_id)
                })
                .is_none_or(|cid| !self.tool_terminal_has_open_durable_owner(cid, call_id))
        {
            // Harness-owned wait and peer completions can have a live agent route
            // without a declared transcript call. They still publish the
            // authoritative provider-shaped fact before projections, but have no
            // semantic journal owner to accept it.
            return Ok(None);
        }
        if !semantic_event_router::should_persist_event(event, persist) {
            return Ok(None);
        }
        if matches!(event, Event::ToolRequest(_) | Event::ToolStarted(_)) {
            if !self.session_restore_event_targets_loaded_agent(event) {
                return Err(HarnessError::Participant(format!(
                    "session restore event {} targets an agent that is not loaded in session {}",
                    event.name(),
                    self.session_runtime.current_session_id
                )));
            }
            self.session_runtime.store.append_session_restore_event_at(
                self.session_runtime.current_session_id.as_str(),
                source,
                event.clone(),
                recorded_at,
            )?;
            return Ok(None);
        }
        if let Some(session_id) = semantic_event_router::session_membership_id_for_event(event) {
            let event_persistence = self.session_membership_event_persistence(event);
            self.session_runtime
                .store
                .append_session_event_at_with_persistence(
                    session_id.as_str(),
                    source,
                    event.clone(),
                    recorded_at,
                    event_persistence,
                )?;
            return Ok(None);
        }
        let Some(agent_id) = self
            .agent_id_for_event(event)
            .or_else(|| self.agent_scoped_agent_id_for_event(event, sync_head_for))
        else {
            return Ok(None);
        };
        let outcome = if let Event::ProviderResponseFinished(response) = event
            && let Some(observation_id) = self
                .tool_routing
                .tool_runtime
                .pending_declaration_observations
                .get(&response.agent_prompt_id)
                .copied()
        {
            let outcome = self
                .session_runtime
                .agent_store
                .append_agent_event_at_with_observation_id(
                    agent_id.as_str(),
                    source,
                    parent,
                    event.clone(),
                    recorded_at,
                    observation_id,
                )?;
            self.tool_routing
                .tool_runtime
                .pending_declaration_observations
                .remove(&response.agent_prompt_id);
            outcome
        } else if let Some(call_id) = canonical_tool_terminal_call_id(event)
            && let Some(observation_id) = self
                .tool_routing
                .tool_runtime
                .pending_terminal_observations
                .get(call_id)
                .map(|terminal| terminal.observation_id)
        {
            let outcome = self
                .session_runtime
                .agent_store
                .append_agent_event_at_with_observation_id(
                    agent_id.as_str(),
                    source,
                    parent,
                    event.clone(),
                    recorded_at,
                    observation_id,
                )?;
            self.tool_routing
                .tool_runtime
                .pending_terminal_observations
                .remove(call_id);
            outcome
        } else {
            self.session_runtime.agent_store.append_agent_event_at(
                agent_id.as_str(),
                source,
                parent,
                event.clone(),
                recorded_at,
            )?
        };
        Ok(Some(outcome))
    }

    pub(super) fn session_restore_event_targets_loaded_agent(&self, event: &Event) -> bool {
        let agent_id = match event {
            Event::ToolRequest(request) => &request.agent_id,
            Event::ToolStarted(started) => &started.agent_id,
            _ => return true,
        };
        self.session_runtime
            .store
            .session(self.session_runtime.current_session_id.as_str())
            .is_some_and(|session| session.contains_agent(agent_id))
            || self
                .agent_runtime
                .agent_registry
                .agent_routes
                .keys()
                .any(|loaded_agent_id| loaded_agent_id == agent_id.as_str())
    }

    pub(super) fn session_membership_event_persistence(
        &self,
        event: &Event,
    ) -> tau_core::SessionPersistenceMode {
        let agent_id = match event {
            Event::SessionAgentLoaded(loaded) => &loaded.agent_id,
            Event::SessionAgentUnloaded(unloaded) => &unloaded.agent_id,
            _ => return tau_core::SessionPersistenceMode::Durable,
        };
        if self.agent_is_ephemeral(agent_id) {
            tau_core::SessionPersistenceMode::Ephemeral
        } else {
            tau_core::SessionPersistenceMode::Durable
        }
    }

    pub(super) fn agent_is_ephemeral(&self, agent_id: &tau_proto::AgentId) -> bool {
        if self
            .agent_runtime
            .agent_registry
            .agent_routes
            .get(agent_id.as_str())
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .is_some_and(|agent| agent.persistence.is_ephemeral())
        {
            return true;
        }
        self.session_runtime
            .agent_store
            .agent_persistence(agent_id.as_str())
            .is_ephemeral()
    }

    pub(super) fn agent_scoped_agent_id_for_event(
        &self,
        event: &Event,
        sync_head_for: Option<&ConversationHeadSync>,
    ) -> Option<tau_proto::AgentId> {
        if !matches!(
            event,
            Event::ProviderToolResult(_)
                | Event::ProviderToolError(_)
                | Event::ToolResult(_)
                | Event::ToolResultDisplay(_)
                | Event::ToolError(_)
                | Event::ToolCancelled(_)
                | Event::ToolBackgroundResult(_)
                | Event::ToolBackgroundResultDisplay(_)
                | Event::ToolBackgroundError(_)
        ) {
            return None;
        }
        let sync = sync_head_for?;
        sync.agent_id.clone().or_else(|| {
            self.agent_runtime
                .agent_registry
                .agents
                .get(&sync.cid)?
                .agent_id
                .as_ref()
                .cloned()
                .map(crate::parse_agent_id)
        })
    }

    pub(super) fn event_targets_ephemeral_agent(
        &self,
        event: &Event,
        sync_head_for: Option<&ConversationHeadSync>,
    ) -> bool {
        self.agent_creation_event_targets_ephemeral_agent(event)
            || self.message_fact_targets_ephemeral_agent(event)
            || self.provider_event_targets_ephemeral_agent(event)
            || self.agent_addressed_event_targets_ephemeral_agent(event)
            || self.agent_operational_event_targets_ephemeral_agent(event)
            || self.tool_event_targets_ephemeral_agent(event)
            || self.agent_scoped_event_targets_ephemeral_agent(event, sync_head_for)
    }

    pub(super) fn provider_event_targets_ephemeral_agent(&self, event: &Event) -> bool {
        let prompt_id = match event {
            Event::ProviderPromptSubmittedReported(value)
            | Event::ProviderPromptSubmitted(value) => Some(&value.agent_prompt_id),
            Event::ProviderResponseUpdatedReported(value)
            | Event::ProviderResponseUpdated(value) => Some(&value.agent_prompt_id),
            Event::ProviderResponseFinishedReported(value)
            | Event::ProviderResponseFinished(value) => Some(&value.agent_prompt_id),
            Event::ProviderCacheMissDiagnosticReported(value)
            | Event::ProviderCacheMissDiagnostic(value) => Some(&value.agent_prompt_id),
            Event::ProviderRetryPromptResultReported(value) => {
                return self
                    .ui_runtime
                    .pending_retry_prompts
                    .get(&value.request_id)
                    .is_some_and(|pending| self.agent_is_ephemeral(&pending.target_agent_id))
                    || self
                        .prompt_coordination
                        .prompt_runtime
                        .ephemeral_provider_retry_requests
                        .contains(&value.request_id);
            }
            _ => None,
        };
        prompt_id.is_some_and(|prompt_id| self.provider_prompt_targets_ephemeral(prompt_id))
    }

    pub(super) fn provider_prompt_targets_ephemeral(&self, prompt_id: &AgentPromptId) -> bool {
        self.prompt_coordination
            .prompt_runtime
            .ephemeral_provider_prompts
            .contains(prompt_id)
            || self
                .prompt_coordination
                .prompt_runtime
                .agents
                .get(prompt_id)
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .is_some_and(|agent| agent.persistence.is_ephemeral())
    }

    /// Return whether a message fact selects an ephemeral agent journal.
    pub(super) fn message_fact_targets_ephemeral_agent(&self, event: &Event) -> bool {
        event
            .message_agent_target()
            .and_then(|target| tau_proto::AgentId::parse(target.as_str()).ok())
            .is_some_and(|agent_id| self.agent_is_ephemeral(&agent_id))
    }

    pub(super) fn agent_operational_event_targets_ephemeral_agent(&self, event: &Event) -> bool {
        match event {
            Event::AgentStatsUpdated(stats) => self.agent_is_ephemeral(&stats.agent_id),
            Event::AgentWatchesUpdated(watches) => {
                self.agent_is_ephemeral(&watches.watcher_id)
                    || watches
                        .watched_agent_ids
                        .iter()
                        .any(|agent_id| self.agent_is_ephemeral(agent_id))
                    || watches
                        .changed_agent_id
                        .as_ref()
                        .is_some_and(|agent_id| self.agent_is_ephemeral(agent_id))
            }
            _ => false,
        }
    }

    pub(super) fn agent_creation_event_targets_ephemeral_agent(&self, event: &Event) -> bool {
        match event {
            Event::UiCreateAgent(req) => {
                req.ephemeral || self.agent_id_is_ephemeral(&req.parent_agent)
            }
            Event::StartAgentRequest(request) => {
                self.agent_id_is_ephemeral(&request.parent_agent)
                    || request
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|call_id| self.tool_call_targets_ephemeral_agent(call_id))
            }
            _ => false,
        }
    }

    pub(super) fn agent_addressed_event_targets_ephemeral_agent(&self, event: &Event) -> bool {
        let shell_report_route_id = match event {
            Event::ShellCommandProgressReported(progress) => Some(&progress.command_id),
            Event::ShellCommandFinishedReported(finished) => Some(&finished.command_id),
            _ => None,
        };
        if shell_report_route_id.is_some_and(|command_id| {
            self.ui_runtime
                .ephemeral_ui_shell_route_ids
                .contains(command_id)
        }) {
            return true;
        }
        let canonical_shell_command_id = match event {
            Event::ShellCommandProgress(progress) => Some(&progress.command_id),
            Event::ShellCommandFinished(finished) => Some(&finished.command_id),
            _ => None,
        };
        if canonical_shell_command_id.is_some_and(|command_id| {
            self.ui_runtime
                .pending_ephemeral_ui_shell_canonical_events
                .contains_key(command_id)
        }) {
            return true;
        }
        Self::agent_addressed_event_agent_id(event)
            .is_some_and(|agent_id| self.agent_is_ephemeral(agent_id))
    }

    /// Classify interceptor payloads without allowing mutable shell target
    /// fields to suppress raw debug audit.
    pub(super) fn debug_intercept_event_targets_ephemeral(&self, event: &Event) -> bool {
        match event {
            Event::ShellCommandProgressReported(progress) => self
                .ui_runtime
                .ephemeral_ui_shell_route_ids
                .contains(&progress.command_id),
            Event::ShellCommandFinishedReported(finished) => self
                .ui_runtime
                .ephemeral_ui_shell_route_ids
                .contains(&finished.command_id),
            Event::ShellCommandProgress(progress) => self
                .ui_runtime
                .pending_ephemeral_ui_shell_canonical_events
                .contains_key(&progress.command_id),
            Event::ShellCommandFinished(finished) => self
                .ui_runtime
                .pending_ephemeral_ui_shell_canonical_events
                .contains_key(&finished.command_id),
            _ => self.event_targets_ephemeral_agent(event, None),
        }
    }

    /// Release ephemeral debug classification when mutable canonical shell
    /// progress is dropped before commit.
    pub(super) fn discard_uncommitted_shell_canonical_marker(
        &mut self,
        command_id: &tau_proto::ShellCommandId,
    ) {
        self.release_pending_ephemeral_shell_canonical_marker(command_id);
    }

    /// Reserve one ephemeral debug-classification marker for a canonical shell
    /// event that has entered publication.
    pub(super) fn mark_pending_ephemeral_shell_canonical(
        &mut self,
        command_id: tau_proto::ShellCommandId,
    ) {
        self.ui_runtime
            .pending_ephemeral_ui_shell_canonical_events
            .entry(command_id)
            .and_modify(|count| {
                *count = NonZeroUsize::new(
                    count
                        .get()
                        .checked_add(1)
                        .expect("pending shell canonical count overflow"),
                )
                .expect("incremented count stays nonzero");
            })
            .or_insert(NonZeroUsize::MIN);
    }

    /// Release one committed or dropped canonical shell event's marker.
    pub(super) fn release_pending_ephemeral_shell_canonical_marker(
        &mut self,
        command_id: &tau_proto::ShellCommandId,
    ) {
        let Some(count) = self
            .ui_runtime
            .pending_ephemeral_ui_shell_canonical_events
            .get_mut(command_id)
        else {
            return;
        };
        if count.get() == 1 {
            self.ui_runtime
                .pending_ephemeral_ui_shell_canonical_events
                .remove(command_id);
        } else {
            *count = NonZeroUsize::new(count.get() - 1).expect("decremented count remains nonzero");
        }
    }

    pub(super) fn agent_addressed_event_agent_id(event: &Event) -> Option<&tau_proto::AgentId> {
        match event {
            Event::UiPromptSubmitted(prompt) => Some(&prompt.agent_id),
            Event::UiShellCommand(command) => command.target_agent_id.as_ref(),
            Event::ShellCommandProgress(progress) => progress.target_agent_id.as_ref(),
            Event::ShellCommandFinished(finished) => finished.target_agent_id.as_ref(),
            Event::AgentPromptCreated(prompt) => Some(&prompt.agent_id),
            Event::AgentPromptStarted(prompt) => Some(&prompt.agent_id),
            Event::AgentPromptQueued(prompt) => Some(&prompt.agent_id),
            Event::AgentPromptRecalled(prompt) => Some(&prompt.agent_id),
            Event::AgentPromptRejected(prompt) => Some(&prompt.agent_id),
            Event::AgentPromptTerminated(prompt) => Some(&prompt.agent_id),
            Event::AgentPromptFailed(prompt) => Some(&prompt.agent_id),
            Event::AgentPromptPrewarmRequested(prompt) => Some(&prompt.agent_id),
            Event::ExtInternalPromptSubmitRequest(request) => Some(&request.agent_id),
            Event::ToolRequest(request) => Some(&request.agent_id),
            Event::ToolStarted(started) => Some(&started.agent_id),
            _ => None,
        }
    }

    pub(super) fn tool_event_targets_ephemeral_agent(&self, event: &Event) -> bool {
        match event {
            Event::ToolRejected(rejected) => {
                self.tool_call_targets_ephemeral_agent(&rejected.call_id)
            }
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                self.tool_call_targets_ephemeral_agent(&result.call_id)
            }
            Event::ToolError(error) | Event::ProviderToolError(error) => {
                self.tool_call_targets_ephemeral_agent(&error.call_id)
            }
            Event::ToolBackgroundResult(result) => {
                self.tool_call_targets_ephemeral_agent(&result.call_id)
            }
            Event::ToolBackgroundError(error) => {
                self.tool_call_targets_ephemeral_agent(&error.call_id)
            }
            Event::ToolProgress(progress) | Event::ToolProgressReported(progress) => {
                self.tool_call_targets_ephemeral_agent(&progress.call_id)
            }
            Event::ToolResultReported(result) => {
                self.tool_call_targets_ephemeral_agent(&result.call_id)
            }
            Event::ToolErrorReported(error) => {
                self.tool_call_targets_ephemeral_agent(&error.call_id)
            }
            Event::ToolCancelRequest(cancel) => {
                self.tool_call_targets_ephemeral_agent(&cancel.target_call_id)
            }
            Event::ToolCancelled(cancelled) | Event::ToolCancelledReported(cancelled) => {
                self.tool_call_targets_ephemeral_agent(&cancelled.call_id)
            }
            Event::ToolDelegateProgress(progress) => {
                self.tool_call_targets_ephemeral_agent(&progress.call_id)
            }
            _ => false,
        }
    }

    pub(super) fn agent_scoped_event_targets_ephemeral_agent(
        &self,
        event: &Event,
        sync_head_for: Option<&ConversationHeadSync>,
    ) -> bool {
        self.agent_id_for_event(event)
            .or_else(|| self.agent_scoped_agent_id_for_event(event, sync_head_for))
            .is_some_and(|agent_id| self.agent_is_ephemeral(&agent_id))
    }

    pub(super) fn agent_id_is_ephemeral(&self, agent_id: &Option<tau_proto::AgentId>) -> bool {
        agent_id
            .as_ref()
            .is_some_and(|agent_id| self.agent_is_ephemeral(agent_id))
    }

    pub(super) fn tool_call_targets_ephemeral_agent(
        &self,
        call_id: &tau_proto::ToolCallId,
    ) -> bool {
        self.tool_routing
            .tool_runtime
            .completed_ephemeral_tool_calls
            .contains(call_id)
            || self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(call_id)
                .or_else(|| {
                    self.tool_routing
                        .tool_runtime
                        .peer_internal_tool_agents
                        .get(call_id)
                })
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .is_some_and(|agent| agent.persistence.is_ephemeral())
    }

    pub(super) fn agent_id_for_event(&self, event: &Event) -> Option<tau_proto::AgentId> {
        match event {
            Event::AgentStarted(started) => Some(started.agent_id.clone()),
            Event::AgentDisplayNameSet(name) => Some(name.agent_id.clone()),
            Event::AgentInitializationContextSet(context) => Some(context.agent_id.clone()),
            Event::AgentMetadataSet(set) => Some(set.agent_id.clone()),
            Event::AgentMetadataUnset(unset) => Some(unset.agent_id.clone()),
            Event::AgentMetadataSetRequest(set) => Some(set.agent_id.clone()),
            Event::AgentMetadataUnsetRequest(unset) => Some(unset.agent_id.clone()),
            Event::AgentPromptSubmitted(prompt) => Some(prompt.agent_id.clone()),
            Event::AgentPromptSteered(prompt) => Some(prompt.agent_id.clone()),
            Event::AgentPromptStarted(prompt) => Some(prompt.agent_id.clone()),
            Event::AgentOuterTurnStarted(turn) => Some(turn.agent_id.clone()),
            Event::AgentOuterTurnFinished(turn) => Some(turn.agent_id.clone()),
            Event::AgentPromptCreated(prompt) => Some(prompt.agent_id.clone()),
            Event::AgentPromptTerminated(prompt) => Some(prompt.agent_id.clone()),
            Event::AgentCompactionTriggered(triggered) => Some(triggered.agent_id.clone()),
            Event::AgentCompacted(compacted) => Some(compacted.agent_id.clone()),
            Event::AgentStandaloneCompactionStarted(started) => Some(started.agent_id.clone()),
            Event::AgentManualCompactionRequested(requested) => {
                Some(requested.target_agent_id.clone())
            }
            Event::AgentManualCompactionRequestFailed(failed) => {
                Some(failed.target_agent_id.clone())
            }
            Event::AgentStandaloneCompactionFailed(failed) => Some(failed.agent_id.clone()),
            Event::AgentInferenceDispatchStarted(started) => Some(started.agent_id.clone()),
            Event::AgentUserMessageInjected(injected) => Some(injected.agent_id.clone()),
            Event::AgentMessageSent(message) => Some(message.sender_id.clone()),
            Event::AgentMessageReceived(message) => Some(message.recipient_id.clone()),
            Event::AgentHeadMoved(moved) => Some(moved.agent_id.clone()),
            Event::ShellCommandFinished(finished) => finished.target_agent_id.clone(),
            Event::ProviderResponseFinished(finished) => Some(finished.agent_id.clone()),
            Event::ProviderToolResult(result) => self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(&result.call_id)
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            Event::ProviderToolError(error) | Event::ToolError(error) => self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(&error.call_id)
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            Event::ToolBackgroundResult(result) => self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(&result.call_id)
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            Event::ToolBackgroundError(error) => self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(&error.call_id)
                .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
                .and_then(|conv| conv.agent_id.as_ref())
                .cloned()
                .map(crate::parse_agent_id),
            _ => None,
        }
    }
}

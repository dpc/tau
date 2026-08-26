//! Agent prompt-queue dispatch.
//!
//! Each live agent owns a `pending_prompts` queue. Some internal notices are
//! passive and do not make an idle agent runnable by themselves. The harness
//! has no global agent slot: each durable materialization fact owns one
//! directed live `AgentPromptCreated` provider delivery, and providers
//! serialize their own work. The dispatch logic here drains one non-passive
//! prompt per *runnable* agent and lets providers interleave them.
//!
//! [`Harness::dispatch_user_prompt`] creates/reuses the session's durable user
//! agent and dispatches one interactive submission;
//! [`Harness::dispatch_prompt_for_agent`] is the shared per-agent
//! primitive (also used by side queries spawned via `StartAgentRequest`).
//! [`Harness::try_advance_queue`] is the react-to-state- change drain that
//! picks the next runnable agent and dispatches one prompt from its queue.
//!
//! [`Harness::dispatch_blocked_for`] is the predicate the rest of the harness
//! uses to decide whether to dispatch immediately or queue.

use std::time::Instant;

use tau_proto::{AgentId, Event, SessionId};

use crate::agent as path_crate_agent;
use crate::agent::{AgentTurnState, PendingPrompt};
use crate::error::HarnessError;
use crate::harness::{AgentPublishCompletion, Harness, InferenceDispatchSelectionError};

const NO_PROVIDER_MODELS_MESSAGE: &str = "No provider models are available. Run `tau provider list` to inspect provider status, then configure or enable a provider before submitting another prompt.";

impl Harness {
    pub(crate) fn dispatch_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), HarnessError> {
        let agent_id = self
            .agent_registry
            .agents
            .iter()
            .find_map(|(cid, conv)| {
                (conv.session_id == session_id
                    && conv.originator.is_user()
                    && conv.agent_id.is_some())
                .then_some(cid.clone())
            })
            .map(Ok)
            .unwrap_or_else(|| {
                let role = self.selected_role.clone();
                self.try_create_durable_user_agent(session_id, &role)
            })?;
        self.dispatch_prompt_for_agent(&agent_id, PendingPrompt::human_ui(text))
    }

    /// Publish one pending prompt as an `AgentPromptSubmitted` event on one
    /// agent's branch without dispatching an agent prompt yet.
    ///
    /// Callers that publish additional prompt-bearing events in the same batch
    /// can use this helper and then call
    /// [`Self::dispatch_prompt_after_publish_idle`] once the full batch has
    /// been queued. That keeps interception from sending the agent a prompt
    /// that only contains the first committed user-message event.
    pub(crate) fn publish_pending_prompt_for_agent(
        &mut self,
        agent_id: &AgentId,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<(), HarnessError> {
        let mut prompt = prompt.into();
        self.ensure_prompt_activation_observed(agent_id, &mut prompt);
        self.promote_lifecycle_notification_turn(agent_id);
        if !prompt.is_internal() {
            self.reset_loop_guard_for_progress(agent_id);
        }
        let target_agent_id: tau_proto::AgentId =
            crate::parse_agent_id(self.ensure_agent_id_for_agent(agent_id).ok_or_else(|| {
                HarnessError::Participant(format!(
                    "publish_pending_prompt_for_agent: unknown agent `{agent_id}`"
                ))
            })?);
        let originator = self
            .agent_registry
            .agents
            .get(agent_id)
            .map(|c| c.originator.clone())
            .ok_or_else(|| {
                HarnessError::Participant(format!(
                    "publish_pending_prompt_for_agent: unknown agent `{agent_id}`"
                ))
            })?;
        if prompt.ctx_id.is_some()
            && let Some(agent) = self.agent_registry.agents.get_mut(agent_id)
        {
            agent.next_ctx_id = prompt.ctx_id.clone();
        }
        let notify_watchers = prompt.should_notify_watchers();
        let notification_text = notify_watchers.then(|| prompt.text.clone());
        let inference_activation = prompt.creates_inference_activation();
        let internal_kind = prompt.internal_kind();
        let completion = prompt
            .initial_prompt_correlation
            .clone()
            .map(|correlation| AgentPublishCompletion::InitialPromptSubmission { correlation });
        let defers_notification = completion.is_some();
        self.publish_event_for_agent_with_completion(
            agent_id,
            None,
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                inference_activation,
                agent_id: target_agent_id,
                text: prompt.text,
                trusted_internal_spans: prompt.trusted_internal_spans,
                message_class: prompt.message_class,
                internal_kind,
                originator,
                submission_source: prompt.submission_source,
                display_name: self.agent_display_name_for_cid(agent_id),
                ctx_id: prompt.ctx_id,
            }),
            completion,
            defers_notification && notify_watchers,
        );
        if !defers_notification
            && let Some(text) = notification_text
            && let Some(public_agent_id) = self.ensure_agent_id_for_agent(agent_id)
        {
            self.notify_agent_watchers_about_user_prompt(&public_agent_id, &text);
        }
        Ok(())
    }

    /// Dispatches one prompt for one agent: publishes the
    /// `AgentPromptSubmitted` event (head-bounced via
    /// `publish_for_agent` so it lands on the agent's
    /// branch), enters `AgentThinking`, and asks the agent for a
    /// completion.
    ///
    /// Used for both interactive user prompts on user agents and side-query
    /// prompts spawned by extensions.
    pub(crate) fn dispatch_prompt_for_agent(
        &mut self,
        agent_id: &AgentId,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<(), HarnessError> {
        let mut prompt = prompt.into();
        if self
            .agent_registry
            .agents
            .get(agent_id)
            .is_none_or(|agent| agent.terminating)
        {
            return Err(HarnessError::Participant(format!(
                "agent `{agent_id}` is terminating"
            )));
        }
        if prompt.initial_prompt_correlation.is_some()
            && !self.validate_prompt_render_for_dispatch(agent_id)
        {
            let correlation = prompt
                .initial_prompt_correlation
                .take()
                .expect("checked initial prompt correlation");
            self.fail_initial_prompt_preflight(correlation);
            return Ok(());
        }
        self.record_durable_agent_session_activity(
            agent_id,
            matches!(
                prompt.submission_source,
                tau_proto::PromptSubmissionSource::HumanUi
            ) && prompt.initial_prompt_correlation.is_none(),
        );
        // A fresh ordinary activation explicitly abandons a response-uncertain
        // inference restored from a previous harness runtime. The historical
        // outer start remains unterminated as the crash boundary; this runtime
        // owns a new prompt-derived turn instead of letting the stale checkpoint
        // block the agent forever.
        if prompt.creates_inference_activation()
            && !prompt.is_internal()
            && let Some((durable_agent_id, uncertain_prompt_id, originator)) =
                self.agent_registry.agents.get(agent_id).and_then(|agent| {
                    if agent.in_flight_prompt.is_none()
                        && let path_crate_agent::ActivationDispatchState::DispatchUncertain {
                            owner: path_crate_agent::InferenceCheckpointOwner::Inference,
                            agent_prompt_id,
                            ..
                        } = &agent.activation_dispatch
                    {
                        Some((
                            agent.agent_id.clone()?,
                            agent_prompt_id.clone(),
                            agent.originator.clone(),
                        ))
                    } else {
                        None
                    }
                })
            && self
                .agent_store
                .agent(&durable_agent_id)
                .and_then(|tree| tree.marked_inference_through(&uncertain_prompt_id))
                .is_some()
        {
            if let Some(agent) = self.agent_registry.agents.get_mut(agent_id) {
                agent.pending_prompts.push_back(prompt);
            }
            self.publish_for_agent(
                agent_id,
                tau_proto::Event::AgentPromptTerminated(tau_proto::AgentPromptTerminated {
                    automatic_compaction_decision: None,
                    agent_id: crate::parse_agent_id(&durable_agent_id),
                    agent_prompt_id: uncertain_prompt_id,
                    reason: tau_proto::AgentPromptTerminationReason::Stale,
                    originator,
                }),
            );
            return Ok(());
        }
        if prompt.creates_inference_activation()
            && !prompt.is_internal()
            && let Some(agent) = self.agent_registry.agents.get_mut(agent_id)
            && agent.in_flight_prompt.is_none()
            && matches!(
                agent.activation_dispatch,
                crate::agent::ActivationDispatchState::DispatchUncertain {
                    owner: crate::agent::InferenceCheckpointOwner::Inference,
                    ..
                }
            )
        {
            agent.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
        }
        if let Some(agent) = self.agent_registry.agents.get_mut(agent_id) {
            agent.lifecycle_notification_only_turn = false;
        }
        if !prompt.is_internal() {
            self.reset_loop_guard_for_progress(agent_id);
            let passive_background_prompts =
                self.take_passive_background_completion_prompts_for_user_prompt(agent_id);
            let restore_prompts = self.take_pending_restore_prompts_for_user_prompt(agent_id);
            if !passive_background_prompts.is_empty() || !restore_prompts.is_empty() {
                for passive_prompt in passive_background_prompts {
                    self.publish_pending_prompt_for_agent(agent_id, passive_prompt)?;
                }
                for restore_prompt in restore_prompts {
                    self.publish_pending_prompt_for_agent(agent_id, restore_prompt)?;
                }
                self.publish_pending_prompt_for_agent(agent_id, prompt)?;
                self.dispatch_activation_after_publish_idle(agent_id);
                return Ok(());
            }
        }

        self.publish_pending_prompt_for_agent(agent_id, prompt)?;
        // If the publish parked in interception (or queued behind one
        // that is), defer the agent dispatch until this user-prompt
        // event actually commits. If it committed inline, the helper
        // dispatches now: the AgentTree already reflects the new
        // user message, so the message list assembled inside
        // `send_prompt_to_agent_for` will include it.
        self.drain_publish_idle_dispatches();
        Ok(())
    }

    /// Drains every runnable agent's pending prompt queue.
    ///
    /// There is no global agent slot. The harness materializes one prompt per
    /// runnable agent (Idle turn state, non-empty queue), appends its compact
    /// fact, directs the transient full request to the selected
    /// provider, and routes responses back via `prompt_agents`.
    ///
    /// Session initialization still happens before prompt dispatch, so
    /// a fresh `chat-*` session can discover AGENTS.md and skills before
    /// the agent sees the first user message.
    ///
    /// Pending extension connections preserve transient startup queueing. Once
    /// every connection and extension activation settles with no provider
    /// models, this drain rejects queued prompts and retires selected-branch
    /// message wakes without creating provider work.
    pub(crate) fn try_advance_queue(&mut self) {
        if !self.turn_state.is_idle()
            || !self.extensions_all_ready()
            || self.extensions.pending_connects != 0
        {
            return;
        }
        loop {
            let has_captured_output_length_owner =
                self.agent_registry.agents.values().any(|agent| {
                    matches!(
                        agent.output_length_continuation,
                        path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                    )
                });
            if self.selected_model.is_none()
                && self.provider_runtime.model_info.is_empty()
                && !has_captured_output_length_owner
            {
                self.reject_runnable_activations_without_provider_models();
                return;
            }
            let Some(agent_id) = self.next_runnable_agent() else {
                break;
            };
            let session_id = self
                .agent_registry
                .agents
                .get(&agent_id)
                .map(|c| c.session_id.clone())
                .expect("runnable agent exists");

            if !self.session_initialized(&session_id) {
                // Reachable only if the bound session somehow lost its
                // `initialized_sessions` entry; treat as a re-init.
                // Init is global, so stop draining until it completes.
                self.start_session_init(session_id, tau_proto::SessionStartReason::Initial);
                return;
            }

            let has_ready_initial_prompt =
                self.agent_registry
                    .agents
                    .get(&agent_id)
                    .is_some_and(|agent| {
                        agent
                            .pending_prompts
                            .iter()
                            .find(|prompt| !prompt.is_passive_background_completion())
                            .is_some_and(|prompt| prompt.initial_prompt_correlation.is_some())
                    });
            let has_durable_activation = !has_ready_initial_prompt
                && self
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .is_some_and(|agent| {
                        agent.pending_replay_activation
                            || self.has_ready_message_wake_on_selected_branch(&agent_id)
                    });
            if has_durable_activation {
                let _ = self.ensure_agent_id_for_agent(&agent_id);
                if self
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .is_some_and(|agent| agent.pending_replay_activation)
                {
                    let restore_prompts =
                        self.take_pending_restore_prompts_for_user_prompt(&agent_id);
                    if let Some(agent) = self.agent_registry.agents.get_mut(&agent_id) {
                        agent.pending_prompts.extend(restore_prompts);
                    }
                    self.fold_pending_prompts_as_steered(&agent_id);
                }
                let output_length_owner_ready = self
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .is_some_and(|agent| {
                        matches!(
                            agent.output_length_continuation,
                            path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                        )
                    });
                if !output_length_owner_ready && self.schedule_standalone_auto_compaction(&agent_id)
                {
                    continue;
                }
                if !output_length_owner_ready
                    && !self.validate_prompt_render_for_dispatch(&agent_id)
                {
                    return;
                }
                if let Some(activation_class) =
                    self.selected_message_wake_activation_class(&agent_id)
                    && let Some(agent) = self.agent_registry.agents.get_mut(&agent_id)
                {
                    agent.lifecycle_notification_only_turn = activation_class
                        == path_crate_agent::AgentMessageActivationClass::IsolatedWatchNotification;
                }
                let captured_activation_cut =
                    self.agent_registry.agents.get(&agent_id).and_then(|agent| {
                        let durable_agent_id = agent.agent_id.as_deref()?;
                        agent
                            .head
                            .and_then(|head| self.agent_store.agent(durable_agent_id)?.node(head))
                            .and_then(|node| node.parent_id)
                            .map(tau_proto::AgentHead::Node)
                            .or(Some(tau_proto::AgentHead::Root))
                    });
                let selection = match self
                    .select_inference_dispatch(&agent_id, captured_activation_cut)
                {
                    Ok(selection) => selection,
                    Err(InferenceDispatchSelectionError::MissingModel) => {
                        let role_name = self.role_name_for_agent_id(&agent_id);
                        self.emit_info(&format!(
                                "role `{role_name}` has no available model — use :role to pick a role, :model <provider>/<model> to pick an agent model, or enable a provider"
                            ));
                        self.set_agent_turn_state(
                            &agent_id,
                            path_crate_agent::AgentTurnState::Idle,
                        );
                        return;
                    }
                    Err(InferenceDispatchSelectionError::OutputLengthBranchInvalid) => {
                        self.repair_dormant_output_length_lineage(&agent_id);
                        return;
                    }
                    Err(InferenceDispatchSelectionError::MissingActivationCut) => {
                        return;
                    }
                };
                self.record_durable_agent_session_activity(&agent_id, false);
                let Some(checkpoint) = self.claim_inference_checkpoint(&agent_id, selection) else {
                    continue;
                };
                self.publish_for_agent(
                    &agent_id,
                    tau_proto::Event::AgentInferenceDispatchStarted(
                        tau_proto::AgentInferenceDispatchStarted {
                            agent_id: checkpoint.durable_agent_id,
                            transaction_id: None,
                            agent_prompt_id: checkpoint.agent_prompt_id,
                            through: checkpoint.through,
                            model: Some(checkpoint.selection.model),
                            operation: Some(checkpoint.selection.operation),
                            activation_cut: Some(checkpoint.selection.activation_cut),
                            output_length_continuation: checkpoint.output_length_continuation,
                        },
                    ),
                );
                continue;
            }

            let prompt = self
                .pop_next_runnable_prompt(&agent_id)
                .expect("runnable agent has a prompt");
            let initial_prompt_correlation = prompt.initial_prompt_correlation.clone();
            let prompt = match self.resolve_pending_user_skill_for_agent(&agent_id, prompt) {
                Ok(prompt) => prompt,
                Err(message) => {
                    if let Some(correlation) = initial_prompt_correlation {
                        self.publish_initial_prompt_failed(
                            correlation,
                            tau_proto::AgentPromptFailureStage::Preprocessing,
                            &message,
                        );
                    }
                    continue;
                }
            };
            if let Err(error) = self.dispatch_prompt_for_agent(&agent_id, prompt) {
                self.emit_harness_failure(&format!("failed to dispatch queued prompt: {error}"));
                if let Some(correlation) = initial_prompt_correlation {
                    self.publish_initial_prompt_failed(
                        correlation,
                        tau_proto::AgentPromptFailureStage::Submission,
                        "failed to submit initial prompt",
                    );
                }
                // Reset the agent so it doesn't wedge as
                // AgentThinking with no in-flight prompt.
                if let Some(conv) = self.agent_registry.agents.get_mut(&agent_id) {
                    conv.in_flight_prompt = None;
                }
                self.set_agent_turn_state(&agent_id, AgentTurnState::Idle);
            }
        }
    }

    /// Activate exact replay occurrences after restore installed all runtime
    /// handlers and routes.
    pub(crate) fn activate_replayed_prompt_occurrences(&mut self) {
        let pending_stale = std::mem::take(&mut self.prompt_runtime.pending_replay_uncertain_stale);
        for (cid, terminated) in pending_stale {
            self.publish_event_for_agent(
                &cid,
                None,
                tau_proto::Event::AgentPromptTerminated(terminated),
            );
        }
        let pending = std::mem::take(&mut self.prompt_runtime.pending_replay_activation_occurrences);
        for (cid, occurrences) in pending {
            if let Some(agent) = self.agent_registry.agents.get_mut(&cid) {
                agent.pending_replay_activation = false;
            }
            for occurrence in occurrences {
                self.enqueue_committed_activation_occurrence(
                    cid.clone(),
                    occurrence.source_seq,
                    occurrence.node_id().map(tau_proto::AgentHead::Node),
                );
            }
        }
        self.drain_publish_idle_dispatches();
    }

    /// Terminalize runnable prompt and message activations after provider
    /// startup has settled with no published models.
    fn reject_runnable_activations_without_provider_models(&mut self) {
        let runnable_agents = self
            .agent_registry
            .agents
            .keys()
            .filter(|agent_id| {
                self.agent_registry
                    .agents
                    .get(*agent_id)
                    .is_some_and(|agent| {
                        agent
                            .pending_prompts
                            .iter()
                            .any(|prompt| !prompt.is_passive_background_completion())
                            || agent.pending_replay_activation
                            || self.has_ready_message_wake_on_selected_branch(agent_id)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();

        for agent_id in runnable_agents {
            let pending = self
                .agent_registry
                .agents
                .get_mut(&agent_id)
                .expect("collected runnable agent must remain loaded")
                .pending_prompts
                .drain(..)
                .collect::<Vec<_>>();
            let (passive, rejected): (Vec<_>, Vec<_>) = pending
                .into_iter()
                .partition(PendingPrompt::is_passive_background_completion);
            self.agent_registry
                .agents
                .get_mut(&agent_id)
                .expect("collected runnable agent must remain loaded")
                .pending_prompts
                .extend(passive);

            for prompt in rejected {
                if let Some(correlation) = prompt.initial_prompt_correlation {
                    self.publish_initial_prompt_failed(
                        correlation,
                        tau_proto::AgentPromptFailureStage::Submission,
                        NO_PROVIDER_MODELS_MESSAGE,
                    );
                } else if let Some(public_agent_id) = self.ensure_agent_id_for_agent(&agent_id) {
                    self.publish_event(
                        Some(crate::harness::harness_connection_id()),
                        Event::AgentPromptRejected(tau_proto::AgentPromptRejected {
                            agent_id: crate::parse_agent_id(&public_agent_id),
                            message_class: prompt.message_class,
                            message: NO_PROVIDER_MODELS_MESSAGE.to_owned(),
                        }),
                    );
                }
            }

            let rejects_message_activation = self
                .agent_registry
                .agents
                .get(&agent_id)
                .is_some_and(|agent| agent.pending_replay_activation)
                || self.has_ready_message_wake_on_selected_branch(&agent_id);
            if rejects_message_activation {
                let through = self
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .and_then(|agent| agent.head)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                self.acknowledge_message_wakes_through(&agent_id, through);
                self.emit_harness_failure(NO_PROVIDER_MODELS_MESSAGE);
            }
        }
    }

    fn next_runnable_agent(&self) -> Option<AgentId> {
        let runnable = self
            .agent_registry
            .agents
            .iter()
            .filter(|(agent_id, conv)| {
                (conv
                    .pending_prompts
                    .iter()
                    .any(|prompt| !prompt.is_passive_background_completion())
                    || self.has_ready_message_wake_on_selected_branch(agent_id)
                    || conv.pending_replay_activation)
                    && matches!(conv.turn_state, AgentTurnState::Idle)
                    && !conv.terminating
                    && matches!(
                        conv.activation_dispatch,
                        crate::agent::ActivationDispatchState::None
                    )
                    && (conv
                        .pending_prompts
                        .iter()
                        .find(|prompt| !prompt.is_passive_background_completion())
                        .is_none_or(|prompt| prompt.initial_prompt_correlation.is_none())
                        || self.agent_initialization_ready_for(agent_id))
                    && !self.has_deferred_prompt_dispatch_for(agent_id)
                    && !self.agent_has_open_foreground_tool_round(agent_id)
            })
            .collect::<Vec<_>>();
        runnable
            .iter()
            .find(|(_, agent)| {
                matches!(
                    agent.output_length_continuation,
                    path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                )
            })
            .or_else(|| runnable.first())
            .map(|(agent_id, _)| (*agent_id).clone())
    }

    fn pop_next_runnable_prompt(&mut self, agent_id: &AgentId) -> Option<PendingPrompt> {
        let conv = self.agent_registry.agents.get_mut(agent_id)?;
        let index = conv
            .pending_prompts
            .iter()
            .position(|prompt| !prompt.is_passive_background_completion())?;
        conv.pending_prompts.remove(index)
    }

    /// Project one accepted durable-agent activation into its session's
    /// best-effort retention hint, timing only authenticated UI prompt
    /// dispatch.
    fn record_durable_agent_session_activity(
        &mut self,
        agent_id: &AgentId,
        trace_human_ui_prompt_acceptance: bool,
    ) {
        let Some(session_id) = self.agent_registry.agents.get(agent_id).and_then(|agent| {
            agent
                .persistence
                .is_durable()
                .then(|| agent.session_id.clone())
        }) else {
            return;
        };
        if trace_human_ui_prompt_acceptance {
            let started = Instant::now();
            let result = self.store.record_session_activity(session_id.as_str());
            tracing::trace!(
                target: "tau_harness::prompt_acceptance",
                stage = "session_meta_touch",
                agent_id = %agent_id,
                result_class = if result.is_ok() { "success" } else { "failure" },
                session_meta_touch_us = started.elapsed().as_micros(),
                "content-free prompt acceptance precursor"
            );
        } else {
            let _ = self.store.record_session_activity(session_id.as_str());
        }
    }

    /// True when a fresh prompt for one agent should *not* be sent
    /// immediately. Two layers of gating:
    /// - global: selected role has no resolved model, harness mid-init,
    ///   extensions not yet `Ready`;
    /// - per-agent: that agent already has a prompt in flight, is waiting on
    ///   tool results, or has a latent dispatch parked behind interception.
    pub(crate) fn dispatch_blocked_for(&self, agent_id: &AgentId) -> bool {
        if self.selected_model.is_none()
            || !self.turn_state.is_idle()
            || self.extensions.resolving_initial_collisions
            || !self.extensions_all_ready()
            || !self.agent_context_ready_for(agent_id)
        {
            return true;
        }
        match self.agent_registry.agents.get(agent_id) {
            Some(conv) => {
                conv.terminating
                    || conv.in_flight_prompt.is_some()
                    || !matches!(
                        conv.activation_dispatch,
                        crate::agent::ActivationDispatchState::None
                    )
                    || !matches!(conv.turn_state, AgentTurnState::Idle)
                    || self.has_deferred_prompt_dispatch_for(agent_id)
                    || self.agent_has_open_foreground_tool_round(agent_id)
            }
            None => true,
        }
    }
}

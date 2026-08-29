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

use std::collections::HashSet;
use std::time::Instant;

use tau_proto::{AgentId, Event, SessionId};

use crate::agent as path_crate_agent;
use crate::agent::{AgentTurnState, InitialPromptCorrelation, PendingPrompt};
use crate::error::HarnessError;
use crate::harness::{
    AgentPublishCompletion, Harness, InferenceDispatchSelectionError, prompt_acceptance_timing,
};

const NO_PROVIDER_MODELS_MESSAGE: &str = "No provider models are available. Run `tau provider list` to inspect provider status, then configure or enable a provider before submitting another prompt.";

/// One streaming scheduler choice and its already-located non-passive prompt.
pub(crate) struct RunnableAgentSelection {
    /// Selected loaded-agent key in current hash-map encounter order.
    pub(super) agent_id: AgentId,
    /// Exact first non-passive queue position, when prompt work made it
    /// runnable.
    pub(super) prompt_index: Option<usize>,
    /// Initial-prompt correlation cloned once from that exact queue entry.
    pub(super) initial_prompt_correlation: Option<InitialPromptCorrelation>,
    /// Whether the selection probe found a ready selected-branch wake.
    pub(super) had_ready_message_wake: bool,
}

/// Exact streaming-selector work exposed to complexity regression tests.
#[derive(Default)]
struct RunnableSelectionWork {
    /// Loaded agents visited in current encounter order.
    agent_visits: usize,
    /// Prompt queue entries inspected while locating first non-passive work.
    prompt_visits: usize,
    /// Linear selected-wake ancestry probes.
    wake_probes: usize,
    /// Wake records visited by readiness probes.
    wake_visits: usize,
    /// Branch nodes visited by readiness probes.
    wake_branch_visits: usize,
    /// Transient wake-membership buffers allocated by readiness probes.
    wake_probe_buffers: usize,
    /// Maximum candidate records retained concurrently.
    high_retained_candidates: usize,
}

impl Harness {
    pub(crate) fn dispatch_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), HarnessError> {
        let agent_id = self
            .agent_runtime
            .agent_registry
            .agents
            .iter()
            .find_map(|(cid, conv)| {
                (conv.identity.session_id == session_id
                    && conv.identity.originator.is_user()
                    && conv.identity.agent_id.is_some())
                .then_some(cid.clone())
            })
            .map(Ok)
            .unwrap_or_else(|| {
                let role = self.config.selected_role.clone();
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
        self.publish_pending_prompt_for_agent_inner(agent_id, prompt.into(), None, true)
    }

    fn publish_pending_prompt_for_agent_inner(
        &mut self,
        agent_id: &AgentId,
        prompt: PendingPrompt,
        prompt_acceptance: Option<prompt_acceptance_timing::PromptAcceptanceTiming>,
        reset_loop_guard_for_progress: bool,
    ) -> Result<(), HarnessError> {
        let mut prompt = prompt;
        self.ensure_prompt_activation_observed(agent_id, &mut prompt);
        self.promote_lifecycle_notification_turn(agent_id);
        if reset_loop_guard_for_progress && !prompt.is_internal() {
            self.reset_loop_guard_for_progress(agent_id);
        }
        let target_agent_id: tau_proto::AgentId =
            crate::parse_agent_id(self.ensure_agent_id_for_agent(agent_id).ok_or_else(|| {
                HarnessError::Participant(format!(
                    "publish_pending_prompt_for_agent: unknown agent `{agent_id}`"
                ))
            })?);
        let originator = self
            .agent_runtime
            .agent_registry
            .agents
            .get(agent_id)
            .map(|c| c.identity.originator.clone())
            .ok_or_else(|| {
                HarnessError::Participant(format!(
                    "publish_pending_prompt_for_agent: unknown agent `{agent_id}`"
                ))
            })?;
        if prompt.ctx_id.is_some()
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(agent_id)
        {
            agent.dispatch.next_ctx_id = prompt.ctx_id.clone();
        }
        let notify_watchers = prompt.should_notify_watchers();
        let inference_activation = prompt.creates_inference_activation();
        let internal_kind = prompt.internal_kind();
        let completion = prompt
            .initial_prompt_correlation
            .clone()
            .map(|correlation| AgentPublishCompletion::InitialPromptSubmission { correlation });
        let defers_notification = completion.is_some();
        let notification_text = (notify_watchers
            && !defers_notification
            && self.has_watchers_for_agent(target_agent_id.as_str()))
        .then(|| self.clone_prompt_text_for_watch_notification(&prompt.text));
        let event = Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
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
        });
        if let Some(prompt_acceptance) = prompt_acceptance {
            self.publish_event_for_agent_with_prompt_acceptance(
                agent_id,
                None,
                event,
                completion,
                prompt_acceptance,
                defers_notification && notify_watchers,
            );
        } else {
            self.publish_event_for_agent_with_completion(
                agent_id,
                None,
                event,
                completion,
                defers_notification && notify_watchers,
            );
        }
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
        self.dispatch_prompt_for_agent_inner(agent_id, prompt.into(), false)
    }

    /// Dispatch an immediately admitted visible Human UI prompt with timing.
    pub(super) fn dispatch_prompt_for_agent_with_acceptance_trace(
        &mut self,
        agent_id: &AgentId,
        prompt: PendingPrompt,
    ) -> Result<(), HarnessError> {
        self.dispatch_prompt_for_agent_inner(agent_id, prompt, true)
    }

    fn dispatch_prompt_for_agent_inner(
        &mut self,
        agent_id: &AgentId,
        mut prompt: PendingPrompt,
        trace_prompt_acceptance: bool,
    ) -> Result<(), HarnessError> {
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(agent_id)
            .is_none_or(|agent| agent.dispatch.terminating)
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
        let mut prompt_acceptance =
            trace_prompt_acceptance.then(prompt_acceptance_timing::PromptAcceptanceTiming::new);
        self.record_durable_agent_session_activity(
            agent_id,
            matches!(
                prompt.submission_source,
                tau_proto::PromptSubmissionSource::HumanUi
            ) && prompt.initial_prompt_correlation.is_none(),
        );
        if let Some(timing) = prompt_acceptance.as_mut() {
            timing.note_precursor_stats();
        }
        // A fresh ordinary activation explicitly abandons a response-uncertain
        // inference restored from a previous harness runtime. The historical
        // outer start remains unterminated as the crash boundary; this runtime
        // owns a new prompt-derived turn instead of letting the stale checkpoint
        // block the agent forever.
        if prompt.creates_inference_activation()
            && !prompt.is_internal()
            && let Some((durable_agent_id, uncertain_prompt_id, originator)) = self
                .agent_runtime
                .agent_registry
                .agents
                .get(agent_id)
                .and_then(|agent| {
                    if agent.dispatch.in_flight_prompt.is_none()
                        && let path_crate_agent::ActivationDispatchState::DispatchUncertain {
                            owner: path_crate_agent::InferenceCheckpointOwner::Inference,
                            agent_prompt_id,
                            ..
                        } = &agent.dispatch.activation_dispatch
                    {
                        Some((
                            agent.identity.agent_id.clone()?,
                            agent_prompt_id.clone(),
                            agent.identity.originator.clone(),
                        ))
                    } else {
                        None
                    }
                })
            && self
                .session_runtime
                .agent_store
                .agent(&durable_agent_id)
                .and_then(|tree| tree.marked_inference_through(&uncertain_prompt_id))
                .is_some()
        {
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(agent_id) {
                agent.dispatch.pending_prompts.push_back(prompt);
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
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(agent_id)
            && agent.dispatch.in_flight_prompt.is_none()
            && matches!(
                agent.dispatch.activation_dispatch,
                crate::agent::ActivationDispatchState::DispatchUncertain {
                    owner: crate::agent::InferenceCheckpointOwner::Inference,
                    ..
                }
            )
        {
            agent.dispatch.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
        }
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(agent_id) {
            agent.turn.lifecycle_notification_only_turn = false;
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
                self.publish_pending_prompt_for_agent_inner(
                    agent_id,
                    prompt,
                    prompt_acceptance.take(),
                    false,
                )?;
                self.dispatch_activation_after_publish_idle(agent_id);
                return Ok(());
            }
        }

        self.publish_pending_prompt_for_agent_inner(
            agent_id,
            prompt,
            prompt_acceptance.take(),
            false,
        )?;
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
        self.try_advance_queue_for(None);
    }

    /// Advances only the exact agents whose admission-rejected activations
    /// became capacity-ready.
    pub(super) fn try_advance_capacity_rejected_agents(&mut self, agents: &HashSet<AgentId>) {
        self.try_advance_queue_for(Some(agents));
    }

    fn try_advance_queue_for(&mut self, allowed: Option<&HashSet<AgentId>>) {
        if !self.session_runtime.turn_state.is_idle()
            || !self.extensions_all_ready()
            || self.extensions.pending_connects != 0
        {
            return;
        }
        loop {
            let has_captured_output_length_owner = self
                .agent_runtime
                .agent_registry
                .agents
                .iter()
                .any(|(agent_id, agent)| {
                    allowed.is_none_or(|allowed| allowed.contains(agent_id))
                        && matches!(
                            agent.turn.output_length_continuation,
                            path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                        )
                });
            if self.config.selected_model.is_none()
                && self.provider_runtime.model_info.is_empty()
                && !has_captured_output_length_owner
            {
                self.reject_runnable_activations_without_provider_models(allowed);
                return;
            }
            let Some(selected) = self.next_runnable_agent(allowed) else {
                break;
            };
            let agent_id = selected.agent_id;
            let session_id = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&agent_id)
                .map(|c| c.identity.session_id.clone())
                .expect("runnable agent exists");

            if !self.session_initialized(&session_id) {
                // Reachable only if the bound session somehow lost its
                // `initialized_sessions` entry; treat as a re-init.
                // Init is global, so stop draining until it completes.
                self.start_session_init(session_id, tau_proto::SessionStartReason::Initial);
                return;
            }

            let has_ready_initial_prompt = selected.initial_prompt_correlation.is_some();
            let has_durable_activation = !has_ready_initial_prompt
                && self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .is_some_and(|agent| {
                        agent.dispatch.pending_replay_activation || selected.had_ready_message_wake
                    });
            if has_durable_activation {
                let _ = self.ensure_agent_id_for_agent(&agent_id);
                if self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .is_some_and(|agent| agent.dispatch.pending_replay_activation)
                {
                    let restore_prompts =
                        self.take_pending_restore_prompts_for_user_prompt(&agent_id);
                    if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&agent_id)
                    {
                        agent.dispatch.pending_prompts.extend(restore_prompts);
                    }
                    self.fold_pending_prompts_as_steered(&agent_id);
                }
                let selected_wakes = self.selected_branch_wake_view(&agent_id);
                let output_length_owner_ready = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .is_some_and(|agent| {
                        matches!(
                            agent.turn.output_length_continuation,
                            path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
                        )
                    });
                if !output_length_owner_ready
                    && self.schedule_standalone_auto_compaction_with_wake_view(
                        &agent_id,
                        selected_wakes.as_ref(),
                    )
                {
                    continue;
                }
                if !output_length_owner_ready
                    && !self.validate_prompt_render_for_dispatch(&agent_id)
                {
                    return;
                }
                if let Some(activation_class) = selected_wakes
                    .as_ref()
                    .and_then(|view| view.activation_class())
                    && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&agent_id)
                {
                    agent.turn.lifecycle_notification_only_turn = activation_class
                        == path_crate_agent::AgentMessageActivationClass::IsolatedWatchNotification;
                }
                let captured_activation_cut = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .and_then(|agent| {
                        let durable_agent_id = agent.identity.agent_id.as_deref()?;
                        agent
                            .identity
                            .head
                            .and_then(|head| {
                                self.session_runtime
                                    .agent_store
                                    .agent(durable_agent_id)?
                                    .node(head)
                            })
                            .and_then(|node| node.parent_id)
                            .map(tau_proto::AgentHead::Node)
                            .or(Some(tau_proto::AgentHead::Root))
                    });
                let selection = match self.select_inference_dispatch_with_wake_view(
                    &agent_id,
                    captured_activation_cut,
                    selected_wakes.as_ref(),
                ) {
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
                if allowed.is_some()
                    || self
                        .runtime_io
                        .publication
                        .capacity_rejected_activations
                        .contains_key(&agent_id)
                {
                    return;
                }
                continue;
            }

            let prompt = self
                .pop_runnable_prompt_at(&agent_id, selected.prompt_index)
                .expect("runnable agent has a prompt");
            let initial_prompt_correlation = selected.initial_prompt_correlation;
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
                if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(&agent_id) {
                    conv.dispatch.in_flight_prompt = None;
                }
                self.set_agent_turn_state(&agent_id, AgentTurnState::Idle);
            }
        }
    }

    /// Activate exact replay occurrences after restore installed all runtime
    /// handlers and routes.
    pub(crate) fn activate_replayed_prompt_occurrences(&mut self) {
        let pending_stale = std::mem::take(
            &mut self
                .prompt_coordination
                .prompt_runtime
                .pending_replay_uncertain_stale,
        );
        for (cid, terminated) in pending_stale {
            self.publish_event_for_agent(
                &cid,
                None,
                tau_proto::Event::AgentPromptTerminated(terminated),
            );
        }
        let pending = std::mem::take(
            &mut self
                .prompt_coordination
                .prompt_runtime
                .pending_replay_activation_occurrences,
        );
        for (cid, occurrences) in pending {
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.dispatch.pending_replay_activation = false;
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
    fn reject_runnable_activations_without_provider_models(
        &mut self,
        allowed: Option<&HashSet<AgentId>>,
    ) {
        let runnable_agents = self
            .agent_runtime
            .agent_registry
            .agents
            .keys()
            .filter(|agent_id| {
                allowed.is_none_or(|allowed| allowed.contains(*agent_id))
                    && self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(*agent_id)
                        .is_some_and(|agent| {
                            agent
                                .dispatch
                                .pending_prompts
                                .iter()
                                .any(|prompt| !prompt.is_passive_background_completion())
                                || agent.dispatch.pending_replay_activation
                                || self.has_ready_message_wake_on_selected_branch(agent_id)
                        })
            })
            .cloned()
            .collect::<Vec<_>>();

        for agent_id in runnable_agents {
            let pending = self
                .agent_runtime
                .agent_registry
                .agents
                .get_mut(&agent_id)
                .expect("collected runnable agent must remain loaded")
                .dispatch
                .pending_prompts
                .drain(..)
                .collect::<Vec<_>>();
            let (passive, rejected): (Vec<_>, Vec<_>) = pending
                .into_iter()
                .partition(PendingPrompt::is_passive_background_completion);
            self.agent_runtime
                .agent_registry
                .agents
                .get_mut(&agent_id)
                .expect("collected runnable agent must remain loaded")
                .dispatch
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
                .agent_runtime
                .agent_registry
                .agents
                .get(&agent_id)
                .is_some_and(|agent| agent.dispatch.pending_replay_activation)
                || self.has_ready_message_wake_on_selected_branch(&agent_id);
            if rejects_message_activation {
                let through = self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&agent_id)
                    .and_then(|agent| agent.identity.head)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
                self.acknowledge_message_wakes_through(&agent_id, through);
                self.emit_harness_failure(NO_PROVIDER_MODELS_MESSAGE);
            }
        }
    }

    fn next_runnable_agent(
        &self,
        allowed: Option<&HashSet<AgentId>>,
    ) -> Option<RunnableAgentSelection> {
        self.next_runnable_agent_inner::<false>(allowed, &mut RunnableSelectionWork::default())
    }

    /// Runs the production selector with exact work accounting.
    #[cfg(test)]
    pub(crate) fn next_runnable_agent_measured(
        &self,
        allowed: Option<&HashSet<AgentId>>,
    ) -> (Option<RunnableAgentSelection>, [usize; 8]) {
        let mut work = RunnableSelectionWork::default();
        let selected = self.next_runnable_agent_inner::<true>(allowed, &mut work);
        (
            selected,
            [
                work.agent_visits,
                work.prompt_visits,
                work.wake_probes,
                work.wake_visits,
                work.wake_branch_visits,
                work.wake_probe_buffers,
                work.high_retained_candidates,
                0,
            ],
        )
    }

    /// Implements current-order selection without collecting runnable agents.
    fn next_runnable_agent_inner<const MEASURE: bool>(
        &self,
        allowed: Option<&HashSet<AgentId>>,
        work: &mut RunnableSelectionWork,
    ) -> Option<RunnableAgentSelection> {
        let mut first = None;
        for (agent_id, conv) in &self.agent_runtime.agent_registry.agents {
            if MEASURE {
                work.agent_visits += 1;
            }
            if allowed.is_some_and(|allowed| !allowed.contains(agent_id))
                || (allowed.is_none()
                    && self
                        .runtime_io
                        .publication
                        .capacity_rejected_activations
                        .contains_key(agent_id))
                || !matches!(conv.turn.turn_state, AgentTurnState::Idle)
                || conv.dispatch.terminating
                || !matches!(
                    conv.dispatch.activation_dispatch,
                    crate::agent::ActivationDispatchState::None
                )
                || self.has_deferred_prompt_dispatch_for(agent_id)
                || self.agent_has_open_foreground_tool_round(agent_id)
            {
                continue;
            }

            let non_passive =
                conv.dispatch
                    .pending_prompts
                    .iter()
                    .enumerate()
                    .find(|(_, prompt)| {
                        if MEASURE {
                            work.prompt_visits += 1;
                        }
                        !prompt.is_passive_background_completion()
                    });
            let prompt_index = non_passive.as_ref().map(|(index, _)| *index);
            let initial_prompt_correlation = non_passive
                .as_ref()
                .and_then(|(_, prompt)| prompt.initial_prompt_correlation.as_ref());
            if initial_prompt_correlation.is_some()
                && !self.agent_initialization_ready_for(agent_id)
            {
                continue;
            }
            let had_ready_message_wake =
                if prompt_index.is_none() && !conv.dispatch.pending_replay_activation {
                    let probe = self.selected_branch_wake_probe(agent_id);
                    if MEASURE {
                        work.wake_probes += 1;
                        if let Some(probe) = &probe {
                            work.wake_visits += probe.wakes;
                            work.wake_branch_visits += probe.branch_nodes;
                            work.wake_probe_buffers += probe.owned_buffers;
                        }
                    }
                    let ready = probe.is_some_and(|probe| probe.ready);
                    if !ready {
                        continue;
                    }
                    Some(true)
                } else {
                    None
                };
            let output_length_owner_ready = matches!(
                conv.turn.output_length_continuation,
                path_crate_agent::OutputLengthContinuationState::OwnerReady(_)
            );
            if output_length_owner_ready {
                if MEASURE {
                    work.high_retained_candidates = 1;
                }
                return Some(self.finish_runnable_selection::<MEASURE>(
                    agent_id,
                    prompt_index,
                    initial_prompt_correlation,
                    had_ready_message_wake,
                    work,
                ));
            }
            if first.is_none() {
                first = Some((
                    agent_id,
                    prompt_index,
                    initial_prompt_correlation,
                    had_ready_message_wake,
                ));
                if MEASURE {
                    work.high_retained_candidates = 1;
                }
            }
        }
        first.map(
            |(agent_id, prompt_index, initial_prompt_correlation, had_ready_message_wake)| {
                self.finish_runnable_selection::<MEASURE>(
                    agent_id,
                    prompt_index,
                    initial_prompt_correlation,
                    had_ready_message_wake,
                    work,
                )
            },
        )
    }

    /// Clones only the winner and resolves its deferred selected-wake probe.
    fn finish_runnable_selection<const MEASURE: bool>(
        &self,
        agent_id: &AgentId,
        prompt_index: Option<usize>,
        initial_prompt_correlation: Option<&InitialPromptCorrelation>,
        known_ready_message_wake: Option<bool>,
        work: &mut RunnableSelectionWork,
    ) -> RunnableAgentSelection {
        let had_ready_message_wake = if initial_prompt_correlation.is_some() {
            false
        } else if let Some(ready) = known_ready_message_wake {
            ready
        } else {
            let probe = self.selected_branch_wake_probe(agent_id);
            if MEASURE {
                work.wake_probes += 1;
                if let Some(probe) = &probe {
                    work.wake_visits += probe.wakes;
                    work.wake_branch_visits += probe.branch_nodes;
                    work.wake_probe_buffers += probe.owned_buffers;
                }
            }
            probe.is_some_and(|probe| probe.ready)
        };
        RunnableAgentSelection {
            agent_id: agent_id.clone(),
            prompt_index,
            initial_prompt_correlation: initial_prompt_correlation.cloned(),
            had_ready_message_wake,
        }
    }

    fn pop_runnable_prompt_at(
        &mut self,
        agent_id: &AgentId,
        index: Option<usize>,
    ) -> Option<PendingPrompt> {
        let conv = self.agent_runtime.agent_registry.agents.get_mut(agent_id)?;
        conv.dispatch.pending_prompts.remove(index?)
    }

    /// Project one accepted durable-agent activation into its session's
    /// best-effort retention hint, timing only authenticated UI prompt
    /// dispatch.
    fn record_durable_agent_session_activity(
        &mut self,
        agent_id: &AgentId,
        trace_human_ui_prompt_acceptance: bool,
    ) {
        let Some(session_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(agent_id)
            .and_then(|agent| {
                agent
                    .identity
                    .persistence
                    .is_durable()
                    .then(|| agent.identity.session_id.clone())
            })
        else {
            return;
        };
        if trace_human_ui_prompt_acceptance {
            let started = Instant::now();
            let result = self
                .session_runtime
                .store
                .record_session_activity(session_id.as_str());
            tracing::trace!(
                target: "tau_harness::prompt_acceptance",
                stage = "session_meta_touch",
                agent_id = %agent_id,
                result_class = if result.is_ok() { "success" } else { "failure" },
                session_meta_touch_us = started.elapsed().as_micros(),
                "content-free prompt acceptance precursor"
            );
        } else {
            let _ = self
                .session_runtime
                .store
                .record_session_activity(session_id.as_str());
        }
    }

    /// True when a fresh prompt for one agent should *not* be sent
    /// immediately. Two layers of gating:
    /// - global: selected role has no resolved model, harness mid-init,
    ///   extensions not yet `Ready`;
    /// - per-agent: that agent already has a prompt in flight, is waiting on
    ///   tool results, or has a latent dispatch parked behind interception.
    pub(crate) fn dispatch_blocked_for(&self, agent_id: &AgentId) -> bool {
        if self.config.selected_model.is_none()
            || !self.session_runtime.turn_state.is_idle()
            || self.extensions.resolving_initial_collisions
            || !self.extensions_all_ready()
            || !self.agent_context_ready_for(agent_id)
        {
            return true;
        }
        match self.agent_runtime.agent_registry.agents.get(agent_id) {
            Some(conv) => {
                conv.dispatch.terminating
                    || conv.dispatch.in_flight_prompt.is_some()
                    || !matches!(
                        conv.dispatch.activation_dispatch,
                        crate::agent::ActivationDispatchState::None
                    )
                    || !matches!(conv.turn.turn_state, AgentTurnState::Idle)
                    || self.has_deferred_prompt_dispatch_for(agent_id)
                    || self.agent_has_open_foreground_tool_round(agent_id)
            }
            None => true,
        }
    }
}

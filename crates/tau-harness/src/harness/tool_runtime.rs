//! Owns tool invocation, background completion, tool-turn settlement, and loop
//! guards.
//!
//! The provider terminal remains canonical before this runtime settles tool and
//! UI continuations.

use super::*;

/// Runtime-only ownership and coordination state for tool calls in the active
/// session.
///
/// Provider response and publication coordination remain separate because they
/// have independent lifecycles and commit authority. Session rollover clears
/// live ownership, scheduler state, and same-session completion tombstones
/// explicitly; canonical terminals remove individual calls only after commit.
/// This type has no drop side effects. Explicit provider-disconnect, session,
/// and agent teardown paths own cancellation and terminal publication; merely
/// dropping this state drops its collections without those effects.
#[derive(Default)]
pub(crate) struct ToolRuntimeState {
    /// Owning transcript agent for every tool call currently in flight.
    pub(crate) tool_agents: HashMap<ToolCallId, AgentId>,
    /// Tool metadata retained while a call awaits its terminal fact.
    pub(crate) pending_tools: HashMap<ToolCallId, PendingTool>,
    /// Envelope identities reserved for provider declarations awaiting commit.
    pub(super) pending_declaration_observations: HashMap<AgentPromptId, tau_proto::ObservationId>,
    /// Envelope identities reserved for canonical tool terminals awaiting
    /// commit.
    pub(super) pending_terminal_observations: HashMap<ToolCallId, PendingTerminalObservation>,
    /// Wait settlements retained until their canonical terminal commits.
    pub(super) pending_wait_settlements: HashMap<ToolCallId, subagents_tool::PendingWaitSettlement>,
    /// Calls whose committed terminal clears runtime state without advancing a
    /// turn.
    pub(super) post_commit_runtime_only_tool_terminals: HashSet<ToolCallId>,
    /// Prompt policy retained until each background terminal commits.
    pub(super) pending_background_completion_modes:
        HashMap<ToolCallId, BackgroundCompletionPromptMode>,
    /// First accepted cancellation observation for each live target.
    pub(super) pending_cancellation_observations: HashMap<ToolCallId, tau_proto::ObservationId>,
    /// Ownerless calls admitted from committed configured-peer requests.
    pub(super) peer_tool_requests: HashSet<ToolCallId>,
    /// Loaded-agent correlation for peer requests routed to internal tools.
    ///
    /// These requests do not own transcript branches, so they must remain
    /// separate from `tool_agents`.
    pub(crate) peer_internal_tool_agents: HashMap<ToolCallId, AgentId>,
    /// Tool calls known to have reached a terminal state in this session.
    ///
    /// This supplies same-session known-id checks after live ownership is gone.
    pub(super) completed_tool_calls: HashSet<ToolCallId>,
    /// Completed calls whose ephemeral owners exclude them from durable debug
    /// logs.
    pub(super) completed_ephemeral_tool_calls: HashSet<ToolCallId>,
    /// Known owning agents for completed calls.
    ///
    /// Internal tools use this retained authority to scope late cancellation
    /// diagnostics to the original conversation.
    pub(super) completed_tool_agents: HashMap<ToolCallId, AgentId>,
    /// Extension connection currently servicing each in-flight call.
    ///
    /// Cancellation must return to this exact selected provider rather than
    /// resolving the tool registry again after routes change.
    pub(super) pending_tool_providers: HashMap<ToolCallId, tau_proto::ConnectionId>,
    /// Foreground and background tool-round state machine.
    pub(crate) tool_turn: ToolTurnMachine,
    /// Calls whose background terminal suppresses a generic completion prompt.
    pub(super) suppressed_background_completion_prompts: HashSet<ToolCallId>,
    /// Owning agents for background completions that can outlive a foreground
    /// turn.
    pub(super) background_completion_targets: HashMap<ToolCallId, AgentId>,
}

impl Harness {
    /// Returns the effective foreground/background support for a tool name.
    /// Missing registration metadata uses the protocol default of
    /// `MinForegroundSeconds(2)`.
    pub(super) fn resolve_tool_background_support(&self, name: &str) -> BackgroundSupport {
        self.registry
            .resolve_provider(name)
            .and_then(|provider| provider.tool.background_support)
            .unwrap_or_else(BackgroundSupport::default_effective)
    }

    /// Drain scheduler-selected tool invocations into harness side effects.
    pub(super) fn drain_pending_tool_invocations(&mut self) -> Result<(), HarnessError> {
        while let Some(next) = self.tool_runtime.tool_turn.next_dispatchable().cloned() {
            if self.tool_call_waits_for_staged_registration(
                &next.conversation_id,
                &next.invocation.name,
                self.prompt_runtime
                    .tool_call_prompts
                    .get(&next.invocation.id),
            ) {
                break;
            }
            let Some((
                PendingToolInvocation {
                    conversation_id,
                    invocation,
                    background_support: _,
                    source,
                    turn_categories: _,
                },
                foreground_action,
            )) = self.tool_runtime.tool_turn.pop_dispatchable(Instant::now())
            else {
                break;
            };
            let call_id = invocation.id.clone();
            if let Some(call) = invocation.call_ref {
                self.record_wait_tool_call_ref(call_id.clone(), call);
                self.append_best_effort_observation(
                    &conversation_id,
                    tau_proto::ObservationId::random(),
                    Event::AgentToolDispatchObserved(tau_proto::AgentToolDispatchObserved { call }),
                );
            }
            // If dispatch fails synchronously, roll back the in-flight
            // entry so a retry or clean-up is not wedged on a phantom
            // slot.
            if let Err(error) =
                self.execute_agent_tool_call_from(&conversation_id, &invocation, source.as_ref())
            {
                self.tool_runtime.tool_turn.rollback_dispatch(&call_id);
                return Err(error);
            }
            self.apply_foreground_action(foreground_action);
        }
        Ok(())
    }

    pub(super) fn apply_foreground_action(&mut self, action: ForegroundAction) {
        match action {
            ForegroundAction::None => {}
            ForegroundAction::Background { call_id } => {
                if self.tool_runtime.tool_turn.begin_backgrounding(&call_id) {
                    self.observe_tool_backgrounded(&call_id);
                    self.publish_synthetic_background_result(&call_id);
                }
            }
        }
    }

    /// Observe a live background-transition decision before publishing its
    /// foreground placeholder.
    pub(crate) fn observe_tool_backgrounded(&mut self, call_id: &ToolCallId) {
        self.provider_runtime
            .cache_refresh_tool_window_calls
            .remove(call_id);
        if self
            .provider_runtime
            .cache_refresh_tool_window_calls
            .is_empty()
        {
            let cancellations = self.provider_runtime.cache_residency.close_window();
            self.send_cache_refresh_cancellations(cancellations);
        }
        let Some(call) = self.wait_tool_call_ref(call_id) else {
            return;
        };
        let Some(owner) = self
            .tool_runtime
            .tool_agents
            .get(call_id)
            .or_else(|| self.tool_runtime.peer_internal_tool_agents.get(call_id))
            .cloned()
        else {
            return;
        };
        self.append_best_effort_observation(
            &owner,
            tau_proto::ObservationId::random(),
            Event::AgentToolBackgroundedObserved(tau_proto::AgentToolBackgroundedObserved { call }),
        );
    }

    pub(super) fn publish_synthetic_background_result(&mut self, call_id: &ToolCallId) {
        self.publish_synthetic_background_result_inner(call_id, None);
    }

    pub(crate) fn publish_internal_background_placeholder(
        &mut self,
        call_id: &ToolCallId,
        result: CborValue,
    ) {
        let Some(cid) = self
            .tool_runtime
            .tool_agents
            .get(call_id)
            .or_else(|| self.tool_runtime.peer_internal_tool_agents.get(call_id))
            .cloned()
        else {
            return;
        };
        let Some(tool) = self.tool_runtime.pending_tools.get(call_id).cloned() else {
            return;
        };
        let result = ToolResult {
            presentation: Default::default(),
            call_id: call_id.clone(),
            tool_name: tool.name,
            tool_type: tool.tool_type,
            result,
            provider_content: Vec::new(),
            kind: ToolResultKind::BackgroundPlaceholder,
            originator: PromptOriginator::User,

            display: None,
        };
        if self
            .tool_runtime
            .peer_internal_tool_agents
            .contains_key(call_id)
        {
            // Peer-internal agent correlation is runtime-only: publish the
            // placeholder without transcript ownership.
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ProviderToolResult(result.clone()),
            );
        } else {
            self.publish_for_agent(&cid, Event::ProviderToolResult(result.clone()));
        }
    }

    pub(super) fn publish_synthetic_background_result_inner(
        &mut self,
        call_id: &ToolCallId,
        agent_ids: Option<(&str, &str)>,
    ) {
        let Some(cid) = self.tool_runtime.tool_agents.get(call_id).cloned() else {
            return;
        };
        let Some(tool) = self.tool_runtime.pending_tools.get(call_id).cloned() else {
            return;
        };
        let agent_id_headers = agent_ids
            .map(|(self_agent_id, sub_agent_id)| {
                format!("self_agent_id: {self_agent_id}\nsub_agent_id: {sub_agent_id}\n")
            })
            .unwrap_or_default();
        let content = format!(
            "{}: true\n{agent_id_headers}\nTool call `{call_id}` is running in the background.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        );
        let result = ToolResult {
            presentation: Default::default(),
            call_id: call_id.clone(),
            tool_name: tool.name,
            tool_type: tool.tool_type,
            result: CborValue::Text(content),
            provider_content: Vec::new(),
            kind: ToolResultKind::BackgroundPlaceholder,
            originator: PromptOriginator::User,

            display: None,
        };
        self.publish_for_agent(&cid, Event::ProviderToolResult(result.clone()));
    }

    pub(super) fn process_background_deadlines_at(&mut self, now: Instant) {
        for call_id in self.tool_runtime.tool_turn.background_due(now) {
            if self.tool_runtime.tool_turn.begin_backgrounding(&call_id) {
                self.observe_tool_backgrounded(&call_id);
                self.publish_synthetic_background_result(&call_id);
            }
        }
    }

    pub(crate) fn on_tool_call_foreground_complete(&mut self, call_id: &str) {
        let owner = self.tool_runtime.tool_agents.get(call_id).cloned();
        if let Some(cid) = owner.as_ref() {
            self.emit_agent_stats_updated(cid);
        }
        self.drain_pending_tool_invocations_or_report();
        self.maybe_complete_agent_turn(call_id);
        if let Some(cid) = owner {
            self.repair_closed_foreground_tool_turn(&cid, &ToolCallId::from(call_id));
        }
        self.try_advance_queue();
    }

    pub(super) fn drain_pending_tool_invocations_or_report(&mut self) {
        if let Err(error) = self.drain_pending_tool_invocations() {
            self.emit_harness_failure(&format!("queued tool dispatch failed: {error}"));
        }
    }

    pub(super) fn handle_background_tool_result(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        result: ToolResult,
    ) {
        self.handle_background_tool_result_inner(
            source_id,
            result,
            BackgroundCompletionPromptMode::QueueAndAdvance,
        );
    }

    pub(super) fn handle_background_tool_result_inner(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        mut result: ToolResult,
        completion_prompt_mode: BackgroundCompletionPromptMode,
    ) {
        let peer_internal = self
            .tool_runtime
            .peer_internal_tool_agents
            .contains_key(&result.call_id);
        let Some(cid) = self
            .tool_runtime
            .tool_agents
            .get(&result.call_id)
            .or_else(|| {
                self.tool_runtime
                    .peer_internal_tool_agents
                    .get(&result.call_id)
            })
            .cloned()
        else {
            return;
        };
        let call_id = result.call_id.clone();
        if let Some(tool) = self.tool_runtime.pending_tools.get(&result.call_id) {
            tool.restore_terminal_result_metadata(&mut result);
        }
        let background = ToolBackgroundResult {
            call_id: result.call_id,
            tool_name: result.tool_name,
            tool_type: result.tool_type,
            result: result.result,
            display: result.display,
            originator: result.originator,
        };
        if peer_internal {
            // Settle ownerless runtime/wait state without creating transcript or
            // background-completion-prompt ownership.
            self.publish_event(
                Some(source_id),
                Event::ToolBackgroundResult(background.clone()),
            );
            self.record_wait_background_result(background, None);
            self.finish_harness_owned_tool_tracking(&call_id);
            return;
        }
        self.observe_tool_terminal(&cid, &call_id, tau_proto::ToolTerminalCause::Completed);
        self.tool_runtime
            .pending_background_completion_modes
            .insert(call_id, completion_prompt_mode);
        self.publish_for_agent_from(
            &cid,
            Some(source_id),
            Event::ToolBackgroundResult(background),
        );
    }

    pub(super) fn handle_background_tool_error(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        error: ToolError,
    ) {
        self.handle_background_tool_error_inner(
            source,
            error,
            BackgroundCompletionPromptMode::QueueAndAdvance,
            tau_proto::ToolTerminalCause::ToolError,
        );
    }

    pub(super) fn handle_background_tool_cancelled(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        cancelled: ToolCancelled,
    ) {
        let cause = self
            .tool_runtime
            .pending_cancellation_observations
            .get(&cancelled.call_id)
            .copied()
            .map_or(tau_proto::ToolTerminalCause::Unknown, |request| {
                tau_proto::ToolTerminalCause::Cancellation { request }
            });
        let error = ToolError {
            presentation: Default::default(),
            call_id: cancelled.call_id,
            tool_name: cancelled.tool_name,
            tool_type: cancelled.tool_type,
            message: "Tool cancelled".to_owned(),
            details: None,
            display: cancelled.display,
            originator: PromptOriginator::User,
        };
        self.handle_background_tool_error_inner(
            Some(source_id),
            error,
            BackgroundCompletionPromptMode::QueueAndAdvance,
            cause,
        );
    }
    pub(super) fn handle_background_tool_error_inner(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        mut error: ToolError,
        completion_prompt_mode: BackgroundCompletionPromptMode,
        cause: tau_proto::ToolTerminalCause,
    ) {
        let peer_internal = self
            .tool_runtime
            .peer_internal_tool_agents
            .contains_key(&error.call_id);
        let Some(cid) = self
            .tool_runtime
            .tool_agents
            .get(&error.call_id)
            .or_else(|| {
                self.tool_runtime
                    .peer_internal_tool_agents
                    .get(&error.call_id)
            })
            .cloned()
        else {
            return;
        };
        let call_id = error.call_id.clone();
        if let Some(tool) = self.tool_runtime.pending_tools.get(&error.call_id) {
            error.tool_name = tool.name.clone();
            error.tool_type = tool.tool_type;
        }
        let background = ToolBackgroundError {
            call_id: error.call_id,
            tool_name: error.tool_name,
            tool_type: error.tool_type,
            message: error.message,
            details: error.details,
            display: error.display,
            originator: error.originator,
        };
        if peer_internal {
            // Settle ownerless runtime/wait state without creating transcript or
            // background-completion-prompt ownership.
            self.publish_event(source, Event::ToolBackgroundError(background.clone()));
            self.record_wait_background_error(background, None);
            self.finish_harness_owned_tool_tracking(&call_id);
            return;
        }
        self.observe_tool_terminal(&cid, &call_id, cause);
        self.tool_runtime
            .pending_background_completion_modes
            .insert(call_id, completion_prompt_mode);
        self.publish_for_agent_from(&cid, source, Event::ToolBackgroundError(background));
    }

    /// Apply dependent runtime effects only after a canonical background
    /// terminal has committed.
    pub(super) fn finish_committed_background_completion(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
        completion_prompt_mode: BackgroundCompletionPromptMode,
    ) {
        self.tool_runtime
            .background_completion_targets
            .insert(call_id.clone(), cid.clone());
        self.reset_loop_guard_for_progress(cid);
        match completion_prompt_mode {
            BackgroundCompletionPromptMode::QueueAndAdvance => {
                self.queue_background_completion_prompt(cid, call_id);
                // Keep the completion prompt queued before draining. If an unblocked
                // queued call closes the tool round, `maybe_complete_agent_turn` can
                // fold this background notification into that follow-up prompt.
                self.drain_pending_tool_invocations_or_report();
            }
            BackgroundCompletionPromptMode::QueueOnly => {
                self.queue_background_completion_prompt_without_advancing(cid, call_id);
            }
            BackgroundCompletionPromptMode::QueuePassive => {
                self.queue_passive_background_completion_prompt(cid, call_id);
            }
            BackgroundCompletionPromptMode::DoNotQueue => {}
        }
        self.clear_tool_call_tracking(call_id.as_str());
    }

    pub(super) fn queue_background_completion_prompt(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
    ) {
        self.queue_background_completion_prompt_inner(cid, call_id, true);
    }

    pub(super) fn queue_background_completion_prompt_without_advancing(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
    ) {
        self.queue_background_completion_prompt_inner(cid, call_id, false);
    }

    pub(super) fn queue_passive_background_completion_prompt(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
    ) {
        self.queue_background_completion_prompt_inner_with(cid, call_id, false, |prompt| {
            PendingPrompt::passive_background_completion(prompt)
        });
    }

    pub(super) fn queue_background_completion_prompt_inner(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
        advance_queue: bool,
    ) {
        self.queue_background_completion_prompt_inner_with(
            cid,
            call_id,
            advance_queue,
            PendingPrompt::activating_background_completion,
        );
    }

    pub(super) fn queue_background_completion_prompt_inner_with(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
        advance_queue: bool,
        make_prompt: impl FnOnce(String) -> PendingPrompt,
    ) {
        if self
            .tool_runtime
            .suppressed_background_completion_prompts
            .contains(call_id)
        {
            return;
        }
        let prompt = background_completion_prompt(call_id);
        let activation = tau_proto::ObservationId::random();
        let queued = if let Some(conv) = self.agent_registry.agents.get_mut(cid) {
            if conv
                .pending_prompts
                .iter()
                .any(|pending| pending.text == prompt)
            {
                return;
            }
            let mut prompt = make_prompt(prompt);
            let inference_activation = prompt.creates_inference_activation();
            prompt.activation_observation = inference_activation.then_some(activation);
            conv.pending_prompts.push_back(prompt);
            inference_activation
        } else {
            false
        };
        if queued {
            self.append_activation_queued(
                cid,
                activation,
                tau_proto::ActivationKind::BackgroundCompletion,
                self.wait_tool_terminal_observation(call_id),
                self.wait_tool_call_ref(call_id),
            );
            self.activate_waits_for(cid, activation);
        }
        if advance_queue {
            self.try_advance_queue();
        }
    }

    pub(super) fn queue_existing_passive_background_completion_prompt(
        &mut self,
        call_id: &ToolCallId,
    ) {
        self.tool_runtime
            .suppressed_background_completion_prompts
            .remove(call_id);
        if let Some(cid) = self
            .tool_runtime
            .background_completion_targets
            .get(call_id)
            .cloned()
        {
            self.queue_passive_background_completion_prompt(&cid, call_id);
        }
    }

    pub(super) fn suppress_background_completion_prompt(&mut self, call_id: ToolCallId) {
        self.tool_runtime
            .suppressed_background_completion_prompts
            .insert(call_id.clone());
        let prompt = background_completion_prompt(&call_id);
        for conv in self.agent_registry.agents.values_mut() {
            conv.pending_prompts
                .retain(|pending| pending.text != prompt);
        }
    }

    pub(super) fn unsuppress_background_completion_prompt(&mut self, call_id: ToolCallId) {
        self.tool_runtime
            .suppressed_background_completion_prompts
            .remove(&call_id);
        if let Some(cid) = self
            .tool_runtime
            .background_completion_targets
            .get(&call_id)
            .cloned()
        {
            self.queue_background_completion_prompt(&cid, &call_id);
        }
    }

    pub(super) fn retire_background_work_before_agent_unload(&mut self, cid: &AgentId) {
        let call_ids = self.background_completion_call_ids_for_teardown(cid);
        self.cancel_remaining_tool_calls(
            cid,
            call_ids.into_iter().collect(),
            BackgroundCompletionPromptMode::DoNotQueue,
        );
        self.discard_background_completion_target_before_teardown(cid);
    }

    pub(super) fn discard_background_completion_target_before_teardown(&mut self, cid: &AgentId) {
        for call_id in self.background_completion_call_ids_for_teardown(cid) {
            self.tool_runtime
                .suppressed_background_completion_prompts
                .remove(&call_id);
            self.tool_runtime
                .background_completion_targets
                .remove(&call_id);
            self.clear_tool_call_tracking(call_id.as_str());
        }
        for call_id in self.discard_wait_owner_before_teardown(cid) {
            self.clear_tool_call_tracking(call_id.as_str());
        }
    }

    pub(super) fn background_completion_call_ids_for_teardown(
        &self,
        cid: &AgentId,
    ) -> HashSet<ToolCallId> {
        let mut call_ids: HashSet<ToolCallId> = self
            .tool_runtime
            .tool_turn
            .backgrounded_calls_for(cid)
            .into_iter()
            .filter(|call_id| self.tool_runtime.peer_internal_tool_agents.get(call_id) != Some(cid))
            .collect();
        call_ids.extend(
            self.tool_runtime
                .tool_agents
                .iter()
                .filter_map(|(call_id, owner)| {
                    (owner == cid && self.tool_runtime.tool_turn.is_backgrounded(call_id))
                        .then_some(call_id.clone())
                }),
        );
        call_ids.extend(
            self.tool_runtime
                .background_completion_targets
                .iter()
                .filter_map(|(call_id, owner)| (owner == cid).then_some(call_id.clone())),
        );
        call_ids
    }

    /// Hook called whenever a tool call has finished (result, error,
    /// synthetic NoProvider error, or inline skill completion). Removes
    /// it from the in-flight set, drains any freshly-eligible queued
    /// calls, and then checks whether the turn is done.
    pub(crate) fn on_tool_call_complete(&mut self, call_id: &str) {
        self.on_tool_call_complete_inner(call_id, true);
    }

    pub(super) fn on_tool_call_complete_inner(&mut self, call_id: &str, drain_queued: bool) {
        let owner = self.finish_tool_call_runtime_state(call_id);
        if drain_queued {
            self.drain_pending_tool_invocations_or_report();
        }
        if let Some(cid) = owner {
            self.maybe_complete_agent_turn_for(&cid, call_id);
        }
        self.try_advance_queue();
    }

    pub(super) fn finish_tool_call_runtime_state(&mut self, call_id: &str) -> Option<AgentId> {
        let owned: ToolCallId = call_id.to_owned().into();
        self.tool_runtime.tool_turn.mark_complete(&owned);
        // `tool_agents` is still populated here: the call
        // sites clear it *after* this function returns. Decrement
        // the agent's in-flight counter and surface the new
        // state to any UI watching this agent before the
        // mapping is cleared.
        let owner = self.tool_runtime.tool_agents.get(call_id).cloned();
        if let Some(cid) = owner.as_ref()
            && let Some(conv) = self.agent_registry.agents.get_mut(cid)
        {
            conv.tools_in_flight = conv.tools_in_flight.saturating_sub(1);
        }
        if let Some(cid) = owner.as_ref() {
            self.emit_agent_stats_updated(cid);
        }
        owner
    }

    /// Bump the per-agent tool counters for a freshly-started
    /// tool call. Emits a generic stats snapshot so watched-agent UI updates
    /// the moment an agent starts a new call rather than waiting for
    /// completion.
    pub(crate) fn bump_tools_started_for(&mut self, cid: &AgentId) {
        if let Some(conv) = self.agent_registry.agents.get_mut(cid) {
            conv.tools_in_flight = conv.tools_in_flight.saturating_add(1);
            conv.tools_total = conv.tools_total.saturating_add(1);
        }
        self.emit_agent_stats_updated(cid);
    }

    pub(super) fn maybe_complete_agent_turn(&mut self, completed_call_id: &str) {
        let Some(cid) = self
            .tool_runtime
            .tool_agents
            .get(completed_call_id)
            .cloned()
        else {
            return;
        };
        self.maybe_complete_agent_turn_for(&cid, completed_call_id);
    }

    pub(super) fn maybe_complete_agent_turn_for(&mut self, cid: &AgentId, completed_call_id: &str) {
        let should_send = if let Some(conv) = self.agent_registry.agents.get_mut(cid) {
            if let AgentTurnState::ToolsRunning { remaining_calls } = &mut conv.turn_state {
                remaining_calls.retain(|id| id.as_str() != completed_call_id);
                if remaining_calls.is_empty() {
                    conv.turn_state = AgentTurnState::Idle;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if should_send {
            self.queue_working_reminder_if_needed(cid);
            let pending_ui = self.compaction_runtime.pending_ui_after_wait.remove(cid);
            if let Some(pending) = pending_ui {
                let remains_valid = pending.wait_call_id.as_str() == completed_call_id
                    && pending.session_generation == self.current_session_generation
                    && self.agent_registry.agents.get(cid).is_some_and(|agent| {
                        !agent.terminating
                            && agent.agent_id.as_deref() == Some(pending.agent_id.as_str())
                            && matches!(agent.turn_state, AgentTurnState::Idle)
                    });
                if remains_valid {
                    self.handle_compact_request(
                        &pending.requester_client_id,
                        self.current_session_id.clone(),
                        Some(pending.agent_id.as_str()),
                    );
                    return;
                }
                self.send_ui_error_response(
                    &pending.requester_client_id,
                    "compaction canceled because deferred continuation became stale",
                );
            }
            let deferred_request = self
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.agent_id.as_deref())
                .and_then(|agent_id| {
                    self.compaction_runtime
                        .accepted_manual_tools
                        .iter()
                        .find_map(|(request_id, accepted)| {
                            (accepted.request.resume_inference
                                && accepted.request.target_agent_id.as_str() == agent_id)
                                .then_some(request_id.clone())
                        })
                });
            if let Some(request_id) = deferred_request
                && self.start_accepted_manual_compaction(cid, &request_id)
            {
                return;
            }
            self.resolve_materialized_message_wakes(cid);
            let has_ready_message_wake = self.has_ready_message_wake_on_selected_branch(cid);
            if self
                .agent_registry
                .agents
                .get(cid)
                .is_some_and(|conv| conv.loop_guard.stop_automatic_continuation())
                && let Some(conv) = self.agent_registry.agents.get_mut(cid)
            {
                conv.pending_prompts
                    .retain(|prompt| !prompt.is_loop_guard());
                if conv.pending_prompts.is_empty() && !has_ready_message_wake {
                    return;
                }
            }
            self.fold_pending_prompts_as_steered(cid);
            // If folding the steered prompts parked any of them in
            // interception (e.g. an extension intercepting
            // `agent.prompt_steered`), defer the agent dispatch
            // until the whole publish chain drains. Waiting for only
            // one user-message commit is not enough when several
            // steered prompts are queued behind one interceptor.
            self.dispatch_activation_after_publish_idle(cid);
        }
    }

    /// Repair a stale runtime tool projection after the durable branch and all
    /// live call owners agree that the foreground round has closed.
    ///
    /// The synthetic one-call `ToolsRunning` state is intentional:
    /// [`Harness::maybe_complete_agent_turn_for`] owns the complete
    /// continuation seam, including reminders, compaction, wakes, steers,
    /// and retained publications. Setting `Idle` directly would bypass
    /// those obligations.
    pub(super) fn repair_closed_foreground_tool_turn(
        &mut self,
        cid: &AgentId,
        completed_call_id: &ToolCallId,
    ) {
        let projected_running =
            self.agent_registry.agents.get(cid).is_some_and(|agent| {
                matches!(agent.turn_state, AgentTurnState::ToolsRunning { .. })
            });
        let live_foreground_call = self
            .tool_runtime
            .tool_agents
            .iter()
            .any(|(call_id, owner)| {
                owner == cid && !self.tool_runtime.tool_turn.is_backgrounded(call_id)
            });
        if !projected_running
            || live_foreground_call
            || self.agent_has_open_foreground_tool_round(cid)
        {
            return;
        }

        tracing::warn!(
            target: "tau_harness",
            conversation_id = %cid,
            call_id = %completed_call_id,
            "repairing closed foreground tool round left in the runtime projection"
        );
        if let Some(agent) = self.agent_registry.agents.get_mut(cid) {
            agent.turn_state = AgentTurnState::ToolsRunning {
                remaining_calls: vec![completed_call_id.clone()],
            };
        }
        self.maybe_complete_agent_turn_for(cid, completed_call_id.as_str());
    }

    /// Fold one pending Working reminder into the complete foreground
    /// tool-round continuation after every parallel terminal has settled.
    pub(super) fn queue_working_reminder_if_needed(&mut self, cid: &AgentId) {
        let Some(agent) = self.agent_registry.agents.get_mut(cid) else {
            return;
        };
        if agent.lifecycle_notification_only_turn {
            agent.work_status.clear_working_reminder();
            return;
        }
        if !agent.work_status.take_working_reminder() {
            return;
        }
        agent
            .pending_prompts
            .push_back(PendingPrompt::internal(STATUS_REMINDER.to_owned()));
    }

    pub(super) fn publish_prompts_as_steered(
        &mut self,
        cid: &AgentId,
        prompts: Vec<PendingPrompt>,
        completion: Option<AgentPublishCompletion>,
    ) {
        let prompt_count = prompts.len();
        let retry_prompts = prompts.clone();
        for (index, prompt) in prompts.into_iter().enumerate() {
            self.promote_lifecycle_notification_turn(cid);
            let agent_id = self
                .agent_registry
                .agents
                .get(cid)
                .and_then(|conv| conv.agent_id.clone())
                .expect("agent has durable id");
            let notify_watchers = prompt.should_notify_watchers();
            let inference_activation = prompt.creates_inference_activation();
            let internal_kind = prompt.internal_kind();
            let event_completion = prompt
                .initial_prompt_correlation
                .clone()
                .map(|correlation| AgentPublishCompletion::InitialPromptSubmission { correlation })
                .or_else(|| {
                    completion.clone().map(|mut completion| {
                        if let AgentPublishCompletion::StandaloneContinuation {
                            retry_prompts: suffix,
                            complete_on_commit,
                            approved_retry_event,
                            ..
                        } = &mut completion
                        {
                            *suffix = retry_prompts[index..].to_vec();
                            *complete_on_commit = index + 1 == prompt_count;
                            *approved_retry_event = None;
                        }
                        completion
                    })
                });
            let event = Event::AgentPromptSteered(tau_proto::AgentPromptSteered {
                inference_activation,
                submission_source: prompt.submission_source,
                agent_id: crate::parse_agent_id(&agent_id),
                text: prompt.text,
                trusted_internal_spans: prompt.trusted_internal_spans,
                message_class: prompt.message_class,
                self_compaction_terminal: prompt.self_compaction_terminal,
                internal_kind,
                ctx_id: prompt.ctx_id,
            });
            self.publish_event_for_agent_with_completion(
                cid,
                None,
                event,
                event_completion,
                notify_watchers,
            );
            if self
                .prompt_runtime
                .pending_publish_completions
                .contains_key(cid)
            {
                break;
            }
        }
    }

    /// Drain any prompts queued on `cid` while the agent was in
    /// flight, and publish a `AgentPromptSteered` event for each. The
    /// folder in `AgentTree::apply_event` appends them as
    /// `UserMessage` entries on this agent's branch, so the
    /// next-round `AgentPromptCreated` (about to be emitted by the
    /// caller) picks them up alongside the tool results without any
    /// extra wiring on the prompt-assembly side.
    ///
    /// Called from `maybe_complete_agent_turn` only — fresh prompts
    /// arriving on an idle conversation go through
    /// `dispatch_prompt_for_agent`, which already publishes its
    /// own `AgentPromptSubmitted`. Folding here exists specifically to
    /// give queued prompts a chance to ride the next per-round prompt
    /// rather than waiting for the whole turn to terminate.
    pub(super) fn fold_pending_prompts_as_steered(&mut self, cid: &AgentId) {
        self.fold_pending_prompts_as_steered_with_completion(cid, None);
    }

    pub(super) fn fold_pending_prompts_as_steered_with_completion(
        &mut self,
        cid: &AgentId,
        completion: Option<AgentPublishCompletion>,
    ) -> bool {
        let mut pending: Vec<PendingPrompt> = self
            .agent_registry
            .agents
            .get_mut(cid)
            .map(|c| c.pending_prompts.drain(..).collect())
            .unwrap_or_default();
        // These markers request a turn only; their payload is already folded by
        // the canonical incoming fact.
        if let Some(user_prompt_pos) = pending.iter().position(|prompt| !prompt.is_internal()) {
            self.reset_loop_guard_for_progress(cid);
            pending.retain(|prompt| !prompt.is_loop_guard());
            let restore_prompts = self.take_pending_restore_prompts_for_user_prompt(cid);
            if !restore_prompts.is_empty() {
                pending.splice(user_prompt_pos..user_prompt_pos, restore_prompts);
            }
        } else {
            let mut active = Vec::new();
            let mut passive = Vec::new();
            for prompt in pending {
                if prompt.is_passive_background_completion() {
                    passive.push(prompt);
                } else {
                    active.push(prompt);
                }
            }
            if !passive.is_empty()
                && let Some(conv) = self.agent_registry.agents.get_mut(cid)
            {
                for prompt in passive.into_iter().rev() {
                    conv.pending_prompts.push_front(prompt);
                }
            }
            pending = active;
        }
        if pending.is_empty() {
            return false;
        }
        if pending.iter().any(PendingPrompt::is_loop_guard) {
            self.mark_loop_guard_breakers_dispatched(cid);
        }
        pending = pending
            .into_iter()
            .filter_map(|prompt| {
                let correlation = prompt.initial_prompt_correlation.clone();
                match self.resolve_pending_user_skill_for_agent(cid, prompt) {
                    Ok(prompt) => Some(prompt),
                    Err(message) => {
                        if let Some(correlation) = correlation {
                            self.publish_initial_prompt_failed(
                                correlation,
                                tau_proto::AgentPromptFailureStage::Preprocessing,
                                &ui_create_agent::bound_create_agent_diagnostic(message),
                            );
                        }
                        None
                    }
                }
            })
            .collect();
        if pending.is_empty() {
            return false;
        }
        self.publish_prompts_as_steered(cid, pending, completion);
        true
    }

    #[cfg(test)]
    pub(super) fn reject_agent_tool_call_before_dispatch(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        tool_name: ToolName,
        message: String,
    ) {
        self.reject_agent_tool_call_before_dispatch_inner(
            cid, call, tool_name, message, true, None,
        );
    }

    pub(super) fn reject_agent_tool_call_before_dispatch_from(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        tool_name: ToolName,
        message: String,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        self.reject_agent_tool_call_before_dispatch_inner(
            cid, call, tool_name, message, true, source,
        );
    }

    pub(super) fn reject_agent_tool_call_before_dispatch_inner(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        tool_name: ToolName,
        message: String,
        complete_turn: bool,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let call_id: ToolCallId = call.id.clone();
        self.tool_runtime
            .tool_agents
            .insert(call_id.clone(), cid.clone());
        self.bump_tools_started_for(cid);
        if !complete_turn && !self.tool_terminal_has_open_durable_owner(cid, &call_id) {
            self.tool_runtime
                .post_commit_runtime_only_tool_terminals
                .insert(call_id.clone());
        }
        self.publish_terminal_tool_error(
            Some(cid),
            source,
            ToolError {
                presentation: Default::default(),
                call_id: call_id.clone(),
                tool_name,
                tool_type: call.tool_type,
                message,
                details: None,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            },
        );
    }

    pub(super) fn tool_owner_agent_id(&self, cid: &AgentId) -> AgentId {
        self.agent_registry
            .agents
            .get(cid)
            .and_then(|conv| conv.agent_id.clone())
            .map(crate::parse_agent_id)
            .unwrap_or_else(|| cid.clone())
    }

    pub(super) fn tool_owner_originator(&self, cid: &AgentId) -> PromptOriginator {
        self.agent_registry
            .agents
            .get(cid)
            .map(|conv| conv.originator.clone())
            .unwrap_or_default()
    }

    pub(super) fn reset_loop_guard_for_progress(&mut self, cid: &AgentId) {
        if let Some(conv) = self.agent_registry.agents.get_mut(cid) {
            conv.loop_guard.reset_for_progress();
            conv.pending_prompts
                .retain(|prompt| !prompt.is_loop_guard());
        }
    }

    pub(super) fn record_loop_signature(
        &mut self,
        cid: &AgentId,
        signature: LoopTurnSignature,
    ) -> Option<LoopGuardTrigger> {
        let conv = self.agent_registry.agents.get_mut(cid)?;
        let guard = &mut conv.loop_guard;
        guard.push_recent(signature.clone(), LOOP_GUARD_RECENT_LIMIT);

        let trigger = match &signature {
            LoopTurnSignature::AssistantText(text) => {
                let repeated =
                    guard.recent_repeats(&signature, LOOP_GUARD_ASSISTANT_REPEAT_THRESHOLD);
                repeated.then(|| {
                    (
                        format!("assistant:{text}"),
                        "repeated assistant response with no tool action".to_owned(),
                    )
                })
            }
            LoopTurnSignature::ToolFailure(failure) => {
                let repeated =
                    guard.repeated_tool_failure(failure, LOOP_GUARD_TOOL_FAILURE_REPEAT_THRESHOLD);
                if repeated {
                    Some((
                        format!("tool-failure:{failure}"),
                        "repeated identical failing tool call".to_owned(),
                    ))
                } else if guard.consecutive_tool_failures()
                    >= LOOP_GUARD_CONSECUTIVE_FAILURE_THRESHOLD
                {
                    Some((
                        "tool-failure-streak".to_owned(),
                        "several consecutive tool failures without a successful result".to_owned(),
                    ))
                } else {
                    None
                }
            }
        }
        .or_else(|| {
            guard.abab_suffix().map(|(a, b)| {
                (
                    format!("abab:{a:?}:{b:?}"),
                    "repeated A/B/A/B turn pattern".to_owned(),
                )
            })
        })?;

        let (cycle_key, reason) = trigger;
        Some(LoopGuardTrigger { cycle_key, reason })
    }

    pub(super) fn handle_loop_guard_trigger(
        &mut self,
        cid: &AgentId,
        cycle_key: String,
        reason: String,
    ) {
        let Some(conv) = self.agent_registry.agents.get_mut(cid) else {
            return;
        };
        if let Some(state) = conv.loop_guard.cycle_state(&cycle_key) {
            match state {
                LoopCycleState::BreakerPending => return,
                LoopCycleState::BreakerDispatched => {
                    conv.loop_guard.mark_cycle_blocked(&cycle_key);
                    self.emit_notice(
                        tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
                        tau_proto::NoticeLevel::Warning,
                        tau_proto::NoticePurpose::Alert,
                        &format!(
                            "Loop guard stopped automatic continuation for agent `{cid}` after repeated cycle: {reason}."
                        ),
                    );
                }
                LoopCycleState::Blocked => {}
            }
            return;
        }

        conv.loop_guard
            .remember_cycle_pending(cycle_key, LOOP_GUARD_CYCLE_LIMIT);
        let activation = tau_proto::ObservationId::random();
        let mut prompt = PendingPrompt::loop_guard(loop_guard_pivot_prompt(&reason));
        prompt.activation_observation = Some(activation);
        conv.pending_prompts.push_back(prompt);
        self.append_activation_queued(
            cid,
            activation,
            tau_proto::ActivationKind::LoopGuard,
            None,
            None,
        );
        self.activate_waits_for(cid, activation);
    }

    pub(super) fn mark_loop_guard_breakers_dispatched(&mut self, cid: &AgentId) {
        let Some(conv) = self.agent_registry.agents.get_mut(cid) else {
            return;
        };
        conv.loop_guard.mark_pending_breakers_dispatched();
    }

    pub(super) fn remember_tool_call_loop_signature(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
    ) {
        let Some(conv) = self.agent_registry.agents.get_mut(cid) else {
            return;
        };
        let signature = format!(
            "{}:{}",
            call.name,
            bounded_loop_text(
                &format!("{:?}", call.arguments),
                LOOP_GUARD_TOOL_ARGUMENT_CHARS
            )
        );
        conv.loop_guard.push_tool_call_signature(
            call.id.clone(),
            signature,
            LOOP_GUARD_RECENT_LIMIT,
        );
    }

    pub(super) fn take_tool_call_loop_signature(
        &mut self,
        cid: &AgentId,
        call_id: &ToolCallId,
    ) -> Option<String> {
        self.agent_registry
            .agents
            .get_mut(cid)?
            .loop_guard
            .take_tool_call_signature(call_id)
    }

    pub(super) fn record_assistant_loop_signature(&mut self, cid: &AgentId, text: Option<&str>) {
        let Some(signature_text) = text.and_then(normalize_loop_text) else {
            return;
        };
        if let Some(trigger) =
            self.record_loop_signature(cid, LoopTurnSignature::AssistantText(signature_text))
        {
            self.handle_loop_guard_trigger(cid, trigger.cycle_key, trigger.reason);
        }
    }

    pub(super) fn record_tool_failure_loop_signature(&mut self, cid: &AgentId, error: &ToolError) {
        let call_signature = self
            .take_tool_call_loop_signature(cid, &error.call_id)
            .unwrap_or_else(|| format!("{}:<arguments unavailable>", error.tool_name));
        let failure = format!(
            "{call_signature}:{}",
            bounded_loop_text(&error.message, LOOP_GUARD_TOOL_ERROR_CHARS)
        );
        if let Some(conv) = self.agent_registry.agents.get_mut(cid) {
            conv.loop_guard
                .push_tool_failure(failure.clone(), LOOP_GUARD_RECENT_LIMIT);
        }
        if let Some(trigger) =
            self.record_loop_signature(cid, LoopTurnSignature::ToolFailure(failure))
        {
            self.handle_loop_guard_trigger(cid, trigger.cycle_key, trigger.reason);
        }
    }

    #[cfg(test)]
    pub(super) fn execute_agent_tool_call(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        self.execute_agent_tool_call_from(cid, call, None)
    }

    pub(super) fn execute_agent_tool_call_from(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        source: Option<&tau_proto::ConnectionId>,
    ) -> Result<(), HarnessError> {
        let tool_name = call.name.clone();
        let role_name = self.role_name_for_agent_id(cid).to_owned();
        self.remember_tool_call_loop_signature(cid, call);

        let prompt_id = self.prompt_runtime.tool_call_prompts.get(&call.id).cloned();
        let prompt_tool_spec = prompt_id
            .as_ref()
            .map(|prompt_id| self.resolve_enabled_tool_spec_for_prompt(&tool_name, prompt_id));
        let current_role_tool_spec =
            || self.resolve_enabled_tool_spec_for_role(&tool_name, &role_name);
        let Some(tool_spec) = prompt_tool_spec.unwrap_or_else(current_role_tool_spec) else {
            let message = if prompt_id.is_some() && self.has_registered_tool_name(&tool_name) {
                prompt_snapshot_tool_error_message(&tool_name)
            } else if self.has_registered_tool_name(&tool_name) {
                disabled_tool_error_message(&tool_name)
            } else {
                let suggestion = prompt_id
                    .as_ref()
                    .and_then(|prompt_id| {
                        self.nearest_enabled_tool_name_for_prompt(&tool_name, prompt_id)
                    })
                    .or_else(|| self.nearest_enabled_tool_name_for_role(&tool_name, &role_name));
                unavailable_tool_error_message_with_suggestion(&tool_name, suggestion)
            };
            let call_id: ToolCallId = call.id.clone();
            let owner_agent_id = self.tool_owner_agent_id(cid);
            let owner_originator = self.tool_owner_originator(cid);
            self.tool_runtime
                .tool_agents
                .insert(call_id.clone(), cid.clone());
            self.tool_runtime.pending_tools.insert(
                call_id.clone(),
                PendingTool {
                    name: tool_name.clone(),
                    internal_name: tool_name.clone(),
                    tool_type: call.tool_type,
                    allows_provider_image: false,
                },
            );
            self.bump_tools_started_for(cid);
            self.record_wait_tool_request(&call_id);
            let request = ToolRequest {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                agent_id: owner_agent_id,
                originator: owner_originator.clone(),
            };
            self.publish_for_agent_from(cid, source, Event::ToolRequest(request));
            self.publish_terminal_tool_error(
                Some(cid),
                source,
                ToolError {
                    presentation: Default::default(),
                    call_id: call_id.clone(),
                    tool_name,
                    tool_type: call.tool_type,
                    message,
                    details: None,
                    originator: owner_originator,

                    display: None,
                },
            );
            return Ok(());
        };
        let internal_tool_name = tool_spec.name.clone();
        let visible_tool_name = self.tool_model_visible_name(tool_spec).clone();
        let allows_provider_image = tool_spec
            .tags
            .iter()
            .any(|tag| tag.as_str() == "provider-content:image");
        let mut arguments = call.arguments.clone();
        if self
            .registry
            .resolve_provider(&internal_tool_name)
            .is_some()
            && let Err(error) = validate_tool_arguments(tool_spec, &arguments)
        {
            if let Some(repair) = repair_tool_arguments(tool_spec, &arguments)
                && validate_tool_arguments(tool_spec, &repair.arguments).is_ok()
            {
                let repair_summary = repair.render_summary();
                tracing::info!(
                    target: "tau_harness",
                    agent_id = %cid,
                    tool_name = %visible_tool_name,
                    repairs = %repair_summary,
                    "repaired tool arguments after schema validation failure"
                );
                self.emit_notice(
                    tau_proto::notice_kind::HARNESS_NOTICE,
                    tau_proto::NoticeLevel::Info,
                    tau_proto::NoticePurpose::Diagnostic,
                    &format!(
                        "Repaired arguments for tool `{visible_tool_name}` after schema validation failure: {}.",
                        repair_summary
                    ),
                );
                arguments = repair.arguments;
            } else {
                let mut message = format!("invalid arguments for tool `{tool_name}`: {error}");
                if let Some(hint) = tool_example_hint(tool_spec, &arguments) {
                    let key = (cid.clone(), visible_tool_name.clone(), hint.clone());
                    if self.prompt_runtime.shown_tool_failure_examples.insert(key) {
                        message.push_str(&hint);
                    }
                }
                self.reject_agent_tool_call_before_dispatch_from(
                    cid,
                    call,
                    visible_tool_name,
                    message,
                    source,
                );
                return Ok(());
            }
        }

        let call_id: ToolCallId = call.id.clone();
        let owner_agent_id = self.tool_owner_agent_id(cid);
        let owner_originator = self.tool_owner_originator(cid);

        // Track conversation attribution before publishing the runtime
        // `ToolRequest`; terminal tool facts use this metadata to fold into the
        // owning agent transcript.
        self.tool_runtime
            .tool_agents
            .insert(call_id.clone(), cid.clone());
        self.tool_runtime.pending_tools.insert(
            call_id.clone(),
            PendingTool {
                name: visible_tool_name.clone(),
                internal_name: internal_tool_name.clone(),
                tool_type: call.tool_type,
                allows_provider_image,
            },
        );
        self.bump_tools_started_for(cid);
        self.record_wait_tool_request(&call_id);
        let published_request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: visible_tool_name.clone(),
            tool_type: call.tool_type,
            arguments: arguments.clone(),
            agent_id: owner_agent_id.clone(),
            originator: owner_originator.clone(),
        };
        self.publish_for_agent_from(cid, source, Event::ToolRequest(published_request));
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: internal_tool_name.clone(),
            tool_type: call.tool_type,
            arguments,
            agent_id: owner_agent_id.clone(),
            originator: owner_originator.clone(),
        };

        match self.registry.route_tool_request(request) {
            Ok(route) => {
                let status_was_available = prompt_id
                    .as_ref()
                    .and_then(|prompt_id| self.prompt_runtime.tool_specs.get(prompt_id))
                    .is_some_and(|specs| {
                        specs
                            .iter()
                            .any(|spec| self.tool_model_visible_name(spec).as_str() == "status")
                    });
                if !matches!(visible_tool_name.as_str(), "status" | "wait")
                    && self
                        .agent_registry
                        .agents
                        .get(cid)
                        .is_some_and(|agent| !agent.lifecycle_notification_only_turn)
                    && let Some(agent) = self.agent_registry.agents.get_mut(cid)
                {
                    if status_was_available {
                        agent.work_status.record_substantive_tool_admission();
                    } else {
                        agent.work_status.record_substantive_tool_progress();
                    }
                }
                let started = route.invoke;
                match route.target {
                    ToolRouteTarget::Internal => {
                        self.publish_for_agent_from(cid, source, Event::ToolStarted(started));
                    }
                    ToolRouteTarget::Extension(provider_connection_id) => {
                        self.ensure_tool_started_subscription(&provider_connection_id);
                        self.tool_runtime
                            .pending_tool_providers
                            .insert(call_id.clone(), provider_connection_id);
                        self.publish_for_agent_from(cid, source, Event::ToolStarted(started));
                    }
                }
            }
            Err(ToolRouteError::NoProvider { tool_name: _ }) => {
                let message = unavailable_tool_error_message(&visible_tool_name);
                self.publish_for_agent_from(
                    cid,
                    source,
                    Event::ToolRejected(ToolRejected {
                        call_id: call_id.clone(),
                        tool_name: visible_tool_name.clone(),
                        tool_type: call.tool_type,
                        message: message.clone(),
                        originator: tau_proto::PromptOriginator::User,
                    }),
                );
                let error = ToolError {
                    presentation: Default::default(),
                    call_id: call_id.clone(),
                    tool_name: visible_tool_name.clone(),
                    tool_type: call.tool_type,
                    message,
                    details: None,
                    originator: tau_proto::PromptOriginator::User,

                    display: None,
                };
                self.publish_terminal_tool_error(Some(cid), source, error);
            }
            Err(error) => return Err(HarnessError::ToolRoute(error)),
        }

        Ok(())
    }
}

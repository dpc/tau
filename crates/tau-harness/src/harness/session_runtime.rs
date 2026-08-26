//! Owns the bound session's prompt entry, navigation, initialization, repair,
//! switching, and cold rehydration.
//!
//! Session-generation and replay cuts remain unchanged; compaction pipelines
//! remain outside this owner.

use super::*;

pub(super) const RESTORE_NOTICE_BODY_PREFIX: &str =
    "Previous session was interrupted and restored.";

pub(super) const CHANGED_SESSION_NOTICE_BODY: &str = "This existing agent was loaded into a different session. Session-scoped tool and extension state may have changed or may not carry over; inspect current tool state and recreate timers or other session-scoped setup if still needed.";

pub(super) fn restore_notice_prompt_for_elapsed_inner(elapsed: Option<Duration>) -> String {
    let timing = elapsed.map_or_else(
        || "The state of the world might have changed since the last session.".to_owned(),
        |elapsed| {
            format!(
                "{} since the last recorded session event, and the state of the world might have changed.",
                format_restore_notice_elapsed(elapsed)
            )
        },
    );
    format!(
        "{RESTORE_NOTICE_BODY_PREFIX} {timing} Session-scoped tool and extension state may also have changed; inspect current tool state and recreate timers or other session-scoped setup if still needed."
    )
}

pub(super) fn changed_session_notice_prompt() -> String {
    CHANGED_SESSION_NOTICE_BODY.to_owned()
}

pub(super) fn restore_notice_elapsed(
    last_recorded_at: Option<tau_proto::UnixMicros>,
    now: tau_proto::UnixMicros,
) -> Option<Duration> {
    let last = last_recorded_at?;
    if last.get() == 0 || now.get() < last.get() {
        return None;
    }
    Some(Duration::from_micros(now.get() - last.get()))
}

pub(super) fn format_restore_notice_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return "Less than 1 minute has passed".to_owned();
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format_elapsed_count(minutes, "minute");
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format_elapsed_count(hours, "hour");
    }

    format_elapsed_count(hours / 24, "day")
}

pub(super) fn format_elapsed_count(count: u64, unit: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    let verb = if count == 1 { "has" } else { "have" };
    format!("{count} {unit}{suffix} {verb} passed")
}

pub(super) fn event_is_internal_prompt_text(event: &Event, text: &str) -> bool {
    match event {
        Event::AgentPromptSubmitted(prompt) => {
            prompt.message_class.is_internal() && prompt.text == text
        }
        Event::AgentPromptSteered(steered) => {
            steered.message_class.is_internal() && steered.text == text
        }
        Event::AgentUserMessageInjected(injected) => {
            injected.message_class.is_internal() && injected.text == text
        }
        _ => false,
    }
}

pub(super) fn event_is_internal_restore_notice(event: &Event) -> bool {
    match event {
        Event::AgentPromptSubmitted(prompt) => {
            prompt.message_class.is_internal() && is_restore_notice_prompt_text(&prompt.text)
        }
        Event::AgentPromptSteered(steered) => {
            steered.message_class.is_internal() && is_restore_notice_prompt_text(&steered.text)
        }
        Event::AgentUserMessageInjected(injected) => {
            injected.message_class.is_internal() && is_restore_notice_prompt_text(&injected.text)
        }
        _ => false,
    }
}

pub(super) fn restored_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

pub(super) fn restored_background_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

pub(super) fn extension_disconnected_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted because extension disconnected. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

pub(super) fn extension_disconnected_background_tool_call_error_message(
    call_id: &ToolCallId,
) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted because extension disconnected. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

/// One user-facing `:tree` prompt rewind anchor derived from durable prompt
/// provenance and resolved through the folded agent tree.
pub(super) struct PromptAnchorTarget<'a> {
    /// One-based prompt anchor shown to the user.
    pub(super) anchor: u64,
    /// Branch head selected when rewinding before this prompt.
    pub(super) head: AgentHead,
    /// Prompt node whose preview explains the anchor.
    pub(super) prompt_node: &'a tau_core::AgentNode,
}

/// Builds one-based prompt anchors from visible prompt-provenance events.
pub(super) fn prompt_anchor_targets<'a>(
    tree: &'a tau_core::AgentTree,
    agent_id: &tau_proto::AgentId,
    events: &[tau_core::PersistedAgentEvent],
) -> Vec<PromptAnchorTarget<'a>> {
    let mut replay = tau_core::AgentTree::from_events(agent_id.clone(), &[]);
    let mut anchors = Vec::new();
    for record in events {
        let is_anchor_event = is_prompt_anchor_event(agent_id, &record.event);
        let folded_node_id = replay
            .apply_persisted_record(record)
            .expect("loaded agent records already passed canonical replay validation");
        if !is_anchor_event {
            continue;
        }
        if let Some(prompt_node) = folded_node_id.and_then(|node_id| tree.node(node_id)) {
            anchors.push(PromptAnchorTarget {
                anchor: anchors.len() as u64 + 1,
                head: prompt_node
                    .parent_id
                    .map(AgentHead::Node)
                    .unwrap_or(AgentHead::Root),
                prompt_node,
            });
        }
    }
    anchors
}

/// Returns whether a durable event should receive a default `:tree` prompt
/// anchor.
///
/// Default anchors are intentionally provenance-based: visible prompts
/// submitted by the user, plus visible queued user prompts steered into an
/// in-flight turn. Synthetic injections, internal control prompts, compaction
/// triggers, and assistant/tool/message nodes remain reachable only through
/// explicit raw-node debug navigation.
pub(super) fn is_prompt_anchor_event(agent_id: &tau_proto::AgentId, event: &Event) -> bool {
    match event {
        Event::AgentPromptSubmitted(prompt) => {
            &prompt.agent_id == agent_id
                && prompt.originator.is_user()
                && !prompt.message_class.is_internal()
        }
        Event::AgentPromptSteered(steered) => {
            &steered.agent_id == agent_id && !steered.message_class.is_internal()
        }
        _ => false,
    }
}

/// Formats a branch head for concise user-facing navigation notices.
pub(super) fn format_agent_head(head: AgentHead) -> String {
    match head {
        AgentHead::Root => "root".to_owned(),
        AgentHead::Node(node_id) => format!("node {node_id}"),
    }
}

/// Runtime facts derived from one restored agent journal before the endpoint is
/// exposed to message and watch admission.
pub(super) struct RestoredAgentRuntime {
    /// Creation role when it remains available in current configuration.
    pub(super) role: Option<String>,
    /// Durable prompt owner, detached to `User` after terminal evidence.
    pub(super) originator: tau_proto::PromptOriginator,
    /// Default navigation mode reconstructed from durable ancestry.
    pub(super) navigation_mode: tau_proto::AgentNavigationMode,
    /// Durable parent identity for an outstanding parented request.
    pub(super) parent_agent: Option<tau_proto::AgentId>,
    /// Whether the immutable creator proves this was an `agent_start` tool
    /// worker.
    pub(super) tool_backed_start: bool,
    /// Whether every run-local route needed to resume the request is derivable.
    pub(super) resumable: bool,
}

/// Placement state for one activating prompt occurrence reconstructed from its
/// durable sequence during cold replay.
#[derive(Clone, Copy, Debug)]
pub(super) enum ReplayPromptActivationPlacement {
    /// Its marked inference owner has not closed, so no context node exists
    /// yet.
    Deferred,
    /// The occurrence has one materialized context node.
    Materialized(tau_proto::NodeId),
}

/// One payload-free prompt activation obligation reconstructed from the
/// journal.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReplayPromptActivationOccurrence {
    /// Durable identity and ordering key for the exact activating occurrence.
    pub(super) source_seq: tau_core::PersistedAgentEventSeq,
    /// Either the occurrence's unique folded node or an explicit statement that
    /// its unresolved marked owner has not materialized that node yet.
    pub(super) placement: ReplayPromptActivationPlacement,
}

impl ReplayPromptActivationOccurrence {
    pub(super) fn node_id(self) -> Option<tau_proto::NodeId> {
        match self.placement {
            ReplayPromptActivationPlacement::Deferred => None,
            ReplayPromptActivationPlacement::Materialized(node_id) => Some(node_id),
        }
    }
}

pub(super) fn agent_initialization_id(
    runtime_id: &tau_proto::AccountingRuntimeId,
    session_generation: u64,
    sequence: u64,
) -> tau_proto::AgentInitializationId {
    tau_proto::AgentInitializationId::parse(format!(
        "{}-{session_generation:016x}-{sequence:016x}",
        runtime_id.as_str(),
    ))
    .expect("Tau-generated agent initialization id must be valid")
}

impl Harness {
    #[cfg(test)]
    pub(super) fn submit_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HarnessError> {
        if session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; prompt for `{}` rejected",
                self.current_session_id.as_str(),
                session_id.as_str()
            );
            return Ok(PromptSubmission::Rejected { reason });
        }
        let cid = self
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
                self.try_create_durable_user_agent(session_id.clone(), &role)
            })?;
        self.preempt_blocking_ext_side_agents(&session_id);
        if !self.session_initialized(&session_id)
            || (self.selected_model.is_none() && self.provider_model_info.is_empty())
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
        {
            self.reset_loop_guard_for_progress(&cid);
            let activation = tau_proto::ObservationId::random();
            if let Some(conv) = self.agents.get_mut(&cid) {
                let mut prompt = PendingPrompt::user(text);
                prompt.activation_observation = Some(activation);
                conv.pending_prompts.push_back(prompt);
            }
            self.append_activation_queued(
                &cid,
                activation,
                tau_proto::ActivationKind::VisibleUser,
                None,
                None,
            );
            self.activate_waits_for(&cid, activation);
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        if self.dispatch_blocked_for(&cid) {
            self.reset_loop_guard_for_progress(&cid);
            let activation = tau_proto::ObservationId::random();
            if let Some(conv) = self.agents.get_mut(&cid) {
                let mut prompt = PendingPrompt::user(text);
                prompt.activation_observation = Some(activation);
                conv.pending_prompts.push_back(prompt);
            }
            self.append_activation_queued(
                &cid,
                activation,
                tau_proto::ActivationKind::VisibleUser,
                None,
                None,
            );
            self.activate_waits_for(&cid, activation);
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        let activation = tau_proto::ObservationId::random();
        self.append_activation_queued(
            &cid,
            activation,
            tau_proto::ActivationKind::VisibleUser,
            None,
            None,
        );
        let mut prompt = PendingPrompt::user(text);
        prompt.activation_observation = Some(activation);
        self.dispatch_prompt_for_agent(&cid, prompt)?;
        Ok(PromptSubmission::Dispatched)
    }

    pub(super) fn submit_prompt_to_agent(
        &mut self,
        session_id: SessionId,
        agent_id: &str,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<PromptSubmission, HarnessError> {
        let prompt = prompt.into();
        if session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; prompt for `{}` rejected",
                self.current_session_id.as_str(),
                session_id.as_str()
            );
            return Ok(PromptSubmission::Rejected { reason });
        }
        let Some(cid) = self.agent_routes.get(agent_id).cloned() else {
            return Ok(PromptSubmission::Rejected {
                reason: format!("unknown agent `{agent_id}`"),
            });
        };
        if self.agents.get(&cid).is_none_or(|agent| agent.terminating) {
            let reason = format!("agent `{agent_id}` is terminating");
            return Ok(PromptSubmission::Rejected { reason });
        }
        if !self.session_initialized(&session_id)
            || (self.selected_model.is_none() && self.provider_model_info.is_empty())
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
        {
            let inference_activation = prompt.creates_inference_activation();
            let activation_kind = prompt.activation_kind();
            let activation = inference_activation.then(tau_proto::ObservationId::random);
            if !prompt.is_internal() {
                self.reset_loop_guard_for_progress(&cid);
            }
            if let Some(conv) = self.agents.get_mut(&cid) {
                let mut queued_prompt = prompt.clone();
                queued_prompt.activation_observation = activation;
                conv.pending_prompts.push_back(queued_prompt);
            }
            if let Some(activation) = activation {
                self.append_prompt_activation_queued(&cid, activation, activation_kind, &prompt);
            }
            self.publish_event(
                None,
                Event::AgentPromptQueued(AgentPromptQueued {
                    agent_id: crate::parse_agent_id(agent_id),
                    text: prompt.text,
                    message_class: prompt.message_class,
                }),
            );
            if let Some(activation) = activation {
                self.activate_waits_for(&cid, activation);
            }
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        if self.dispatch_blocked_for(&cid) {
            let inference_activation = prompt.creates_inference_activation();
            let activation_kind = prompt.activation_kind();
            let activation = inference_activation.then(tau_proto::ObservationId::random);
            if !prompt.is_internal() {
                self.reset_loop_guard_for_progress(&cid);
            }
            if let Some(conv) = self.agents.get_mut(&cid) {
                let mut queued_prompt = prompt.clone();
                queued_prompt.activation_observation = activation;
                conv.pending_prompts.push_back(queued_prompt);
            }
            if let Some(activation) = activation {
                self.append_prompt_activation_queued(&cid, activation, activation_kind, &prompt);
            }
            self.publish_event(
                None,
                Event::AgentPromptQueued(AgentPromptQueued {
                    agent_id: crate::parse_agent_id(agent_id),
                    text: prompt.text,
                    message_class: prompt.message_class,
                }),
            );
            if let Some(activation) = activation {
                self.activate_waits_for(&cid, activation);
            }
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        let mut prompt = prompt;
        if prompt.creates_inference_activation() {
            let activation = tau_proto::ObservationId::random();
            self.append_prompt_activation_queued(
                &cid,
                activation,
                prompt.activation_kind(),
                &prompt,
            );
            prompt.activation_observation = Some(activation);
        }
        self.dispatch_prompt_for_agent(&cid, prompt)?;
        Ok(PromptSubmission::Dispatched)
    }

    /// Cancel every in-flight non-tool extension side agent
    /// (idle-summary and friends) so the agent's single prompt slot
    /// is free for the incoming user turn. Delegate sub-agents are
    /// left alone — they're part of an active user turn already and
    /// cancelling them would orphan the parent's tool call.
    ///
    /// Side effects per matching conversation: clear in-flight
    /// state, drop the spid from `prompt_agents`, mark it
    /// canceled, and publish a terminal prompt lifecycle event. A
    /// targeted `UiCancelPrompt` event is then published so the
    /// agent's retry-sleep wakes and aborts whatever it's currently
    /// processing.
    pub(super) fn preempt_blocking_ext_side_agents(&mut self, session_id: &SessionId) {
        let to_cancel: Vec<(AgentId, SessionId, AgentPromptId, PromptOriginator)> = self
            .agents
            .iter()
            .filter_map(|(cid, conv)| {
                if conv.parent_tool_call_id.is_some() {
                    return None;
                }
                if !matches!(
                    conv.originator,
                    tau_proto::PromptOriginator::Extension { .. }
                ) {
                    return None;
                }
                let in_flight = conv.in_flight_prompt.clone().or_else(|| {
                    match &conv.activation_dispatch {
                        path_crate_agent::ActivationDispatchState::ContextRecoveryClaimPending {
                            checkpoint,
                            ..
                        } => Some(checkpoint.agent_prompt_id.clone()),
                        _ => None,
                    }
                })?;
                Some((
                    cid.clone(),
                    conv.session_id.clone(),
                    in_flight,
                    conv.originator.clone(),
                ))
            })
            .collect();

        if to_cancel.is_empty() {
            return;
        }

        for (cid, prompt_session_id, spid, originator) in &to_cancel {
            let marked_owner = self
                .agents
                .get(cid)
                .and_then(|agent| agent.agent_id.as_deref())
                .and_then(|agent_id| self.agent_store.agent(agent_id))
                .and_then(|tree| tree.marked_inference_through(spid))
                .is_some();
            if marked_owner {
                self.publish_prompt_terminated(
                    prompt_session_id.clone(),
                    spid.clone(),
                    AgentPromptTerminationReason::Canceled,
                    originator.clone(),
                );
                self.publish_event(
                    None,
                    Event::UiCancelPrompt(UiCancelPrompt {
                        session_id: session_id.clone(),
                        target_agent_id: self
                            .target_agent_id_for_agent(cid)
                            .map(crate::parse_agent_id),
                        agent_prompt_id: Some(spid.clone()),
                    }),
                );
                continue;
            }
            self.cancel_pending_context_claim(cid);
            self.cancel_running_compaction(cid, spid);
            self.prompt_semantic_output.remove(spid);
            self.canceled_prompts.insert(spid.clone());
            self.fail_pending_initial_prompts(
                cid,
                tau_proto::AgentPromptFailureStage::Canceled,
                "initial prompt was canceled",
            );
            if let Some(conv) = self.agents.get_mut(cid) {
                conv.in_flight_prompt = None;
                conv.pending_prompts.clear();
            }
            self.set_agent_turn_state(cid, AgentTurnState::Idle);
            self.release_start_agent_request(cid);
            self.publish_prompt_terminated(
                prompt_session_id.clone(),
                spid.clone(),
                AgentPromptTerminationReason::Canceled,
                originator.clone(),
            );
            self.remember_ephemeral_provider_prompt(spid);
            self.prompt_agents.remove(spid);
            self.prompt_operations.remove(spid);
            self.prompt_context_limits.remove(spid);
            self.prompt_context_size_alerts.remove(spid);
            self.prompt_compaction_policies.remove(spid);
            self.prompt_compaction_projected_tokens.remove(spid);
            self.emit_info(&format!(
                "preempting side conv `{cid}` ({spid}) for incoming user prompt",
            ));
            // Targeted cancel per spid. A broadcast cancel
            // (`agent_prompt_id: None`) would also abort an
            // unrelated user/delegate prompt that happens to be
            // retry-sleeping on the agent side — the very prompt
            // we're trying to *unblock* by preempting these side
            // convs. Targeted form: the agent only aborts the
            // matching attempt, or records the spid in
            // `canceled_spids` if the prompt is still queued.
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id: session_id.clone(),
                    target_agent_id: self
                        .target_agent_id_for_agent(cid)
                        .map(crate::parse_agent_id),
                    agent_prompt_id: Some(spid.clone()),
                }),
            );
        }
    }

    /// Render the selected agent tree as one user-facing multiline result.
    ///
    /// Bound-session-only: returns the existing refusal text when `session_id`
    /// does not match.
    pub(super) fn tree_request_result(
        &self,
        session_id: &SessionId,
        target_agent_id: Option<&str>,
    ) -> String {
        if session_id != &self.current_session_id {
            return format!(
                "tree request for `{}` ignored; harness is bound to `{}`",
                session_id.as_str(),
                self.current_session_id.as_str()
            );
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(target_agent_id) else {
            return "tree request ignored: unknown agent".to_owned();
        };
        let agent_id = self
            .target_agent_id_for_agent(&cid)
            .expect("agent has durable id");
        let parsed_agent_id = crate::parse_agent_id(&agent_id);
        let events = match self.agent_store.agent_events(&agent_id) {
            Ok(events) => events,
            Err(error) => {
                return format!("tree request ignored: failed to load agent log: {error}");
            }
        };
        let lines: Vec<String> = match self.agent_store.agent(&agent_id) {
            Some(tree) if !tree.nodes().is_empty() => {
                let selected_head = self.agents.get(&cid).and_then(|conv| conv.head);
                let mut lines = Vec::new();
                let root_marker = if selected_head.is_none() { '*' } else { ' ' };
                lines.push(format!(
                    "  {:>3} {} before first prompt (root)",
                    0, root_marker
                ));
                lines.extend(
                    prompt_anchor_targets(tree, &parsed_agent_id, &events)
                        .into_iter()
                        .map(
                            |PromptAnchorTarget {
                                 anchor,
                                 head,
                                 prompt_node,
                             }| {
                                let marker = if !matches!(head, AgentHead::Root)
                                    && head.as_option() == selected_head
                                {
                                    '*'
                                } else {
                                    ' '
                                };
                                let preview = render_entry_preview(&prompt_node.entry);
                                format!("  {anchor:>3} {marker} before prompt  {preview}")
                            },
                        ),
                );
                lines
            }
            _ => {
                return format!("agent `{agent_id}` has no entries yet");
            }
        };
        lines.join("\n")
    }

    /// Validates a `UiNavigateTree` request against the bound session and
    /// resolves the durable agent-owned head-move target.
    pub(super) fn validate_navigate_tree_target(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        session_id: &SessionId,
        target_agent_id: Option<&str>,
        target: UiTreeNavigationTarget,
    ) -> Option<(AgentId, tau_proto::AgentId, AgentHead)> {
        if session_id != &self.current_session_id {
            self.send_ui_error_response(
                client_id,
                format!(
                    "navigate ignored: harness is bound to `{}`",
                    self.current_session_id.as_str()
                ),
            );
            return None;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(target_agent_id) else {
            self.send_ui_error_response(client_id, "navigate ignored: unknown agent");
            return None;
        };
        let agent_id: tau_proto::AgentId = crate::parse_agent_id(
            self.target_agent_id_for_agent(&cid)
                .expect("agent has durable id"),
        );
        let tree = self.agent_store.agent(agent_id.as_str());
        let events = match self.agent_store.agent_events(agent_id.as_str()) {
            Ok(events) => events,
            Err(error) => {
                self.send_ui_error_response(
                    client_id,
                    format!("navigate ignored: failed to load agent log: {error}"),
                );
                return None;
            }
        };
        let head = match target {
            UiTreeNavigationTarget::Root => AgentHead::Root,
            UiTreeNavigationTarget::PromptAnchor(anchor) => {
                let Some(tree) = tree else {
                    self.send_ui_error_response(
                        client_id,
                        "navigate ignored: agent tree is not loaded",
                    );
                    return None;
                };
                let Some(target) = prompt_anchor_targets(tree, &agent_id, &events)
                    .into_iter()
                    .find(|candidate| candidate.anchor == anchor)
                else {
                    self.send_ui_error_response(
                        client_id,
                        format!("no prompt anchor `{anchor}` in agent tree"),
                    );
                    return None;
                };
                target.head
            }
            UiTreeNavigationTarget::Node(node_id) => {
                let valid = tree.and_then(|t| t.node(node_id)).is_some();
                if !valid {
                    self.send_ui_error_response(
                        client_id,
                        format!("no node `{}` in agent tree", node_id.get()),
                    );
                    return None;
                }
                AgentHead::Node(node_id)
            }
        };
        Some((cid, agent_id, head))
    }

    /// Tear down the current session and bind the harness to a new one.
    ///
    /// Pi-style: emit `SessionShutdown` for the old, drop in-flight
    /// prompts, swap the bound id, then run a fresh `start_session_init`
    /// for the new id with the given reason. Extension processes are
    /// kept across sessions (they're not respawned); extensions that
    /// hold per-session state subscribe to `session.shutdown` to
    /// flush/clean up.
    pub(super) fn switch_session(
        &mut self,
        new_session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) -> Result<(), HarnessError> {
        if new_session_id == self.current_session_id
            && !matches!(reason, tau_proto::SessionStartReason::New)
        {
            self.emit_info(&format!("already on session `{}`", new_session_id.as_str()));
            return Ok(());
        }

        let old_id = self.current_session_id.clone();
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::SessionChanged);
        self.cancel_rendered_previews(|_| true);
        // Invalidate every admitted old-session action before quiescing
        // interception. Observation events may still commit, but their captured
        // admission generation can no longer create or retarget work.
        self.current_session_generation = self.current_session_generation.saturating_add(1);
        // Callback capabilities are live only for the generation that issued
        // them. Clearing them before a session id can be selected again makes
        // an old callback fail even after an S -> other -> S rollover.
        self.pending_external_message_auth.clear();
        self.publish_event(
            None,
            Event::SessionShutdown(tau_proto::SessionShutdown { session_id: old_id }),
        );
        self.fail_all_pending_ui_shell_commands(
            "the session shut down before the shell command completed",
        );

        // A rollover is terminal for controls correlated to the old session.
        // Resolve requesters rather than silently dropping their pending request;
        // the shutdown event above concurrently tells providers to cancel the
        // scheduler-owned old-session jobs.
        for (_, pending) in std::mem::take(&mut self.ui_runtime.pending_retry_prompts) {
            let _ = self.bus.send_to(
                &pending.requester_client_id,
                None,
                HarnessOutputMessage::deliver(Event::UiRetryPromptResult(
                    tau_proto::UiRetryPromptResult {
                        request_id: pending.ui_request_id,
                        target_agent_id: Some(pending.target_agent_id),
                        target_label: pending.target_label,
                        status: None,
                        message: "Cannot retry: the session was replaced.".into(),
                    },
                )),
            );
        }

        // Drop in-flight work bound to the old session. Pending prompts
        // for it are abandoned (the user explicitly switched away), and
        // each agent's per-turn state is reset.
        self.turn_state = TurnState::Idle;
        let agent_ids = self.agents.keys().cloned().collect::<Vec<_>>();
        for cid in agent_ids {
            self.cancel_pending_context_claim(&cid);
            if let Some(prompt_id) = self
                .agents
                .get(&cid)
                .and_then(|agent| agent.in_flight_prompt.clone())
            {
                self.cancel_running_compaction(&cid, &prompt_id);
            }
            self.fail_pending_initial_prompts(
                &cid,
                tau_proto::AgentPromptFailureStage::LifecycleTeardown,
                "session switch discarded initial prompt",
            );
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_prompts.clear();
                conv.in_flight_prompt = None;
            }
            self.set_agent_turn_state(&cid, AgentTurnState::Idle);
        }
        self.tool_runtime.tool_turn.clear();
        self.tool_runtime.tool_agents.clear();
        self.tool_runtime.pending_tools.clear();
        self.tool_runtime.pending_declaration_observations.clear();
        self.tool_runtime.pending_terminal_observations.clear();
        self.tool_runtime.pending_wait_settlements.clear();
        self.tool_runtime
            .post_commit_runtime_only_tool_terminals
            .clear();
        self.tool_runtime.pending_cancellation_observations.clear();
        self.tool_runtime.peer_tool_requests.clear();
        self.tool_runtime.peer_internal_tool_agents.clear();
        self.tool_runtime.completed_tool_calls.clear();
        self.tool_runtime.completed_ephemeral_tool_calls.clear();
        self.tool_runtime.completed_tool_agents.clear();
        self.tool_runtime.pending_tool_providers.clear();
        self.ui_runtime
            .completed_action_invocations
            .extend(self.ui_runtime.pending_action_invocations.keys().cloned());
        self.ui_runtime.pending_action_invocations.clear();
        self.prompt_agents.clear();
        self.ephemeral_provider_prompts.clear();
        self.ephemeral_provider_retry_requests.clear();
        self.pending_provider_prompts.clear();
        self.pending_prompt_dispatches.clear();
        self.prompt_models.clear();
        self.prompt_estimated_cost_rates.clear();
        self.prompt_context_limits.clear();
        self.prompt_context_size_alerts.clear();
        self.prompt_compaction_policies.clear();
        self.prompt_compaction_projected_tokens.clear();
        self.prompt_semantic_output.clear();
        self.pending_stale_provider_responses.clear();
        self.pending_replay_prompt_activation_occurrences.clear();
        self.pending_replay_uncertain_stale.clear();
        self.local_route_failure_prompts.clear();
        self.prompt_operations.clear();
        self.prompt_tool_specs.clear();
        self.prompt_tool_call_prompts.clear();
        self.shown_tool_failure_examples.clear();
        self.tool_runtime
            .suppressed_background_completion_prompts
            .clear();
        self.tool_runtime.background_completion_targets.clear();
        self.canceled_prompts.clear();
        self.pending_notices.restore_sessions.clear();
        self.pending_notices.restore_background_notices.clear();
        self.pending_notices.changed_session_agents.clear();
        self.pending_notices.tool_availability.clear();
        self.pending_notices.unavailable_tools_delivered.clear();
        self.pending_start_agent_requests.clear();
        let canceled_message_ids = self
            .pending_external_receive_acks
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let canceled_receives = self
            .pending_external_receive_acks
            .drain()
            .map(|(_, pending)| pending)
            .collect::<Vec<_>>();
        for mut pending in canceled_receives {
            pending.canceled = true;
            self.release_peer_input_rate(&pending.recipient_id, pending.rate_admitted_at);
            self.cleanup_uncommitted_peer_auto_start(&pending.recipient_id);
            if let PendingPeerReceiveCompletion::Remote {
                client_id,
                request_id,
            } = &pending.completion
            {
                let _ = self.bus.send_to(
                    client_id,
                    None,
                    HarnessOutputMessage::ExternalAgentMessageResult(
                        tau_proto::ExternalAgentMessageResult {
                            request_id: request_id.clone(),
                            failure: Some(
                                tau_proto::ExternalAgentMessageFailure::TargetSessionChanged,
                            ),
                            recipient_id: None,
                            started: false,
                        },
                    ),
                );
            }
        }
        self.discard_canceled_peer_receive_publishes(&canceled_message_ids);
        for cancellation in self.peer_io_cancellations.drain(..) {
            if let Some(cancellation) = cancellation.upgrade() {
                cancellation.store(true, path_std_sync_atomic::Ordering::Release);
            }
        }
        for cancellations in self
            .inbound_peer_io_cancellations
            .drain()
            .map(|(_, value)| value)
        {
            for cancellation in cancellations {
                if let Some(cancellation) = cancellation.upgrade() {
                    cancellation.store(true, path_std_sync_atomic::Ordering::Release);
                }
            }
        }
        self.peer_route_clock = 0;
        self.peer_last_routed.clear();
        self.peer_input_rate.clear();
        self.uncommitted_peer_auto_starts.clear();
        self.pending_manual_compaction_tools.clear();
        self.accepted_manual_compaction_tools.clear();
        let pending_compactions = std::mem::take(&mut self.pending_ui_compactions_after_wait);
        for pending in pending_compactions.into_values() {
            self.send_ui_error_response(
                &pending.requester_client_id,
                "compaction canceled because the session changed",
            );
        }
        // Specialized cancellation paths above resolved accepted old-session
        // work. Suspend any responder whose publication is destructively
        // canceled, cancel transaction-owned checkpoints, and commit the queued
        // mandatory SessionShutdown before switching the bound session.
        self.quiesce_synchronized_publications_for_rollover();
        self.pending_agent_publish_completions.clear();
        self.enqueued_standalone_inference_checkpoints.clear();
        self.pending_publish_idle_dispatches.clear();
        self.clear_session_agent_context();
        self.agent_runtime_indicators.clear();
        self.subagents =
            SubagentToolState::with_input_wait_timeout_bounds(self.input_wait_timeout_bounds());

        // Token and context accounting are session-scoped. Reset them
        // before `SessionStarted` so clients recreating status UI for
        // the new session do not inherit the previous transcript's
        // cumulative totals.
        self.current_session_state = CurrentSessionState::default();
        self.creator_topology = AgentCreatorTopology::default();
        self.cost_ledger = AgentCostLedger::default();

        // Drop agents from the previous bound session. New user agents are
        // created explicitly by `UiCreateAgent`/first prompt in the new session.
        self.agents.clear();
        self.agent_routes.clear();
        self.session_loaded_agents.clear();
        self.session_ever_loaded_agents.clear();
        self.session_roster_loaded_agents.clear();
        self.session_roster_ever_loaded_agents.clear();
        self.session_roster_valid = true;
        self.agent_navigation_modes.clear();
        self.agent_watches.clear();
        self.agent_watchers.clear();
        self.agent_watch_subscriptions.clear();
        self.agent_watch_provider_status.clear();
        self.agent_watch_provider_deliveries.clear();
        self.pending_long_wait_notifications.clear();
        self.stopped_agent_ids.clear();
        self.restored_unavailable_agents.clear();
        self.pending_agent_unload_reasons.clear();
        self.expected_agent_unloads.clear();
        self.pending_watch_retirements.clear();
        self.pending_builtin_delegates.clear();

        self.current_session_id = new_session_id.clone();
        self.current_session_start_reason = reason;
        self.reset_extension_restart_budgets_at(Instant::now());
        if self.storage_mode.is_durable() {
            // Take write ownership before replay loads the durable sequence
            // cursor. Retention cleanup holds this same lock through deletion.
            let _ = self.store.lock_and_load_session(new_session_id.as_str())?;
        }
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.rehydrate_agents_from_session();
            self.activate_replayed_prompt_occurrences();
        }
        self.publish_delegate_roles_context();

        if self.storage_mode.is_durable() {
            // Refresh metadata after replay; write ownership was acquired before
            // loading the membership log above.
            self.store.record_session_meta(new_session_id.as_str())?;

            // Send the new debug log to the new session's dir, so each
            // session is self-contained.
            let _ = self.enable_debug_log(&self.sessions_dir().join(new_session_id.as_str()));
        }
        if let Some(path) = &self.runtime_harness_path
            && let Err(error) = crate::runtime_dir::update_session_id(path, new_session_id.as_str())
        {
            tracing::warn!(
                target: "tau_harness::runtime_dir",
                path = %path.display(),
                session_id = %new_session_id,
                %error,
                "failed to update runtime metadata after session switch"
            );
        }
        self.start_session_init(new_session_id.clone(), reason);
        self.publish_current_session_dir();
        Ok(())
    }

    pub(super) fn publish_current_session_dir(&mut self) {
        self.publish_event(None, self.current_session_dir_event());
    }

    pub(crate) fn current_session_dir_event(&self) -> Event {
        let (path, status) = if self.storage_mode.is_ephemeral() {
            (
                PathBuf::from("<ephemeral>"),
                tau_proto::SessionDirStatus::Ephemeral,
            )
        } else {
            (
                self.sessions_dir().join(self.current_session_id.as_str()),
                session_dir_status_from_reason(self.current_session_start_reason),
            )
        };
        Event::HarnessSessionDir(tau_proto::HarnessSessionDir {
            session_id: self.current_session_id.clone(),
            path,
            status,
        })
    }

    pub(super) fn sessions_dir(&self) -> PathBuf {
        // The harness doesn't currently store the sessions dir directly;
        // derive it from the session store's location. SessionStore
        // exposes its root via the `sessions_dir()` accessor.
        self.store.sessions_dir().to_path_buf()
    }

    pub(super) fn loaded_agent_ids_for_session(
        &self,
        session_id: &SessionId,
    ) -> Vec<tau_proto::AgentId> {
        self.store
            .session(session_id.as_str())
            .map(|membership| membership.loaded_agents().into_iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(super) fn any_loaded_agent_event(
        &self,
        session_id: &SessionId,
        matches_event: impl Fn(&Event) -> bool,
    ) -> bool {
        self.loaded_agent_ids_for_session(session_id)
            .into_iter()
            .filter_map(|agent_id| match self.agent_store.agent_events(agent_id.as_str()) {
                Ok(events) => Some(events),
                Err(error) => {
                    tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events while checking restored prompts");
                    None
                }
            })
            .flatten()
            .any(|entry| matches_event(&entry.event))
    }

    pub(super) fn agent_internal_prompt_already_persisted(
        &self,
        agent_id: &AgentId,
        text: &str,
    ) -> bool {
        self.agent_store
            .agent_events(agent_id.as_str())
            .inspect_err(|error| {
                tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events while checking restored background notice");
            })
            .ok()
            .into_iter()
            .flatten()
            .any(|entry| event_is_internal_prompt_text(&entry.event, text))
    }

    pub(super) fn restore_notice_already_persisted(&self, session_id: &SessionId) -> bool {
        self.any_loaded_agent_event(session_id, event_is_internal_restore_notice)
    }

    pub(super) fn last_recorded_session_event_at(
        &self,
        session_id: &SessionId,
    ) -> Option<tau_proto::UnixMicros> {
        self.loaded_agent_ids_for_session(session_id)
            .into_iter()
            .filter_map(|agent_id| match self.agent_store.agent_events(agent_id.as_str()) {
                Ok(events) => Some(events),
                Err(error) => {
                    tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events while checking restored timestamps");
                    None
                }
            })
            .flatten()
            .filter_map(|entry| (entry.recorded_at.get() != 0).then_some(entry.recorded_at))
            .max_by_key(|recorded_at| recorded_at.get())
    }

    pub(super) fn queue_restore_notice_for_resumed_session(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            return;
        }
        if self.restore_notice_already_persisted(session_id) {
            self.pending_notices.restore_sessions.remove(session_id);
            return;
        }
        let last_recorded_at = self.last_recorded_session_event_at(session_id);
        self.pending_notices
            .restore_sessions
            .insert(session_id.clone(), last_recorded_at);
    }

    pub(super) fn queue_restore_background_notices_for_resumed_session(
        &mut self,
        session_id: &SessionId,
    ) {
        if session_id != &self.current_session_id {
            return;
        }
        let mut notices_by_agent = HashMap::new();
        for cid in self.restored_agent_ids(session_id) {
            let mut seen = HashSet::new();
            let mut notices = Vec::new();
            for state in self.restored_background_tool_states_for_agent(&cid) {
                let Some(tau_core::BackgroundToolCompletion::Error(error)) = state.completion
                else {
                    continue;
                };
                let notice = restored_background_tool_call_error_message(&error.call_id);
                if error.message != notice || !seen.insert(notice.clone()) {
                    continue;
                }
                if self.agent_internal_prompt_already_persisted(&cid, &notice) {
                    continue;
                }
                notices.push(notice);
            }
            if !notices.is_empty() {
                notices_by_agent.insert((session_id.clone(), cid), notices);
            }
        }
        if notices_by_agent.is_empty() {
            self.pending_notices
                .restore_background_notices
                .retain(|(queued_session_id, _), _| queued_session_id != session_id);
        } else {
            self.pending_notices
                .restore_background_notices
                .retain(|(queued_session_id, _), _| queued_session_id != session_id);
            self.pending_notices
                .restore_background_notices
                .extend(notices_by_agent);
        }
    }

    pub(super) fn mark_tool_unavailable_for_notice(
        &mut self,
        internal_name: ToolName,
        visible_name: ToolName,
    ) {
        let internal_name = internal_name.into_string();
        if matches!(
            self.pending_notices.tool_availability.get(&internal_name),
            Some(PendingToolAvailabilityNotice::Unavailable { .. })
        ) {
            return;
        }
        if matches!(
            self.pending_notices.tool_availability.get(&internal_name),
            Some(PendingToolAvailabilityNotice::AvailableAgain { .. })
        ) {
            self.pending_notices
                .tool_availability
                .remove(&internal_name);
            return;
        }
        if self
            .pending_notices
            .unavailable_tools_delivered
            .contains_key(&internal_name)
        {
            return;
        }
        self.pending_notices.tool_availability.insert(
            internal_name,
            PendingToolAvailabilityNotice::Unavailable { visible_name },
        );
    }

    pub(super) fn mark_tool_available_for_notice(
        &mut self,
        internal_name: ToolName,
        visible_name: ToolName,
    ) {
        let internal_name = internal_name.into_string();
        if matches!(
            self.pending_notices.tool_availability.get(&internal_name),
            Some(PendingToolAvailabilityNotice::Unavailable { .. })
        ) {
            self.pending_notices
                .tool_availability
                .remove(&internal_name);
            return;
        }
        if self
            .pending_notices
            .unavailable_tools_delivered
            .contains_key(&internal_name)
        {
            self.pending_notices.tool_availability.insert(
                internal_name,
                PendingToolAvailabilityNotice::AvailableAgain { visible_name },
            );
        }
    }

    pub(super) fn take_pending_tool_availability_prompts_for_user_prompt(
        &mut self,
    ) -> Vec<PendingPrompt> {
        let pending = std::mem::take(&mut self.pending_notices.tool_availability);
        let mut prompts = Vec::new();
        for (internal_name, notice) in pending {
            match &notice {
                PendingToolAvailabilityNotice::Unavailable { visible_name } => {
                    self.pending_notices
                        .unavailable_tools_delivered
                        .insert(internal_name, visible_name.clone());
                }
                PendingToolAvailabilityNotice::AvailableAgain { .. } => {
                    self.pending_notices
                        .unavailable_tools_delivered
                        .remove(&internal_name);
                }
            }
            prompts.push(PendingPrompt::internal(notice.prompt_text()));
        }
        prompts
    }

    /// Consume pending internal notices before the next real user prompt on the
    /// current session.
    pub(crate) fn take_pending_restore_prompts_for_user_prompt(
        &mut self,
        cid: &AgentId,
    ) -> Vec<PendingPrompt> {
        let Some((session_id, agent_id)) = self.agents.get(cid).map(|conv| {
            let agent_id = conv
                .agent_id
                .as_deref()
                .map(crate::parse_agent_id)
                .unwrap_or_else(|| cid.clone());
            (conv.session_id.clone(), agent_id)
        }) else {
            return Vec::new();
        };
        if session_id != self.current_session_id {
            return Vec::new();
        }

        let mut prompts = Vec::new();
        if self.restore_notice_already_persisted(&session_id) {
            self.pending_notices.restore_sessions.remove(&session_id);
        } else if let Some(last_recorded_at) =
            self.pending_notices.restore_sessions.remove(&session_id)
        {
            prompts.push(PendingPrompt::passive_restore_notice(
                restore_notice_prompt(last_recorded_at, tau_proto::UnixMicros::now()),
            ));
        }

        if let Some(notices) = self
            .pending_notices
            .restore_background_notices
            .remove(&(session_id.clone(), agent_id.clone()))
        {
            for notice in notices {
                if !self.agent_internal_prompt_already_persisted(&agent_id, &notice) {
                    prompts.push(PendingPrompt::passive_restore_notice(notice));
                }
            }
        }
        if self
            .pending_notices
            .changed_session_agents
            .remove(&(session_id, agent_id))
        {
            prompts.push(PendingPrompt::internal(changed_session_notice_prompt()));
        }
        prompts.extend(self.take_pending_tool_availability_prompts_for_user_prompt());
        prompts
    }

    /// Consume passive background-completion notices before the next real user
    /// prompt. These notices are queued on live cancellation so a background
    /// placeholder has a visible terminal event, but they must not create a
    /// standalone automatic agent turn.
    pub(crate) fn take_passive_background_completion_prompts_for_user_prompt(
        &mut self,
        cid: &AgentId,
    ) -> Vec<PendingPrompt> {
        let Some(conv) = self.agents.get_mut(cid) else {
            return Vec::new();
        };
        let mut prompts = Vec::new();
        let mut remaining =
            path_std_collections::VecDeque::with_capacity(conv.pending_prompts.len());
        while let Some(prompt) = conv.pending_prompts.pop_front() {
            if prompt.is_passive_background_completion() {
                prompts.push(prompt);
            } else {
                remaining.push_back(prompt);
            }
        }
        conv.pending_prompts = remaining;
        prompts
    }

    pub(super) fn repair_restored_foreground_tool_calls(
        &mut self,
        session_id: &SessionId,
    ) -> usize {
        if session_id != &self.current_session_id {
            return 0;
        }
        let mut count = 0;
        for cid in self.restored_agent_ids(session_id) {
            let calls: Vec<ToolCallItem> = self
                .agents
                .get(&cid)
                .and_then(|conv| conv.agent_id.as_deref())
                .and_then(|agent_id| self.agent_store.agent(agent_id))
                .map(|tree| {
                    tree.unresolved_foreground_tool_calls()
                        .into_iter()
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for call in calls {
                if self.is_pending_manual_compaction_call(&call.call_id) {
                    continue;
                }
                count += 1;
                self.tool_runtime
                    .tool_agents
                    .insert(call.call_id.clone(), cid.clone());
                let display = (call.name.as_str() == "wait")
                    .then(|| {
                        self.normalized_input_wait_timeout_minutes(&call.arguments)
                            .ok()
                            .flatten()
                    })
                    .flatten()
                    .map(|minutes| tau_proto::ToolUseState {
                        args: format!("{minutes}m"),
                        status: tau_proto::ToolUseStatus::Error,
                        status_text: "interrupted".to_owned(),
                        ..Default::default()
                    });
                let error = ToolError {
                    presentation: Default::default(),
                    call_id: call.call_id.clone(),
                    tool_name: call.name,
                    tool_type: call.tool_type,
                    message: restored_tool_call_error_message(&call.call_id),
                    details: None,
                    originator: tau_proto::PromptOriginator::User,

                    display,
                };
                if let Some(call_ref) = self.persisted_tool_call_ref(&cid, &call.call_id) {
                    self.record_wait_tool_call_ref(call.call_id.clone(), call_ref);
                }
                self.publish_terminal_tool_error_with_cause(
                    Some(&cid),
                    Some(crate::harness::harness_connection_id()),
                    error,
                    tau_proto::ToolTerminalCause::RestartRepair,
                );
            }
        }
        count
    }

    pub(super) fn restored_background_tool_states_for_agent(
        &self,
        cid: &AgentId,
    ) -> Vec<tau_core::BackgroundToolCallState> {
        let Some(head) = self.agents.get(cid).map(|conv| conv.head) else {
            return Vec::new();
        };
        let Some(agent_id) = self
            .agents
            .get(cid)
            .and_then(|conv| conv.agent_id.as_deref())
        else {
            return Vec::new();
        };
        let Ok(events) = self.agent_store.agent_events(agent_id) else {
            tracing::warn!(target: "tau_harness", %agent_id, "failed to load restored agent events");
            return Vec::new();
        };
        self.agent_store
            .agent(agent_id)
            .map(|tree| tree.background_tool_calls_from(head, &events))
            .unwrap_or_default()
    }

    pub(super) fn restored_agent_ids(&self, session_id: &SessionId) -> Vec<AgentId> {
        if session_id != &self.current_session_id {
            return Vec::new();
        }
        self.agents
            .iter()
            .filter_map(|(cid, conv)| {
                (conv.session_id == *session_id && conv.agent_id.is_some()).then_some(cid.clone())
            })
            .collect()
    }

    pub(super) fn seed_restored_wait_background_completions(&mut self, session_id: &SessionId) {
        for cid in self.restored_agent_ids(session_id) {
            for state in self.restored_background_tool_states_for_agent(&cid) {
                let call_id = state.placeholder.call_id.clone();
                self.tool_runtime
                    .tool_agents
                    .insert(call_id.clone(), cid.clone());
                if let Some(call_ref) = state.call_ref {
                    self.record_wait_tool_call_ref(call_id.clone(), call_ref);
                }
                let terminal = state.terminal_observation;
                match state.completion {
                    Some(tau_core::BackgroundToolCompletion::Result(result)) => {
                        self.record_wait_background_result(result, terminal);
                    }
                    Some(tau_core::BackgroundToolCompletion::Error(error)) => {
                        self.record_wait_background_error(error, terminal);
                    }
                    None => {}
                }
            }
        }
    }

    pub(super) fn repair_restored_background_tool_calls(
        &mut self,
        session_id: &SessionId,
    ) -> usize {
        if session_id != &self.current_session_id {
            return 0;
        }
        let mut count = 0;
        for cid in self.restored_agent_ids(session_id) {
            let Some(head) = self.agents.get(&cid).map(|conv| conv.head) else {
                continue;
            };
            let Some(agent_id) = self
                .agents
                .get(&cid)
                .and_then(|conv| conv.agent_id.as_deref())
            else {
                continue;
            };
            let Ok(events) = self.agent_store.agent_events(agent_id) else {
                tracing::warn!(target: "tau_harness", %agent_id, "failed to load restored agent events");
                continue;
            };
            let calls = self
                .agent_store
                .agent(agent_id)
                .map(|tree| tree.unresolved_background_tool_calls_from(head, &events))
                .unwrap_or_default();
            let call_refs = events
                .iter()
                .flat_map(|record| {
                    let Event::ProviderResponseFinished(response) = &record.event else {
                        return Vec::new();
                    };
                    response
                        .output_items
                        .iter()
                        .enumerate()
                        .filter_map(|(item_index, item)| {
                            let ContextItem::ToolCall(call) = item else {
                                return None;
                            };
                            Some((
                                call.call_id.clone(),
                                tau_proto::ToolCallRef {
                                    declaration: record.observation_id,
                                    item_index: u32::try_from(item_index).ok()?,
                                },
                            ))
                        })
                        .collect()
                })
                .collect::<HashMap<_, _>>();
            for call in calls {
                if self.is_pending_manual_compaction_call(&call.call_id) {
                    continue;
                }
                count += 1;
                self.tool_runtime
                    .tool_agents
                    .insert(call.call_id.clone(), cid.clone());
                if let Some(call_ref) = call_refs.get(&call.call_id).copied() {
                    self.record_wait_tool_call_ref(call.call_id.clone(), call_ref);
                    self.observe_tool_terminal(
                        &cid,
                        &call.call_id,
                        tau_proto::ToolTerminalCause::RestartRepair,
                    );
                }
                let error = ToolBackgroundError {
                    call_id: call.call_id.clone(),
                    tool_name: call.tool_name,
                    tool_type: call.tool_type,
                    message: restored_background_tool_call_error_message(&call.call_id),
                    details: None,
                    originator: call.originator,

                    display: None,
                };
                self.tool_runtime
                    .pending_background_completion_modes
                    .insert(
                        call.call_id.clone(),
                        BackgroundCompletionPromptMode::DoNotQueue,
                    );
                self.publish_terminal_background_error(
                    &cid,
                    Some(crate::harness::harness_connection_id()),
                    error,
                );
            }
        }
        count
    }

    pub(super) fn repair_restored_session_tool_state(&mut self, session_id: &SessionId) {
        self.repair_restored_foreground_tool_calls(session_id);
        self.repair_restored_background_tool_calls(session_id);
        self.seed_restored_wait_background_completions(session_id);
        let ready_requests: Vec<_> = self
            .accepted_manual_compaction_tools
            .iter()
            .filter_map(|(request_id, accepted)| {
                (accepted.request.resume_inference
                    && self.manual_request_has_complete_tool_round(request_id))
                .then(|| {
                    self.runtime_agent_id_for_target_agent(Some(
                        accepted.request.target_agent_id.as_str(),
                    ))
                    .map(|target_cid| (target_cid, request_id.clone()))
                })
                .flatten()
            })
            .collect();
        for (target_cid, request_id) in ready_requests {
            self.start_accepted_manual_compaction(&target_cid, &request_id);
        }
        self.queue_restore_background_notices_for_resumed_session(session_id);
    }

    pub(crate) fn start_session_init(
        &mut self,
        session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) {
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.queue_restore_notice_for_resumed_session(&session_id);
        }
        let waiting_on = self.session_init_provider_ids();
        self.publish_event(
            None,
            Event::SessionStarted(tau_proto::SessionStarted {
                session_id: session_id.clone(),
                reason,
            }),
        );
        if waiting_on.is_empty() {
            if let Err(error) = self.complete_session_init(session_id, reason) {
                self.emit_harness_failure(&format!("failed to initialize session: {error}"));
                self.turn_state = TurnState::Idle;
            }
            return;
        }

        self.turn_state = TurnState::InitializingSession {
            session_id,
            reason,
            waiting_on,
        };
    }

    pub(super) fn handle_extension_context_ready(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        if ready.session_id != self.current_session_id {
            return Ok(());
        }
        let source_id = source_id.clone();
        let should_finalize = self
            .pending_agent_discovery
            .get_mut(&ready.agent_id)
            .filter(|pending| pending.initialization_id == ready.agent_initialization_id)
            .is_some_and(|pending| {
                pending.waiting_on.remove(&source_id) && pending.waiting_on.is_empty()
            });
        if should_finalize {
            self.finalize_agent_discovery(&ready.agent_id)?;
        }
        Ok(())
    }

    pub(super) fn handle_extension_session_context_ready(
        &mut self,
        source_id: &tau_proto::ConnectionId,
        ready: tau_proto::ExtensionSessionContextReady,
    ) -> Result<(), HarnessError> {
        if ready.session_id != self.current_session_id {
            return Ok(());
        }
        let source_id = source_id.clone();
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                reason,
                waiting_on,
            } => {
                let removed = waiting_on.remove(&source_id);
                if removed {
                    self.session_init_progress_generation.advance();
                }
                if removed && waiting_on.is_empty() {
                    Some((session_id.clone(), *reason))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some((session_id, reason)) = completed_session {
            self.complete_session_init(session_id, reason)?;
        }
        Ok(())
    }

    /// Records accepted discovery from a provider still outstanding in the
    /// current session-initialization generation.
    pub(super) fn record_session_init_provider_progress(
        &mut self,
        source_id: &tau_proto::ConnectionId,
    ) {
        let is_outstanding = matches!(
            &self.turn_state,
            TurnState::InitializingSession { waiting_on, .. } if waiting_on.contains(source_id)
        );
        if is_outstanding {
            self.session_init_progress_generation.advance();
        }
    }

    pub(super) fn maybe_complete_session_init_for_disconnect(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                reason,
                waiting_on,
            } => {
                let removed = waiting_on.remove(connection_id);
                if removed && waiting_on.is_empty() {
                    Some((session_id.clone(), *reason))
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((session_id, reason)) = completed_session
            && let Err(error) = self.complete_session_init(session_id, reason)
        {
            self.emit_harness_failure(&format!("failed to initialize session: {error}"));
            self.turn_state = TurnState::Idle;
        }
    }

    pub(super) fn required_skill_unavailable_reason(
        &self,
        skill_name: &tau_proto::SkillName,
    ) -> Option<String> {
        if let Some(message) = tau_skills::skill_name_validation_message(skill_name.as_str()) {
            return Some(format!("`{skill_name}` has invalid skill name: {message}"));
        }
        let Some(skill) = self.discovered_skills.get(skill_name) else {
            return Some(format!("`{skill_name}` is not discovered"));
        };
        if skill.disable_model_invocation {
            return Some(format!(
                "`{skill_name}` is hidden from model-side skill loading"
            ));
        }
        if let Err(error) = user_skill_invocation::read_user_invoked_skill_body(&skill.source) {
            return Some(format!(
                "`{skill_name}` could not be loaded from {}: {error}",
                skill.source.label()
            ));
        }
        None
    }

    pub(super) fn unavailable_required_skills(
        &self,
        role: &tau_config::settings::AgentRole,
    ) -> Vec<String> {
        role.required_skills
            .iter()
            .filter_map(|skill| self.required_skill_unavailable_reason(skill))
            .collect()
    }

    pub(super) fn format_required_skill_role_notice(role_name: &str, reasons: &[String]) -> String {
        let reason_text = reasons.join("; ");
        format!(
            "role `{role_name}` disabled: required skill(s) unavailable: {reason_text}; \
             install/fix the required skill(s) or remove them from `required_skills`/`requiredSkills`"
        )
    }

    pub(super) fn enforce_required_role_skills(&mut self) -> Result<(), HarnessError> {
        let disabled = self
            .available_roles
            .iter()
            .filter_map(|(role_name, role)| {
                let reasons = self.unavailable_required_skills(role);
                (!reasons.is_empty()).then(|| {
                    (
                        role_name.clone(),
                        Self::format_required_skill_role_notice(role_name, &reasons),
                    )
                })
            })
            .collect::<Vec<_>>();
        if disabled.is_empty() {
            return Ok(());
        }

        let selected_role = self.selected_role.clone();
        let mut selected_role_error = None;
        for (role_name, message) in disabled {
            self.available_roles.remove(&role_name);
            self.role_overrides.remove(&role_name);
            self.disabled_role_reasons.insert(
                role_name.clone(),
                DisabledRoleReason {
                    message: message.clone(),
                },
            );
            self.emit_notice(
                tau_proto::notice_kind::HARNESS_CONFIG_ERROR,
                tau_proto::NoticeLevel::Warning,
                tau_proto::NoticePurpose::Alert,
                &message,
            );
            if role_name == selected_role {
                selected_role_error = Some(message);
            }
        }
        self.publish_event(
            None,
            Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                roles: role_infos(
                    &self.provider_model_info,
                    &self.available_roles,
                    &self.available_models,
                ),
                groups: self.current_role_groups(),
                custom_prompts: self.custom_prompts.clone(),
            }),
        );
        self.publish_delegate_roles_context();
        if let Some(message) = selected_role_error {
            return Err(HarnessError::Participant(format!(
                "{message}; selected/default role is unavailable"
            )));
        }
        if self.available_roles.is_empty() {
            return Err(HarnessError::Participant(
                "no roles remain enabled after required skill validation".to_owned(),
            ));
        }
        Ok(())
    }

    pub(super) fn complete_session_init(
        &mut self,
        session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) -> Result<(), HarnessError> {
        // AGENTS.md and skill context is agent-scoped. Session init only waits
        // for discovery; the discovered context is injected when a durable agent
        // is explicitly created from the UI's current role/cwd state.
        self.enforce_required_role_skills()?;
        self.publish_session_skills_projection();
        // A resumed roster is already live before session discovery completes.
        // Start one fresh correlated initialization for every restored member
        // before any replay activation or queued prompt can dispatch.
        let restored = self
            .agents
            .iter()
            .filter_map(|(cid, agent)| {
                agent
                    .agent_id
                    .clone()
                    .map(|agent_id| (cid.clone(), agent_id))
            })
            .collect::<Vec<_>>();
        for (cid, agent_id) in restored {
            self.ensure_loaded_agent_for_agent(&cid, &agent_id);
        }
        self.initialized_sessions.insert(session_id.clone());
        // Catch up before repair: repair appends its synthetic tool errors to
        // the durable log as it publishes them live, so running it first
        // would deliver each error twice (live, then replay-marked) to peers
        // subscribed before init.
        self.catch_up_subscribers_after_session_init();
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.repair_restored_session_tool_state(&session_id);
            self.consume_restored_self_compaction_deliveries();
            self.release_restored_self_compaction_failure_continuations();
        }
        self.reconcile_pending_context_recoveries(true);
        self.resume_restored_compaction_checkpoints(RestoredCheckpointAuthority::DiscoveryComplete);
        self.request_prompt_prewarm(&session_id);
        self.turn_state = TurnState::Idle;
        self.try_advance_queue();
        Ok(())
    }

    pub(super) fn request_prompt_prewarm(&mut self, session_id: &SessionId) {
        tracing::debug!(
            target: "harness",
            session_id = %session_id,
            "skipping prompt prewarm: no agent has been created yet",
        );
    }

    pub(super) fn finalize_agent_discovery(
        &mut self,
        agent_id: &tau_proto::AgentId,
    ) -> Result<(), HarnessError> {
        if self
            .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .is_none()
        {
            self.pending_agent_discovery.remove(agent_id);
            return Ok(());
        }
        let Some(pending) = self.pending_agent_discovery.get_mut(agent_id) else {
            return Ok(());
        };
        if !pending.waiting_on.is_empty() {
            return Ok(());
        }
        let mut diagnostics = Vec::new();
        let names = pending.skill_candidates.keys().cloned().collect::<Vec<_>>();
        for name in names {
            loop {
                let Some(winner) = pending
                    .skill_candidates
                    .get(&name)
                    .and_then(|slots| selected_skill_candidate(slots))
                    .cloned()
                else {
                    pending.skills.remove(&name);
                    break;
                };
                if user_skill_invocation::read_user_invoked_skill_body(&winner.source).is_ok() {
                    pending.skills.insert(name.clone(), winner);
                    break;
                }
                diagnostics.push(format!(
                    "skill skipped at agent initialization: {} from {} is unreadable",
                    name,
                    winner.source.label()
                ));
                if let Some(slots) = pending.skill_candidates.get_mut(&name) {
                    slots.retain(|candidate| candidate.source_id != winner.source_id);
                    if slots.is_empty() {
                        pending.skill_candidates.remove(&name);
                    }
                }
            }
        }
        let initialization_id = pending.initialization_id.clone();
        let agents_message = (!pending.agents_files.is_empty())
            .then(|| render_agents_context_message(pending.agents_files.iter()));
        let agents_files = pending
            .agents_files
            .iter()
            .map(|file| tau_proto::DiscoveryAgentsFileSummary {
                file_path: file.file_path.clone(),
                lines: file.content.lines().count() as u64,
                bytes: file.content.len() as u64,
            })
            .collect();
        let context = tau_proto::AgentInitializationContextSet {
            session_id: self.current_session_id.clone(),
            agent_id: agent_id.clone(),
            agent_initialization_id: initialization_id,
            agents_message,
            effective_skills: effective_skills(&pending.skills),
            agents_files,
        };
        for diagnostic in diagnostics {
            self.emit_info_important(&diagnostic);
        }
        let event = Event::AgentInitializationContextSet(context);
        let cid = self
            .runtime_agent_id_for_target_agent(Some(agent_id.as_str()))
            .expect("agent route checked before discovery finalization");
        self.publish_event_for_agent(&cid, None, event);
        Ok(())
    }

    pub(super) fn apply_finalized_agent_initialization_context(
        &mut self,
        context: &tau_proto::AgentInitializationContextSet,
    ) {
        let Some(pending) = self.pending_agent_discovery.remove(&context.agent_id) else {
            return;
        };
        if pending.initialization_id != context.agent_initialization_id {
            return;
        }
        let frozen = FrozenAgentDiscovery {
            initialization_id: pending.initialization_id,
            skills: pending.skills,
        };
        self.frozen_agent_discovery
            .insert(context.agent_id.clone(), frozen);
        let frozen = self
            .frozen_agent_discovery
            .get(&context.agent_id)
            .expect("just inserted frozen discovery");
        let projection = tau_proto::HarnessAgentContextInitialized {
            session_id: context.session_id.clone(),
            agent_id: context.agent_id.clone(),
            agent_initialization_id: frozen.initialization_id.clone(),
            listed_skills: effective_skills(&frozen.skills)
                .into_iter()
                .filter(|skill| skill.add_to_prompt && !skill.disable_model_invocation)
                .collect(),
            agents_files: context.agents_files.clone(),
        };
        self.agent_context_initialized
            .insert(context.agent_id.clone(), projection.clone());
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::HarnessAgentContextInitialized(projection),
        );
        self.complete_rendered_previews(&context.agent_id);
        self.drain_publish_idle_dispatches();
        self.try_advance_queue();
    }

    /// Persist a user-initiated `!` shell command's output as a
    /// tagged user message so the agent sees it in the next prompt.
    ///
    /// The XML-ish `<user_shell>` envelope lets the model reliably
    /// distinguish output the user pasted vs. output from its own
    /// tool calls, and survives round-tripping through conversation
    /// assembly.
    pub(super) fn inject_user_shell_output(&mut self, finished: &tau_proto::ShellCommandFinished) {
        let exit = finished
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| if finished.cancelled { "cancelled" } else { "?" }.to_owned());
        let text = format!(
            "<user_shell command={:?} exit_code={:?}>\n{}\n</user_shell>",
            finished.command, exit, finished.output,
        );
        let Some((cid, agent_id)) = self.resolve_shell_output_target_agent(finished) else {
            return;
        };
        let event = Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
            inference_activation: false,
            agent_id,
            text,
            message_class: tau_proto::PromptMessageClass::User,
        });
        // Stamp the publish with the target agent so the fold lands on the
        // branch whose transcript owned the command and the post-commit hook
        // syncs the conversation head.
        self.publish_event_for_agent(&cid, None, event);
    }

    /// Rebuild payload-free activation obligations from uncovered durable
    /// facts.
    pub(super) fn derive_replay_message_wakes(
        &self,
        agent_id: &str,
    ) -> VecDeque<PendingMessageWake> {
        let Some(tree) = self.agent_store.agent(agent_id) else {
            return VecDeque::new();
        };
        let Ok(records) = self.agent_store.agent_events(agent_id) else {
            return VecDeque::new();
        };
        records
            .iter()
            .filter_map(|record| {
                let source = match &record.event {
                    Event::AgentMessageReceived(message) => {
                        Some(PendingMessageWakeSource::AgentMessageReceived {
                            durable_event_seq: record.seq,
                            activation_class: agent_message_activation_class(message)?,
                            peer_admission_bytes: None,
                        })
                    }
                    event if event.message_agent_target().is_some() => {
                        tau_proto::project_message_fact(event)
                            .and_then(Result::ok)
                            .filter(|projection| projection.activates_model)
                            .map(|_| PendingMessageWakeSource::MessageFact {
                                durable_event_seq: record.seq,
                            })
                    }
                    _ => None,
                }?;
                let node_id = tree.node_for_durable_event_seq(record.seq);
                let covered = node_id.is_some_and(|node_id| {
                    records
                        .iter()
                        .skip(record.seq.get() as usize + 1)
                        .any(|later| {
                            matches!(
                                &later.event,
                                Event::AgentInferenceDispatchStarted(checkpoint)
                                    if tree.is_ancestor_head(
                                        tau_proto::AgentHead::Node(node_id),
                                        checkpoint.through,
                                    )
                            )
                        })
                });
                if covered {
                    return None;
                }
                Some(crate::agent::PendingMessageWake {
                    source,
                    node_id,
                    activation_observation: None,
                    source_observation: Some(record.observation_id),
                })
            })
            .collect()
    }

    /// Rebuild each uncovered activating prompt occurrence by durable sequence.
    pub(super) fn derive_replay_prompt_activations(
        &self,
        agent_id: &str,
    ) -> Vec<ReplayPromptActivationOccurrence> {
        let Some(tree) = self.agent_store.agent(agent_id) else {
            return Vec::new();
        };
        let Ok(records) = self.agent_store.agent_events(agent_id) else {
            return Vec::new();
        };
        records
            .iter()
            .filter(|record| {
                matches!(
                    &record.event,
                    Event::AgentPromptSubmitted(prompt) if prompt.inference_activation
                ) || matches!(
                    &record.event,
                    Event::AgentUserMessageInjected(prompt) if prompt.inference_activation
                ) || matches!(
                    &record.event,
                    Event::AgentPromptSteered(prompt) if prompt.inference_activation
                )
            })
            .filter_map(|record| {
                let node_id = tree.node_for_durable_event_seq(record.seq);
                let covered = node_id.is_some_and(|node_id| {
                    records
                        .iter()
                        .skip(record.seq.get() as usize + 1)
                        .any(|later| {
                            matches!(
                                &later.event,
                                Event::AgentInferenceDispatchStarted(checkpoint)
                                    if tree.is_ancestor_head(
                                        tau_proto::AgentHead::Node(node_id),
                                        checkpoint.through,
                                    )
                            )
                        })
                });
                (!covered).then_some(ReplayPromptActivationOccurrence {
                    source_seq: record.seq,
                    placement: node_id.map_or(
                        ReplayPromptActivationPlacement::Deferred,
                        ReplayPromptActivationPlacement::Materialized,
                    ),
                })
            })
            .collect()
    }

    pub(super) fn rehydrate_agents_from_session(&mut self) {
        if let Err(error) = self
            .store
            .lock_and_load_session(self.current_session_id.as_str())
        {
            self.session_roster_valid = false;
            self.emit_harness_failure(&format!(
                "failed to recover session membership during restore: {error}"
            ));
            return;
        }
        if let Err(error) = self
            .store
            .lock_and_recover_session_restore_events(self.current_session_id.as_str())
        {
            self.emit_harness_failure(&format!(
                "failed to recover session restore facts during restore: {error}"
            ));
            return;
        }
        let history = self.store.session_events(self.current_session_id.as_str());
        let ephemeral_history = self
            .store
            .ephemeral_membership_events(self.current_session_id.as_str());
        let membership = self
            .store
            .load_session(self.current_session_id.as_str())
            .map(|membership| membership.cloned());
        let (events, ephemeral_events, membership) = match (history, ephemeral_history, membership)
        {
            (Ok(events), Ok(ephemeral_events), Ok(Some(membership))) => {
                (events, ephemeral_events, membership)
            }
            (Ok(events), Ok(ephemeral_events), Ok(None))
                if events.is_empty() && ephemeral_events.is_empty() =>
            {
                return;
            }
            (history, ephemeral_history, membership) => {
                self.session_roster_valid = false;
                self.emit_harness_failure(&format!(
                    "failed to load session membership during restore: history={:?}, ephemeral={:?}, current={:?}",
                    history.err(),
                    ephemeral_history.err(),
                    membership.err()
                ));
                return;
            }
        };
        let ever_loaded = events
            .into_iter()
            .chain(ephemeral_events)
            .filter_map(|entry| match entry.event {
                Event::SessionAgentLoaded(loaded) => Some(loaded.agent_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let loaded_agents = membership
            .loaded_agents()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        self.session_ever_loaded_agents
            .extend(ever_loaded.iter().cloned());
        self.session_roster_ever_loaded_agents = ever_loaded;
        self.session_roster_loaded_agents = loaded_agents.iter().cloned().collect();
        let mut restored_parent_edges = Vec::new();
        let mut restored_output_length_steers = Vec::new();
        let mut restored_output_length_dormant = Vec::new();
        for agent_id in loaded_agents {
            let agent_id_string = agent_id.to_string();
            match self.agent_store.lock_and_recover_agent(agent_id.as_str()) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    self.emit_harness_failure(&format!(
                        "failed to load restored agent `{agent_id}`: journal is missing"
                    ));
                    continue;
                }
                Err(error) => {
                    self.emit_harness_failure(&format!(
                        "failed to load restored agent `{agent_id}`: {error}"
                    ));
                    continue;
                }
            }
            if !self.agent_store.agent_has_committed_identity(&agent_id) {
                self.emit_harness_failure(&format!(
                    "failed to load restored agent `{agent_id}`: journal has no committed creation"
                ));
                continue;
            }
            let head = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(|tree| tree.head());
            let cid: AgentId = crate::parse_agent_id(&agent_id_string);
            self.seed_agent_creator_topology(&agent_id);
            let meta = self
                .agent_store
                .agent_meta(agent_id.as_str())
                .ok()
                .flatten();
            let display_name = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(|tree| tree.display_name().map(str::to_owned))
                .or_else(|| meta.and_then(|meta| meta.display_name));
            let restored_runtime = self.restored_agent_runtime_from_log(agent_id.as_str());
            if !restored_runtime.resumable {
                self.restored_unavailable_agents.insert(
                    agent_id_string,
                    restored_runtime
                        .role
                        .unwrap_or_else(|| self.selected_role.clone()),
                );
                continue;
            }
            let builtin_query_id = match &restored_runtime.originator {
                tau_proto::PromptOriginator::Extension { name, query_id }
                    if name == harness_extension_name() && restored_runtime.tool_backed_start =>
                {
                    Some(query_id.clone())
                }
                _ => None,
            };
            let RestoredAgentRuntime {
                role,
                originator,
                navigation_mode,
                parent_agent,
                tool_backed_start,
                resumable: _,
            } = restored_runtime;
            let peer_entrypoint_endpoint = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(|tree| {
                    tree.metadata().get(&tau_proto::AgentMetadataKey::new(
                        path_crate_harness::subagents_tool::PEER_ENTRYPOINT_AGENT_METADATA_KEY,
                    ))
                })
                .is_some_and(|entry| entry.value == CborValue::Bool(true));
            let persistence = self.agent_store.agent_persistence(agent_id.as_str());
            let restored_compaction = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(tau_core::AgentTree::standalone_compaction_recovery);
            let restored_inference = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(tau_core::AgentTree::inference_dispatch_recovery);
            let restored_output_length = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(tau_core::AgentTree::output_length_continuation_recovery);
            let restored_output_length_spent = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(tau_core::AgentTree::output_length_budget_spent_outer_turn);
            let restored_output_length_terminal = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(tau_core::AgentTree::output_length_terminal_incomplete);
            let dormant_output_length_repair = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(tau_core::AgentTree::output_length_dormant_repair);
            let defer_manual_checkpoint = self
                .agent_store
                .agent(agent_id.as_str())
                .map(tau_core::AgentTree::manual_compaction_recoveries)
                .unwrap_or_default()
                .into_iter()
                .any(|recovery| {
                    matches!(
                        recovery,
                        tau_core::ManualCompactionRecovery::Started {
                            requested,
                            outcome: Some(ref outcome),
                            ..
                        } if requested.resume_inference
                            && matches!(
                                outcome.as_ref(),
                                tau_core::ManualCompactionOutcome::Succeeded(_)
                            )
                            && !self.self_compaction_terminal_delivered(&requested)
                    )
                });
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.agent_id = Some(agent_id_string.clone());
                conv.head = head;
                conv.originator = originator;
                conv.source_connection =
                    tool_backed_start.then(|| crate::harness::harness_connection_id().clone());
                conv.role = role.clone();
                conv.display_name = display_name.clone();
                conv.persistence = persistence;
                conv.peer_entrypoint_endpoint = peer_entrypoint_endpoint;
            } else {
                let runtime_incarnation = self.mint_agent_runtime_incarnation();
                let mut conv = Agent::new(
                    cid.clone(),
                    runtime_incarnation,
                    self.current_session_id.clone(),
                    originator,
                    head,
                    tool_backed_start.then(|| crate::harness::harness_connection_id().clone()),
                );
                conv.agent_id = Some(agent_id_string.clone());
                conv.role = role.clone();
                conv.display_name = display_name.clone();
                conv.persistence = persistence;
                conv.peer_entrypoint_endpoint = peer_entrypoint_endpoint;
                self.agents.insert(cid.clone(), conv);
            }
            if let Some(parent_agent) = parent_agent {
                restored_parent_edges.push((cid.clone(), parent_agent));
            }
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.restored_tool_backed_start = tool_backed_start;
                if dormant_output_length_repair.is_none()
                    && let Some(outer_turn_id) = restored_output_length_spent
                {
                    conv.outer_turn =
                        path_crate_agent::OuterTurnRuntimeState::Active(outer_turn_id.clone());
                    conv.output_length_continuation =
                        path_crate_agent::OutputLengthContinuationState::Spent { outer_turn_id };
                }
                if let Some(restored) = restored_output_length {
                    let repair = match restored {
                        tau_core::OutputLengthContinuationRecovery::SteerNeeded {
                            source,
                            successor_agent_prompt_id,
                            outer_turn_id,
                        } => Some((source, successor_agent_prompt_id, outer_turn_id, None)),
                        tau_core::OutputLengthContinuationRecovery::OwnerNeeded {
                            source,
                            successor_agent_prompt_id,
                            outer_turn_id,
                            through,
                        } => Some((
                            source,
                            successor_agent_prompt_id,
                            outer_turn_id,
                            Some(through),
                        )),
                        tau_core::OutputLengthContinuationRecovery::BranchInvalid { .. } => None,
                    };
                    if let Some((source, successor_agent_prompt_id, outer_turn_id, through)) =
                        repair
                        && let (Some(model), Some(operation), Some(activation_cut)) =
                            (source.model, source.operation, source.activation_cut)
                    {
                        conv.outer_turn =
                            path_crate_agent::OuterTurnRuntimeState::Active(outer_turn_id.clone());
                        let plan = path_crate_agent::OutputLengthContinuationPlan {
                            agent_prompt_id: successor_agent_prompt_id,
                            owner: tau_proto::OutputLengthContinuationOwner {
                                source_agent_prompt_id: source.agent_prompt_id,
                                outer_turn_id,
                                ordinal: 1,
                            },
                            dispatch: path_crate_agent::InferenceDispatchOwnership {
                                model,
                                operation,
                                activation_cut,
                            },
                        };
                        if let Some(through) = through {
                            conv.output_length_continuation =
                                path_crate_agent::OutputLengthContinuationState::OwnerReady(
                                    path_crate_agent::OutputLengthContinuationDispatch {
                                        plan,
                                        through,
                                    },
                                );
                            conv.pending_replay_activation = true;
                        } else {
                            conv.output_length_continuation =
                                path_crate_agent::OutputLengthContinuationState::Planned(plan);
                            restored_output_length_steers.push(cid.clone());
                        }
                    }
                }
            }
            if dormant_output_length_repair.is_some() {
                restored_output_length_dormant.push(cid.clone());
            }
            if let Some(query_id) = builtin_query_id {
                self.pending_builtin_delegates
                    .insert(query_id, agent_id_string.clone());
            }
            let restored_next_prompt_index = self.next_prompt_index_from_log(agent_id.as_str());
            if let Some(conv) = self.agents.get_mut(&cid)
                && !conv.prompt_index_initialized
            {
                conv.next_prompt_index = restored_next_prompt_index;
                conv.prompt_index_initialized = true;
            }
            self.clear_agent_context_usage(&cid);
            let restored_context_usage = self.restored_agent_context_usage(agent_id.as_str());
            if let Some((model, input_tokens, cached_tokens, usage_head)) = restored_context_usage
                && let Some(conv) = self.agents.get_mut(&cid)
            {
                conv.context_input_tokens = Some(input_tokens);
                conv.context_cached_tokens = Some(cached_tokens);
                conv.context_usage_head = usage_head;
                conv.context_usage_model = Some(model.clone());
                conv.context_percent_used =
                    context_window_for_model(&self.provider_model_info, &model)
                        .map(|window| context_percent_used(input_tokens, window));
            }
            self.agent_routes
                .insert(agent_id_string.clone(), cid.clone());
            if let Some(terminal) = restored_output_length_terminal {
                self.project_agent_watch_provider_state(
                    &cid,
                    terminal.agent_prompt_id,
                    tau_proto::AgentWatchProviderState::TerminalIncomplete {
                        category: tau_proto::AgentWatchProviderCategory::OutputLength,
                        attempt: terminal.provider_attempt.get(),
                    },
                );
            }
            self.agent_navigation_modes
                .entry(agent_id.clone())
                .or_insert(navigation_mode);
            match restored_compaction.clone() {
                Some(tau_core::StandaloneCompactionRecovery::AwaitingAutomaticStart {
                    ref decision,
                    finish_committed,
                    ..
                }) => {
                    if let Some(conv) = self.agents.get_mut(&cid) {
                        conv.pending_automatic_compaction_decision =
                            Some(decision.transaction_id.clone());
                        if !finish_committed {
                            conv.outer_turn =
                                path_crate_agent::OuterTurnRuntimeState::FinishInFlight(
                                    decision.outer_turn_id.clone(),
                                );
                        }
                    }
                }
                Some(tau_core::StandaloneCompactionRecovery::Blocked {
                    failed,
                    compact_prompt_id,
                }) => {
                    if let Some(conv) = self.agents.get_mut(&cid) {
                        conv.activation_dispatch =
                            path_crate_agent::ActivationDispatchState::Blocked {
                                failed_id: failed.transaction_id,
                                cut: failed.cut,
                                resume_through: failed.resume_through,
                            };
                    }
                    self.project_agent_watch_provider_state(
                        &cid,
                        compact_prompt_id,
                        tau_proto::AgentWatchProviderState::Blocked {
                            category: tau_proto::AgentWatchProviderCategory::Compaction,
                        },
                    );
                }
                Some(
                    ref recovery @ tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint {
                        ref transaction_id,
                        ..
                    },
                ) if !defer_manual_checkpoint => {
                    let lineage_owner =
                        self.agent_store.agent(agent_id.as_str()).and_then(|tree| {
                            tree.output_length_lineage_owner_for_transaction(transaction_id)
                        });
                    self.stage_restored_compaction_recovery(&cid, recovery)
                        .expect("restored agent exists");
                    if let Some(owner) = lineage_owner
                        && let Some(conv) = self.agents.get_mut(&cid)
                        && let path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                            agent_prompt_id,
                            through,
                            dispatch,
                            ..
                        } = &conv.activation_dispatch
                    {
                        conv.outer_turn = path_crate_agent::OuterTurnRuntimeState::Active(
                            owner.outer_turn_id.clone(),
                        );
                        conv.output_length_continuation =
                            path_crate_agent::OutputLengthContinuationState::Active(
                                path_crate_agent::OutputLengthContinuationDispatch {
                                    plan: path_crate_agent::OutputLengthContinuationPlan {
                                        agent_prompt_id: agent_prompt_id.clone(),
                                        owner,
                                        dispatch: dispatch.clone(),
                                    },
                                    through: *through,
                                },
                            );
                    }
                }
                Some(tau_core::StandaloneCompactionRecovery::DispatchUncertain(checkpoint)) => {
                    let status_prompt_id = checkpoint.agent_prompt_id.clone();
                    self.prompt_agents
                        .insert(status_prompt_id.clone(), cid.clone());
                    if let Some(model) = checkpoint.model.clone() {
                        self.prompt_models.insert(status_prompt_id.clone(), model);
                    }
                    if let Some(operation) = checkpoint.operation {
                        self.prompt_operations
                            .insert(status_prompt_id.clone(), (operation, true));
                    }
                    let restored_lineage_owner =
                        self.agent_store.agent(agent_id.as_str()).and_then(|tree| {
                            tree.output_length_lineage_owner_for_prompt(&checkpoint.agent_prompt_id)
                        });
                    if let Some(conv) = self.agents.get_mut(&cid) {
                        if let Some(owner) = restored_lineage_owner
                            && let (Some(model), Some(operation), Some(activation_cut)) = (
                                checkpoint.model.clone(),
                                checkpoint.operation,
                                checkpoint.activation_cut,
                            )
                        {
                            conv.outer_turn = path_crate_agent::OuterTurnRuntimeState::Active(
                                owner.outer_turn_id.clone(),
                            );
                            conv.output_length_continuation =
                                path_crate_agent::OutputLengthContinuationState::Active(
                                    path_crate_agent::OutputLengthContinuationDispatch {
                                        plan: path_crate_agent::OutputLengthContinuationPlan {
                                            agent_prompt_id: checkpoint.agent_prompt_id.clone(),
                                            owner,
                                            dispatch:
                                                path_crate_agent::InferenceDispatchOwnership {
                                                    model,
                                                    operation,
                                                    activation_cut,
                                                },
                                        },
                                        through: checkpoint.through,
                                    },
                                );
                        }
                        conv.activation_dispatch =
                            path_crate_agent::ActivationDispatchState::DispatchUncertain {
                                owner: path_crate_agent::InferenceCheckpointOwner::Standalone {
                                    id: checkpoint
                                        .transaction_id
                                        .expect("standalone checkpoint has transaction"),
                                },
                                agent_prompt_id: checkpoint.agent_prompt_id,
                                through: checkpoint.through,
                                model: checkpoint.model,
                                operation: checkpoint.operation,
                                activation_cut: checkpoint.activation_cut,
                            };
                    }
                    self.project_agent_watch_provider_state(
                        &cid,
                        status_prompt_id,
                        tau_proto::AgentWatchProviderState::DispatchUncertain {
                            category: tau_proto::AgentWatchProviderCategory::Compaction,
                        },
                    );
                    self.emit_info_important(&format!(
                        "inference dispatch for restored agent `{cid}` is uncertain; retry explicitly"
                    ));
                }
                Some(tau_core::StandaloneCompactionRecovery::Interrupted(_)) | None => {}
                Some(tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint { .. }) => {}
            }
            if restored_compaction.is_none()
                && let Some(tau_core::InferenceDispatchRecovery::ContextRecoveryRequired(
                    checkpoint,
                )) = restored_inference.clone()
                && let Some(conv) = self.agents.get_mut(&cid)
            {
                if let Some(owner) = self.agent_store.agent(agent_id.as_str()).and_then(|tree| {
                    tree.output_length_lineage_owner_for_prompt(&checkpoint.agent_prompt_id)
                }) && let (Some(model), Some(operation), Some(activation_cut)) = (
                    checkpoint.model.clone(),
                    checkpoint.operation,
                    checkpoint.activation_cut,
                ) {
                    conv.outer_turn = path_crate_agent::OuterTurnRuntimeState::Active(
                        owner.outer_turn_id.clone(),
                    );
                    conv.output_length_continuation =
                        path_crate_agent::OutputLengthContinuationState::Active(
                            path_crate_agent::OutputLengthContinuationDispatch {
                                plan: path_crate_agent::OutputLengthContinuationPlan {
                                    agent_prompt_id: checkpoint.agent_prompt_id.clone(),
                                    owner,
                                    dispatch: path_crate_agent::InferenceDispatchOwnership {
                                        model,
                                        operation,
                                        activation_cut,
                                    },
                                },
                                through: checkpoint.through,
                            },
                        );
                }
                conv.activation_dispatch =
                    path_crate_agent::ActivationDispatchState::ContextRecoveryPending {
                        checkpoint,
                    };
            }
            if restored_compaction.is_none()
                && let Some(tau_core::InferenceDispatchRecovery::DispatchUncertain(checkpoint)) =
                    restored_inference.clone()
            {
                let status_prompt_id = checkpoint.agent_prompt_id.clone();
                if let Some(conv) = self.agents.get_mut(&cid) {
                    conv.activation_dispatch =
                        path_crate_agent::ActivationDispatchState::DispatchUncertain {
                            owner: match checkpoint.transaction_id {
                                Some(id) => {
                                    path_crate_agent::InferenceCheckpointOwner::Standalone { id }
                                }
                                None => path_crate_agent::InferenceCheckpointOwner::Inference,
                            },
                            agent_prompt_id: checkpoint.agent_prompt_id,
                            through: checkpoint.through,
                            model: checkpoint.model,
                            operation: checkpoint.operation,
                            activation_cut: checkpoint.activation_cut,
                        };
                }
                self.project_agent_watch_provider_state(
                    &cid,
                    status_prompt_id,
                    tau_proto::AgentWatchProviderState::DispatchUncertain {
                        category: tau_proto::AgentWatchProviderCategory::Unknown,
                    },
                );
                self.emit_info_important(&format!(
                    "inference dispatch for restored agent `{cid}` is uncertain; retry explicitly"
                ));
            }
            let derive_activations = !matches!(
                restored_compaction,
                Some(
                    tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint { .. }
                        | tau_core::StandaloneCompactionRecovery::AwaitingAutomaticStart { .. }
                )
            );
            let replay_message_wakes = self.derive_replay_message_wakes(agent_id.as_str());
            let replay_prompt_activations =
                self.derive_replay_prompt_activations(agent_id.as_str());
            if let Some(conv) = self.agents.get_mut(&cid) {
                conv.pending_message_wakes = replay_message_wakes;
            }
            if derive_activations && !replay_prompt_activations.is_empty() {
                self.pending_replay_prompt_activation_occurrences
                    .insert(cid.clone(), replay_prompt_activations);
            }
            let uncertain_prompt_activation = restored_inference.as_ref().is_some_and(|recovery| {
                let tau_core::InferenceDispatchRecovery::DispatchUncertain(checkpoint) = recovery
                else {
                    return false;
                };
                self.agent_store
                    .agent(agent_id.as_str())
                    .is_some_and(|tree| {
                        tree.marked_inference_has_deferred_prompt_activation(
                            &checkpoint.agent_prompt_id,
                        )
                    })
            });
            let uncertain_with_deferred_activation = restored_compaction.is_none()
                && matches!(
                    restored_inference,
                    Some(tau_core::InferenceDispatchRecovery::DispatchUncertain(_))
                )
                && (uncertain_prompt_activation
                    || self.agents.get(&cid).is_some_and(|agent| {
                        agent
                            .pending_message_wakes
                            .iter()
                            .any(|wake| wake.node_id.is_none())
                    }));
            if uncertain_with_deferred_activation
                && let Some(tau_core::InferenceDispatchRecovery::DispatchUncertain(checkpoint)) =
                    restored_inference.clone()
                && let Some(originator) =
                    self.agents.get(&cid).map(|agent| agent.originator.clone())
            {
                self.pending_replay_uncertain_stale.insert(
                    cid.clone(),
                    AgentPromptTerminated {
                        automatic_compaction_decision: None,
                        agent_id: agent_id.clone(),
                        agent_prompt_id: checkpoint.agent_prompt_id,
                        reason: AgentPromptTerminationReason::Stale,
                        originator,
                    },
                );
            }
            self.session_loaded_agents.insert(agent_id.clone());
            if let Some(tau_core::StandaloneCompactionRecovery::AwaitingAutomaticStart {
                decision,
                cut,
                finish_committed,
            }) = restored_compaction.clone()
            {
                if finish_committed {
                    self.start_eager_automatic_compaction(&cid, decision, cut);
                } else {
                    self.publish_for_agent(
                        &cid,
                        Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
                            automatic_compaction_decision: Some(decision.transaction_id.clone()),
                            agent_id: agent_id.clone(),
                            session_id: self.current_session_id.clone(),
                            outer_turn_id: decision.outer_turn_id,
                            disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                        }),
                    );
                }
            }
            let finish_repair = self
                .agent_store
                .agent(agent_id.as_str())
                .and_then(tau_core::AgentTree::output_length_outer_turn_finish_repair);
            if dormant_output_length_repair.is_none()
                && let Some(outer_turn_id) = finish_repair
            {
                if let Some(agent) = self.agents.get_mut(&cid) {
                    agent.outer_turn = path_crate_agent::OuterTurnRuntimeState::FinishInFlight(
                        outer_turn_id.clone(),
                    );
                }
                self.publish_for_agent(
                    &cid,
                    Event::AgentOuterTurnFinished(tau_proto::AgentOuterTurnFinished {
                        automatic_compaction_decision: None,
                        agent_id: agent_id.clone(),
                        session_id: self.current_session_id.clone(),
                        outer_turn_id,
                        disposition: tau_proto::AgentOuterTurnDisposition::Settled,
                    }),
                );
            }
            if let Some(tau_core::StandaloneCompactionRecovery::Interrupted(started)) =
                restored_compaction
            {
                self.publish_for_agent(
                    &cid,
                    Event::AgentStandaloneCompactionFailed(
                        tau_proto::AgentStandaloneCompactionFailed {
                            agent_id: started.agent_id,
                            transaction_id: started.transaction_id,
                            cut: started.cut,
                            reason: tau_proto::StandaloneCompactionFailureReason::Interrupted,
                            resume_through: started.resume_through,
                        },
                    ),
                );
            }
        }
        for (child_cid, parent_agent_id) in restored_parent_edges {
            if let Some(parent_cid) = self.agent_routes.get(parent_agent_id.as_str()).cloned()
                && let Some(child) = self.agents.get_mut(&child_cid)
            {
                child.parent_agent_id = Some(parent_cid);
            }
        }
        for cid in restored_output_length_steers {
            if let Some(agent) = self.agents.get_mut(&cid) {
                agent
                    .pending_prompts
                    .push_back(PendingPrompt::output_length_continuation());
                agent.turn_state = AgentTurnState::Idle;
            }
            self.fold_pending_prompts_as_steered(&cid);
            self.dispatch_activation_after_publish_idle(&cid);
        }
        for cid in restored_output_length_dormant {
            self.repair_dormant_output_length_lineage(&cid);
        }
        if !self.provider_model_info.is_empty() {
            self.reconcile_agent_context_usage_models();
        }
        let restored_manual_compactions = self
            .agents
            .iter()
            .flat_map(|(cid, agent)| {
                agent
                    .agent_id
                    .as_deref()
                    .and_then(|agent_id| self.agent_store.agent(agent_id))
                    .map(tau_core::AgentTree::manual_compaction_recoveries)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|recovery| (cid.clone(), recovery))
            })
            .collect();
        self.restore_manual_compaction_tools(restored_manual_compactions);
    }

    /// Restores durable descriptive facts while detaching a completed worker
    /// from the run-local start request that created it.
    pub(super) fn restored_agent_runtime_from_log(&self, agent_id: &str) -> RestoredAgentRuntime {
        let events = self
            .agent_store
            .agent_events(agent_id)
            .inspect_err(|error| {
                tracing::warn!(target: "tau_harness", %agent_id, %error, "failed to load agent events for runtime restore");
            })
            .unwrap_or_default();
        let creation = events.iter().find_map(|record| match &record.event {
            Event::AgentStarted(started) => Some(started),
            _ => None,
        });
        let role = creation
            .map(|started| started.role.clone())
            .filter(|role| self.available_roles.contains_key(role));
        let historical_originator = events.iter().find_map(|record| match &record.event {
            Event::AgentPromptSubmitted(submitted) => Some(submitted.originator.clone()),
            Event::ProviderResponseFinished(finished) => Some(finished.originator.clone()),
            Event::ProviderToolResult(result) => Some(result.originator.clone()),
            Event::ToolBackgroundResult(result) => Some(result.originator.clone()),
            Event::ToolBackgroundError(error) => Some(error.originator.clone()),
            _ => None,
        });
        let durable_extension_originator = matches!(
            historical_originator,
            Some(tau_proto::PromptOriginator::Extension { .. })
        );
        let historical_originator =
            historical_originator.unwrap_or_else(|| tau_proto::PromptOriginator::Extension {
                name: harness_extension_name().clone(),
                query_id: format!("restored-{agent_id}"),
            });
        let completed_worker = Self::journal_proves_completed_start_agent_worker(
            &events,
            creation,
            &historical_originator,
        );
        if completed_worker {
            return RestoredAgentRuntime {
                role,
                originator: tau_proto::PromptOriginator::User,
                navigation_mode: tau_proto::AgentNavigationMode::ActiveAuto,
                parent_agent: None,
                tool_backed_start: false,
                resumable: true,
            };
        }
        let parent_agent = creation.and_then(|started| started.parent_agent.clone());
        let tool_backed_start = creation.is_some_and(|started| {
            matches!(started.creator, Some(tau_proto::AgentCreator::Agent { .. }))
                && started.parent_agent.is_some()
        });
        let peer_entrypoint = creation.is_some_and(|started| {
            started.metadata.iter().any(|metadata| {
                metadata.key.as_str()
                    == path_crate_harness::subagents_tool::PEER_ENTRYPOINT_AGENT_METADATA_KEY
                    && metadata.value == CborValue::Bool(true)
            })
        });
        let harness_delegation = matches!(
            &historical_originator,
            tau_proto::PromptOriginator::Extension { name, query_id }
                if name == harness_extension_name()
                    && query_id.starts_with("delegate-")
                    && tool_backed_start
        );
        let extension_side_request = creation.is_some_and(|started| {
            matches!(
                started.creator,
                Some(tau_proto::AgentCreator::Extension { .. })
            )
        });
        let resumable = historical_originator.is_user()
            || harness_delegation
            || peer_entrypoint
            || (!durable_extension_originator && !extension_side_request);
        RestoredAgentRuntime {
            role,
            navigation_mode: default_navigation_mode(&historical_originator),
            originator: historical_originator,
            parent_agent,
            tool_backed_start,
            resumable,
        }
    }

    /// Returns whether durable events match a warm side-request terminal path.
    pub(super) fn journal_proves_completed_start_agent_worker(
        events: &[tau_core::PersistedAgentEvent],
        creation: Option<&tau_proto::AgentStarted>,
        historical_originator: &tau_proto::PromptOriginator,
    ) -> bool {
        if !matches!(
            (historical_originator, creation),
            (
                tau_proto::PromptOriginator::Extension { .. },
                Some(tau_proto::AgentStarted {
                    parent_agent: Some(_),
                    ..
                })
            )
        ) {
            return false;
        }
        if events.iter().any(|record| {
            matches!(
                &record.event,
                Event::AgentPromptSubmitted(submitted) if submitted.originator.is_user()
            )
        }) {
            return true;
        }

        let Some(agent_id) = creation.map(|started| started.agent_id.clone()) else {
            return false;
        };
        let Ok(tree) = tau_core::AgentTree::try_from_events(agent_id, events) else {
            return false;
        };
        let message_nodes: HashMap<_, _> = tree
            .nodes()
            .iter()
            .filter_map(|node| match node.entry {
                tau_core::AgentEntry::AgentMessage {
                    durable_event_seq,
                    direction: tau_core::AgentMessageDirection::Inbound,
                    ..
                } => Some((durable_event_seq, node.id)),
                _ => None,
            })
            .collect();
        let mut outstanding_message_nodes = HashSet::new();
        let mut completed = false;
        for (index, record) in events.iter().enumerate() {
            match &record.event {
                Event::AgentMessageReceived(message)
                    if agent_message_activation_class(message).is_some() =>
                {
                    if let Some(node_id) = message_nodes.get(&record.seq) {
                        outstanding_message_nodes.insert(*node_id);
                    }
                    completed = false;
                }
                Event::AgentStandaloneCompactionFailed(failed)
                    if failed.reason != tau_proto::StandaloneCompactionFailureReason::Cancelled
                        && Self::compaction_failure_matches_originator(
                            &events[..index],
                            failed,
                            historical_originator,
                        ) =>
                {
                    completed = outstanding_message_nodes.is_empty();
                }
                Event::ProviderResponseFinished(finished)
                    if finished.originator == *historical_originator
                        && finished.recovery_disposition
                            == tau_proto::ContextRecoveryDisposition::None
                        && !finished
                            .output_items
                            .iter()
                            .any(|item| matches!(item, ContextItem::ToolCall(_)))
                        && let Some(checkpoint) = Self::response_inference_checkpoint(
                            &events[..index],
                            &finished.agent_prompt_id,
                        ) =>
                {
                    let branch: HashSet<_> = tree
                        .branch_node_ids_from(checkpoint.through.as_option())
                        .into_iter()
                        .collect();
                    outstanding_message_nodes.retain(|node_id| !branch.contains(node_id));
                    completed = outstanding_message_nodes.is_empty();
                }
                _ => {}
            }
        }
        completed
    }

    pub(super) fn restored_agent_context_usage(
        &self,
        agent_id: &str,
    ) -> Option<(ModelId, u64, u64, Option<tau_proto::NodeId>)> {
        let tree = self.agent_store.agent(agent_id)?;
        self.agent_context_usage_at(agent_id, tree.head())
    }

    /// Returns the newest model-qualified usage on one selected branch without
    /// crossing a compaction boundary.
    pub(super) fn agent_context_usage_at(
        &self,
        agent_id: &str,
        head: Option<tau_proto::NodeId>,
    ) -> Option<(ModelId, u64, u64, Option<tau_proto::NodeId>)> {
        let tree = self.agent_store.agent(agent_id)?;
        for node_id in tree.branch_node_ids_from(head).into_iter().rev() {
            let node = tree.node(node_id)?;
            match &node.entry {
                tau_core::AgentEntry::Compaction { .. } => return None,
                tau_core::AgentEntry::AssistantResponse {
                    usage: Some(usage), ..
                } => {
                    let model = usage.model.as_ref()?;
                    return Some((
                        model.clone(),
                        usage.prompt_sent_tokens,
                        usage.prompt_cached_tokens,
                        node.parent_id,
                    ));
                }
                _ => {}
            }
        }
        None
    }
}

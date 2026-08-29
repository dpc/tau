//! Owns live, restored, manual, and automatic compaction runtime authority.
//!
//! The recovery and closed-prefix rules are governed by
//! `SPEC-compaction-and-context-recovery`.

use super::*;

/// Result of checking one durable successful compaction for rolling work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RollingCompactionPass {
    /// No additional pass is authorized.
    NotNeeded,
    /// A provider-backed continuation start was published.
    Started,
    /// A linked typed local failure start was published and is terminalizing.
    Terminalizing,
}

impl Harness {
    /// Claim a queued UI request at a provider-closed boundary before ordinary
    /// inference can advance.
    pub(super) fn try_start_queued_ui_compaction(&mut self, cid: &AgentId) -> bool {
        let request_key = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .iter()
            .find_map(|(request_key, accepted)| {
                (accepted.request.is_ui_request()
                    && self
                        .runtime_agent_id_for_target_agent(Some(
                            accepted.request.target_agent_id.as_str(),
                        ))
                        .as_ref()
                        == Some(cid))
                .then(|| request_key.clone())
            });
        request_key
            .is_some_and(|request_key| self.start_accepted_manual_compaction(cid, &request_key))
    }

    pub(super) fn handle_compact_request(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        session_id: SessionId,
        target_agent_id: Option<&str>,
    ) {
        if session_id != self.session_runtime.current_session_id {
            self.send_ui_error_response(
                client_id,
                format!(
                    "cannot compact session `{session_id}` in this harness; active session is `{}`",
                    self.session_runtime.current_session_id
                ),
            );
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(target_agent_id) else {
            self.send_ui_error_response(client_id, "unknown agent for compaction");
            return;
        };
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(&cid) else {
            self.send_ui_error_response(client_id, "target user agent is missing");
            return;
        };
        if agent.dispatch.terminating {
            self.send_ui_error_response(
                client_id,
                "cannot compact while a prompt or tool turn is in flight",
            );
            return;
        }
        if !self.agent_model_supports_compaction(&cid) {
            self.send_ui_error_response(client_id, "selected model does not support compaction");
            return;
        }
        let Some(agent_id) = agent
            .identity
            .agent_id
            .as_deref()
            .map(crate::parse_agent_id)
        else {
            self.send_ui_error_response(client_id, "nothing to compact yet");
            return;
        };
        let turn_state = agent.turn.turn_state.clone();
        let activation_dispatch = agent.dispatch.activation_dispatch.clone();
        if let Some(request_key) = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .iter()
            .find_map(|(request_key, accepted)| {
                (accepted.request.target_agent_id == agent_id && accepted.request.is_ui_request())
                    .then(|| request_key.clone())
            })
        {
            if self
                .prompt_coordination
                .compaction_runtime
                .pending_manual_acceptances
                .get(&request_key)
                .is_some_and(|pending| matches!(pending, PendingManualCompactionAcceptance::Ui(_)))
            {
                self.prompt_coordination
                    .compaction_runtime
                    .pending_ui_acknowledgements
                    .entry(request_key)
                    .or_default()
                    .push(client_id.clone());
            } else {
                self.send_ui_response(client_id, "compaction already queued");
                self.continue_committed_ui_compaction(&cid, agent_id.clone());
            }
            return;
        }
        if self
            .prompt_coordination
            .compaction_runtime
            .has_ui_start_for_agent(&agent_id)
        {
            self.send_ui_response(client_id, "compaction already queued");
            return;
        }
        if let Some(request_key) = self
            .prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .iter()
            .find_map(|(key, pending)| {
                (pending.request().target_agent_id == agent_id).then(|| key.clone())
            })
        {
            if matches!(
                self.prompt_coordination
                    .compaction_runtime
                    .pending_manual_acceptances
                    .get(&request_key),
                Some(PendingManualCompactionAcceptance::Ui(_))
            ) {
                self.prompt_coordination
                    .compaction_runtime
                    .pending_ui_acknowledgements
                    .entry(request_key)
                    .or_default()
                    .push(client_id.clone());
            } else {
                self.send_ui_response(client_id, "compaction already queued");
            }
            return;
        }
        let standalone =
            self.model_for_agent_role(agent).filter(|model| {
                self.provider_runtime.model_info.get(model).is_some_and(
                    tau_proto::ProviderModelInfo::supports_explicit_standalone_compaction,
                ) && self.provider_runtime.model_routes.contains_key(model)
            });
        if standalone.is_none() {
            if matches!(turn_state, AgentTurnState::Idle)
                && matches!(
                    activation_dispatch,
                    crate::agent::ActivationDispatchState::None
                )
            {
                self.start_admitted_manual_compaction(&cid);
            } else if self.try_start_inline_compaction_after_wait(&cid, &turn_state) {
                // The legacy inline compactor has no standalone transaction.
                // Preserve its sole-live-wait optimization as an immediate,
                // closed-round operation rather than pretending it is queued.
            } else {
                self.send_ui_error_response(
                    client_id,
                    "selected model cannot queue standalone compaction",
                );
            }
            return;
        }
        self.accept_ui_manual_compaction(
            &cid,
            agent_id,
            standalone.expect("checked"),
            client_id.clone(),
        );
        self.drain_publish_idle_dispatches();
    }

    /// Preserve inline compaction's legacy sole-wait preemption when the model
    /// has no standalone transaction support.
    fn try_start_inline_compaction_after_wait(
        &mut self,
        cid: &AgentId,
        turn_state: &AgentTurnState,
    ) -> bool {
        let AgentTurnState::ToolsRunning { remaining_calls } = turn_state else {
            return false;
        };
        let [wait_call_id] = remaining_calls.as_slice() else {
            return false;
        };
        if self
            .tool_routing
            .tool_runtime
            .pending_terminal_observations
            .contains_key(wait_call_id)
        {
            return false;
        }
        let Some(tool) = self
            .tool_routing
            .tool_runtime
            .pending_tools
            .get(wait_call_id)
            .cloned()
        else {
            return false;
        };
        if tool.name.as_str() != path_crate_harness::subagents_tool::WAIT_TOOL_NAME
            || !self.claim_wait_for_manual_compaction(cid, wait_call_id)
        {
            return false;
        }
        self.observe_tool_terminal(cid, wait_call_id, tau_proto::ToolTerminalCause::Unknown);
        self.publish_for_agent(
            cid,
            Event::ToolCancelled(ToolCancelled {
                presentation: Default::default(),
                call_id: wait_call_id.clone(),
                tool_name: tool.name,
                tool_type: tool.tool_type,
                display: None,
            }),
        );
        self.start_admitted_manual_compaction(cid);
        true
    }

    /// Continue a UI request only after its durable acceptance has committed.
    pub(super) fn continue_committed_ui_compaction(
        &mut self,
        cid: &AgentId,
        agent_id: tau_proto::AgentId,
    ) {
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return;
        };
        let turn_state = agent.turn.turn_state.clone();
        let activation_dispatch = agent.dispatch.activation_dispatch.clone();
        let request_key = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .iter()
            .find_map(|(id, accepted)| {
                (accepted.request.target_agent_id == agent_id && accepted.request.is_ui_request())
                    .then(|| id.clone())
            })
            .expect("committed UI request is installed");
        if matches!(turn_state, AgentTurnState::Idle) {
            self.start_accepted_manual_compaction(cid, &request_key);
            return;
        }
        if !matches!(
            activation_dispatch,
            crate::agent::ActivationDispatchState::None
        ) {
            return;
        }
        let AgentTurnState::ToolsRunning { remaining_calls } = &turn_state else {
            return;
        };
        let [wait_call_id] = remaining_calls.as_slice() else {
            return;
        };
        if let Some(pending) = self
            .prompt_coordination
            .compaction_runtime
            .pending_ui_after_wait
            .get(cid)
            && pending.wait_call_id == *wait_call_id
            && self.wait_claimed_for_manual_compaction(cid, wait_call_id)
        {
            return;
        }
        if self
            .tool_routing
            .tool_runtime
            .pending_terminal_observations
            .contains_key(wait_call_id)
        {
            return;
        }
        let wait_call_id = wait_call_id.clone();
        let Some(tool) = self
            .tool_routing
            .tool_runtime
            .pending_tools
            .get(&wait_call_id)
            .cloned()
        else {
            return;
        };
        if tool.name.as_str() != path_crate_harness::subagents_tool::WAIT_TOOL_NAME
            || !self.claim_wait_for_manual_compaction(cid, &wait_call_id)
        {
            return;
        }
        self.prompt_coordination
            .compaction_runtime
            .pending_ui_after_wait
            .insert(
                cid.clone(),
                PendingUiCompactionAfterWait {
                    session_generation: self.session_runtime.current_session_generation,
                    agent_id,
                    wait_call_id: wait_call_id.clone(),
                },
            );
        self.observe_tool_terminal(cid, &wait_call_id, tau_proto::ToolTerminalCause::Unknown);
        self.publish_for_agent(
            cid,
            Event::ToolCancelled(ToolCancelled {
                presentation: Default::default(),
                call_id: wait_call_id,
                tool_name: tool.name,
                tool_type: tool.tool_type,
                display: None,
            }),
        );
    }

    /// Durably accept one UI compaction intent before acknowledging it.
    fn accept_ui_manual_compaction(
        &mut self,
        cid: &AgentId,
        agent_id: tau_proto::AgentId,
        model: ModelId,
        requester_client_id: tau_proto::ConnectionId,
    ) {
        let agent = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .expect("accepted UI compaction has a loaded target");
        let requested_target_head = agent
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let target_generation = self
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .map_or(
                tau_proto::MaterializedPromptGeneration::initial(),
                tau_core::AgentTree::ordinary_inference_generation,
            );
        let target_role = self.role_name_for_agent(agent);
        let ordinal = self
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .map_or(0, tau_core::AgentTree::manual_compaction_request_count);
        let request_id = tau_proto::CompactionRequestId::parse(format!(
            "cr-ui-{}-{ordinal}",
            agent.dispatch.next_prompt_index
        ))
        .expect("generated UI compaction request id");
        let request = tau_proto::AgentManualCompactionRequested {
            request_id: request_id.clone(),
            target_agent_id: agent_id,
            source: tau_proto::ManualCompactionSource::UiCompact {
                ui_compact: tau_proto::UiManualCompactionSource {
                    eligible_automatic_transaction_id: agent
                        .turn
                        .automatic_compaction
                        .transaction_id()
                        .cloned()
                        .or_else(|| match &agent.dispatch.activation_dispatch {
                            ActivationDispatchState::Running { id, .. } => Some(id.clone()),
                            _ => None,
                        }),
                    target_role,
                },
            },
            requested_target_head,
            target_generation,
            model,
        };
        self.prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .insert(
                ManualCompactionRequestKey::for_request(&request),
                PendingManualCompactionAcceptance::Ui(AcceptedManualCompactionTool {
                    request: request.clone(),
                    visible_tool_name: ToolName::new("compact"),
                }),
            );
        self.prompt_coordination
            .compaction_runtime
            .pending_ui_acknowledgements
            .insert(
                ManualCompactionRequestKey::for_request(&request),
                vec![requester_client_id],
            );
        self.publish_for_agent(cid, Event::AgentManualCompactionRequested(request));
    }

    /// Clear one UI admission that failed before its durable acceptance.
    pub(super) fn rollback_rejected_ui_compaction_acceptance(&mut self, event: &Event) {
        let Event::AgentManualCompactionRequested(requested) = event else {
            return;
        };
        if !requested.is_ui_request() {
            return;
        }
        self.prompt_coordination
            .compaction_runtime
            .remove_pending_ui_acceptance(&ManualCompactionRequestKey::for_request(requested));
        if let Some(requesters) = self
            .prompt_coordination
            .compaction_runtime
            .pending_ui_acknowledgements
            .remove(&ManualCompactionRequestKey::for_request(requested))
        {
            for requester in requesters {
                self.send_ui_error_response(&requester, "compaction could not be queued durably");
            }
        }
    }

    /// Retain an append-rejected UI claim for exact retry without deriving a
    /// second transaction or dispatching provider work.
    pub(super) fn retain_rejected_ui_compaction_start(&mut self, event: &Event) {
        if let Event::AgentStandaloneCompactionStarted(started) = event
            && matches!(
                started.trigger,
                tau_proto::StandaloneCompactionTrigger::ManualUi { .. }
            )
            && let Some(cid) =
                self.runtime_agent_id_for_target_agent(Some(started.agent_id.as_str()))
        {
            self.prompt_coordination
                .compaction_runtime
                .rejected_ui_starts
                .insert(cid, event.clone());
        }
    }

    /// Start the existing manual compaction flow after all admission checks
    /// have established an idle target.
    pub(super) fn start_admitted_manual_compaction(&mut self, cid: &AgentId) {
        let conv = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .expect("admitted manual compaction has a loaded target");
        let agent_id = conv
            .identity
            .agent_id
            .clone()
            .expect("admitted manual compaction has a durable target");
        let standalone_model = self.model_for_agent_role(conv).filter(|model| {
            self.provider_runtime
                .model_info
                .get(model)
                .is_some_and(tau_proto::ProviderModelInfo::supports_explicit_standalone_compaction)
        });
        if let Some(model) = standalone_model {
            let current_head = conv
                .identity
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
            let blocked_recovery = self
                .matching_durable_failed_recovery(&agent_id, &model, current_head)
                .map(|failed| {
                    (
                        failed.transaction_id.clone(),
                        failed.cut,
                        failed.resume_through,
                    )
                });
            let normalized_blocked_cut =
                blocked_recovery
                    .as_ref()
                    .and_then(|(_, failed_cut, resume)| {
                        self.normalized_blocked_recovery_cut(
                            &agent_id,
                            *failed_cut,
                            *resume,
                            current_head,
                        )
                    });
            if blocked_recovery.is_some() && normalized_blocked_cut.is_none() {
                self.emit_info(
                    "cannot recover blocked compaction after navigating away from its owed branch",
                );
                return;
            }
            let (cut, resume_through, supersedes) = blocked_recovery.map_or_else(
                || (current_head, None, None),
                |(failed_id, _, resume)| {
                    (
                        normalized_blocked_cut
                            .expect("validated blocked recovery has a normalized cut"),
                        resume.map(|_| current_head),
                        Some(failed_id),
                    )
                },
            );
            let transaction_id = tau_proto::CompactionTransactionId::parse(format!(
                "ct-{}",
                conv.dispatch.next_prompt_index
            ))
            .expect("generated compaction transaction id is valid");
            let compact_prompt_id = tau_proto::AgentPromptId::parse(format!(
                "ap-{agent_id}-{}",
                conv.dispatch.next_prompt_index
            ))
            .expect("known-safe AgentPromptId must be valid");
            let originator = conv.identity.originator.clone();
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent.dispatch.next_prompt_index =
                    agent.dispatch.next_prompt_index.saturating_add(1);
            }
            self.publish_for_agent(
                cid,
                Event::AgentStandaloneCompactionStarted(
                    tau_proto::AgentStandaloneCompactionStarted {
                        compact_prompt_id,
                        operation: tau_proto::PromptOperation::StandaloneCompaction,
                        agent_id: crate::parse_agent_id(&agent_id),
                        transaction_id,
                        cut,
                        resume_through,
                        model,
                        originator,
                        supersedes,
                        trigger: tau_proto::StandaloneCompactionTrigger::Manual,
                    },
                ),
            );
            return;
        }
        self.publish_for_agent(
            cid,
            Event::AgentCompactionTriggered(tau_proto::AgentCompactionTriggered {
                agent_id: crate::parse_agent_id(&agent_id),
                originator: conv.identity.originator.clone(),
                resume_inference: false,
            }),
        );
        self.dispatch_prompt_after_publish_idle(cid);
    }

    /// Validate and durably accept a model-authorized standalone compaction.
    ///
    /// `None` targets the caller; `Some` must name another loaded agent.
    /// Durable acceptance precedes the background placeholder. Self
    /// requests defer until their complete tool round folds, while
    /// cross-agent requests start immediately; every rejection completes as
    /// a foreground error.
    ///
    /// See `SPEC-compaction-and-context-recovery` for capability and replay
    /// ownership.
    pub(crate) fn request_agent_tool_compaction(
        &mut self,
        caller_cid: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
        target_agent_id: Option<&tau_proto::AgentId>,
    ) {
        let Some(caller_public_id) = self.ensure_agent_id_for_agent(caller_cid) else {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "caller unavailable".into(),
                None,
            );
            return;
        };
        if target_agent_id.is_some_and(|target| target.as_str() == caller_public_id) {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "target_must_be_other_agent".into(),
                None,
            );
            return;
        }
        let target_cid = match target_agent_id {
            Some(target) => self.runtime_agent_id_for_target_agent(Some(target.as_str())),
            None => Some(caller_cid.clone()),
        };
        let Some(target_cid) = target_cid else {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "target_unavailable_or_unauthorized".into(),
                None,
            );
            return;
        };
        let Some(target) = self.agent_runtime.agent_registry.agents.get(&target_cid) else {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "target_unavailable_or_unauthorized".into(),
                None,
            );
            return;
        };
        let self_request = target_cid == *caller_cid;
        let target_public_id = target.identity.agent_id.clone();
        let committed_target_pending = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .values()
            .any(|entry| Some(entry.request.target_agent_id.to_string()) == target_public_id);
        let staged_target_pending = self
            .prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .values()
            .any(|entry| Some(entry.request().target_agent_id.to_string()) == target_public_id);
        if committed_target_pending || staged_target_pending {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "already_pending".into(),
                None,
            );
            return;
        }
        let target_head = target
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let dispatch_uncertain = matches!(
            target.dispatch.activation_dispatch,
            crate::agent::ActivationDispatchState::DispatchUncertain { .. }
        );
        let already_pending = matches!(
            target.dispatch.activation_dispatch,
            ActivationDispatchState::Running { .. }
                | crate::agent::ActivationDispatchState::ContextRecoveryPending { .. }
                | crate::agent::ActivationDispatchState::ContextRecoveryClaimPending { .. }
        );
        let valid_state = !target.dispatch.terminating
            && if self_request {
                matches!(target.turn.turn_state, AgentTurnState::ToolsRunning { .. })
            } else {
                matches!(target.turn.turn_state, AgentTurnState::Idle)
                    && matches!(
                        target.dispatch.activation_dispatch,
                        crate::agent::ActivationDispatchState::None
                    )
            };
        if dispatch_uncertain || already_pending || !valid_state {
            let message = if dispatch_uncertain {
                "dispatch_uncertain"
            } else if already_pending {
                "already_pending"
            } else {
                "target_busy"
            };
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                message.into(),
                None,
            );
            return;
        }
        let Some(model) =
            self.model_for_agent_role(target).filter(|model| {
                self.provider_runtime.model_info.get(model).is_some_and(
                    tau_proto::ProviderModelInfo::supports_explicit_standalone_compaction,
                ) && self.provider_runtime.model_routes.contains_key(model)
            })
        else {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "standalone_compaction_unsupported".into(),
                None,
            );
            return;
        };
        let Some(target_public_id) = target_public_id else {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "not_needed".into(),
                None,
            );
            return;
        };
        let caller_active_requests = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .values()
            .filter(|entry| {
                entry
                    .request
                    .tool_source()
                    .is_some_and(|source| source.caller_agent_id.as_str() == caller_public_id)
            })
            .count()
            + self
                .prompt_coordination
                .compaction_runtime
                .pending_manual_acceptances
                .values()
                .filter(|entry| {
                    matches!(
                        entry,
                        PendingManualCompactionAcceptance::ModelTool(staged)
                            if staged.request.tool_source().is_some_and(|source| {
                                source.caller_agent_id.as_str() == caller_public_id
                            })
                    )
                })
                .count()
            + self
                .prompt_coordination
                .compaction_runtime
                .model_tool_start_count_for_caller(&caller_public_id);
        if 4 <= caller_active_requests {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "caller_compaction_limit".into(),
                None,
            );
            return;
        }
        let Some(initiating_agent_prompt_id) = self
            .prompt_coordination
            .prompt_runtime
            .tool_call_prompts
            .get(&call.id)
            .cloned()
        else {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "missing_prompt_authority".into(),
                None,
            );
            return;
        };
        let target_generation = self
            .session_runtime
            .agent_store
            .agent(target_public_id.as_str())
            .map_or(
                tau_proto::MaterializedPromptGeneration::initial(),
                tau_core::AgentTree::ordinary_inference_generation,
            );
        let repeated_generation = self
            .session_runtime
            .agent_store
            .agent(target_public_id.as_str())
            .and_then(|tree| tree.manual_compaction_recoveries().into_iter().last())
            .is_some_and(|recovery| {
                let previous = match recovery {
                    tau_core::ManualCompactionRecovery::Waiting(request)
                    | tau_core::ManualCompactionRecovery::Started {
                        requested: request, ..
                    }
                    | tau_core::ManualCompactionRecovery::Failed {
                        requested: request, ..
                    } => request,
                };
                target_generation <= previous.target_generation
            });
        let may_bypass_repeat_guard = repeated_generation
            && self
                .matching_durable_failed_recovery(target_public_id.as_str(), &model, target_head)
                .is_some();
        if repeated_generation && !may_bypass_repeat_guard {
            self.finish_harness_owned_tool_with_error(
                caller_cid,
                call.id.clone(),
                visible_tool_name,
                call.tool_type,
                "not_needed".into(),
                None,
            );
            return;
        }
        let request_ordinal = self
            .session_runtime
            .agent_store
            .agent(target_public_id.as_str())
            .map_or(0, |tree| tree.manual_compaction_recoveries().len());
        let request_id = tau_proto::CompactionRequestId::parse(format!(
            "cr-{}-{request_ordinal}",
            target.dispatch.next_prompt_index
        ))
        .expect("generated request id");
        let request = tau_proto::AgentManualCompactionRequested {
            request_id: request_id.clone(),
            target_agent_id: crate::parse_agent_id(&target_public_id),
            source: tau_proto::ManualCompactionSource::Tool(
                tau_proto::ManualToolCompactionSource {
                    caller_agent_id: crate::parse_agent_id(&caller_public_id),
                    initiating_agent_prompt_id,
                    initiating_tool_call_id: call.id.clone(),
                    initiating_tool_name: if self_request {
                        tau_proto::ManualCompactionTool::Compact
                    } else {
                        tau_proto::ManualCompactionTool::AgentCompact
                    },
                    visible_tool_name: visible_tool_name.clone(),
                    resume_inference: self_request,
                },
            ),
            requested_target_head: target_head,
            target_generation,
            model,
        };
        self.prompt_coordination
            .compaction_runtime
            .pending_manual_acceptances
            .insert(
                ManualCompactionRequestKey::for_request(&request),
                PendingManualCompactionAcceptance::ModelTool(StagedManualCompactionTool {
                    request: request.clone(),
                    visible_tool_name,
                }),
            );
        self.publish_owed_compaction_fact(
            &target_cid,
            target_head,
            Event::AgentManualCompactionRequested(request),
        );
    }

    pub(super) fn start_accepted_manual_compaction(
        &mut self,
        target_cid: &AgentId,
        request_key: &ManualCompactionRequestKey,
    ) -> bool {
        let request_id = request_key.request_id();
        if self
            .prompt_coordination
            .compaction_runtime
            .rejected_ui_starts
            .contains_key(target_cid)
        {
            return true;
        }
        let Some(accepted) = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .get(request_key)
            .cloned()
        else {
            return false;
        };
        let Some(target) = self.agent_runtime.agent_registry.agents.get(target_cid) else {
            self.fail_accepted_manual_compaction(
                target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::TargetUnloaded,
            );
            return false;
        };
        let current_model = self.model_for_agent_role(target);
        if current_model.as_ref() != Some(&accepted.request.model) {
            self.fail_accepted_manual_compaction(
                target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::ModelChanged,
            );
            return false;
        }
        if !self
            .provider_runtime
            .model_info
            .get(&accepted.request.model)
            .is_some_and(tau_proto::ProviderModelInfo::supports_explicit_standalone_compaction)
        {
            self.fail_accepted_manual_compaction(
                target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::Unsupported,
            );
            return false;
        }
        if !self
            .provider_runtime
            .model_routes
            .contains_key(&accepted.request.model)
        {
            self.fail_accepted_manual_compaction(
                target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::RouteFailed,
            );
            return false;
        }
        let current_head = target
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let ui_request = accepted.request.is_ui_request();
        let ui_resume_inference = ui_request
            && (!matches!(target.turn.turn_state, AgentTurnState::Idle)
                || !target.dispatch.pending_prompts.is_empty());
        let blocked_recovery = self
            .matching_durable_failed_recovery(
                accepted.request.target_agent_id.as_str(),
                &accepted.request.model,
                current_head,
            )
            .map(|failed| {
                (
                    failed.transaction_id.clone(),
                    failed.cut,
                    failed.resume_through,
                )
            });
        let normalized_blocked_cut =
            blocked_recovery
                .as_ref()
                .and_then(|(_, failed_cut, resume)| {
                    self.normalized_blocked_recovery_cut(
                        accepted.request.target_agent_id.as_str(),
                        *failed_cut,
                        *resume,
                        current_head,
                    )
                });
        let safe_boundary = self
            .session_runtime
            .agent_store
            .agent(accepted.request.target_agent_id.as_str())
            .is_some_and(|tree| {
                if ui_request {
                    tree.contains_head_ancestry(
                        accepted.request.requested_target_head,
                        current_head,
                    ) && (blocked_recovery.is_some() && normalized_blocked_cut.is_some()
                        || tree.closed_provider_prefix_at_or_before(current_head) == current_head)
                        && tree.ordinary_inference_generation()
                            == accepted.request.target_generation
                } else if accepted.request.required_tool_source().resume_inference {
                    tree.contains_head_ancestry(
                        accepted.request.requested_target_head,
                        current_head,
                    ) && tree.has_complete_tool_round_for(
                        current_head.as_option(),
                        &accepted
                            .request
                            .required_tool_source()
                            .initiating_tool_call_id,
                    )
                } else {
                    blocked_recovery.as_ref().map_or_else(
                        || current_head == accepted.request.requested_target_head,
                        |_| normalized_blocked_cut.is_some(),
                    )
                }
            });
        if !safe_boundary {
            self.fail_accepted_manual_compaction(
                target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::StaleBranch,
            );
            return false;
        }
        let (cut, resume_through, supersedes) = blocked_recovery.map_or_else(
            || {
                (
                    current_head,
                    (accepted
                        .request
                        .tool_source()
                        .is_some_and(|source| source.resume_inference)
                        || ui_resume_inference)
                        .then_some(current_head),
                    None,
                )
            },
            |(failed_id, _, resume)| {
                (
                    normalized_blocked_cut.expect("safe blocked recovery has a normalized cut"),
                    resume.map(|_| current_head),
                    Some(failed_id),
                )
            },
        );
        let target_public_id = accepted.request.target_agent_id.clone();
        let next_prompt_index = target.dispatch.next_prompt_index;
        let originator = target.identity.originator.clone();
        let transaction_id =
            tau_proto::CompactionTransactionId::parse(format!("ct-{next_prompt_index}"))
                .expect("generated transaction id");
        let compact_prompt_id =
            tau_proto::AgentPromptId::parse(format!("ap-{target_public_id}-{next_prompt_index}"))
                .expect("known-safe AgentPromptId must be valid");
        if let Some(target) = self.agent_runtime.agent_registry.agents.get_mut(target_cid) {
            target.dispatch.next_prompt_index = target.dispatch.next_prompt_index.saturating_add(1);
        }
        let trigger = if ui_request {
            tau_proto::StandaloneCompactionTrigger::ManualUi {
                request_id: request_id.clone(),
            }
        } else {
            tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                request_id: request_id.clone(),
                caller_agent_id: accepted
                    .request
                    .required_tool_source()
                    .caller_agent_id
                    .clone(),
                initiating_tool_call_id: accepted
                    .request
                    .required_tool_source()
                    .initiating_tool_call_id
                    .clone(),
            }
        };
        let event =
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                compact_prompt_id,
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                agent_id: target_public_id,
                transaction_id,
                cut,
                resume_through,
                model: accepted.request.model.clone(),
                originator,
                supersedes,
                trigger,
            });
        if ui_request {
            self.publish_for_agent(target_cid, event);
        } else {
            self.publish_owed_compaction_fact(target_cid, current_head, event);
        }
        true
    }

    pub(super) fn fail_accepted_manual_compaction(
        &mut self,
        target_cid: &AgentId,
        request: &tau_proto::AgentManualCompactionRequested,
        reason: tau_proto::ManualCompactionRequestFailureReason,
    ) {
        self.publish_for_agent(
            target_cid,
            Event::AgentManualCompactionRequestFailed(
                tau_proto::AgentManualCompactionRequestFailed {
                    request_id: request.request_id.clone(),
                    target_agent_id: request.target_agent_id.clone(),
                    reason,
                },
            ),
        );
    }

    pub(super) fn compaction_context_for_agent(
        &self,
        cid: &AgentId,
        model: &ModelId,
    ) -> Option<tau_proto::PromptCompactionContext> {
        let supports_compaction = self
            .provider_runtime
            .model_info
            .get(model)
            .is_some_and(|info| info.supports_compaction);
        if !supports_compaction {
            return None;
        }

        let role_name = self.role_name_for_agent_id(cid);
        let role_compaction = self
            .config
            .available_roles
            .get(&role_name)
            .and_then(|role| role.inference_compaction.or(role.compaction))
            .unwrap_or(path_tau_config_settings::RoleCompaction::ProviderDefault);
        match role_compaction {
            path_tau_config_settings::RoleCompaction::ProviderDefault => {
                Some(tau_proto::PromptCompactionContext {
                    compact_threshold: None,
                })
            }
            path_tau_config_settings::RoleCompaction::Threshold(compact_threshold) => {
                Some(tau_proto::PromptCompactionContext {
                    compact_threshold: Some(tau_proto::TokenCount::new(compact_threshold)),
                })
            }
            path_tau_config_settings::RoleCompaction::Disabled => None,
        }
    }

    pub(super) fn agent_model_supports_compaction(&self, cid: &AgentId) -> bool {
        let Some(conv) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return false;
        };
        let continuation_model = match &conv.turn.output_length_continuation {
            path_crate_agent::OutputLengthContinuationState::Planned(continuation) => {
                Some(continuation.dispatch.model.clone())
            }
            path_crate_agent::OutputLengthContinuationState::OwnerReady(continuation)
            | path_crate_agent::OutputLengthContinuationState::OwnerPending(continuation) => {
                Some(continuation.plan.dispatch.model.clone())
            }
            _ => None,
        };
        let Some(model) = continuation_model.or_else(|| self.model_for_agent_role(conv)) else {
            return false;
        };
        self.provider_runtime
            .model_info
            .get(&model)
            .is_some_and(|info| {
                info.supports_compaction || info.supports_explicit_standalone_compaction()
            })
    }

    /// Normalizes one provisional cut against the agent's durable transcript.
    pub(super) fn closed_provider_prefix_for_agent(
        &self,
        agent_id: &str,
        provisional_cut: tau_proto::AgentHead,
    ) -> tau_proto::AgentHead {
        self.session_runtime
            .agent_store
            .agent(agent_id)
            .map_or(provisional_cut, |tree| {
                tree.closed_provider_prefix_at_or_before(provisional_cut)
            })
    }

    /// Select the latest provider-closed prefix that fits the adapter's
    /// conservative standalone request budget.
    ///
    /// The scan starts at the active replacement boundary, so a new compaction
    /// never resurrects history that an earlier boundary removed. Tool-calling
    /// assistant nodes are not candidates until their complete results node has
    /// closed the round.
    pub(super) fn fitting_automatic_compaction_cut(
        &self,
        agent_id: &str,
        active_head: tau_proto::AgentHead,
        maximum_cut: Option<tau_proto::AgentHead>,
        budget: tau_proto::ByteCount,
    ) -> Option<tau_proto::AgentHead> {
        let tree = self.session_runtime.agent_store.agent(agent_id)?;
        let window = tree.active_provider_window(active_head.as_option());
        let measurements =
            crate::prompt::active_prompt_prefix_json_measurements(tree, active_head.as_option())?;
        // A replacement alone is not a progress-making rolling pass: it can
        // produce an equally large replacement forever. Every automatic pass
        // must also consume at least one surviving transcript group.
        let candidates = window
            .transcript
            .iter()
            .zip(measurements)
            .filter_map(|((id, entry), (measured_id, bytes))| {
                if *id != measured_id {
                    return None;
                }
                let open_tool_round = matches!(
                    entry,
                    tau_core::AgentEntry::AssistantResponse { output_items, .. }
                        if output_items
                            .iter()
                            .any(|item| matches!(item, tau_proto::ContextItem::ToolCall(_)))
                );
                (!open_tool_round).then_some((tau_proto::AgentHead::Node(*id), bytes))
            })
            .collect::<Vec<_>>();
        let candidate_limit = match maximum_cut {
            None => candidates.len(),
            Some(tau_proto::AgentHead::Node(maximum)) => candidates
                .iter()
                .position(|(cut, _)| *cut == tau_proto::AgentHead::Node(maximum))
                .map(|index| index + 1)
                .unwrap_or(0),
            Some(tau_proto::AgentHead::Root) => 0,
        };
        candidates
            .into_iter()
            .take(candidate_limit)
            .take_while(|(_, bytes)| *bytes <= budget)
            .map(|(cut, _)| cut)
            .last()
    }

    /// Return the immediate previous useful provider-closed cut.
    pub(super) fn previous_useful_compaction_cut(
        &self,
        agent_id: &str,
        active_head: tau_proto::AgentHead,
        rejected_cut: tau_proto::AgentHead,
    ) -> Option<tau_proto::AgentHead> {
        let tree = self.session_runtime.agent_store.agent(agent_id)?;
        tree.previous_provider_closed_cut_in_active_window(active_head, rejected_cut)
    }

    /// Starts another bounded rolling pass after a durable successful boundary.
    ///
    /// Automatic work requires the active window to reach its local scheduling
    /// threshold. An unfinished chain rooted at a provider context rejection
    /// instead retains its durable recovery authority until it reaches the end
    /// of the logical provider window preceding the rejected activation.
    pub(super) fn start_rolling_compaction_pass(
        &mut self,
        cid: &AgentId,
        model: &ModelId,
        selected: tau_proto::AgentHead,
    ) -> RollingCompactionPass {
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return RollingCompactionPass::NotNeeded;
        };
        let Some(tree) = self.session_runtime.agent_store.agent(&agent_id) else {
            return RollingCompactionPass::NotNeeded;
        };
        let (previous_transaction_id, _automatic) = match tree.standalone_compaction_recovery() {
            Some(tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint {
                transaction_id,
                through,
                automatic,
                ..
            }) if through == selected => (transaction_id, automatic),
            _ => return RollingCompactionPass::NotNeeded,
        };
        let reactive_progress = tree.reactive_compaction_progress(&previous_transaction_id);
        if reactive_progress == Some(tau_core::ReactiveCompactionProgress::ReachedTargetCut) {
            return RollingCompactionPass::NotNeeded;
        }
        let reactive_target_cut = match reactive_progress {
            Some(tau_core::ReactiveCompactionProgress::NeedsContinuation { target_cut }) => {
                Some(target_cut)
            }
            _ => None,
        };
        let reactive_continuation = reactive_target_cut.is_some();
        if !reactive_continuation {
            return RollingCompactionPass::NotNeeded;
        }
        let info = self.provider_runtime.model_info.get(model);
        let budget = info.and_then(|info| info.standalone_compaction_prefix_budget);
        let role_name = self.role_name_for_agent_id(cid);
        let role = self.config.available_roles.get(&role_name);
        let status_available = self
            .gather_effective_tool_specs_for_role_model(&role_name, Some(model))
            .iter()
            .any(|spec| self.tool_model_visible_name(spec).as_str() == "status");
        let logical_status = if status_available {
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .map_or(tau_proto::AgentWorkStatusPhase::Working, |agent| {
                    agent.turn.work_status.phase()
                })
        } else {
            tau_proto::AgentWorkStatusPhase::Working
        };
        let threshold = role
            .and_then(|role| {
                if role.compactions.is_empty() {
                    return match role
                        .compaction
                        .unwrap_or(path_tau_config_settings::RoleCompaction::ProviderDefault)
                    {
                        path_tau_config_settings::RoleCompaction::ProviderDefault => {
                            info.and_then(|info| info.standalone_compaction_threshold)
                        }
                        path_tau_config_settings::RoleCompaction::Threshold(tokens) => {
                            Some(tau_proto::TokenCount::new(tokens))
                        }
                        path_tau_config_settings::RoleCompaction::Disabled => None,
                    };
                }
                role.compactions
                    .values()
                    .filter(|policy| {
                        policy.enable
                            && policy.when.at
                                == path_tau_config_settings::ContextPolicyPoint::BeforeInference
                            && policy
                                .when
                                .statuses
                                .as_ref()
                                .is_none_or(|statuses| statuses.contains(&logical_status))
                    })
                    .filter_map(|policy| {
                        match policy.threshold {
                            path_tau_config_settings::CompactionPolicyThreshold::ProviderDefault => {
                                info.and_then(|info| info.standalone_compaction_threshold)
                            }
                            path_tau_config_settings::CompactionPolicyThreshold::Tokens(tokens) => {
                                Some(tau_proto::TokenCount::new(tokens))
                            }
                        }
                        .filter(|threshold| *threshold > tau_proto::TokenCount::ZERO)
                    })
                    .min()
            })
            .filter(|threshold| *threshold > tau_proto::TokenCount::ZERO);
        let reported_input = self
            .automatic_compaction_reported_input_tokens(cid, model)
            .unwrap_or(tau_proto::TokenCount::ZERO);
        if !reactive_continuation {
            let Some(threshold) = threshold else {
                return RollingCompactionPass::NotNeeded;
            };
            if reported_input < threshold {
                return RollingCompactionPass::NotNeeded;
            }
        }
        let provisional_cut = reactive_target_cut.unwrap_or(selected);
        let fitting = budget.map_or(Some(provisional_cut), |budget| {
            self.fitting_automatic_compaction_cut(&agent_id, selected, reactive_target_cut, budget)
        });
        let failure_reason = tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge;
        let cut = fitting.unwrap_or(provisional_cut);
        let Some((next, originator)) =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .map(|agent| {
                    (
                        agent.dispatch.next_prompt_index,
                        agent.identity.originator.clone(),
                    )
                })
        else {
            return RollingCompactionPass::NotNeeded;
        };
        let transaction_id = tau_proto::CompactionTransactionId::parse(format!("ct-{next}"))
            .expect("generated compaction transaction id is valid");
        let compact_prompt_id = tau_proto::AgentPromptId::parse(format!("ap-{agent_id}-{next}"))
            .expect("known-safe AgentPromptId must be valid");
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
        }
        if fitting.is_none() {
            self.prompt_coordination
                .compaction_runtime
                .suppress_start_for_preflight(
                    crate::parse_agent_id(&agent_id),
                    transaction_id.clone(),
                    failure_reason,
                );
        }
        let trigger = fitting.map_or_else(
            || tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                decision_id: None,
                previous_transaction_id: Some(previous_transaction_id.clone()),
                reason: failure_reason,
            },
            |_| tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                previous_transaction_id: previous_transaction_id.clone(),
            },
        );
        self.publish_event_for_agent_with_completion(
            cid,
            None,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: crate::parse_agent_id(&agent_id),
                transaction_id,
                compact_prompt_id,
                cut,
                resume_through: Some(selected),
                model: model.clone(),
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator,
                supersedes: None,
                trigger,
            }),
            Some(AgentPublishCompletion::RollingCompactionStart {
                owned_publication: None,
            }),
            false,
        );
        if fitting.is_some() {
            RollingCompactionPass::Started
        } else {
            RollingCompactionPass::Terminalizing
        }
    }

    /// Returns the normalized failed cut only when the selected head still
    /// covers both that boundary and its exact owed resume watermark.
    pub(super) fn normalized_blocked_recovery_cut(
        &self,
        agent_id: &str,
        failed_cut: tau_proto::AgentHead,
        resume_through: Option<tau_proto::AgentHead>,
        current_head: tau_proto::AgentHead,
    ) -> Option<tau_proto::AgentHead> {
        let tree = self.session_runtime.agent_store.agent(agent_id)?;
        let normalized = tree.closed_provider_prefix_at_or_before(failed_cut);
        (tree.contains_head_ancestry(normalized, current_head)
            && resume_through.is_none_or(|owed| tree.contains_head_ancestry(owed, current_head)))
        .then_some(normalized)
    }

    /// Returns the latest durable failed transaction only when its start
    /// supplies the same provider-qualified model and its saved boundary still
    /// belongs to the selected branch.
    pub(super) fn matching_durable_failed_recovery(
        &self,
        agent_id: &str,
        model: &tau_proto::ModelId,
        current_head: tau_proto::AgentHead,
    ) -> Option<tau_proto::AgentStandaloneCompactionFailed> {
        let tree = self.session_runtime.agent_store.agent(agent_id)?;
        let failed = tree.unresolved_standalone_compaction_failure(model, current_head)?;
        self.normalized_blocked_recovery_cut(
            agent_id,
            failed.cut,
            failed.resume_through,
            current_head,
        )
        .is_some()
        .then(|| failed.clone())
    }

    /// Returns whether durable recovery authority prevents another automatic
    /// transaction for this model and selected branch.
    pub(super) fn durable_recovery_blocks_automatic(
        &self,
        agent_id: &str,
        model: &tau_proto::ModelId,
        current_head: tau_proto::AgentHead,
    ) -> bool {
        self.matching_durable_failed_recovery(agent_id, model, current_head)
            .is_some()
    }

    /// Inserts one automatic standalone compaction boundary before inference
    /// when the last accepted context usage reaches the role/model threshold.
    #[cfg(test)]
    pub(crate) fn schedule_standalone_auto_compaction(&mut self, cid: &AgentId) -> bool {
        self.schedule_standalone_auto_compaction_with_wake_view(cid, None)
    }

    /// Schedules automatic compaction using an existing selected-wake
    /// projection.
    pub(crate) fn schedule_standalone_auto_compaction_with_wake_view(
        &mut self,
        cid: &AgentId,
        selected_wakes: Option<&super::selected_branch_wake_view::SelectedBranchWakeView>,
    ) -> bool {
        self.schedule_standalone_auto_compaction_for_activation_with_wake_view(
            cid,
            false,
            None,
            selected_wakes,
        )
    }

    pub(super) fn schedule_standalone_auto_compaction_for_activation(
        &mut self,
        cid: &AgentId,
        committed_activation: bool,
        activation_cut: Option<tau_proto::AgentHead>,
    ) -> bool {
        self.schedule_standalone_auto_compaction_for_activation_with_wake_view(
            cid,
            committed_activation,
            activation_cut,
            None,
        )
    }

    /// Schedules activation-aware compaction with one optional wake projection.
    fn schedule_standalone_auto_compaction_for_activation_with_wake_view(
        &mut self,
        cid: &AgentId,
        committed_activation: bool,
        activation_cut: Option<tau_proto::AgentHead>,
        selected_wakes: Option<&super::selected_branch_wake_view::SelectedBranchWakeView>,
    ) -> bool {
        let owed = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .and_then(tau_core::AgentTree::standalone_compaction_recovery);
        if let Some(tau_core::StandaloneCompactionRecovery::AwaitingAutomaticStart {
            decision,
            cut,
            finish_committed: true,
        }) = owed.clone()
        {
            self.start_eager_automatic_compaction(cid, decision, cut);
            return true;
        }
        if matches!(
            owed,
            Some(tau_core::StandaloneCompactionRecovery::AwaitingAutomaticStart { .. })
        ) {
            return true;
        }
        if self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .values()
            .any(|accepted| {
                accepted.request.is_ui_request()
                    && self
                        .runtime_agent_id_for_target_agent(Some(
                            accepted.request.target_agent_id.as_str(),
                        ))
                        .as_ref()
                        == Some(cid)
            })
        {
            return self.try_start_queued_ui_compaction(cid);
        }
        self.schedule_standalone_auto_compaction_at(
            cid,
            committed_activation,
            activation_cut,
            path_tau_config_settings::ContextPolicyPoint::BeforeInference,
            selected_wakes,
        )
    }

    /// Resolve one coalesced eager decision at the final canonical terminal
    /// boundary. The returned identity is persisted on that terminal.
    pub(super) fn eager_automatic_compaction_decision(
        &mut self,
        cid: &AgentId,
        model: ModelId,
        reported_input: Option<tau_proto::TokenCount>,
        provider_prompt_id: Option<tau_proto::AgentPromptId>,
        policies: &BTreeMap<String, tau_config::settings::CompactionPolicy>,
    ) -> Option<tau_proto::AutomaticCompactionDecision> {
        let conv = self.agent_runtime.agent_registry.agents.get(cid)?;
        if self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .values()
            .any(|accepted| {
                accepted.request.is_ui_request()
                    && accepted.request.target_agent_id.as_str()
                        == conv.identity.agent_id.as_deref().unwrap_or_default()
            })
        {
            return None;
        }
        let reported_input = reported_input?;
        let historical_evidence = provider_prompt_id.is_none();
        let provider_prompt_id =
            provider_prompt_id.or_else(|| conv.execution.context_usage_prompt_id.clone())?;
        let agent_id = conv.identity.agent_id.as_deref()?;
        let selected_head = conv
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        if self.durable_recovery_blocks_automatic(agent_id, &model, selected_head) {
            return None;
        }
        let info = self.provider_runtime.model_info.get(&model)?;
        if !info.supports_standalone_compaction {
            return None;
        }
        let logical_status = Self::finalizing_outer_turn_policy_status(
            conv.turn.terminal_status_was_available,
            conv.turn.work_status.phase(),
        );
        let matches = policies
            .iter()
            .filter(|(_, policy)| {
                policy.enable
                    && policy.when.at
                        == path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished
                    && policy
                        .when
                        .statuses
                        .as_ref()
                        .is_none_or(|statuses| statuses.contains(&logical_status))
            })
            .filter_map(|(name, policy)| {
                let threshold = match policy.threshold {
                    path_tau_config_settings::CompactionPolicyThreshold::ProviderDefault => {
                        info.standalone_compaction_threshold
                    }
                    path_tau_config_settings::CompactionPolicyThreshold::Tokens(tokens) => {
                        Some(tau_proto::TokenCount::new(tokens))
                    }
                }
                .filter(|threshold| *threshold > tau_proto::TokenCount::ZERO)?;
                (threshold <= reported_input).then_some((name.as_str(), threshold))
            })
            .collect::<Vec<_>>();
        let threshold = matches.iter().map(|(_, threshold)| *threshold).min()?;
        let matched_policy_names = matches
            .iter()
            .map(|(name, _)| *name)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let matched_names = matched_policy_names.join(",");
        tracing::debug!(
            target: "tau_harness",
            agent = %cid,
            policies = %matched_names,
            threshold = %threshold,
            "coalesced outer-turn-finished automatic compaction policies"
        );
        let evidence = tau_proto::ProactiveCompactionEvidence {
            provider_prompt_id,
            provider_input_tokens: reported_input,
            threshold,
            threshold_source: tau_proto::CompactionThresholdSource::NamedPolicies {
                names: matched_policy_names,
            },
        };
        if historical_evidence
            && !self
                .session_runtime
                .agent_store
                .agent(agent_id)
                .is_some_and(|tree| tree.historical_proactive_evidence_is_valid(&evidence, &model))
        {
            return None;
        }
        let outer_turn_id = conv.turn.outer_turn.owned_id().cloned()?;
        let transaction_id = tau_proto::CompactionTransactionId::parse(format!(
            "ct-{}",
            conv.dispatch.next_prompt_index
        ))
        .expect("generated compaction transaction id is valid");
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
        }
        Some(tau_proto::AutomaticCompactionDecision {
            transaction_id,
            outer_turn_id,
            model,
            threshold,
            evidence: Some(evidence),
        })
    }

    /// Derive the policy-only status at a settled terminal without mutating the
    /// runtime work-status projection.
    pub(super) fn finalizing_outer_turn_policy_status(
        status_was_available: bool,
        phase: tau_proto::AgentWorkStatusPhase,
    ) -> tau_proto::AgentWorkStatusPhase {
        if !status_was_available {
            return tau_proto::AgentWorkStatusPhase::Done;
        }
        if phase == tau_proto::AgentWorkStatusPhase::Working {
            // An accepted settled final invalidates an unresolved Working epoch
            // immediately after this canonical terminal commits.
            tau_proto::AgentWorkStatusPhase::Unknown
        } else {
            phase
        }
    }

    /// Claim one finished terminal-owned eager decision with the existing
    /// protected standalone start protocol.
    pub(super) fn start_eager_automatic_compaction(
        &mut self,
        cid: &AgentId,
        decision: tau_proto::AutomaticCompactionDecision,
        cut: tau_proto::AgentHead,
    ) -> bool {
        let Some(conv) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return false;
        };
        if conv
            .turn
            .automatic_compaction
            .start_is_pending_for(&decision.transaction_id)
        {
            return true;
        }
        let Some(agent_id) = conv.identity.agent_id.clone() else {
            return false;
        };
        let selected = conv
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        if self
            .session_runtime
            .agent_store
            .agent(&agent_id)
            .is_some_and(|tree| !tree.is_ancestor_head(cut, selected))
        {
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent
                    .turn
                    .automatic_compaction
                    .record_start(decision.transaction_id.clone());
            }
            self.publish_for_agent(
                cid,
                Event::AgentStandaloneCompactionFailed(
                    tau_proto::AgentStandaloneCompactionFailed {
                        agent_id: crate::parse_agent_id(&agent_id),
                        transaction_id: decision.transaction_id,
                        cut,
                        reason: tau_proto::StandaloneCompactionFailureReason::StaleBranch,
                        resume_through: None,
                        context_retreat: None,
                        incomplete_response: None,
                    },
                ),
            );
            return true;
        }
        let prefix_budget = self
            .provider_runtime
            .model_info
            .get(&decision.model)
            .and_then(|info| info.standalone_compaction_prefix_budget);
        let fitting_cut = match prefix_budget {
            None => Some(cut),
            Some(budget) => self.fitting_automatic_compaction_cut(&agent_id, cut, None, budget),
        };
        let cut = fitting_cut.unwrap_or(cut);
        let compact_prompt_id = tau_proto::AgentPromptId::parse(format!(
            "ap-{agent_id}-{}",
            conv.dispatch.next_prompt_index
        ))
        .expect("known-safe AgentPromptId must be valid");
        let originator = conv.identity.originator.clone();
        let resume_through = Some(selected);
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
            agent
                .turn
                .automatic_compaction
                .record_start(decision.transaction_id.clone());
        }
        if fitting_cut.is_none() {
            self.prompt_coordination
                .compaction_runtime
                .suppress_start_for_preflight(
                    crate::parse_agent_id(&agent_id),
                    decision.transaction_id.clone(),
                    tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
                );
        }
        let trigger = fitting_cut.map_or_else(
            || tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                decision_id: Some(decision.transaction_id.clone()),
                previous_transaction_id: None,
                reason: tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
            },
            |_| tau_proto::StandaloneCompactionTrigger::AutomaticPolicy {
                decision_id: decision.transaction_id.clone(),
            },
        );
        self.publish_for_agent(
            cid,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: crate::parse_agent_id(&agent_id),
                transaction_id: decision.transaction_id.clone(),
                compact_prompt_id,
                cut,
                resume_through,
                model: decision.model,
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator,
                supersedes: None,
                trigger,
            }),
        );
        true
    }

    pub(super) fn schedule_standalone_auto_compaction_at(
        &mut self,
        cid: &AgentId,
        committed_activation: bool,
        activation_cut: Option<tau_proto::AgentHead>,
        point: path_tau_config_settings::ContextPolicyPoint,
        selected_wakes: Option<&super::selected_branch_wake_view::SelectedBranchWakeView>,
    ) -> bool {
        let Some(conv) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return false;
        };
        let Some(model) = self.model_for_agent_role(conv) else {
            return false;
        };
        let Some(info) = self.provider_runtime.model_info.get(&model) else {
            return false;
        };
        if !info.supports_standalone_compaction {
            return false;
        }
        let prefix_budget = info.standalone_compaction_prefix_budget;
        let role_name = self.role_name_for_agent_id(cid);
        let role = self.config.available_roles.get(&role_name);
        let status_available =
            if point == path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished {
                conv.turn.terminal_status_was_available
            } else {
                self.gather_effective_tool_specs_for_role_model(&role_name, Some(&model))
                    .iter()
                    .any(|spec| self.tool_model_visible_name(spec).as_str() == "status")
            };
        let logical_status = if status_available {
            conv.turn.work_status.phase()
        } else if point == path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished {
            tau_proto::AgentWorkStatusPhase::Done
        } else {
            tau_proto::AgentWorkStatusPhase::Working
        };
        let threshold = role
            .and_then(|role| {
                if role.compactions.is_empty() {
                    if point != path_tau_config_settings::ContextPolicyPoint::BeforeInference {
                        return None;
                    }
                    return match role
                        .compaction
                        .unwrap_or(path_tau_config_settings::RoleCompaction::ProviderDefault)
                    {
                        path_tau_config_settings::RoleCompaction::ProviderDefault => {
                            info.standalone_compaction_threshold
                        }
                        path_tau_config_settings::RoleCompaction::Threshold(threshold) => {
                            Some(tau_proto::TokenCount::new(threshold))
                        }
                        path_tau_config_settings::RoleCompaction::Disabled => None,
                    };
                }
                role.compactions
                    .values()
                    .filter(|policy| {
                        policy.enable
                            && policy.when.at == point
                            && policy
                                .when
                                .statuses
                                .as_ref()
                                .is_none_or(|statuses| statuses.contains(&logical_status))
                    })
                    .filter_map(|policy| {
                        match policy.threshold {
                            path_tau_config_settings::CompactionPolicyThreshold::ProviderDefault => {
                                info.standalone_compaction_threshold
                            }
                            path_tau_config_settings::CompactionPolicyThreshold::Tokens(tokens) => {
                                Some(tau_proto::TokenCount::new(tokens))
                            }
                        }
                        .filter(|threshold| *threshold > tau_proto::TokenCount::ZERO)
                    })
                    .min()
            })
            .filter(|threshold| *threshold > tau_proto::TokenCount::ZERO);
        let Some(agent_id) = conv.identity.agent_id.clone() else {
            return false;
        };
        let selected_head = conv
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        if !matches!(
            conv.dispatch.activation_dispatch,
            crate::agent::ActivationDispatchState::None
        ) || self.durable_recovery_blocks_automatic(agent_id.as_str(), &model, selected_head)
        {
            return false;
        }
        let reported_input = self
            .automatic_compaction_reported_input_tokens(cid, &model)
            .unwrap_or(tau_proto::TokenCount::ZERO);
        if threshold.is_none_or(|threshold| reported_input < threshold) {
            return false;
        }
        let threshold = threshold.expect("eligible threshold is present");
        let Some(provider_prompt_id) = conv.execution.context_usage_prompt_id.clone() else {
            return false;
        };
        let threshold_source = if role.is_some_and(|role| !role.compactions.is_empty()) {
            let names = role
                .into_iter()
                .flat_map(|role| role.compactions.iter())
                .filter_map(|(name, policy)| {
                    if !policy.enable
                        || policy.when.at != point
                        || policy
                            .when
                            .statuses
                            .as_ref()
                            .is_some_and(|statuses| !statuses.contains(&logical_status))
                    {
                        return None;
                    }
                    let policy_threshold = match policy.threshold {
                        path_tau_config_settings::CompactionPolicyThreshold::ProviderDefault => {
                            info.standalone_compaction_threshold
                        }
                        path_tau_config_settings::CompactionPolicyThreshold::Tokens(tokens) => {
                            Some(tau_proto::TokenCount::new(tokens))
                        }
                    }
                    .filter(|threshold| *threshold > tau_proto::TokenCount::ZERO)?;
                    (policy_threshold <= reported_input).then(|| name.clone())
                })
                .collect();
            tau_proto::CompactionThresholdSource::NamedPolicies { names }
        } else if matches!(
            role.and_then(|role| role.compaction),
            Some(path_tau_config_settings::RoleCompaction::Threshold(_))
        ) {
            tau_proto::CompactionThresholdSource::RoleThreshold
        } else {
            tau_proto::CompactionThresholdSource::ProviderDefault
        };
        let selected_head = conv
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let resume_through = (committed_activation
            || !conv.dispatch.pending_message_wakes.is_empty())
        .then_some(selected_head);
        let owned_selected_wakes;
        let selected_wakes = if let Some(view) = selected_wakes {
            Some(view)
        } else {
            owned_selected_wakes = self.selected_branch_wake_view(cid);
            owned_selected_wakes.as_ref()
        };
        let selected_message_cut =
            selected_wakes.and_then(|view| view.earliest_activation_cut(None));
        let activation_cut = if activation_cut.is_some() || selected_message_cut.is_some() {
            let Some(cut) = selected_wakes.map_or_else(
                || self.earliest_activation_cut(cid, activation_cut),
                |view| view.earliest_activation_cut(activation_cut),
            ) else {
                return false;
            };
            Some(cut)
        } else {
            None
        };
        let provisional_cut = activation_cut.unwrap_or_else(|| {
            if resume_through.is_some() {
                self.session_runtime
                    .agent_store
                    .agent(&agent_id)
                    .and_then(|tree| conv.identity.head.and_then(|head| tree.node(head)))
                    .and_then(|node| node.parent_id)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
            } else {
                conv.identity
                    .head
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
            }
        });
        let fitting_cut = prefix_budget.map_or(Some(provisional_cut), |budget| {
            self.fitting_automatic_compaction_cut(&agent_id, provisional_cut, None, budget)
        });
        if fitting_cut.is_none()
            && self
                .session_runtime
                .agent_store
                .agent(&agent_id)
                .is_some_and(|tree| {
                    tree.active_provider_window_replacement(provisional_cut.as_option())
                        .is_some()
                        && tree.active_provider_window_transcript_count(provisional_cut.as_option())
                            == 0
                })
        {
            // A replacement-only window has no progress-making automatic cut.
            // Let the already-durable activation claim ordinary inference rather
            // than terminally blocking it with an impossible compaction pass.
            return false;
        }
        let cut = fitting_cut
            .unwrap_or_else(|| self.closed_provider_prefix_for_agent(&agent_id, provisional_cut));
        let resume_through = resume_through.or(Some(selected_head));
        let originator = conv.identity.originator.clone();
        let transaction_id = tau_proto::CompactionTransactionId::parse(format!(
            "ct-{}",
            conv.dispatch.next_prompt_index
        ))
        .expect("generated compaction transaction id is valid");
        let compact_prompt_id = tau_proto::AgentPromptId::parse(format!(
            "ap-{agent_id}-{}",
            conv.dispatch.next_prompt_index
        ))
        .expect("known-safe AgentPromptId must be valid");
        if fitting_cut.is_none() {
            self.prompt_coordination
                .compaction_runtime
                .suppress_start_for_preflight(
                    crate::parse_agent_id(&agent_id),
                    transaction_id.clone(),
                    tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
                );
        }
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
        }
        let trigger = fitting_cut.map_or_else(
            || tau_proto::StandaloneCompactionTrigger::AutomaticPreflightFailure {
                decision_id: None,
                previous_transaction_id: None,
                reason: tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
            },
            |_| tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence {
                evidence: tau_proto::ProactiveCompactionEvidence {
                    provider_prompt_id,
                    provider_input_tokens: reported_input,
                    threshold,
                    threshold_source,
                },
            },
        );
        self.publish_for_agent(
            cid,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                compact_prompt_id,
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                agent_id: crate::parse_agent_id(&agent_id),
                transaction_id,
                cut,
                resume_through,
                model,
                originator,
                supersedes: None,
                trigger,
            }),
        );
        true
    }

    /// Return the applicable nonzero provider-reported ordinary input count.
    ///
    /// Unknown suffix growth is not estimated. Missing, zero, model-mismatched,
    /// off-branch, and post-compaction observations provide no proactive
    /// scheduling authority.
    pub(super) fn automatic_compaction_reported_input_tokens(
        &self,
        cid: &AgentId,
        model: &tau_proto::ModelId,
    ) -> Option<tau_proto::TokenCount> {
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        (agent.execution.context_usage_model.as_ref() == Some(model)
            && self.context_usage_baseline_applies(agent))
        .then_some(agent.execution.context_input_tokens)
        .flatten()
        .filter(|tokens| *tokens > 0)
        .map(tau_proto::TokenCount::new)
    }

    /// Reapply durable self-compaction consumption after generic restored tool
    /// state has seeded the run-local wait tracker.
    pub(super) fn consume_restored_self_compaction_deliveries(&mut self) {
        let delivered = self
            .agent_runtime
            .agent_registry
            .agents
            .values()
            .filter_map(|agent| agent.identity.agent_id.as_deref())
            .filter_map(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .flat_map(tau_core::AgentTree::manual_compaction_recoveries)
            .filter_map(|recovery| match recovery {
                tau_core::ManualCompactionRecovery::Waiting(_) => None,
                tau_core::ManualCompactionRecovery::Started { requested, .. }
                | tau_core::ManualCompactionRecovery::Failed { requested, .. } => requested
                    .tool_source()
                    .and_then(|source| {
                        self.session_runtime
                            .agent_store
                            .agent(source.caller_agent_id.as_str())
                            .map(|tree| (source, tree))
                    })
                    .and_then(|(source, tree)| {
                        tree.self_compaction_delivery(&requested.request_id)
                            .map(|_| source.initiating_tool_call_id.clone())
                    }),
            })
            .collect::<Vec<_>>();
        for call_id in delivered {
            self.consume_wait_background_completion(&call_id);
        }
    }

    // -----------------------------------------------------------------------
    // Agent prompt assembly
    // -----------------------------------------------------------------------

    /// Projects one core-validated successful compaction recovery into the
    /// runtime checkpoint state used by provider-discovery reconciliation,
    /// following `SPEC-compaction-and-context-recovery`.
    pub(super) fn stage_restored_compaction_recovery(
        &mut self,
        cid: &AgentId,
        recovery: &tau_core::StandaloneCompactionRecovery,
    ) -> Option<AgentPromptId> {
        let tau_core::StandaloneCompactionRecovery::AwaitingCheckpoint {
            transaction_id,
            cut,
            model,
            through,
            ..
        } = recovery
        else {
            return None;
        };
        let conv = self.agent_runtime.agent_registry.agents.get_mut(cid)?;
        let agent_id = conv.identity.agent_id.as_deref()?;
        let prompt_id = tau_proto::AgentPromptId::parse(format!(
            "ap-{agent_id}-{}",
            conv.dispatch.next_prompt_index
        ))
        .expect("known-safe AgentPromptId must be valid");
        conv.dispatch.next_prompt_index = conv.dispatch.next_prompt_index.saturating_add(1);
        conv.dispatch.activation_dispatch =
            path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                owner: path_crate_agent::InferenceCheckpointOwner::Standalone {
                    id: transaction_id.clone(),
                },
                agent_prompt_id: prompt_id.clone(),
                through: *through,
                dispatch: crate::agent::InferenceDispatchOwnership {
                    model: model.clone(),
                    operation: tau_proto::PromptOperation::Inference,
                    activation_cut: *cut,
                },
            };
        Some(prompt_id)
    }

    pub(super) fn restore_manual_compaction_tools(
        &mut self,
        recoveries: Vec<(AgentId, tau_core::ManualCompactionRecovery)>,
    ) {
        // AgentTree recovery is authoritative. These runtime maps are rebuilt
        // only to repair accepted-before-placeholder, complete-round-before-start,
        // transaction-terminal-before-background-terminal, and
        // background-terminal-before-checkpoint crash windows. An outcome-less
        // started transaction is never resent: generic standalone recovery first
        // terminalizes it as interrupted.
        let mut waiting = Vec::new();
        for (target_cid, recovery) in recoveries {
            let (request, started) = match recovery {
                tau_core::ManualCompactionRecovery::Waiting(request) => {
                    let tool_name = request.tool_source().map_or_else(
                        || ToolName::new("compact"),
                        |source| source.visible_tool_name.clone(),
                    );
                    self.prompt_coordination
                        .compaction_runtime
                        .accepted_manual_tools
                        .insert(
                            ManualCompactionRequestKey::for_request(&request),
                            AcceptedManualCompactionTool {
                                request: request.clone(),
                                visible_tool_name: tool_name,
                            },
                        );
                    if request.is_ui_request() {
                        waiting.push((
                            target_cid,
                            ManualCompactionRequestKey::for_request(&request),
                        ));
                        continue;
                    }
                    if let Some(caller_cid) = self.runtime_agent_id_for_target_agent(Some(
                        request.required_tool_source().caller_agent_id.as_str(),
                    )) && !self
                        .restored_background_tool_states_for_agent(&caller_cid)
                        .into_iter()
                        .any(|state| {
                            state.placeholder.call_id
                                == request.required_tool_source().initiating_tool_call_id
                        })
                    {
                        self.restore_manual_tool_runtime(&caller_cid, &request);
                        self.publish_internal_background_placeholder(
                            &request.required_tool_source().initiating_tool_call_id,
                            tau_proto::CborValue::Map(vec![
                                (
                                    tau_proto::CborValue::Text("status".into()),
                                    tau_proto::CborValue::Text("accepted".into()),
                                ),
                                (
                                    tau_proto::CborValue::Text("request_id".into()),
                                    tau_proto::CborValue::Text(request.request_id.to_string()),
                                ),
                                (
                                    tau_proto::CborValue::Text("target_agent_id".into()),
                                    tau_proto::CborValue::Text(request.target_agent_id.to_string()),
                                ),
                                (
                                    tau_proto::CborValue::Text("deferred".into()),
                                    tau_proto::CborValue::Bool(
                                        request.required_tool_source().resume_inference,
                                    ),
                                ),
                            ]),
                        );
                    }
                    waiting.push((
                        target_cid,
                        ManualCompactionRequestKey::for_request(&request),
                    ));
                    continue;
                }
                tau_core::ManualCompactionRecovery::Started {
                    requested,
                    started,
                    outcome,
                } => {
                    if requested.is_ui_request() {
                        if outcome.is_none() {
                            self.prompt_coordination.compaction_runtime.record_ui_start(
                                requested.target_agent_id.clone(),
                                started.transaction_id.clone(),
                                requested.request_id.clone(),
                            );
                        }
                        continue;
                    }
                    let pending = PendingManualCompactionTool {
                        request_id: requested.request_id.clone(),
                        caller_agent_id: requested.required_tool_source().caller_agent_id.clone(),
                        call_id: requested
                            .required_tool_source()
                            .initiating_tool_call_id
                            .clone(),
                        tool_name: requested.required_tool_source().visible_tool_name.clone(),
                        target_agent_id: requested.target_agent_id.clone(),
                    };
                    self.prompt_coordination
                        .compaction_runtime
                        .record_model_tool_start(
                            requested.target_agent_id.clone(),
                            started.transaction_id.clone(),
                            pending,
                        );
                    (requested, Some((started, outcome)))
                }
                tau_core::ManualCompactionRecovery::Failed { requested, failed } => {
                    if requested.is_ui_request() {
                        continue;
                    }
                    let Some(caller_cid) = self.runtime_agent_id_for_target_agent(Some(
                        requested.required_tool_source().caller_agent_id.as_str(),
                    )) else {
                        continue;
                    };
                    let completed = self.manual_tool_background_completion_exists(
                        &caller_cid,
                        &requested.required_tool_source().initiating_tool_call_id,
                    );
                    if !completed {
                        self.restore_manual_tool_runtime(&caller_cid, &requested);
                        self.finish_prebuilt_internal_tool_error_with_mode(
                            ToolError {
                                presentation: Default::default(),
                                call_id: requested
                                    .required_tool_source()
                                    .initiating_tool_call_id
                                    .clone(),
                                tool_name: requested
                                    .required_tool_source()
                                    .visible_tool_name
                                    .clone(),
                                tool_type: tau_proto::ToolType::Function,
                                message: manual_request_failure_message(failed.reason).to_owned(),
                                details: None,
                                display: None,
                                originator: PromptOriginator::User,
                            },
                            if requested.required_tool_source().resume_inference {
                                BackgroundCompletionPromptMode::DoNotQueue
                            } else {
                                BackgroundCompletionPromptMode::QueueAndAdvance
                            },
                        );
                    }
                    if requested.required_tool_source().resume_inference
                        && !self.self_compaction_terminal_delivered(&requested)
                    {
                        self.consume_wait_background_completion(
                            &requested.required_tool_source().initiating_tool_call_id,
                        );
                        if let Some(agent) = self
                            .agent_runtime
                            .agent_registry
                            .agents
                            .get_mut(&caller_cid)
                        {
                            agent
                                .dispatch
                                .pending_prompts
                                .push_back(self_compaction_terminal_pending_prompt(
                                tau_proto::SelfCompactionTerminal {
                                    request_id: requested.request_id.clone(),
                                    tool_call_id: requested
                                        .required_tool_source()
                                        .initiating_tool_call_id
                                        .clone(),
                                    transaction_id: None,
                                    outcome:
                                        tau_proto::SelfCompactionTerminalOutcome::RequestFailed {
                                            reason: failed.reason,
                                        },
                                },
                            ));
                        }
                        self.fold_pending_prompts_as_steered(&caller_cid);
                    }
                    continue;
                }
            };
            let Some((started, outcome)) = started else {
                continue;
            };
            let Some(caller_cid) = self.runtime_agent_id_for_target_agent(Some(
                request.required_tool_source().caller_agent_id.as_str(),
            )) else {
                continue;
            };
            if self.manual_tool_background_completion_exists(
                &caller_cid,
                &request.required_tool_source().initiating_tool_call_id,
            ) {
                self.prompt_coordination
                    .compaction_runtime
                    .take_model_tool_start(
                        request.target_agent_id.clone(),
                        started.transaction_id.clone(),
                    );
                if request.required_tool_source().resume_inference
                    && !self.self_compaction_terminal_delivered(&request)
                {
                    self.consume_wait_background_completion(
                        &request.required_tool_source().initiating_tool_call_id,
                    );
                    let terminal = match outcome.as_deref() {
                        Some(tau_core::ManualCompactionOutcome::Succeeded(_)) => {
                            tau_proto::SelfCompactionTerminal {
                                request_id: request.request_id.clone(),
                                tool_call_id: request
                                    .required_tool_source()
                                    .initiating_tool_call_id
                                    .clone(),
                                transaction_id: Some(started.transaction_id.clone()),
                                outcome: tau_proto::SelfCompactionTerminalOutcome::Compacted,
                            }
                        }
                        Some(tau_core::ManualCompactionOutcome::Failed(failed)) => {
                            tau_proto::SelfCompactionTerminal {
                                request_id: request.request_id.clone(),
                                tool_call_id: request
                                    .required_tool_source()
                                    .initiating_tool_call_id
                                    .clone(),
                                transaction_id: Some(started.transaction_id.clone()),
                                outcome: tau_proto::SelfCompactionTerminalOutcome::Failed {
                                    reason: failed.reason,
                                },
                            }
                        }
                        None => continue,
                    };
                    if let Some(agent) = self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get_mut(&caller_cid)
                    {
                        agent
                            .dispatch
                            .pending_prompts
                            .push_back(self_compaction_terminal_pending_prompt(terminal));
                    }
                    self.fold_pending_prompts_as_steered(&caller_cid);
                    if matches!(
                        outcome.as_deref(),
                        Some(tau_core::ManualCompactionOutcome::Succeeded(_))
                    ) {
                        self.stage_restored_manual_checkpoint(&target_cid, &started);
                    }
                }
                continue;
            }
            self.restore_manual_tool_runtime(&caller_cid, &request);
            match outcome.map(|outcome| *outcome) {
                Some(tau_core::ManualCompactionOutcome::Succeeded(_)) => {
                    self.finish_prebuilt_internal_tool_result_with_mode(
                        ToolResult {
                            presentation: Default::default(),
                            call_id: request
                                .required_tool_source()
                                .initiating_tool_call_id
                                .clone(),
                            tool_name: request.required_tool_source().visible_tool_name.clone(),
                            tool_type: tau_proto::ToolType::Function,
                            result: tau_proto::CborValue::Map(vec![
                                (
                                    tau_proto::CborValue::Text("status".into()),
                                    tau_proto::CborValue::Text("compacted".into()),
                                ),
                                (
                                    tau_proto::CborValue::Text("request_id".into()),
                                    tau_proto::CborValue::Text(request.request_id.to_string()),
                                ),
                                (
                                    tau_proto::CborValue::Text("target_agent_id".into()),
                                    tau_proto::CborValue::Text(request.target_agent_id.to_string()),
                                ),
                                (
                                    tau_proto::CborValue::Text("transaction_id".into()),
                                    tau_proto::CborValue::Text(started.transaction_id.to_string()),
                                ),
                            ]),
                            provider_content: Vec::new(),
                            kind: ToolResultKind::Final,
                            display: None,
                            originator: PromptOriginator::User,
                        },
                        if request.required_tool_source().resume_inference {
                            BackgroundCompletionPromptMode::DoNotQueue
                        } else {
                            BackgroundCompletionPromptMode::QueueOnly
                        },
                    );
                    self.prompt_coordination
                        .compaction_runtime
                        .take_model_tool_start(
                            request.target_agent_id.clone(),
                            started.transaction_id.clone(),
                        );
                    if request.required_tool_source().resume_inference {
                        self.consume_wait_background_completion(
                            &request.required_tool_source().initiating_tool_call_id,
                        );
                        if let Some(agent) = self
                            .agent_runtime
                            .agent_registry
                            .agents
                            .get_mut(&target_cid)
                        {
                            agent.dispatch.pending_prompts.push_back(
                                self_compaction_terminal_pending_prompt(
                                    tau_proto::SelfCompactionTerminal {
                                        request_id: request.request_id.clone(),
                                        tool_call_id: request
                                            .required_tool_source()
                                            .initiating_tool_call_id
                                            .clone(),
                                        transaction_id: Some(started.transaction_id.clone()),
                                        outcome:
                                            tau_proto::SelfCompactionTerminalOutcome::Compacted,
                                    },
                                ),
                            );
                        }
                        self.fold_pending_prompts_as_steered(&target_cid);
                        self.stage_restored_manual_checkpoint(&target_cid, &started);
                    }
                }
                Some(tau_core::ManualCompactionOutcome::Failed(failed)) => {
                    let call_id = request
                        .required_tool_source()
                        .initiating_tool_call_id
                        .clone();
                    self.finish_prebuilt_internal_tool_error_with_mode(
                        ToolError {
                            presentation: Default::default(),
                            call_id: call_id.clone(),
                            tool_name: request.required_tool_source().visible_tool_name.clone(),
                            tool_type: tau_proto::ToolType::Function,
                            message: standalone_compaction_failure_message(failed.reason)
                                .to_owned(),
                            details: None,
                            display: None,
                            originator: PromptOriginator::User,
                        },
                        if request.required_tool_source().resume_inference {
                            BackgroundCompletionPromptMode::DoNotQueue
                        } else {
                            BackgroundCompletionPromptMode::QueueAndAdvance
                        },
                    );
                    self.prompt_coordination
                        .compaction_runtime
                        .take_model_tool_start(
                            request.target_agent_id.clone(),
                            started.transaction_id.clone(),
                        );
                    if request.required_tool_source().resume_inference {
                        self.consume_wait_background_completion(&call_id);
                        if let Some(agent) = self
                            .agent_runtime
                            .agent_registry
                            .agents
                            .get_mut(&caller_cid)
                        {
                            agent.dispatch.pending_prompts.push_back(
                                self_compaction_terminal_pending_prompt(
                                    tau_proto::SelfCompactionTerminal {
                                        request_id: request.request_id.clone(),
                                        tool_call_id: call_id.clone(),
                                        transaction_id: Some(started.transaction_id.clone()),
                                        outcome: tau_proto::SelfCompactionTerminalOutcome::Failed {
                                            reason: failed.reason,
                                        },
                                    },
                                ),
                            );
                        }
                        self.fold_pending_prompts_as_steered(&caller_cid);
                    }
                }
                None => {}
            }
        }
        for (target_cid, request_key) in waiting {
            let self_request = self
                .prompt_coordination
                .compaction_runtime
                .accepted_manual_tools
                .get(&request_key)
                .is_some_and(|accepted| {
                    accepted
                        .request
                        .tool_source()
                        .is_some_and(|source| source.resume_inference)
                });
            if !self_request || self.manual_request_has_complete_tool_round(&request_key) {
                self.start_accepted_manual_compaction(&target_cid, &request_key);
            }
        }
    }

    pub(super) fn restore_manual_tool_runtime(
        &mut self,
        caller_cid: &AgentId,
        request: &tau_proto::AgentManualCompactionRequested,
    ) {
        self.tool_routing.tool_runtime.tool_agents.insert(
            request
                .required_tool_source()
                .initiating_tool_call_id
                .clone(),
            caller_cid.clone(),
        );
        self.tool_routing.tool_runtime.pending_tools.insert(
            request
                .required_tool_source()
                .initiating_tool_call_id
                .clone(),
            PendingTool {
                name: request.required_tool_source().visible_tool_name.clone(),
                internal_name: manual_compaction_tool_name(
                    request.required_tool_source().initiating_tool_name,
                ),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
            },
        );
        self.tool_routing
            .tool_runtime
            .tool_turn
            .restore_backgrounded(
                caller_cid.clone(),
                request
                    .required_tool_source()
                    .initiating_tool_call_id
                    .clone(),
            );
    }

    pub(super) fn manual_tool_background_completion_exists(
        &self,
        caller_cid: &AgentId,
        call_id: &ToolCallId,
    ) -> bool {
        self.restored_background_tool_states_for_agent(caller_cid)
            .into_iter()
            .any(|state| state.placeholder.call_id == *call_id && state.completion.is_some())
    }

    pub(super) fn self_compaction_terminal_delivered(
        &self,
        request: &tau_proto::AgentManualCompactionRequested,
    ) -> bool {
        self.session_runtime
            .agent_store
            .agent(request.required_tool_source().caller_agent_id.as_str())
            .is_some_and(|tree| tree.self_compaction_delivery(&request.request_id).is_some())
    }

    /// Stages an exact manual-compaction continuation for the common
    /// provider-discovery reconciliation path.
    pub(super) fn stage_restored_manual_checkpoint(
        &mut self,
        target_cid: &AgentId,
        started: &tau_proto::AgentStandaloneCompactionStarted,
    ) {
        let Some((agent_prompt_id, through)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(target_cid)
            .and_then(|agent| {
                let agent_id = agent.identity.agent_id.clone()?;
                Some((
                    tau_proto::AgentPromptId::parse(format!(
                        "ap-{agent_id}-{}",
                        agent.dispatch.next_prompt_index
                    ))
                    .expect("known-safe AgentPromptId must be valid"),
                    agent
                        .identity
                        .head
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                ))
            })
        else {
            return;
        };
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(target_cid) {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
            agent.dispatch.activation_dispatch =
                path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
                    owner: path_crate_agent::InferenceCheckpointOwner::Standalone {
                        id: started.transaction_id.clone(),
                    },
                    agent_prompt_id: agent_prompt_id.clone(),
                    through,
                    dispatch: crate::agent::InferenceDispatchOwnership {
                        model: started.model.clone(),
                        operation: tau_proto::PromptOperation::Inference,
                        activation_cut: started.cut,
                    },
                };
        }
    }

    pub(super) fn is_pending_manual_compaction_call(&self, call_id: &ToolCallId) -> bool {
        self.prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .values()
            .any(|accepted| {
                accepted
                    .request
                    .tool_source()
                    .is_some_and(|source| source.initiating_tool_call_id == *call_id)
            })
            || self
                .prompt_coordination
                .compaction_runtime
                .has_model_tool_start_for_call(call_id)
    }

    pub(super) fn manual_request_has_complete_tool_round(
        &self,
        request_key: &ManualCompactionRequestKey,
    ) -> bool {
        let Some(accepted) = self
            .prompt_coordination
            .compaction_runtime
            .accepted_manual_tools
            .get(request_key)
        else {
            return false;
        };
        let Some(caller_cid) = self.runtime_agent_id_for_target_agent(Some(
            accepted
                .request
                .required_tool_source()
                .caller_agent_id
                .as_str(),
        )) else {
            return false;
        };
        self.agent_runtime
            .agent_registry
            .agents
            .get(&caller_cid)
            .and_then(|agent| {
                agent
                    .identity
                    .agent_id
                    .as_deref()
                    .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                    .map(|tree| {
                        tree.has_complete_tool_round_for(
                            agent.identity.head,
                            &accepted
                                .request
                                .required_tool_source()
                                .initiating_tool_call_id,
                        )
                    })
            })
            .unwrap_or(false)
    }

    /// Confirms that a terminal compaction failure belongs to the side request
    /// whose runtime ownership is being restored.
    pub(super) fn compaction_failure_matches_originator(
        events: &[tau_core::PersistedAgentEvent],
        failed: &tau_proto::AgentStandaloneCompactionFailed,
        originator: &tau_proto::PromptOriginator,
    ) -> bool {
        events.iter().rev().any(|record| {
            matches!(
                &record.event,
                Event::AgentStandaloneCompactionStarted(started)
                    if started.transaction_id == failed.transaction_id
                        && &started.originator == originator
            )
        })
    }

    /// Returns the ordinary inference checkpoint that owns one terminal
    /// response.
    pub(super) fn response_inference_checkpoint<'a>(
        events: &'a [tau_core::PersistedAgentEvent],
        prompt_id: &tau_proto::AgentPromptId,
    ) -> Option<&'a tau_proto::AgentInferenceDispatchStarted> {
        events.iter().rev().find_map(|record| {
            let Event::AgentInferenceDispatchStarted(started) = &record.event else {
                return None;
            };
            (&started.agent_prompt_id == prompt_id
                && started.operation == Some(tau_proto::PromptOperation::Inference))
            .then_some(started)
        })
    }
}

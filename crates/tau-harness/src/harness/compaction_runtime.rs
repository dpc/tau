//! Owns live, restored, manual, and automatic compaction runtime authority.
//!
//! The recovery and closed-prefix rules are governed by
//! `SPEC-compaction-and-context-recovery`.

use super::*;

impl Harness {
    pub(super) fn handle_compact_request(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        session_id: SessionId,
        target_agent_id: Option<&str>,
    ) {
        if session_id != self.current_session_id {
            self.send_ui_error_response(
                client_id,
                format!(
                    "cannot compact session `{session_id}` in this harness; active session is `{}`",
                    self.current_session_id
                ),
            );
            return;
        }
        let Some(cid) = self.runtime_agent_id_for_target_agent(target_agent_id) else {
            self.send_ui_error_response(client_id, "unknown agent for compaction");
            return;
        };
        let Some(agent) = self.agents.get(&cid) else {
            self.send_ui_error_response(client_id, "target user agent is missing");
            return;
        };
        if agent.terminating
            || !matches!(
                agent.activation_dispatch,
                crate::agent::ActivationDispatchState::None
                    | crate::agent::ActivationDispatchState::Blocked { .. }
            )
        {
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
        let Some(agent_id) = agent.agent_id.as_deref().map(crate::parse_agent_id) else {
            self.send_ui_error_response(client_id, "nothing to compact yet");
            return;
        };
        if matches!(agent.turn_state, AgentTurnState::Idle) {
            self.start_admitted_manual_compaction(&cid);
            return;
        }
        let AgentTurnState::ToolsRunning { remaining_calls } = &agent.turn_state else {
            self.send_ui_error_response(
                client_id,
                "cannot compact while a prompt or tool turn is in flight",
            );
            return;
        };
        let [wait_call_id] = remaining_calls.as_slice() else {
            self.send_ui_error_response(
                client_id,
                "cannot compact while a prompt or tool turn is in flight",
            );
            return;
        };
        if let Some(pending) = self.pending_ui_compactions_after_wait.get(&cid)
            && pending.wait_call_id == *wait_call_id
            && self.wait_claimed_for_manual_compaction(&cid, wait_call_id)
        {
            self.send_ui_error_response(
                client_id,
                "compaction already pending after wait cancellation",
            );
            return;
        }
        if self
            .pending_terminal_observations
            .contains_key(wait_call_id)
        {
            self.send_ui_error_response(
                client_id,
                "cannot compact while a prompt or tool turn is in flight",
            );
            return;
        }
        let wait_call_id = wait_call_id.clone();
        let Some(tool) = self.pending_tools.get(&wait_call_id).cloned() else {
            self.send_ui_error_response(
                client_id,
                "cannot compact while a prompt or tool turn is in flight",
            );
            return;
        };
        if tool.name.as_str() != path_crate_harness::subagents_tool::WAIT_TOOL_NAME
            || !self.claim_wait_for_manual_compaction(&cid, &wait_call_id)
        {
            self.send_ui_error_response(
                client_id,
                "cannot compact while a prompt or tool turn is in flight",
            );
            return;
        }
        self.pending_ui_compactions_after_wait.insert(
            cid.clone(),
            PendingUiCompactionAfterWait {
                session_generation: self.current_session_generation,
                agent_id,
                wait_call_id: wait_call_id.clone(),
                requester_client_id: client_id.clone(),
            },
        );
        self.observe_tool_terminal(&cid, &wait_call_id, tau_proto::ToolTerminalCause::Unknown);
        self.publish_for_agent(
            &cid,
            Event::ToolCancelled(ToolCancelled {
                presentation: Default::default(),
                call_id: wait_call_id,
                tool_name: tool.name,
                tool_type: tool.tool_type,
                display: None,
            }),
        );
    }

    /// Start the existing manual compaction flow after all admission checks
    /// have established an idle target.
    pub(super) fn start_admitted_manual_compaction(&mut self, cid: &AgentId) {
        let conv = self
            .agents
            .get(cid)
            .expect("admitted manual compaction has a loaded target");
        let agent_id = conv
            .agent_id
            .clone()
            .expect("admitted manual compaction has a durable target");
        let standalone_model = self.model_for_agent_role(conv).filter(|model| {
            self.provider_model_info
                .get(model)
                .is_some_and(|info| info.supports_standalone_compaction)
        });
        if let Some(model) = standalone_model {
            let blocked_recovery = conv
                .activation_dispatch
                .blocked_recovery()
                .map(|(failed_id, cut, resume)| (failed_id.clone(), cut, resume));
            let current_head = conv
                .head
                .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
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
            let transaction_id =
                tau_proto::CompactionTransactionId::parse(format!("ct-{}", conv.next_prompt_index))
                    .expect("generated compaction transaction id is valid");
            let compact_prompt_id = tau_proto::AgentPromptId::parse(format!(
                "ap-{agent_id}-{}",
                conv.next_prompt_index
            ))
            .expect("known-safe AgentPromptId must be valid");
            let originator = conv.originator.clone();
            if let Some(agent) = self.agents.get_mut(cid) {
                agent.next_prompt_index = agent.next_prompt_index.saturating_add(1);
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
                originator: conv.originator.clone(),
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
        let Some(target) = self.agents.get(&target_cid) else {
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
        let target_public_id = target.agent_id.clone();
        if self
            .accepted_manual_compaction_tools
            .values()
            .any(|entry| Some(entry.request.target_agent_id.to_string()) == target_public_id)
        {
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
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let dispatch_uncertain = matches!(
            target.activation_dispatch,
            crate::agent::ActivationDispatchState::DispatchUncertain { .. }
        );
        let already_pending = matches!(
            target.activation_dispatch,
            crate::agent::ActivationDispatchState::Running { .. }
                | crate::agent::ActivationDispatchState::ContextRecoveryPending { .. }
                | crate::agent::ActivationDispatchState::ContextRecoveryClaimPending { .. }
        );
        let valid_state = !target.terminating
            && if self_request {
                matches!(target.turn_state, AgentTurnState::ToolsRunning { .. })
            } else {
                matches!(target.turn_state, AgentTurnState::Idle)
                    && matches!(
                        target.activation_dispatch,
                        crate::agent::ActivationDispatchState::None
                            | crate::agent::ActivationDispatchState::Blocked { .. }
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
        let Some(model) = self.model_for_agent_role(target).filter(|model| {
            self.provider_model_info
                .get(model)
                .is_some_and(|info| info.supports_standalone_compaction)
                && self.provider_model_routes.contains_key(model)
        }) else {
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
            .accepted_manual_compaction_tools
            .values()
            .filter(|entry| entry.request.caller_agent_id.as_str() == caller_public_id)
            .count()
            + self
                .pending_manual_compaction_tools
                .values()
                .filter(|entry| entry.caller_agent_id.as_str() == caller_public_id)
                .count();
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
        let Some(initiating_agent_prompt_id) = self.prompt_tool_call_prompts.get(&call.id).cloned()
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
            .agent_store
            .agent(target_public_id.as_str())
            .map_or(0, tau_core::AgentTree::ordinary_inference_generation);
        let repeated_generation = self
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
            && !self_request
            && self.has_matching_blocked_recovery(
                target_public_id.as_str(),
                &target.activation_dispatch,
                target_head,
            );
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
            .agent_store
            .agent(target_public_id.as_str())
            .map_or(0, |tree| tree.manual_compaction_recoveries().len());
        let request_id = tau_proto::CompactionRequestId::parse(format!(
            "cr-{}-{request_ordinal}",
            target.next_prompt_index
        ))
        .expect("generated request id");
        let request = tau_proto::AgentManualCompactionRequested {
            request_id: request_id.clone(),
            caller_agent_id: crate::parse_agent_id(&caller_public_id),
            target_agent_id: crate::parse_agent_id(&target_public_id),
            initiating_agent_prompt_id,
            initiating_tool_call_id: call.id.clone(),
            initiating_tool_name: if self_request {
                tau_proto::ManualCompactionTool::Compact
            } else {
                tau_proto::ManualCompactionTool::AgentCompact
            },
            visible_tool_name: visible_tool_name.clone(),
            requested_target_head: target_head,
            target_generation,
            model,
            resume_inference: self_request,
        };
        self.publish_for_agent(
            &target_cid,
            Event::AgentManualCompactionRequested(request.clone()),
        );
        self.accepted_manual_compaction_tools.insert(
            request_id.clone(),
            AcceptedManualCompactionTool {
                request: request.clone(),
                visible_tool_name,
            },
        );
        if self.tool_turn.begin_backgrounding(&call.id) {
            self.observe_tool_backgrounded(&call.id);
            self.publish_internal_background_placeholder(
                &call.id,
                tau_proto::CborValue::Map(vec![
                    (
                        tau_proto::CborValue::Text("status".into()),
                        tau_proto::CborValue::Text("accepted".into()),
                    ),
                    (
                        tau_proto::CborValue::Text("target_agent_id".into()),
                        tau_proto::CborValue::Text(target_public_id.clone()),
                    ),
                    (
                        tau_proto::CborValue::Text("request_id".into()),
                        tau_proto::CborValue::Text(request_id.to_string()),
                    ),
                    (
                        tau_proto::CborValue::Text("deferred".into()),
                        tau_proto::CborValue::Bool(self_request),
                    ),
                ]),
            );
        }
        if !self_request {
            self.start_accepted_manual_compaction(&target_cid, &request_id);
        }
    }

    pub(super) fn start_accepted_manual_compaction(
        &mut self,
        target_cid: &AgentId,
        request_id: &tau_proto::CompactionRequestId,
    ) -> bool {
        let Some(accepted) = self
            .accepted_manual_compaction_tools
            .get(request_id)
            .cloned()
        else {
            return false;
        };
        let Some(target) = self.agents.get(target_cid) else {
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
            .provider_model_info
            .get(&accepted.request.model)
            .is_some_and(|info| info.supports_standalone_compaction)
        {
            self.fail_accepted_manual_compaction(
                target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::Unsupported,
            );
            return false;
        }
        if !self
            .provider_model_routes
            .contains_key(&accepted.request.model)
        {
            self.fail_accepted_manual_compaction(
                target_cid,
                &accepted.request,
                tau_proto::ManualCompactionRequestFailureReason::RouteFailed,
            );
            return false;
        }
        let blocked_recovery = target
            .activation_dispatch
            .blocked_recovery()
            .map(|(failed_id, cut, resume)| (failed_id.clone(), cut, resume));
        let current_head = target
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
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
            .agent_store
            .agent(accepted.request.target_agent_id.as_str())
            .is_some_and(|tree| {
                if accepted.request.resume_inference {
                    tree.contains_head_ancestry(
                        accepted.request.requested_target_head,
                        current_head,
                    ) && tree.has_complete_tool_round_for(
                        current_head.as_option(),
                        &accepted.request.initiating_tool_call_id,
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
                    accepted.request.resume_inference.then_some(current_head),
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
        let next_prompt_index = target.next_prompt_index;
        let originator = target.originator.clone();
        let transaction_id =
            tau_proto::CompactionTransactionId::parse(format!("ct-{next_prompt_index}"))
                .expect("generated transaction id");
        let compact_prompt_id =
            tau_proto::AgentPromptId::parse(format!("ap-{target_public_id}-{next_prompt_index}"))
                .expect("known-safe AgentPromptId must be valid");
        if let Some(target) = self.agents.get_mut(target_cid) {
            target.next_prompt_index = target.next_prompt_index.saturating_add(1);
        }
        self.accepted_manual_compaction_tools.remove(request_id);
        self.pending_manual_compaction_tools.insert(
            transaction_id.clone(),
            PendingManualCompactionTool {
                request_id: request_id.clone(),
                caller_agent_id: accepted.request.caller_agent_id.clone(),
                call_id: accepted.request.initiating_tool_call_id.clone(),
                tool_name: accepted.request.visible_tool_name.clone(),
                target_agent_id: accepted.request.target_agent_id.clone(),
            },
        );
        self.publish_for_agent(
            target_cid,
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
                trigger: tau_proto::StandaloneCompactionTrigger::ManualAgentTool {
                    request_id: request_id.clone(),
                    caller_agent_id: accepted.request.caller_agent_id,
                    initiating_tool_call_id: accepted.request.initiating_tool_call_id,
                },
            }),
        );
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
            .provider_model_info
            .get(model)
            .is_some_and(|info| info.supports_compaction);
        if !supports_compaction {
            return None;
        }

        let role_name = self.role_name_for_agent_id(cid);
        let role_compaction = self
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
                    compact_threshold: Some(compact_threshold),
                })
            }
            path_tau_config_settings::RoleCompaction::Disabled => None,
        }
    }

    pub(super) fn agent_model_supports_compaction(&self, cid: &AgentId) -> bool {
        let Some(conv) = self.agents.get(cid) else {
            return false;
        };
        let continuation_model = match &conv.output_length_continuation {
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
        self.provider_model_info
            .get(&model)
            .is_some_and(|info| info.supports_compaction || info.supports_standalone_compaction)
    }

    /// Normalizes one provisional cut against the agent's durable transcript.
    pub(super) fn closed_provider_prefix_for_agent(
        &self,
        agent_id: &str,
        provisional_cut: tau_proto::AgentHead,
    ) -> tau_proto::AgentHead {
        self.agent_store
            .agent(agent_id)
            .map_or(provisional_cut, |tree| {
                tree.closed_provider_prefix_at_or_before(provisional_cut)
            })
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
        let tree = self.agent_store.agent(agent_id)?;
        let normalized = tree.closed_provider_prefix_at_or_before(failed_cut);
        (tree.contains_head_ancestry(normalized, current_head)
            && resume_through.is_none_or(|owed| tree.contains_head_ancestry(owed, current_head)))
        .then_some(normalized)
    }

    /// Returns whether runtime Blocked state matches the latest durable
    /// failure's transaction id, cut, and resume watermark, and the current
    /// head must permit the existing safe cut and owed-branch
    /// normalization.
    pub(super) fn has_matching_blocked_recovery(
        &self,
        agent_id: &str,
        dispatch: &crate::agent::ActivationDispatchState,
        current_head: tau_proto::AgentHead,
    ) -> bool {
        let Some((failed_id, failed_cut, resume_through)) = dispatch.blocked_recovery() else {
            return false;
        };
        let Some(tree) = self.agent_store.agent(agent_id) else {
            return false;
        };
        let Some(tau_core::StandaloneCompactionRecovery::Blocked { failed, .. }) =
            tree.standalone_compaction_recovery()
        else {
            return false;
        };
        failed.transaction_id == *failed_id
            && failed.cut == failed_cut
            && failed.resume_through == resume_through
            && self
                .normalized_blocked_recovery_cut(agent_id, failed_cut, resume_through, current_head)
                .is_some()
    }

    /// Inserts one automatic standalone compaction boundary before inference
    /// when the last accepted context usage reaches the role/model threshold.
    pub(crate) fn schedule_standalone_auto_compaction(&mut self, cid: &AgentId) -> bool {
        self.schedule_standalone_auto_compaction_for_activation(cid, false, None)
    }

    pub(super) fn schedule_standalone_auto_compaction_for_activation(
        &mut self,
        cid: &AgentId,
        committed_activation: bool,
        activation_cut: Option<tau_proto::AgentHead>,
    ) -> bool {
        let owed = self
            .agents
            .get(cid)
            .and_then(|agent| agent.agent_id.as_deref())
            .and_then(|agent_id| self.agent_store.agent(agent_id))
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
        self.schedule_standalone_auto_compaction_at(
            cid,
            committed_activation,
            activation_cut,
            path_tau_config_settings::ContextPolicyPoint::BeforeInference,
        )
    }

    /// Resolve one coalesced eager decision at the final canonical terminal
    /// boundary. The returned identity is persisted on that terminal.
    pub(super) fn eager_automatic_compaction_decision(
        &mut self,
        cid: &AgentId,
        model: ModelId,
        projected_tokens: Option<u64>,
        policies: &BTreeMap<String, tau_config::settings::CompactionPolicy>,
    ) -> Option<tau_proto::AutomaticCompactionDecision> {
        let conv = self.agents.get(cid)?;
        let projected_tokens = projected_tokens?;
        if conv
            .agent_id
            .as_deref()
            .and_then(|agent_id| self.agent_store.agent(agent_id))
            .and_then(tau_core::AgentTree::standalone_compaction_recovery)
            .is_some()
        {
            return None;
        }
        let info = self.provider_model_info.get(&model)?;
        if !info.supports_standalone_compaction {
            return None;
        }
        let logical_status = Self::finalizing_outer_turn_policy_status(
            conv.terminal_status_was_available,
            conv.work_status.phase(),
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
                        Some(tokens)
                    }
                }?;
                (threshold <= projected_tokens).then_some((name.as_str(), threshold))
            })
            .collect::<Vec<_>>();
        let threshold = matches.iter().map(|(_, threshold)| *threshold).min()?;
        let matched_names = matches
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(",");
        tracing::debug!(
            target: "tau_harness",
            agent = %cid,
            policies = %matched_names,
            threshold,
            "coalesced outer-turn-finished automatic compaction policies"
        );
        let outer_turn_id = conv.outer_turn.owned_id().cloned()?;
        let transaction_id =
            tau_proto::CompactionTransactionId::parse(format!("ct-{}", conv.next_prompt_index))
                .expect("generated compaction transaction id is valid");
        if let Some(agent) = self.agents.get_mut(cid) {
            agent.next_prompt_index = agent.next_prompt_index.saturating_add(1);
        }
        Some(tau_proto::AutomaticCompactionDecision {
            transaction_id,
            outer_turn_id,
            model,
            threshold,
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
        let Some(conv) = self.agents.get(cid) else {
            return false;
        };
        if conv.pending_automatic_compaction_start.as_ref() == Some(&decision.transaction_id) {
            return true;
        }
        let Some(agent_id) = conv.agent_id.clone() else {
            return false;
        };
        let selected = conv
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        if self
            .agent_store
            .agent(&agent_id)
            .is_some_and(|tree| !tree.is_ancestor_head(cut, selected))
        {
            if let Some(agent) = self.agents.get_mut(cid) {
                agent.pending_automatic_compaction_start = Some(decision.transaction_id.clone());
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
                    },
                ),
            );
            return true;
        }
        let compact_prompt_id =
            tau_proto::AgentPromptId::parse(format!("ap-{agent_id}-{}", conv.next_prompt_index))
                .expect("known-safe AgentPromptId must be valid");
        let originator = conv.originator.clone();
        let resume_through = (selected != cut).then_some(selected);
        if let Some(agent) = self.agents.get_mut(cid) {
            agent.next_prompt_index = agent.next_prompt_index.saturating_add(1);
            agent.pending_automatic_compaction_start = Some(decision.transaction_id.clone());
        }
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
                trigger: tau_proto::StandaloneCompactionTrigger::AutomaticPolicy {
                    decision_id: decision.transaction_id,
                },
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
    ) -> bool {
        let Some(conv) = self.agents.get(cid) else {
            return false;
        };
        let Some(input_tokens) = conv.context_input_tokens else {
            return false;
        };
        let Some(model) = self.model_for_agent_role(conv) else {
            return false;
        };
        if conv.context_usage_model.as_ref() != Some(&model) {
            return false;
        }
        if !self.context_usage_baseline_applies(conv) {
            return false;
        }
        let Some(info) = self.provider_model_info.get(&model) else {
            return false;
        };
        if !info.supports_standalone_compaction {
            return false;
        }
        let role_name = self.role_name_for_agent_id(cid);
        let role = self.available_roles.get(&role_name);
        let status_available =
            if point == path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished {
                conv.terminal_status_was_available
            } else {
                self.gather_effective_tool_specs_for_role_model(&role_name, Some(&model))
                    .iter()
                    .any(|spec| self.tool_model_visible_name(spec).as_str() == "status")
            };
        let logical_status = if status_available {
            conv.work_status.phase()
        } else if point == path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished {
            tau_proto::AgentWorkStatusPhase::Done
        } else {
            tau_proto::AgentWorkStatusPhase::Working
        };
        let threshold = role.and_then(|role| {
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
                        Some(threshold)
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
                .filter_map(|policy| match policy.threshold {
                    path_tau_config_settings::CompactionPolicyThreshold::ProviderDefault => {
                        info.standalone_compaction_threshold
                    }
                    path_tau_config_settings::CompactionPolicyThreshold::Tokens(tokens) => {
                        Some(tokens)
                    }
                })
                .min()
        });
        if !matches!(
            conv.activation_dispatch,
            crate::agent::ActivationDispatchState::None
        ) {
            return false;
        }
        let Some(agent_id) = conv.agent_id.clone() else {
            return false;
        };
        let delta_tokens = self
            .transcript_growth_since(Some(agent_id.as_str()), conv.head, conv.context_usage_head)
            .projected_tokens;
        let control_reserve = context_projection_reserve(info.context_window);
        let projected_tokens = delta_tokens
            .and_then(|delta| input_tokens.checked_add(delta))
            .and_then(|tokens| tokens.checked_add(control_reserve))
            .unwrap_or(u64::MAX);
        if threshold.is_none_or(|threshold| projected_tokens < threshold) {
            return false;
        }
        let resume_through = (committed_activation || !conv.pending_message_wakes.is_empty())
            .then_some(
                conv.head
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
            );
        let selected_message_cut = self.selected_message_activation_cut(cid);
        let activation_cut = if activation_cut.is_some() || selected_message_cut.is_some() {
            let Some(cut) = self.earliest_activation_cut(cid, activation_cut) else {
                return false;
            };
            Some(cut)
        } else {
            None
        };
        let provisional_cut = activation_cut.unwrap_or_else(|| {
            if resume_through.is_some() {
                self.agent_store
                    .agent(&agent_id)
                    .and_then(|tree| conv.head.and_then(|head| tree.node(head)))
                    .and_then(|node| node.parent_id)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
            } else {
                conv.head
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node)
            }
        });
        let cut = self.closed_provider_prefix_for_agent(&agent_id, provisional_cut);
        let originator = conv.originator.clone();
        let transaction_id =
            tau_proto::CompactionTransactionId::parse(format!("ct-{}", conv.next_prompt_index))
                .expect("generated compaction transaction id is valid");
        let compact_prompt_id =
            tau_proto::AgentPromptId::parse(format!("ap-{agent_id}-{}", conv.next_prompt_index))
                .expect("known-safe AgentPromptId must be valid");
        if let Some(agent) = self.agents.get_mut(cid) {
            agent.next_prompt_index = agent.next_prompt_index.saturating_add(1);
        }
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
                trigger: tau_proto::StandaloneCompactionTrigger::AutomaticThreshold,
            }),
        );
        true
    }

    /// Reapply durable self-compaction consumption after generic restored tool
    /// state has seeded the run-local wait tracker.
    pub(super) fn consume_restored_self_compaction_deliveries(&mut self) {
        let delivered = self
            .agents
            .values()
            .filter_map(|agent| agent.agent_id.as_deref())
            .filter_map(|agent_id| self.agent_store.agent(agent_id))
            .flat_map(tau_core::AgentTree::manual_compaction_recoveries)
            .filter_map(|recovery| match recovery {
                tau_core::ManualCompactionRecovery::Waiting(_) => None,
                tau_core::ManualCompactionRecovery::Started { requested, .. }
                | tau_core::ManualCompactionRecovery::Failed { requested, .. } => self
                    .agent_store
                    .agent(requested.caller_agent_id.as_str())
                    .and_then(|tree| {
                        tree.self_compaction_delivery(&requested.request_id)
                            .map(|_| requested.initiating_tool_call_id)
                    }),
            })
            .collect::<Vec<_>>();
        for call_id in delivered {
            self.consume_wait_background_completion(&call_id);
        }
    }

    /// Release a restored run-local block only when a typed failed terminal has
    /// already committed an inference activation that still lacks a checkpoint.
    pub(super) fn release_restored_self_compaction_failure_continuations(&mut self) {
        let releasable = self
            .agents
            .iter()
            .filter_map(|(cid, agent)| {
                let agent_id = agent.agent_id.as_deref()?;
                let tree = self.agent_store.agent(agent_id)?;
                let typed_failure = tree.manual_compaction_recoveries().into_iter().any(
                    |recovery| match recovery {
                        tau_core::ManualCompactionRecovery::Started {
                            requested,
                            outcome: Some(outcome),
                            ..
                        } => {
                            matches!(
                                outcome.as_ref(),
                                tau_core::ManualCompactionOutcome::Failed(_)
                            ) && tree
                                .self_compaction_delivery_needs_checkpoint(&requested.request_id)
                        }
                        tau_core::ManualCompactionRecovery::Failed { requested, .. } => {
                            tree.self_compaction_delivery_needs_checkpoint(&requested.request_id)
                        }
                        _ => false,
                    },
                );
                (typed_failure
                    && matches!(
                        agent.activation_dispatch,
                        crate::agent::ActivationDispatchState::Blocked { .. }
                    ))
                .then_some(cid.clone())
            })
            .collect::<Vec<_>>();
        for cid in releasable {
            if let Some(agent) = self.agents.get_mut(&cid) {
                agent.activation_dispatch = path_crate_agent::ActivationDispatchState::None;
            }
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
        } = recovery
        else {
            return None;
        };
        let conv = self.agents.get_mut(cid)?;
        let agent_id = conv.agent_id.as_deref()?;
        let prompt_id =
            tau_proto::AgentPromptId::parse(format!("ap-{agent_id}-{}", conv.next_prompt_index))
                .expect("known-safe AgentPromptId must be valid");
        conv.next_prompt_index = conv.next_prompt_index.saturating_add(1);
        conv.activation_dispatch = path_crate_agent::ActivationDispatchState::AwaitingCheckpoint {
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
                    let tool_name = request.visible_tool_name.clone();
                    self.accepted_manual_compaction_tools.insert(
                        request.request_id.clone(),
                        AcceptedManualCompactionTool {
                            request: request.clone(),
                            visible_tool_name: tool_name,
                        },
                    );
                    if let Some(caller_cid) = self
                        .runtime_agent_id_for_target_agent(Some(request.caller_agent_id.as_str()))
                        && !self
                            .restored_background_tool_states_for_agent(&caller_cid)
                            .into_iter()
                            .any(|state| {
                                state.placeholder.call_id == request.initiating_tool_call_id
                            })
                    {
                        self.restore_manual_tool_runtime(&caller_cid, &request);
                        self.publish_internal_background_placeholder(
                            &request.initiating_tool_call_id,
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
                                    tau_proto::CborValue::Bool(request.resume_inference),
                                ),
                            ]),
                        );
                    }
                    waiting.push((target_cid, request.request_id));
                    continue;
                }
                tau_core::ManualCompactionRecovery::Started {
                    requested,
                    started,
                    outcome,
                } => {
                    let pending = PendingManualCompactionTool {
                        request_id: requested.request_id.clone(),
                        caller_agent_id: requested.caller_agent_id.clone(),
                        call_id: requested.initiating_tool_call_id.clone(),
                        tool_name: requested.visible_tool_name.clone(),
                        target_agent_id: requested.target_agent_id.clone(),
                    };
                    self.pending_manual_compaction_tools
                        .insert(started.transaction_id.clone(), pending);
                    (requested, Some((started, outcome)))
                }
                tau_core::ManualCompactionRecovery::Failed { requested, failed } => {
                    let Some(caller_cid) = self.runtime_agent_id_for_target_agent(Some(
                        requested.caller_agent_id.as_str(),
                    )) else {
                        continue;
                    };
                    let completed = self.manual_tool_background_completion_exists(
                        &caller_cid,
                        &requested.initiating_tool_call_id,
                    );
                    if !completed {
                        self.restore_manual_tool_runtime(&caller_cid, &requested);
                        self.finish_prebuilt_internal_tool_error_with_mode(
                            ToolError {
                                presentation: Default::default(),
                                call_id: requested.initiating_tool_call_id.clone(),
                                tool_name: requested.visible_tool_name.clone(),
                                tool_type: tau_proto::ToolType::Function,
                                message: manual_request_failure_message(failed.reason).to_owned(),
                                details: None,
                                display: None,
                                originator: PromptOriginator::User,
                            },
                            if requested.resume_inference {
                                BackgroundCompletionPromptMode::DoNotQueue
                            } else {
                                BackgroundCompletionPromptMode::QueueAndAdvance
                            },
                        );
                    }
                    if requested.resume_inference
                        && !self.self_compaction_terminal_delivered(&requested)
                    {
                        self.consume_wait_background_completion(&requested.initiating_tool_call_id);
                        if let Some(agent) = self.agents.get_mut(&caller_cid) {
                            agent
                                .pending_prompts
                                .push_back(self_compaction_terminal_pending_prompt(
                                tau_proto::SelfCompactionTerminal {
                                    request_id: requested.request_id.clone(),
                                    tool_call_id: requested.initiating_tool_call_id.clone(),
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
            let Some(caller_cid) =
                self.runtime_agent_id_for_target_agent(Some(request.caller_agent_id.as_str()))
            else {
                continue;
            };
            if self.manual_tool_background_completion_exists(
                &caller_cid,
                &request.initiating_tool_call_id,
            ) {
                self.pending_manual_compaction_tools
                    .remove(&started.transaction_id);
                if request.resume_inference && !self.self_compaction_terminal_delivered(&request) {
                    self.consume_wait_background_completion(&request.initiating_tool_call_id);
                    let terminal = match outcome.as_deref() {
                        Some(tau_core::ManualCompactionOutcome::Succeeded(_)) => {
                            tau_proto::SelfCompactionTerminal {
                                request_id: request.request_id.clone(),
                                tool_call_id: request.initiating_tool_call_id.clone(),
                                transaction_id: Some(started.transaction_id.clone()),
                                outcome: tau_proto::SelfCompactionTerminalOutcome::Compacted,
                            }
                        }
                        Some(tau_core::ManualCompactionOutcome::Failed(failed)) => {
                            tau_proto::SelfCompactionTerminal {
                                request_id: request.request_id.clone(),
                                tool_call_id: request.initiating_tool_call_id.clone(),
                                transaction_id: Some(started.transaction_id.clone()),
                                outcome: tau_proto::SelfCompactionTerminalOutcome::Failed {
                                    reason: failed.reason,
                                },
                            }
                        }
                        None => continue,
                    };
                    if let Some(agent) = self.agents.get_mut(&caller_cid) {
                        agent
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
                            call_id: request.initiating_tool_call_id.clone(),
                            tool_name: request.visible_tool_name.clone(),
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
                        if request.resume_inference {
                            BackgroundCompletionPromptMode::DoNotQueue
                        } else {
                            BackgroundCompletionPromptMode::QueueOnly
                        },
                    );
                    self.pending_manual_compaction_tools
                        .remove(&started.transaction_id);
                    if request.resume_inference {
                        self.consume_wait_background_completion(&request.initiating_tool_call_id);
                        if let Some(agent) = self.agents.get_mut(&target_cid) {
                            agent.pending_prompts.push_back(
                                self_compaction_terminal_pending_prompt(
                                    tau_proto::SelfCompactionTerminal {
                                        request_id: request.request_id.clone(),
                                        tool_call_id: request.initiating_tool_call_id.clone(),
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
                    let call_id = request.initiating_tool_call_id.clone();
                    self.finish_prebuilt_internal_tool_error_with_mode(
                        ToolError {
                            presentation: Default::default(),
                            call_id: call_id.clone(),
                            tool_name: request.visible_tool_name.clone(),
                            tool_type: tau_proto::ToolType::Function,
                            message: standalone_compaction_failure_message(failed.reason)
                                .to_owned(),
                            details: None,
                            display: None,
                            originator: PromptOriginator::User,
                        },
                        if request.resume_inference {
                            BackgroundCompletionPromptMode::DoNotQueue
                        } else {
                            BackgroundCompletionPromptMode::QueueAndAdvance
                        },
                    );
                    self.pending_manual_compaction_tools
                        .remove(&started.transaction_id);
                    if request.resume_inference {
                        self.consume_wait_background_completion(&call_id);
                        if let Some(agent) = self.agents.get_mut(&caller_cid) {
                            agent.pending_prompts.push_back(
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
        for (target_cid, request_id) in waiting {
            let self_request = self
                .accepted_manual_compaction_tools
                .get(&request_id)
                .is_some_and(|accepted| accepted.request.resume_inference);
            if !self_request || self.manual_request_has_complete_tool_round(&request_id) {
                self.start_accepted_manual_compaction(&target_cid, &request_id);
            }
        }
    }

    pub(super) fn restore_manual_tool_runtime(
        &mut self,
        caller_cid: &AgentId,
        request: &tau_proto::AgentManualCompactionRequested,
    ) {
        self.tool_agents
            .insert(request.initiating_tool_call_id.clone(), caller_cid.clone());
        self.pending_tools.insert(
            request.initiating_tool_call_id.clone(),
            PendingTool {
                name: request.visible_tool_name.clone(),
                internal_name: manual_compaction_tool_name(request.initiating_tool_name),
                tool_type: tau_proto::ToolType::Function,
                allows_provider_image: false,
            },
        );
        self.tool_turn
            .restore_backgrounded(caller_cid.clone(), request.initiating_tool_call_id.clone());
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
        self.agent_store
            .agent(request.caller_agent_id.as_str())
            .is_some_and(|tree| tree.self_compaction_delivery(&request.request_id).is_some())
    }

    /// Stages an exact manual-compaction continuation for the common
    /// provider-discovery reconciliation path.
    pub(super) fn stage_restored_manual_checkpoint(
        &mut self,
        target_cid: &AgentId,
        started: &tau_proto::AgentStandaloneCompactionStarted,
    ) {
        let Some((agent_prompt_id, through)) = self.agents.get(target_cid).and_then(|agent| {
            let agent_id = agent.agent_id.clone()?;
            Some((
                tau_proto::AgentPromptId::parse(format!(
                    "ap-{agent_id}-{}",
                    agent.next_prompt_index
                ))
                .expect("known-safe AgentPromptId must be valid"),
                agent
                    .head
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
            ))
        }) else {
            return;
        };
        if let Some(agent) = self.agents.get_mut(target_cid) {
            agent.next_prompt_index = agent.next_prompt_index.saturating_add(1);
            agent.activation_dispatch =
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
        self.accepted_manual_compaction_tools
            .values()
            .any(|accepted| accepted.request.initiating_tool_call_id == *call_id)
            || self
                .pending_manual_compaction_tools
                .values()
                .any(|pending| pending.call_id == *call_id)
    }

    pub(super) fn manual_request_has_complete_tool_round(
        &self,
        request_id: &tau_proto::CompactionRequestId,
    ) -> bool {
        let Some(accepted) = self.accepted_manual_compaction_tools.get(request_id) else {
            return false;
        };
        let Some(caller_cid) =
            self.runtime_agent_id_for_target_agent(Some(accepted.request.caller_agent_id.as_str()))
        else {
            return false;
        };
        self.agents
            .get(&caller_cid)
            .and_then(|agent| {
                agent
                    .agent_id
                    .as_deref()
                    .and_then(|agent_id| self.agent_store.agent(agent_id))
                    .map(|tree| {
                        tree.has_complete_tool_round_for(
                            agent.head,
                            &accepted.request.initiating_tool_call_id,
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

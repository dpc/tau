//! Owns configured-extension disconnect, restart, cleanup, and lifecycle
//! readiness.
//!
//! Shared shutdown grace, source validation, discovery cleanup, and
//! exactly-once exit facts remain coupled here.

use super::*;

impl Harness {
    pub(super) fn remove_extension_context_for_connection(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) {
        let disconnected = connection_id.clone();
        self.prompt_coordination
            .context_discovery
            .prompt_fragments
            .remove(&disconnected);
        self.prompt_coordination
            .context_discovery
            .agent_context
            .remove_contributor(&disconnected);
        self.prompt_coordination
            .context_discovery
            .agent_context_providers
            .remove(&disconnected);
        self.prompt_coordination
            .context_discovery
            .session_context_providers
            .remove(&disconnected);
        let mut finalize = Vec::new();
        for (agent_id, pending) in &mut self.prompt_coordination.context_discovery.pending_agents {
            replace_discovery_source(
                &mut pending.skill_candidates,
                &mut pending.skills,
                &mut pending.agents_files,
                connection_id,
                Vec::new(),
                Vec::new(),
            );
            if pending.waiting_on.remove(&disconnected) && pending.waiting_on.is_empty() {
                finalize.push(agent_id.clone());
            }
        }
        for agent_id in finalize {
            if let Err(error) = self.finalize_agent_discovery(&agent_id) {
                self.emit_harness_failure(&format!(
                    "failed to finalize agent discovery after disconnect: {error}"
                ));
            }
        }
    }

    pub(super) fn clear_session_agent_context(&mut self) {
        self.prompt_coordination
            .context_discovery
            .agent_context
            .clear();
        self.prompt_coordination
            .context_discovery
            .pending_agents
            .clear();
        self.prompt_coordination
            .context_discovery
            .frozen_agents
            .clear();
        self.prompt_coordination
            .context_discovery
            .initialized_agent_context
            .clear();
    }

    pub(super) fn disable_optional_extension(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        message: &str,
    ) {
        if let Some(entry) = self.extensions.entries.get_mut(connection_id) {
            entry.respawn_allowed = false;
        }
        self.emit_optional_extension_skipped(message);
        self.handle_disconnect(connection_id);
    }

    pub(super) fn handle_disconnect(&mut self, connection_id: &tau_proto::ConnectionId) {
        self.handle_disconnect_at(connection_id, Instant::now());
    }

    /// Stop routing immediately and begin supervised cleanup against `now`.
    pub(super) fn handle_disconnect_at(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        now: Instant,
    ) {
        self.extensions.startup_deadlines.remove(connection_id);
        self.extensions
            .expired_startup_connects
            .remove(connection_id);
        self.cancel_rendered_previews(|request| match request {
            PendingRenderedPrompt::System {
                connection_id: requester,
                ..
            }
            | PendingRenderedPrompt::Prompt {
                connection_id: requester,
                ..
            }
            | PendingRenderedPrompt::Tools {
                connection_id: requester,
                ..
            } => requester == connection_id,
        });
        let meta = self.runtime_io.bus.connection(connection_id).cloned();
        let is_extension = meta.as_ref().is_some_and(|meta| {
            meta.origin == ConnectionOrigin::Supervised || meta.origin == ConnectionOrigin::InMemory
        });
        let disconnected_meta = self.runtime_io.bus.disconnect(connection_id);
        if meta
            .as_ref()
            .is_some_and(|meta| meta.origin == ConnectionOrigin::Supervised)
        {
            self.begin_supervised_cleanup_at(connection_id, now);
        }
        if is_extension {
            // Mark the extension non-blocking before any cleanup can advance
            // session init or prompt dispatch.
            self.set_extension_state(connection_id, ExtensionState::Disconnected);
        }
        let disconnected_provider = self
            .extensions
            .entries
            .get(connection_id)
            .filter(|entry| entry.kind == ClientKind::Provider)
            .map(|entry| entry.name.clone());
        if let Some(publisher_extension_id) = disconnected_provider
            && self
                .provider_runtime
                .models_by_extension
                .contains_key(connection_id)
            && !self.clear_parked_provider_model_updates(&publisher_extension_id)
        {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::ProviderModelsUpdated(tau_proto::ProviderModelsUpdated {
                    publisher_extension_id,
                    models: Vec::new(),
                }),
            );
        }
        self.extensions.activation_staging.remove(connection_id);
        self.extensions
            .pending_provider_model_declarations
            .remove(connection_id);
        self.extensions
            .pending_tool_lifecycle_declarations
            .remove(connection_id);
        self.extensions
            .pending_action_schema_declarations
            .remove(connection_id);
        self.extensions
            .pending_prompt_fragment_declarations
            .remove(connection_id);
        self.extensions
            .pending_session_discovery_declarations
            .remove(connection_id);
        self.extensions
            .pending_agent_context_declarations
            .remove(connection_id);
        self.extensions.ready_received.remove(connection_id);
        self.remove_discovered_context(connection_id);
        self.publish_session_skills_projection();
        // Remove prompt/context projections before resolving an interception
        // owned by this connection. Resolution may synchronously commit deferred
        // readiness and dispatch a prompt snapshot.
        self.remove_extension_context_for_connection(connection_id);
        self.clear_agent_runtime_indicators_for_source(connection_id);
        self.runtime_io
            .publication
            .suspended_interceptor_connections
            .remove(&connection_id.clone());
        self.runtime_io
            .publication
            .interceptors
            .remove_connection(connection_id);
        self.fail_pending_intercept_for_disconnect(connection_id);
        if is_extension {
            self.unregister_connection_tools_for_disconnect(connection_id);
            self.tool_routing
                .action_registry
                .unregister_connection(connection_id);
        }

        self.fail_pending_action_invocations_for_connection(connection_id);
        let failed_retries: Vec<_> = self
            .ui_runtime
            .pending_retry_prompts
            .iter()
            .filter(|(_, pending)| pending.provider_connection_id == *connection_id)
            .map(|(id, pending)| (id.clone(), pending.clone()))
            .collect();
        for (request_id, pending) in failed_retries {
            self.ui_runtime.pending_retry_prompts.remove(&request_id);
            let _ = self.runtime_io.bus.send_to(
                &pending.requester_client_id,
                None,
                HarnessOutputMessage::deliver(Event::UiRetryPromptResult(
                    tau_proto::UiRetryPromptResult {
                        request_id: pending.ui_request_id,
                        target_agent_id: Some(pending.target_agent_id),
                        target_label: pending.target_label,
                        status: None,
                        message: "Cannot retry: the prompt's provider route disconnected.".into(),
                    },
                )),
            );
        }
        self.ui_runtime
            .pending_retry_prompts
            .retain(|_, pending| &pending.requester_client_id != connection_id);
        self.fail_pending_ui_shell_commands_for_provider(
            connection_id,
            "the shell extension instance disconnected before the command completed",
        );
        let completed_foreground_calls = self.fail_pending_tool_calls_for_connection(connection_id);
        let lost_provider_prompts = self
            .provider_runtime
            .pending_prompts
            .iter()
            .filter_map(|(prompt_id, provider_id)| {
                (provider_id == connection_id).then_some(prompt_id.clone())
            })
            .collect::<Vec<_>>();
        self.provider_runtime
            .pending_prompts
            .retain(|_, provider_id| provider_id != connection_id);
        for prompt_id in lost_provider_prompts {
            self.publish_final_unknown_standalone_accounting(&prompt_id);
            let Some(cid) = self
                .prompt_coordination
                .prompt_runtime
                .agents
                .get(&prompt_id)
                .cloned()
            else {
                continue;
            };
            let deferred_activation = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| {
                    agent
                        .dispatch
                        .pending_message_wakes
                        .iter()
                        .any(|wake| wake.node_id.is_none())
                        || agent
                            .identity
                            .agent_id
                            .as_deref()
                            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                            .is_some_and(|tree| {
                                tree.marked_inference_has_deferred_prompt_activation(&prompt_id)
                            })
                });
            let checkpoint = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .and_then(|agent| agent.identity.agent_id.as_deref())
                .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                .and_then(|tree| tree.marked_inference_checkpoint(&prompt_id).cloned());
            if let Some(checkpoint) = checkpoint {
                if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                    agent.dispatch.activation_dispatch =
                        path_crate_agent::ActivationDispatchState::DispatchUncertain {
                            owner: checkpoint.transaction_id.clone().map_or(
                                path_crate_agent::InferenceCheckpointOwner::Inference,
                                |id| path_crate_agent::InferenceCheckpointOwner::Standalone { id },
                            ),
                            agent_prompt_id: prompt_id.clone(),
                            through: checkpoint.through,
                            model: checkpoint.model,
                            operation: checkpoint.operation,
                            activation_cut: checkpoint.activation_cut,
                        };
                    agent.dispatch.in_flight_prompt = None;
                }
                if checkpoint.transaction_id.is_none()
                    && deferred_activation
                    && let Some((agent_id, originator)) = self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(&cid)
                        .and_then(|agent| {
                            Some((
                                agent.identity.agent_id.clone()?,
                                agent.identity.originator.clone(),
                            ))
                        })
                {
                    self.publish_for_agent(
                        &cid,
                        Event::AgentPromptTerminated(AgentPromptTerminated {
                            automatic_compaction_decision: None,
                            agent_id,
                            agent_prompt_id: prompt_id,
                            reason: AgentPromptTerminationReason::Stale,
                            originator,
                        }),
                    );
                }
            }
        }
        self.ui_runtime
            .client_writers
            .remove(&connection_id.clone());
        self.peer_messaging
            .external_message_peers
            .remove(&connection_id.clone());
        let canceled_peer_receives = self
            .peer_messaging
            .pending_external_receive_acks
            .iter()
            .filter_map(|(message_id, pending)| {
                matches!(
                    &pending.completion,
                    PendingPeerReceiveCompletion::Remote { client_id, .. }
                        if client_id == connection_id
                )
                .then_some((
                    message_id.clone(),
                    pending.recipient_id.clone(),
                    pending.rate_admitted_at,
                ))
            })
            .collect::<Vec<_>>();
        let canceled_message_ids = canceled_peer_receives
            .iter()
            .map(|(message_id, _, _)| message_id.clone())
            .collect::<HashSet<_>>();
        for (message_id, recipient_id, admitted_at) in canceled_peer_receives {
            if let Some(pending) = self
                .peer_messaging
                .pending_external_receive_acks
                .get_mut(&message_id)
            {
                pending.canceled = true;
            }
            self.release_peer_input_rate(&recipient_id, admitted_at);
            self.cleanup_uncommitted_peer_auto_start(&recipient_id);
        }
        self.discard_canceled_peer_receive_publishes(&canceled_message_ids);
        self.peer_messaging
            .pending_external_receive_acks
            .retain(|message_id, _| !canceled_message_ids.contains(message_id));
        if let Some(cancellations) = self
            .peer_messaging
            .inbound_peer_io_cancellations
            .remove(&connection_id.clone())
        {
            for cancellation in cancellations {
                if let Some(cancellation) = cancellation.upgrade() {
                    cancellation.store(true, path_std_sync_atomic::Ordering::Release);
                }
            }
        }
        let removed_providers = self
            .provider_runtime
            .quota
            .iter()
            .filter(|(_, quota)| quota.source_id == *connection_id)
            .map(|(provider, _)| provider.clone())
            .collect::<Vec<_>>();
        for provider in removed_providers {
            self.remove_provider_quota(&provider);
        }
        if self
            .provider_runtime
            .models_by_extension
            .remove(connection_id)
            .is_some()
        {
            self.refresh_provider_models_and_publish_state();
        }
        if !self.extensions.resolving_initial_collisions {
            if self
                .runtime_io
                .publication
                .disconnect_terminal_batch_pending
                .is_empty()
            {
                self.drain_pending_tool_invocations_or_report();
                for (call_id, cid) in completed_foreground_calls {
                    self.maybe_complete_agent_turn_for(&cid, call_id.as_str());
                }
                self.drain_publish_idle_dispatches();
                self.try_advance_queue();
            }
            self.maybe_complete_session_init_for_disconnect(connection_id);
        }
        let Some(meta) = disconnected_meta.or(meta) else {
            return;
        };
        if is_extension {
            self.emit_extension_exited(&meta.name);
        }
        if meta.origin == ConnectionOrigin::Supervised
            && !self
                .extensions
                .supervised_writers
                .contains_key(connection_id)
        {
            self.schedule_extension_restart_at(connection_id, now);
        }
    }

    /// Arm one absolute disconnect-to-kill deadline for a retained writer.
    pub(super) fn begin_supervised_cleanup_at(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        now: Instant,
    ) {
        let connection_id = connection_id.clone();
        if let Some(writer) = self.extensions.supervised_writers.get(&connection_id) {
            let deadline = now + SUPERVISED_CLEANUP_GRACE;
            writer.arm_cleanup_deadline(deadline);
            self.extensions
                .cleanup_deadlines
                .entry(connection_id)
                .or_insert(deadline);
        }
    }

    /// Join a reaped writer and make its disconnected tool eligible for delay.
    pub(super) fn handle_supervised_writer_cleanup_complete_at(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        now: Instant,
    ) -> Result<(), HarnessError> {
        let connection_id = connection_id.clone();
        self.extensions.cleanup_deadlines.remove(&connection_id);
        let Some(mut writer) = self.extensions.supervised_writers.remove(&connection_id) else {
            return Ok(());
        };
        let name = self
            .extensions
            .entries
            .get(&connection_id)
            .map(|entry| entry.name.to_string())
            .unwrap_or_else(|| connection_id.to_string());
        writer
            .join()
            .map_err(|_| HarnessError::ThreadJoin(name.to_string()))?;
        if self
            .extensions
            .entries
            .get(&connection_id)
            .is_some_and(|entry| entry.state == ExtensionState::Disconnected)
        {
            self.schedule_extension_restart_at(&connection_id, now);
        }
        Ok(())
    }

    /// Schedule one session-budgeted tool replacement, or disable it at cap.
    pub(super) fn schedule_extension_restart_at(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
        now: Instant,
    ) {
        let connection_id = connection_id.clone();
        let Some(entry) = self.extensions.entries.get_mut(&connection_id) else {
            return;
        };
        if entry.kind == ClientKind::Provider
            || entry.supervised_config.is_none()
            || !entry.respawn_allowed
            || entry.state != ExtensionState::Disconnected
        {
            return;
        }
        if MAX_EXTENSION_RESTART_ATTEMPTS <= entry.restart_attempt {
            entry.respawn_allowed = false;
            self.extensions.restart_deadlines.remove(&connection_id);
            self.extensions
                .restart_budget_disabled
                .insert(connection_id);
            let message = extension_restart_disabled_notice(&entry.name);
            self.emit_info_important(&message);
            return;
        }
        self.extensions
            .restart_deadlines
            .entry(connection_id)
            .or_insert(now + EXTENSION_RESTART_DELAY);
    }

    /// Reset only session-scoped restart budget at a logical session rollover.
    ///
    /// Permanently disabled optional/configuration peers remain disabled. A
    /// tool disabled specifically by the prior session's budget becomes
    /// eligible again after the ordinary one-second delay.
    pub(super) fn reset_extension_restart_budgets_at(&mut self, now: Instant) {
        self.extensions.restart_deadlines.clear();
        for entry in self.extensions.entries.values_mut() {
            entry.restart_attempt = 0;
        }
        for connection_id in std::mem::take(&mut self.extensions.restart_budget_disabled) {
            if let Some(entry) = self.extensions.entries.get_mut(&connection_id) {
                entry.respawn_allowed = true;
            }
        }
        let restartable = self
            .extensions
            .order
            .iter()
            .filter(|connection_id| {
                self.extensions
                    .entries
                    .get(*connection_id)
                    .is_some_and(|entry| {
                        entry.state == ExtensionState::Disconnected
                            && entry.kind == ClientKind::Tool
                            && entry.respawn_allowed
                            && entry.supervised_config.is_some()
                    })
                    && !self
                        .extensions
                        .supervised_writers
                        .contains_key(*connection_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        for connection_id in restartable {
            self.schedule_extension_restart_at(&connection_id, now);
        }
    }

    pub(super) fn unregister_connection_tools_for_disconnect(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) {
        self.provider_runtime
            .cache_residency
            .release_connection(connection_id);
        self.clear_cache_refreshes(tau_proto::ProviderCacheRefreshCancelReason::ProviderRotated);
        let removing_tools: Vec<(ToolName, ToolName)> = self
            .tool_routing
            .registry
            .all_tool_names()
            .into_iter()
            .filter_map(|tool_name| {
                self.tool_routing
                    .registry
                    .providers_for(tool_name.as_str())
                    .into_iter()
                    .find(|provider| provider.connection_id == *connection_id)
                    .map(|provider| {
                        (
                            tool_name.clone(),
                            self.tool_model_visible_name(&provider.tool).clone(),
                        )
                    })
            })
            .collect();

        let _ = self
            .tool_routing
            .registry
            .unregister_connection(connection_id);
        for (internal_name, visible_name) in removing_tools {
            if self
                .tool_routing
                .registry
                .providers_for(internal_name.as_str())
                .is_empty()
            {
                self.mark_tool_unavailable_for_notice(internal_name, visible_name);
            }
        }
    }

    pub(super) fn is_provider_extension(&self, connection_id: &tau_proto::ConnectionId) -> bool {
        self.extensions
            .entries
            .get(connection_id)
            .is_some_and(|entry| entry.kind == ClientKind::Provider)
    }

    pub(super) fn accepts_provider_event_from(
        &self,
        source_id: &tau_proto::ConnectionId,
        event_name: &tau_proto::EventName,
    ) -> bool {
        match self.runtime_io.bus.connection(source_id) {
            Some(metadata) if metadata.kind == ClientKind::Provider => true,
            Some(metadata) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    source_id = %source_id,
                    kind = ?metadata.kind,
                    "discarding provider event from non-provider connection"
                );
                false
            }
            None => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    source_id = %source_id,
                    "discarding provider event from unknown connection"
                );
                false
            }
        }
    }

    pub(super) fn provider_prompt_owner_matches(
        &self,
        source_id: &tau_proto::ConnectionId,
        agent_prompt_id: &AgentPromptId,
        event_name: tau_proto::EventName,
    ) -> bool {
        match self.provider_runtime.pending_prompts.get(agent_prompt_id) {
            Some(expected) if *expected.as_str() == **source_id => true,
            Some(expected) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    agent_prompt_id = %agent_prompt_id,
                    expected_provider = %expected,
                    source_id = %source_id,
                    "discarding provider event from non-owning provider"
                );
                false
            }
            None => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    agent_prompt_id = %agent_prompt_id,
                    source_id = %source_id,
                    "discarding provider event for prompt without a pending provider route"
                );
                false
            }
        }
    }

    pub(super) fn fail_pending_tool_calls_for_connection(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) -> Vec<(ToolCallId, AgentId)> {
        let mut failed_call_ids: Vec<ToolCallId> = self
            .tool_routing
            .tool_runtime
            .pending_tool_providers
            .iter()
            .filter_map(|(call_id, provider_id)| {
                if provider_id == connection_id {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();
        // Keep disconnect cleanup deterministic and publish background
        // terminals before a foreground terminal can complete the agent turn.
        // Queued work is drained only after the whole batch below.
        failed_call_ids.sort_by_key(|call_id| {
            (
                !self
                    .tool_routing
                    .tool_runtime
                    .tool_turn
                    .is_backgrounded(call_id),
                call_id.clone(),
            )
        });
        let foreground_batch = failed_call_ids
            .iter()
            .filter(|call_id| {
                !self
                    .tool_routing
                    .tool_runtime
                    .tool_turn
                    .is_backgrounded(call_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        self.runtime_io
            .publication
            .disconnect_terminal_batch_pending
            .extend(foreground_batch);

        let completed_foreground_calls: Vec<(ToolCallId, AgentId)> = Vec::new();

        for call_id in failed_call_ids {
            let Some(tool) = self
                .tool_routing
                .tool_runtime
                .pending_tools
                .get(&call_id)
                .cloned()
            else {
                continue;
            };
            let mut error = ToolError {
                presentation: Default::default(),
                call_id: call_id.clone(),
                tool_name: tool.name,
                tool_type: tool.tool_type,
                message: extension_disconnected_tool_call_error_message(&call_id),
                details: None,
                originator: tau_proto::PromptOriginator::User,

                display: None,
            };
            if self
                .tool_routing
                .tool_runtime
                .tool_turn
                .is_backgrounded(&call_id)
            {
                error.message = extension_disconnected_background_tool_call_error_message(&call_id);
                if self
                    .tool_routing
                    .tool_runtime
                    .tool_agents
                    .contains_key(call_id.as_str())
                {
                    self.handle_background_tool_error_inner(
                        Some(crate::harness::harness_connection_id()),
                        error,
                        BackgroundCompletionPromptMode::QueueOnly,
                        tau_proto::ToolTerminalCause::ProviderDisconnected,
                    );
                } else {
                    self.publish_terminal_tool_error(
                        None,
                        Some(crate::harness::harness_connection_id()),
                        error,
                    );
                }
                continue;
            }

            // Publish on the owning agent's branch so the
            // synthesized failure folds onto the right node. Without
            // the snap, sibling side agents could leave
            // `tree.head` on the wrong branch and the fold would land
            // there instead. Complete the failed in-flight calls without
            // draining queued calls or advancing prompts yet; disconnect
            // handling unregisters the dead provider first, then drains
            // the scheduler and completes turns after all interrupted calls
            // have been terminalized.
            let owner = self
                .tool_routing
                .tool_runtime
                .tool_agents
                .get(call_id.as_str())
                .cloned();
            if let Some(cid) = owner.as_ref() {
                self.publish_terminal_tool_error_with_cause(
                    Some(cid),
                    Some(crate::harness::harness_connection_id()),
                    error,
                    tau_proto::ToolTerminalCause::ProviderDisconnected,
                );
            } else {
                // No conversation attribution — fall back to the
                // unsnapped publish so the error still reaches the
                // bus / log.
                self.publish_terminal_tool_error(
                    None,
                    Some(crate::harness::harness_connection_id()),
                    error,
                );
            }
        }

        completed_foreground_calls
    }

    pub(super) fn send_action_error_to_client(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        invocation_id: ActionInvocationId,
        action_id: String,
        message: String,
    ) {
        let _ = self.runtime_io.bus.send_to(
            client_id,
            Some(crate::harness::harness_connection_id()),
            HarnessOutputMessage::deliver(Event::ActionError(ActionError {
                invocation_id,
                action_id,
                message,
                details: None,
            })),
        );
    }

    pub(super) fn handle_action_invoke(
        &mut self,
        client_id: &tau_proto::ConnectionId,
        invoke: ActionInvoke,
    ) -> Result<bool, HarnessError> {
        if self
            .runtime_io
            .bus
            .connection(client_id)
            .is_none_or(|metadata| metadata.kind != ClientKind::Ui)
        {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                "only UI clients may invoke extension actions".to_owned(),
            );
            return Ok(true);
        }
        if invoke.session_id != self.session_runtime.current_session_id {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                format!(
                    "action invocation targets session `{}` but current session is `{}`",
                    invoke.session_id, self.session_runtime.current_session_id
                ),
            );
            return Ok(true);
        }
        if self
            .ui_runtime
            .pending_action_invocations
            .contains_key(&invoke.invocation_id)
            || self
                .ui_runtime
                .completed_action_invocations
                .contains(&invoke.invocation_id)
        {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                "duplicate pending action invocation id".to_owned(),
            );
            return Ok(true);
        }

        let provider_connection_id = match self
            .tool_routing
            .action_registry
            .route_action_invoke(&invoke)
        {
            Ok(provider_connection_id) => provider_connection_id,
            Err(error) => {
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    error.to_string(),
                );
                return Ok(true);
            }
        };
        let provider = self
            .tool_routing
            .action_registry
            .schema_for_connection(&provider_connection_id)
            .expect("routed Action provider must retain its schema");

        match self.runtime_io.bus.send_to(
            &provider_connection_id,
            Some(client_id),
            HarnessOutputMessage::deliver(Event::ActionInvoke(invoke.clone())),
        ) {
            Ok(report) if !report.delivered_to.is_empty() => {
                self.ui_runtime.pending_action_invocations.insert(
                    invoke.invocation_id.clone(),
                    PendingActionInvocation {
                        owner_name: provider.extension_name.clone(),
                        owner_instance_id: provider.instance_id,
                        provider_connection_id,
                        requester_client_id: client_id.clone(),
                        session_id: invoke.session_id,
                        action_id: invoke.action_id,
                    },
                );
            }
            Ok(report) => {
                tracing::warn!(
                    target: "tau_harness",
                    invocation_id = %invoke.invocation_id,
                    ?report,
                    "action invocation route did not deliver"
                );
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    "action provider is unavailable".to_owned(),
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness",
                    invocation_id = %invoke.invocation_id,
                    %error,
                    "action invocation route failed"
                );
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    "action provider is unavailable".to_owned(),
                );
            }
        }
        Ok(true)
    }

    pub(super) fn handle_action_result(
        &mut self,
        publisher: &interception::AuthenticatedExtensionPublication,
        result: ActionResult,
    ) {
        let source_id = &publisher.source;
        let Some(pending) = self
            .ui_runtime
            .pending_action_invocations
            .get(&result.invocation_id)
            .cloned()
        else {
            return;
        };
        if pending.provider_connection_id != *source_id
            || pending.owner_name != publisher.publisher
            || pending.owner_instance_id != publisher.instance_id
            || pending.session_id != self.session_runtime.current_session_id
            || pending.action_id != result.action_id
        {
            tracing::warn!(
                target: "tau_harness",
                invocation_id = %result.invocation_id,
                source_id = %source_id,
                expected_provider = %pending.provider_connection_id,
                expected_action = %pending.action_id,
                action_id = %result.action_id,
                "discarding action result from non-owning or mismatched source"
            );
            return;
        }
        self.ui_runtime
            .pending_action_invocations
            .remove(&result.invocation_id);
        self.ui_runtime
            .completed_action_invocations
            .insert(result.invocation_id.clone());
        let _ = self.runtime_io.bus.send_to(
            &pending.requester_client_id,
            Some(crate::harness::harness_connection_id()),
            HarnessOutputMessage::deliver(Event::ActionResult(result)),
        );
    }

    pub(super) fn handle_action_error(
        &mut self,
        publisher: &interception::AuthenticatedExtensionPublication,
        error: ActionError,
    ) {
        let source_id = &publisher.source;
        let Some(pending) = self
            .ui_runtime
            .pending_action_invocations
            .get(&error.invocation_id)
            .cloned()
        else {
            return;
        };
        if pending.provider_connection_id != *source_id
            || pending.owner_name != publisher.publisher
            || pending.owner_instance_id != publisher.instance_id
            || pending.session_id != self.session_runtime.current_session_id
            || pending.action_id != error.action_id
        {
            tracing::warn!(
                target: "tau_harness",
                invocation_id = %error.invocation_id,
                source_id = %source_id,
                expected_provider = %pending.provider_connection_id,
                expected_action = %pending.action_id,
                action_id = %error.action_id,
                "discarding action error from non-owning or mismatched source"
            );
            return;
        }
        self.ui_runtime
            .pending_action_invocations
            .remove(&error.invocation_id);
        self.ui_runtime
            .completed_action_invocations
            .insert(error.invocation_id.clone());
        let _ = self.runtime_io.bus.send_to(
            &pending.requester_client_id,
            Some(crate::harness::harness_connection_id()),
            HarnessOutputMessage::deliver(Event::ActionError(error)),
        );
    }

    pub(super) fn fail_pending_action_invocations_for_connection(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) {
        let mut failed: Vec<_> = self
            .ui_runtime
            .pending_action_invocations
            .iter()
            .filter_map(|(invocation_id, pending)| {
                (pending.provider_connection_id == *connection_id)
                    .then_some((invocation_id.clone(), pending.clone()))
            })
            .collect();
        failed.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        for (invocation_id, pending) in failed {
            self.ui_runtime
                .pending_action_invocations
                .remove(&invocation_id);
            self.ui_runtime
                .completed_action_invocations
                .insert(invocation_id.clone());
            if &pending.requester_client_id == connection_id {
                continue;
            }
            self.send_action_error_to_client(
                &pending.requester_client_id,
                invocation_id,
                pending.action_id.clone(),
                format!(
                    "action `{}` was interrupted because extension disconnected",
                    pending.action_id
                ),
            );
        }
        let requester_disconnected = self
            .ui_runtime
            .pending_action_invocations
            .iter()
            .filter_map(|(invocation_id, pending)| {
                (&pending.requester_client_id == connection_id).then_some(invocation_id.clone())
            })
            .collect::<Vec<_>>();
        for invocation_id in requester_disconnected {
            self.ui_runtime
                .pending_action_invocations
                .remove(&invocation_id);
            self.ui_runtime
                .completed_action_invocations
                .insert(invocation_id);
        }
    }

    pub(super) fn try_respawn_supervised_extension(
        &mut self,
        connection_id: &tau_proto::ConnectionId,
    ) -> Result<(), HarnessError> {
        let Some(entry) = self.extensions.entries.get_mut(connection_id) else {
            return Ok(());
        };
        let Some(config) = entry.supervised_config.clone() else {
            return Ok(());
        };
        if entry.kind == ClientKind::Provider || !entry.respawn_allowed {
            return Ok(());
        }

        entry.restart_attempt += 1;
        let attempt = entry.restart_attempt;
        let instance_id = entry.instance_id;
        let name = entry.name.clone();
        let kind = entry.kind.clone();
        let secrets = entry.secrets.clone();
        let tool_prefix = entry.tool_prefix.clone();
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::ExtensionRestarting(tau_proto::ExtensionRestarting {
                instance_id,
                extension_name: name.clone(),
                pid: None,
                attempt,
                reason: Some("unexpected disconnect".to_owned()),
            }),
        );

        let log_path = if self.session_runtime.storage_mode.is_ephemeral() {
            None
        } else {
            Some(
                extension_stderr_log_path(
                    &self.sessions_dir(),
                    self.session_runtime.current_session_id.as_str(),
                    &config.name,
                )
                .map_err(|error| HarnessError::Participant(error.to_string()))?,
            )
        };
        let spawned = spawn_supervised(
            &config,
            kind.clone(),
            log_path,
            &self.runtime_io.tx,
            &self.runtime_io.component_ingress_tx,
            &self.session_runtime.state_dir,
            self.session_runtime.storage_mode.is_memory_only(),
            self.config
                .provider_settings_snapshots
                .get(config.name.as_str())
                .unwrap_or(&BTreeMap::new()),
            self.extensions.stderr_mirror.as_ref(),
            attempt,
        )?;
        let new_connection_id = spawned.connection_id.clone();
        tracing::info!(
            target: "tau_harness::startup",
            extension = %config.name,
            pid = spawned.child_pid,
            attempt,
            "extension respawned",
        );

        let old_key = connection_id.clone();
        self.connect_extension(ExtensionConnectCommand {
            entry: ExtensionEntry {
                name,
                instance_id,
                connection_id: new_connection_id,
                kind,
                peer_capabilities: Default::default(),
                tool_prefix,
                require: config.require,
                respawn_allowed: true,
                pid: Some(spawned.child_pid),
                in_process_thread: None,
                supervised_config: Some(config),
                secrets,
                restart_attempt: attempt,
                state: ExtensionState::Spawning,
                protocol_io: spawned.protocol_io,
            },
            origin: ConnectionOrigin::Supervised,
            writer_tx: spawned.writer_tx,
            initialized_ack: spawned.initialized_ack,
            supervised_writer: Some(spawned.writer),
            replaces: Some(old_key),
        })?;
        Ok(())
    }

    pub(super) fn extension_tool_request_rejection(&self, request: &ToolRequest) -> Option<String> {
        self.known_tool_call_ids().contains(&request.call_id).then(|| {
            format!(
                "extension emitted tool request `{}` with already-known call_id `{}`; refusing to route it",
                request.tool_name, request.call_id
            )
        })
    }

    pub(super) fn reject_extension_tool_request(&mut self, message: String) {
        self.emit_info_important(&message);
    }

    // -----------------------------------------------------------------------
    // Tool-call session bookkeeping
    // -----------------------------------------------------------------------
    //
    // Terminal tool facts (`ToolResult` / `ToolError` and provider variants)
    // are persisted into the owning agent transcript. `ToolRequest` itself is a
    // runtime routing intent; these helpers maintain maps that attribute later
    // terminal events back to the originating agent or extension request.

    /// Records runtime bookkeeping for an extension-originated `ToolRequest`
    /// that does not have an owning agent. Agent-owned tool calls are tracked
    /// through the prompt/tool routing path instead.
    pub(super) fn track_extension_tool_request_metadata(&mut self, request: &ToolRequest) {
        self.tool_routing.tool_runtime.pending_tools.insert(
            request.call_id.clone(),
            PendingTool {
                name: request.tool_name.clone(),
                internal_name: request.tool_name.clone(),
                tool_type: request.tool_type,
                allows_provider_image: false,
            },
        );
    }

    /// Clears one prompt's tool snapshots and every exact call backreference.
    pub(super) fn clear_prompt_tool_snapshot(&mut self, agent_prompt_id: &AgentPromptId) {
        self.prompt_coordination
            .prompt_runtime
            .clear_prompt_tool_snapshot(agent_prompt_id);
    }

    /// Releases the conversation/name/provider mappings for a completed tool
    /// call. Must run *after* the result/error event has been published so
    /// terminal-event enrichment and transcript attribution can still read the
    /// runtime metadata.
    pub(crate) fn clear_tool_call_tracking(&mut self, call_id: &str) {
        let owner = self
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
        if owner
            .as_ref()
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .is_some_and(|agent| agent.identity.persistence.is_ephemeral())
        {
            self.tool_routing
                .tool_runtime
                .completed_ephemeral_tool_calls
                .insert(call_id.into());
        }
        self.tool_routing
            .tool_runtime
            .completed_tool_calls
            .insert(call_id.into());
        self.tool_routing
            .tool_runtime
            .peer_tool_requests
            .remove(call_id);
        self.tool_routing
            .tool_runtime
            .peer_internal_tool_agents
            .remove(call_id);
        if let Some(owner) = owner {
            self.tool_routing
                .tool_runtime
                .completed_tool_agents
                .insert(call_id.into(), owner);
        }
        self.tool_routing.tool_runtime.tool_agents.remove(call_id);
        self.tool_routing.tool_runtime.pending_tools.remove(call_id);
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
        self.tool_routing
            .tool_runtime
            .pending_tool_providers
            .remove(call_id);
        self.tool_routing
            .tool_runtime
            .pending_terminal_observations
            .remove(call_id);
        self.tool_routing
            .tool_runtime
            .pending_wait_settlements
            .remove(call_id);
        self.tool_routing
            .tool_runtime
            .post_commit_runtime_only_tool_terminals
            .remove(call_id);
        self.tool_routing
            .tool_runtime
            .pending_background_completion_modes
            .remove(call_id);
        self.tool_routing
            .tool_runtime
            .pending_cancellation_observations
            .remove(call_id);
        self.prompt_coordination
            .prompt_runtime
            .remove_tool_call_prompt(call_id);
    }

    pub(super) fn validate_tool_event_source(
        &self,
        call_id: &ToolCallId,
        source_id: &tau_proto::ConnectionId,
    ) -> bool {
        match self
            .tool_routing
            .tool_runtime
            .pending_tool_providers
            .get(call_id)
        {
            Some(provider_id) => provider_id == source_id,
            None if self.is_harness_owned_tool_call(call_id) => {
                source_id == harness_connection_id()
            }
            None => true,
        }
    }

    pub(super) fn is_extension_fallback_emit_allowed(event: &Event) -> bool {
        matches!(
            event,
            Event::MessageDeliveredReported(_)
                | Event::MessageEditedReported(_)
                | Event::MessageDeletedReported(_)
                | Event::MessageReactionAddedReported(_)
                | Event::MessageReactionRemovedReported(_)
                | Event::MessageSentReported(_)
                | Event::ToolProgressReported(_)
                | Event::ToolResultReported(_)
                | Event::ToolErrorReported(_)
                | Event::ToolCancelledReported(_)
                | Event::AgentRuntimeIndicatorsDeclared(_)
                | Event::ShellCommandProgressReported(_)
                | Event::ShellCommandFinishedReported(_)
                | Event::ProviderQuotaReplaceReported(_)
                | Event::ProviderQuotaPatchReported(_)
                | Event::ProviderQuotaClearReported(_)
                | Event::ProviderPromptSubmittedReported(_)
                | Event::ProviderResponseUpdatedReported(_)
                | Event::ProviderResponseFinishedReported(_)
                | Event::ProviderRetryPromptResultReported(_)
                | Event::ProviderCacheMissDiagnosticReported(_)
                | Event::ProviderCacheRefreshFinishedReported(_)
        )
    }

    pub(super) fn is_client_fallback_emit_allowed(event: &Event) -> bool {
        matches!(event, Event::UiShellCommand(_))
    }

    pub(super) fn requires_tool_event_intake(event: &Event) -> bool {
        matches!(
            event,
            Event::ToolResult(_)
                | Event::ToolResultDisplay(_)
                | Event::ToolError(_)
                | Event::ToolResultReported(_)
                | Event::ToolErrorReported(_)
                | Event::ProviderToolResult(_)
                | Event::ProviderToolError(_)
                | Event::ToolCancelled(_)
                | Event::ToolCancelledReported(_)
                | Event::ToolBackgroundResult(_)
                | Event::ToolBackgroundResultDisplay(_)
                | Event::ToolBackgroundError(_)
        )
    }

    pub(super) fn is_peer_forbidden_harness_fact(event: &Event) -> bool {
        // Peer intake rejects the same harness-owned facts that interception
        // treats as protected. Update this with `MUST_PASS_BY_DEFAULT` and
        // `immutable_protected_fact_was_modified` when adding new
        // harness-owned immutable facts.
        matches!(
            event,
            Event::SessionStarted(_)
                | Event::SessionShutdown(_)
                | Event::SessionAgentLoaded(_)
                | Event::SessionAgentUnloaded(_)
                | Event::AgentStatsUpdated(_)
                | Event::AgentStarted(_)
                | Event::AgentPromptStarted(_)
                | Event::AgentPromptFailed(_)
                | Event::AgentPromptRejected(_)
                | Event::AgentOuterTurnStarted(_)
                | Event::AgentOuterTurnFinished(_)
                | Event::AgentPromptCreated(_)
                | Event::AgentMessageSent(_)
                | Event::AgentMessageReceived(_)
                | Event::ProviderModelsUpdated(_)
                | Event::AgentInitializationContextSet(_)
                | Event::HarnessAgentContextInitialized(_)
                | Event::HarnessSessionSkillsAvailable(_)
                | Event::ToolRegister(_)
                | Event::ToolUnregister(_)
                | Event::ToolProgress(_)
                | Event::ToolResult(_)
                | Event::ToolResultDisplay(_)
                | Event::ToolError(_)
                | Event::ToolCancelled(_)
                | Event::ActionSchemaPublished(_)
                | Event::ActionResult(_)
                | Event::ActionError(_)
                | Event::HarnessProviderQuotaChanged(_)
        )
    }

    pub(super) fn is_harness_owned_tool_call(&self, call_id: &ToolCallId) -> bool {
        self.tool_routing
            .tool_runtime
            .tool_agents
            .contains_key(call_id)
            && self
                .tool_routing
                .tool_runtime
                .pending_tools
                .get(call_id)
                .is_some_and(|tool| {
                    self.tool_routing
                        .internal_tool_handlers
                        .iter()
                        .any(|handler| handler.handles(&tool.internal_name))
                })
    }

    // -----------------------------------------------------------------------
    // Lifecycle helpers
    // -----------------------------------------------------------------------

    pub(super) fn authenticated_source_name(
        &self,
        source_id: &tau_proto::ConnectionId,
    ) -> Option<tau_proto::ExtensionName> {
        if source_id == harness_connection_id() {
            return Some(harness_extension_name().clone());
        }
        self.extensions
            .entries
            .get(source_id)
            .map(|entry| entry.name.clone())
            .or_else(|| {
                self.runtime_io
                    .bus
                    .connection(source_id)
                    .map(|metadata| metadata.name.clone())
            })
    }

    pub(super) fn find_extension_by_name(
        &self,
        name: &tau_proto::ExtensionName,
    ) -> Option<&ExtensionEntry> {
        self.extensions.entries.values().find(|e| &e.name == name)
    }

    pub(super) fn find_extension_by_connection(
        &self,
        connection_id: &tau_proto::ConnectionId,
    ) -> Option<&ExtensionEntry> {
        self.extensions.entries.get(connection_id)
    }

    pub(super) fn publish_lifecycle_event(&mut self, event: Event) {
        self.publish_event(Some(crate::harness::harness_connection_id()), event);
    }

    pub(super) fn emit_extension_starting(&mut self, extension_name: &tau_proto::ExtensionName) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.session_runtime
            .lifecycle_messages
            .push(format!("extension {extension_name} starting"));
        self.publish_lifecycle_event(Event::ExtensionStarting(tau_proto::ExtensionStarting {
            instance_id: iid,
            extension_name: extension_name.clone(),
            pid,
        }));
    }

    pub(super) fn emit_extension_ready(&mut self, connection_id: &tau_proto::ConnectionId) {
        let Some(ext) = self.find_extension_by_connection(connection_id) else {
            return;
        };
        let name = ext.name.clone();
        let iid = ext.instance_id;
        let pid = ext.pid;
        self.session_runtime
            .lifecycle_messages
            .push(format!("extension {name} ready"));
        self.publish_lifecycle_event(Event::ExtensionReady(tau_proto::ExtensionReady {
            instance_id: iid,
            extension_name: name,
            pid,
        }));
    }

    pub(super) fn emit_extension_exited(&mut self, extension_name: &tau_proto::ExtensionName) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.session_runtime
            .lifecycle_messages
            .push(format!("extension {extension_name} exited"));
        self.publish_lifecycle_event(Event::ExtensionExited(tau_proto::ExtensionExited {
            instance_id: iid,
            extension_name: extension_name.clone(),
            pid,
            exit_code: None,
            signal: None,
        }));
    }

    pub(super) fn check_config_exists(&mut self) {
        if let Some(dir) = tau_config::settings::config_dir()
            && !dir.join("harness.yaml").exists()
        {
            self.emit_info_important(
                "no config found; run `tau init` to create sample config files",
            );
        }
    }

    pub(super) fn emit_missing_default_role(&mut self, missing: Option<MissingDefaultRole>) {
        if let Some(MissingDefaultRole {
            requested,
            fallback,
        }) = missing
        {
            self.emit_info_important(&format!(
                "default_role `{requested}` is not configured; selected `{fallback}` instead"
            ));
        }
    }

    /// Push the configured `config` value (from `harness.yaml`) to
    /// the just-said-Hello extension. Sends point-to-point so it
    /// arrives even if the extension hasn't subscribed to the
    /// `lifecycle` category yet. In-process extensions don't carry
    /// a `supervised_config` so they get the empty default — they
    /// already accept configuration via constructor parameters.
    pub(super) fn send_lifecycle_configure(&mut self, source_id: &tau_proto::ConnectionId) {
        let Some(entry) = self.extensions.entries.get(source_id) else {
            return;
        };
        let config_json = entry
            .supervised_config
            .as_ref()
            .map(|cfg| cfg.config.clone())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let secrets = entry.secrets.clone();
        let tool_prefix = entry.tool_prefix.clone();
        let state_dir = if self.session_runtime.storage_mode.is_memory_only() {
            None
        } else {
            Some(
                match tau_config::settings::extension_state_dir_of(
                    &self.session_runtime.state_dir,
                    &entry.name,
                ) {
                    Ok(state_dir) => state_dir,
                    Err(error) => {
                        tracing::warn!(
                            extension = %entry.name,
                            error = %error,
                            "refusing to configure extension with unsafe state directory name"
                        );
                        let _ = self.runtime_io.bus.send_to(
                            source_id,
                            None,
                            HarnessOutputMessage::Disconnect(Disconnect {
                                reason: Some(error.to_string()),
                            }),
                        );
                        return;
                    }
                },
            )
        };
        let settings_files =
            if entry.supervised_config.is_none() || entry.kind != ClientKind::Provider {
                BTreeMap::new()
            } else {
                self.config
                    .provider_settings_snapshots
                    .get(entry.name.as_str())
                    .cloned()
                    .unwrap_or_default()
            };
        if let Some(state_dir) = &state_dir
            && let Err(error) = std::fs::create_dir_all(state_dir)
        {
            tracing::warn!(
                extension = %entry.name,
                state_dir = %state_dir.display(),
                error = %error,
                "failed to create extension state directory before configure"
            );
        }
        let configure = HarnessOutputMessage::Configure(tau_proto::Configure {
            config: tau_proto::json_to_cbor(&config_json),
            instance_name: self
                .extensions
                .entries
                .get(source_id)
                .map(|entry| entry.name.clone())
                .expect("configured extension has a stable instance name"),
            tool_prefix,
            state_dir,
            secrets,
            settings_files,
        });
        let configure_fits = tau_proto::encode_harness_output_to_vec(&configure)
            .is_ok_and(|encoded| encoded.len() as u64 <= tau_proto::MAX_PROTOCOL_MESSAGE_BYTES);
        if !configure_fits {
            let _ = self.runtime_io.bus.send_to(
                source_id,
                None,
                HarnessOutputMessage::Disconnect(Disconnect {
                    reason: Some("extension Configure exceeds protocol frame limit".to_owned()),
                }),
            );
            return;
        }
        let _ = self.runtime_io.bus.send_to(source_id, None, configure);
    }

    pub(crate) fn emit_info(&mut self, message: &str) {
        self.emit_info_with_level(message, tau_proto::NoticeLevel::Info);
    }

    pub(super) fn emit_harness_failure(&mut self, message: &str) {
        self.emit_notice(
            tau_proto::notice_kind::HARNESS_FAILURE,
            tau_proto::NoticeLevel::Warning,
            tau_proto::NoticePurpose::Alert,
            message,
        );
    }

    pub(crate) fn emit_info_important(&mut self, message: &str) {
        self.emit_notice(
            tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
            tau_proto::NoticeLevel::Warning,
            tau_proto::NoticePurpose::Alert,
            message,
        );
    }

    pub(super) fn emit_optional_extension_skipped(&mut self, message: &str) {
        self.emit_notice(
            tau_proto::notice_kind::EXTENSION_OPTIONAL_SKIPPED,
            tau_proto::NoticeLevel::Warning,
            tau_proto::NoticePurpose::Alert,
            message,
        );
    }

    pub(super) fn emit_extension_startup_diagnostics(
        &mut self,
        diagnostics: &[ExtensionStartupDiagnostic],
    ) {
        for diagnostic in diagnostics {
            match diagnostic.kind {
                ExtensionStartupDiagnosticKind::OptionalSkip => {
                    self.emit_optional_extension_skipped(&diagnostic.message);
                }
                ExtensionStartupDiagnosticKind::StateAccess { .. } => {
                    self.emit_notice(
                        tau_proto::notice_kind::EXTENSION_STATE_ACCESS,
                        tau_proto::NoticeLevel::Warning,
                        tau_proto::NoticePurpose::Alert,
                        &diagnostic.message,
                    );
                }
            }
        }
    }

    pub(super) fn emit_info_with_level(&mut self, message: &str, level: tau_proto::NoticeLevel) {
        let (kind, purpose) = if matches!(
            level,
            tau_proto::NoticeLevel::Critical | tau_proto::NoticeLevel::Warning
        ) {
            (
                tau_proto::notice_kind::HARNESS_INTERNAL_WARNING,
                tau_proto::NoticePurpose::Alert,
            )
        } else {
            (
                tau_proto::notice_kind::HARNESS_NOTICE,
                tau_proto::NoticePurpose::Diagnostic,
            )
        };
        self.emit_notice(kind, level, purpose, message);
    }

    pub(super) fn emit_notice(
        &mut self,
        kind: &str,
        level: tau_proto::NoticeLevel,
        purpose: tau_proto::NoticePurpose,
        message: &str,
    ) {
        let notice = tau_proto::HarnessNotice {
            kind: kind.to_owned(),
            message: message.to_owned(),
            level,
            purpose,
        };
        let is_alert =
            purpose == tau_proto::NoticePurpose::Alert || level == tau_proto::NoticeLevel::Critical;
        if is_alert {
            self.runtime_io
                .replayable_harness_notices
                .push(notice.clone());
        }
        self.enqueue_publish(
            Some(crate::harness::harness_connection_id()),
            Event::HarnessNotice(notice),
            true,
            is_alert,
            None,
        );
    }

    pub(super) fn publish_prompt_terminated(
        &mut self,
        session_id: SessionId,
        agent_prompt_id: AgentPromptId,
        reason: AgentPromptTerminationReason,
        originator: PromptOriginator,
    ) {
        self.publish_prompt_terminated_with_decision(
            session_id,
            agent_prompt_id,
            reason,
            originator,
            None,
        );
    }

    pub(super) fn publish_prompt_terminated_with_decision(
        &mut self,
        _session_id: SessionId,
        agent_prompt_id: AgentPromptId,
        reason: AgentPromptTerminationReason,
        originator: PromptOriginator,
        automatic_compaction_decision: Option<tau_proto::AutomaticCompactionDecision>,
    ) {
        self.publish_prompt_terminated_from(
            _session_id,
            agent_prompt_id,
            reason,
            originator,
            automatic_compaction_decision,
            None,
        );
    }

    pub(super) fn publish_prompt_terminated_from(
        &mut self,
        _session_id: SessionId,
        agent_prompt_id: AgentPromptId,
        reason: AgentPromptTerminationReason,
        originator: PromptOriginator,
        automatic_compaction_decision: Option<tau_proto::AutomaticCompactionDecision>,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let cid = self
            .prompt_coordination
            .prompt_runtime
            .agents
            .get(&agent_prompt_id)
            .cloned();
        let agent_id = cid
            .as_ref()
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .and_then(|conv| conv.identity.agent_id.clone())
            .expect("agent has durable id");
        let event = Event::AgentPromptTerminated(AgentPromptTerminated {
            automatic_compaction_decision,
            agent_id,
            agent_prompt_id,
            reason,
            originator,
        });
        if let Some(cid) = cid {
            self.publish_event_for_agent(&cid, source, event);
        } else {
            self.publish_event(source, event);
        }
    }

    pub(super) fn remove_discovered_context(&mut self, source_id: &tau_proto::ConnectionId) {
        let affected_names = self
            .prompt_coordination
            .context_discovery
            .skill_candidates
            .iter_mut()
            .filter_map(|(name, candidates)| {
                let old_len = candidates.len();
                candidates.retain(|skill| {
                    matches!(skill.source, DiscoveredSkillSource::BuiltIn { .. })
                        || skill.source_id != *source_id
                });
                (candidates.len() != old_len).then(|| name.clone())
            })
            .collect::<Vec<_>>();
        self.prompt_coordination
            .context_discovery
            .skill_candidates
            .retain(|_, candidates| !candidates.is_empty());
        for name in affected_names {
            self.recompute_discovered_skill_winner(&name);
        }
        self.prompt_coordination
            .context_discovery
            .agents_files
            .retain(|file| file.source_id != *source_id);
    }

    pub(super) fn recompute_discovered_skill_winner(&mut self, name: &tau_proto::SkillName) {
        let winner = self
            .prompt_coordination
            .context_discovery
            .skill_candidates
            .get(name)
            .and_then(|candidates| selected_skill_candidate(candidates).cloned());
        if let Some(winner) = winner {
            self.prompt_coordination
                .context_discovery
                .skills
                .insert(name.clone(), winner);
        } else {
            self.prompt_coordination
                .context_discovery
                .skills
                .remove(name);
        }
    }

    pub(super) fn session_init_provider_ids(
        &self,
    ) -> std::collections::HashSet<tau_proto::ConnectionId> {
        let event = Event::SessionStarted(tau_proto::SessionStarted {
            session_id: self.session_runtime.current_session_id.clone(),
            reason: self.session_runtime.current_session_start_reason,
        });
        self.tool_connections_subscribed_to(&event)
            .into_iter()
            .filter(|connection_id| {
                self.prompt_coordination
                    .context_discovery
                    .session_context_providers
                    .contains(connection_id)
            })
            .collect()
    }

    pub(super) fn agent_context_provider_ids(
        &self,
        agent_id: tau_proto::AgentId,
        agent_initialization_id: tau_proto::AgentInitializationId,
    ) -> HashSet<tau_proto::ConnectionId> {
        let event = Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
            session_id: self.session_runtime.current_session_id.clone(),
            ephemeral: self.agent_is_ephemeral(&agent_id),
            agent_id,
            agent_initialization_id,
        });
        self.tool_connections_subscribed_to(&event)
            .into_iter()
            .filter(|connection_id| {
                self.prompt_coordination
                    .context_discovery
                    .agent_context_providers
                    .contains(connection_id)
            })
            .collect()
    }

    pub(super) fn mint_agent_initialization_id(&mut self) -> tau_proto::AgentInitializationId {
        let next = self.agent_runtime.agent_registry.next_initialization_id;
        self.agent_runtime.agent_registry.next_initialization_id = self
            .agent_runtime
            .agent_registry
            .next_initialization_id
            .saturating_add(1);
        agent_initialization_id(
            &self.agent_runtime.agent_registry.accounting_runtime_id,
            self.session_runtime.current_session_generation,
            next,
        )
    }

    pub(super) fn tool_connections_subscribed_to(
        &self,
        event: &Event,
    ) -> HashSet<tau_proto::ConnectionId> {
        self.runtime_io
            .bus
            .connections()
            .into_iter()
            .filter(|connection| {
                connection.kind == ClientKind::Tool
                    && connection.origin != ConnectionOrigin::Socket
                    && self
                        .runtime_io
                        .bus
                        .live_subscriptions(&connection.id)
                        .is_some_and(|selectors| selector_matches_event(selectors, event))
            })
            .map(|connection| connection.id)
            .collect()
    }

    pub(crate) fn session_initialized(&self, session_id: &SessionId) -> bool {
        self.prompt_coordination
            .context_discovery
            .initialized_sessions
            .contains(session_id)
    }

    pub(crate) fn agent_context_ready_for(&self, cid: &AgentId) -> bool {
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return true;
        };
        self.agent_context_ready_for_loaded_agent(&agent_id)
    }

    /// Returns whether one new agent has frozen the exact initialization
    /// generation that its eager initial prompt must render against.
    pub(crate) fn agent_initialization_ready_for(&self, cid: &AgentId) -> bool {
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return false;
        };
        self.prompt_coordination
            .context_discovery
            .frozen_agents
            .contains_key(&agent_id)
            && !self
                .prompt_coordination
                .context_discovery
                .pending_agents
                .contains_key(&agent_id)
    }

    /// Returns whether the durable agent tree has one unfinished foreground
    /// provider tool round on any branch.
    pub(crate) fn agent_has_open_foreground_tool_round(&self, cid: &AgentId) -> bool {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .is_some_and(tau_core::AgentTree::has_open_foreground_tool_round)
    }

    pub(super) fn agent_context_ready_for_loaded_agent(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> bool {
        !self
            .agent_runtime
            .agent_registry
            .session_loaded
            .contains(agent_id)
            || (self
                .prompt_coordination
                .context_discovery
                .frozen_agents
                .contains_key(agent_id)
                && !self
                    .prompt_coordination
                    .context_discovery
                    .pending_agents
                    .contains_key(agent_id))
    }
}

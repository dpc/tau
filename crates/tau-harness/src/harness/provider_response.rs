//! Owns provider terminal normalization, telemetry, recovery, and response
//! continuation.
//!
//! Provider reports remain commit-gated except for the established eager
//! terminal effects preserved by this pipeline.

use super::*;

impl Harness {
    #[cfg(test)]
    pub(super) fn handle_provider_response_finished(
        &mut self,
        response: ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        self.handle_provider_response_finished_from(None, response)
    }

    pub(super) fn handle_provider_response_finished_from(
        &mut self,
        source: Option<&tau_proto::ConnectionId>,
        mut response: ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        // Recovery authorization belongs exclusively to the harness. Provider
        // extensions share this wire type for transport, so discard any value
        // supplied across that trust boundary before evaluating eligibility.
        response.recovery_disposition = tau_proto::ContextRecoveryDisposition::None;
        response.output_length_disposition = tau_proto::OutputLengthDisposition::None;
        response.provider_attempt = tau_proto::ProviderAttempt::ONE;
        response.automatic_compaction_decision = None;
        response.context_limit_telemetry = None;
        response.estimated_api_cost_rates = None;
        response.estimated_api_cost_increment = None;
        let raw_response_contains_tool_calls = response
            .output_items
            .iter()
            .any(|item| matches!(item, ContextItem::ToolCall(_)));
        if self.discard_finished_response_if_canceled(&response.agent_prompt_id) {
            return Ok(());
        }

        let Some(cid) = self.agent_id_for_prompt(&response.agent_prompt_id) else {
            self.emit_duplicate_finished_response_notice(&response.agent_prompt_id);
            return Ok(());
        };
        if !self.assign_finished_response_agent_id(&cid, &mut response) {
            return Ok(());
        }
        let active_compaction_response = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .is_some_and(|agent| {
                matches!(
                    &agent.dispatch.activation_dispatch,
                    crate::agent::ActivationDispatchState::Running {
                        compact_prompt_id: prompt_id,
                        ..
                    } if prompt_id == &response.agent_prompt_id
                )
            });
        if !active_compaction_response
            && self.discard_finished_response_if_stale(&cid, &response, source)
        {
            return Ok(());
        }
        // A tool-bearing response cannot acquire a second foreground round in
        // this AgentTree. Enforce that ownership boundary before attaching
        // telemetry or mutating usage, alerts, provider-watch state, or watcher
        // journals: rejected provider work must have no semantic side effects.
        let standalone_compaction = active_compaction_response
            || self
                .prompt_coordination
                .prompt_runtime
                .operations
                .get(&response.agent_prompt_id)
                .is_some_and(|operation| {
                    operation.0 == tau_proto::PromptOperation::StandaloneCompaction
                });
        let contains_private_compaction_output = response
            .output_items
            .iter()
            .any(|item| matches!(item, ContextItem::LocalCompactionNarrative(_)));
        if contains_private_compaction_output && !active_compaction_response {
            self.emit_harness_failure(
                "rejecting private local-compaction output outside its active standalone transaction",
            );
            response.output_items.clear();
            if standalone_compaction {
                self.prompt_coordination
                    .compaction_runtime
                    .silent_failure_prompts
                    .insert(response.agent_prompt_id.clone());
                self.reject_standalone_compaction(
                    &cid,
                    &response,
                    StandaloneCompactionRejection::InvalidWindow,
                    source,
                );
                self.discard_finished_response_prompt_tracking(&response.agent_prompt_id);
            } else {
                self.terminalize_global_round_rejected_prompt(&cid, &response, source);
            }
            return Ok(());
        }
        if !standalone_compaction
            && raw_response_contains_tool_calls
            && self.agent_has_open_foreground_tool_round(&cid)
        {
            let standalone = self
                .prompt_coordination
                .prompt_runtime
                .operations
                .remove(&response.agent_prompt_id)
                .is_some_and(|operation| {
                    operation.0 == tau_proto::PromptOperation::StandaloneCompaction
                })
                || active_compaction_response;
            self.emit_harness_failure(
                "rejecting provider response: agent tree already has an open foreground tool round",
            );
            if standalone {
                self.prompt_coordination
                    .compaction_runtime
                    .silent_failure_prompts
                    .insert(response.agent_prompt_id.clone());
                self.reject_standalone_compaction(
                    &cid,
                    &response,
                    StandaloneCompactionRejection::InvalidWindow,
                    source,
                );
                self.discard_finished_response_prompt_tracking(&response.agent_prompt_id);
            } else {
                self.terminalize_global_round_rejected_prompt(&cid, &response, source);
            }
            return Ok(());
        }
        if active_compaction_response
            && !self.standalone_compaction_response_matches_current_branch(&cid, &response)
        {
            self.fail_standalone_compaction(
                &cid,
                &response,
                tau_proto::StandaloneCompactionFailureReason::StaleBranch,
                source,
            );
            self.discard_finished_response_prompt_tracking(&response.agent_prompt_id);
            return Ok(());
        }
        let standalone_terminal = standalone_compaction
            .then(|| self.classify_standalone_compaction_terminal(&cid, &response));
        if !standalone_compaction {
            self.clear_malformed_repetition_output(&mut response);
        }
        normalize_finished_response_cached_usage(&mut response);
        let standalone_success = matches!(
            standalone_terminal,
            Some(StandaloneCompactionTerminal::Accepted(_))
        );
        let refresh_success = response.error.is_none()
            && response.failure_kind.is_none()
            && matches!(
                response.stop_reason,
                ProviderStopReason::EndTurn
                    | ProviderStopReason::ToolCalls
                    | ProviderStopReason::Length
            );
        if !standalone_compaction || standalone_success {
            self.provider_runtime.cache_residency.finish_prompt(
                &response.agent_prompt_id,
                refresh_success,
                response.usage.as_ref(),
            );
        }
        let mut tool_calls = tool_calls_from_output_items(&response.output_items);
        let assistant_text = assistant_text_from_output_items(&response.output_items);
        let input_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_sent_tokens);
        let cached_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_cached_tokens);
        let output_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.response_received_tokens);
        let terminal_attempt = self
            .target_agent_id_for_agent(&cid)
            .and_then(|agent_id| {
                self.agent_runtime
                    .agent_watch
                    .provider_status
                    .get(&agent_id)
            })
            .filter(|status| status.agent_prompt_id == response.agent_prompt_id)
            .and_then(|status| match status.state {
                tau_proto::AgentWatchProviderState::Retrying { attempt, .. }
                | tau_proto::AgentWatchProviderState::RecoveringContext { attempt }
                | tau_proto::AgentWatchProviderState::TerminalError { attempt, .. }
                | tau_proto::AgentWatchProviderState::TerminalIncomplete { attempt, .. } => {
                    Some(attempt)
                }
                tau_proto::AgentWatchProviderState::Blocked { .. }
                | tau_proto::AgentWatchProviderState::DispatchUncertain { .. } => None,
            })
            .map_or(1, |attempt| attempt.saturating_add(1));
        response.provider_attempt = tau_proto::ProviderAttempt::new(terminal_attempt)
            .expect("terminal attempt is one-based");
        let terminal_model = self
            .prompt_coordination
            .prompt_runtime
            .models
            .get(&response.agent_prompt_id)
            .cloned();
        self.attach_context_limit_telemetry(&mut response);
        let response_contains_compaction = response
            .output_items
            .iter()
            .any(|item| matches!(item, ContextItem::Compaction(_)));
        let response_owner_is_selected = self
            .agent_runtime
            .agent_registry
            .agents
            .get(&cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .and_then(|tree| {
                tree.marked_inference_through(&response.agent_prompt_id)
                    .map(|through| {
                        tree.is_ancestor_head(
                            through,
                            self.selected_head_for_agent(&cid)
                                .unwrap_or(tau_proto::AgentHead::Root),
                        )
                    })
            })
            .unwrap_or(standalone_compaction);
        if (!standalone_compaction || standalone_success) && response_owner_is_selected {
            self.update_finished_response_context_usage(
                Some(&cid),
                &response.agent_prompt_id,
                input_tokens,
                cached_tokens,
                source,
            );
        }

        let context_size_alerts = (!standalone_compaction || standalone_success)
            .then(|| {
                self.prompt_coordination
                    .prompt_runtime
                    .context_size_alerts
                    .remove(&response.agent_prompt_id)
            })
            .flatten()
            .unwrap_or_default();
        let compaction_policies = self
            .prompt_coordination
            .prompt_runtime
            .compaction_policies
            .remove(&response.agent_prompt_id)
            .unwrap_or_default();
        let reported_input_tokens = input_tokens
            .filter(|tokens| *tokens > 0)
            .map(tau_proto::TokenCount::new);
        let terminal_plan = if !standalone_compaction || standalone_success {
            self.classify_reactive_context_recovery(&cid, &response, source)
        } else {
            ProviderTerminalPlan::Other
        };
        if self.execute_provider_terminal_plan(&cid, &mut response, terminal_plan) {
            return Ok(());
        }
        self.prompt_coordination
            .prompt_runtime
            .semantic_output
            .remove(&response.agent_prompt_id);
        let safe_failure_kind = response.failure_kind.or(response
            .error
            .as_ref()
            .map(|_| tau_proto::ProviderFailureKind::Unknown));
        if (!standalone_compaction || standalone_success)
            && let Some(failure_kind) = safe_failure_kind
            && let Some(public_id) = self.ensure_agent_id_for_agent(&cid)
            && !self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| agent.turn.lifecycle_notification_only_turn)
        {
            let turn_generation = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .map_or(tau_proto::AgentOuterTurnGeneration::initial(), |agent| {
                    agent.turn.turn_generation
                });
            self.update_agent_watch_provider_status(
                &public_id,
                tau_proto::AgentWatchProviderStatusNotification {
                    session_id: self.session_runtime.current_session_id.clone(),
                    subscription_id: String::new(),
                    turn_generation,
                    agent_prompt_id: response.agent_prompt_id.clone(),
                    state: tau_proto::AgentWatchProviderState::TerminalError {
                        failure_kind,
                        attempt: response.provider_attempt.get(),
                    },
                    initial: false,
                },
            );
        } else if (!standalone_compaction || standalone_success)
            && response.error.is_none()
            && let Some(public_id) = self.ensure_agent_id_for_agent(&cid)
        {
            self.agent_runtime
                .agent_watch
                .provider_status
                .remove(&public_id);
        }

        self.attach_finished_response_usage(
            &mut response,
            input_tokens,
            cached_tokens,
            output_tokens,
        );
        self.add_finished_response_estimated_cost(&cid, &mut response, source);
        let prompt_operation = self
            .prompt_coordination
            .prompt_runtime
            .operations
            .remove(&response.agent_prompt_id)
            .unwrap_or_default();
        if prompt_operation.0 == tau_proto::PromptOperation::StandaloneCompaction
            || standalone_compaction
        {
            match standalone_terminal.expect("standalone compaction was classified before mutation")
            {
                StandaloneCompactionTerminal::Accepted(replacement_window) => {
                    self.accept_standalone_compaction(&cid, &response, replacement_window, source);
                }
                StandaloneCompactionTerminal::Rejected(reason) => {
                    self.reject_standalone_compaction(&cid, &response, reason, source);
                }
            }
            return Ok(());
        }
        let (mut requested_tool_calls, tool_calls_with_non_tool_stop) =
            self.reconcile_finished_response_tool_call_stop(&response, &tool_calls);
        // A length-stopped call is incomplete provider output. Preserve it for
        // inspection, but never execute it or use synthetic closure to activate
        // another inference. Suppress before deriving the output-length
        // disposition so the continuation finish bit reflects the actual
        // post-suppression tool continuation.
        if response.stop_reason == ProviderStopReason::Length && requested_tool_calls {
            requested_tool_calls = false;
            tool_calls.clear();
        }
        self.derive_output_length_continuation(
            &cid,
            &mut response,
            prompt_operation.0,
            requested_tool_calls,
        );
        if response_contains_compaction {
            self.attach_finished_response_compaction_usage(&mut response, input_tokens);
        }

        let is_non_tool_ext_query = self.is_non_tool_extension_query(&cid);
        let mut normalized_tool_calls = NormalizedFinishedToolCalls::default();
        if requested_tool_calls {
            normalized_tool_calls = self.normalize_finished_response_tool_calls(
                &mut response,
                &mut tool_calls,
                is_non_tool_ext_query,
                tool_calls_with_non_tool_stop,
            );
            let declaration = tau_proto::ObservationId::random();
            self.tool_routing
                .tool_runtime
                .pending_declaration_observations
                .insert(response.agent_prompt_id.clone(), declaration);
            let item_indices = response
                .output_items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| match item {
                    ContextItem::ToolCall(call) => Some((call.call_id.clone(), index)),
                    _ => None,
                })
                .collect::<HashMap<_, _>>();
            for entry in &mut normalized_tool_calls.calls {
                entry.call.call_ref = item_indices
                    .get(&entry.call.id)
                    .and_then(|index| u32::try_from(*index).ok())
                    .map(|item_index| tau_proto::ToolCallRef {
                        declaration,
                        item_index,
                    });
            }
        }

        let successful = response.error.is_none()
            && response.failure_kind.is_none()
            && !matches!(
                response.stop_reason,
                ProviderStopReason::Length
                    | ProviderStopReason::Error
                    | ProviderStopReason::RepetitionDetected
            );
        let final_status_gate = (!requested_tool_calls)
            .then(|| self.apply_final_status_response_gate(&cid, &response))
            .flatten();
        if !requested_tool_calls
            && !matches!(
                final_status_gate,
                Some(path_crate_agent::FinalStatusDecision::Challenge(_))
            )
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid)
        {
            agent.turn.terminal_notice_eligible = successful;
            agent.turn.terminal_notice_outer_turn_id = agent.turn.outer_turn.owned_id().cloned();
            agent.turn.terminal_context_size_alerts = context_size_alerts.clone();
        }
        let final_status_challenged = matches!(
            final_status_gate,
            Some(path_crate_agent::FinalStatusDecision::Challenge(_))
        );
        if final_status_challenged
            && let tau_proto::OutputLengthDisposition::ContinuationTerminal {
                outer_turn_finish_owed,
                ..
            } = &mut response.output_length_disposition
        {
            *outer_turn_finish_owed = false;
        }
        let continues_for_pending_message_wake = self
            .finished_side_conversation_continues_for_pending_message_wake(
                &cid,
                &response,
                requested_tool_calls,
                is_non_tool_ext_query,
            );
        let eager_decision_eligible = !final_status_challenged
            && !requested_tool_calls
            && !response_contains_compaction
            && !continues_for_pending_message_wake
            && response.failure_kind != Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
            && response.recovery_disposition == tau_proto::ContextRecoveryDisposition::None
            && !matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
            );
        if eager_decision_eligible {
            response.automatic_compaction_decision = response
                .usage
                .as_ref()
                .and_then(|usage| usage.model.clone())
                .or(terminal_model)
                .and_then(|model| {
                    self.eager_automatic_compaction_decision(
                        &cid,
                        model,
                        reported_input_tokens,
                        Some(response.agent_prompt_id.clone()),
                        &compaction_policies,
                    )
                });
        }
        let final_status_gated = final_status_gate.is_some();
        let completion = match final_status_gate {
            Some(path_crate_agent::FinalStatusDecision::Challenge(challenge)) => {
                Some(AgentPublishCompletion::GatedFinal {
                    batch_parent: self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(&cid)
                        .and_then(|agent| agent.identity.head)
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                    disposition: GatedFinalDisposition::Challenge { challenge },
                    retry_event: None,
                })
            }
            Some(path_crate_agent::FinalStatusDecision::Accept) => {
                Some(AgentPublishCompletion::GatedFinal {
                    batch_parent: self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(&cid)
                        .and_then(|agent| agent.identity.head)
                        .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                    disposition: GatedFinalDisposition::Accept {
                        terminal: Box::new(CommittedGatedFinal {
                            response: response.clone(),
                            response_contains_compaction,
                            input_tokens,
                            context_size_alerts: context_size_alerts.clone(),
                            is_non_tool_ext_query,
                            source: source.cloned(),
                            tool_effect: CommittedOutputLengthToolEffect::None,
                        }),
                    },
                    retry_event: None,
                })
            }
            None => None,
        };
        let eager_terminal_owned = response.automatic_compaction_decision.is_some();
        let commit_gated_terminal = eager_terminal_owned || continues_for_pending_message_wake;
        let completion = if commit_gated_terminal && completion.is_none() {
            Some(AgentPublishCompletion::GatedFinal {
                batch_parent: self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|agent| agent.identity.head)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                disposition: GatedFinalDisposition::Accept {
                    terminal: Box::new(CommittedGatedFinal {
                        response: response.clone(),
                        response_contains_compaction,
                        input_tokens,
                        context_size_alerts: context_size_alerts.clone(),
                        is_non_tool_ext_query,
                        source: source.cloned(),
                        tool_effect: if requested_tool_calls {
                            CommittedOutputLengthToolEffect::Dispatch(normalized_tool_calls.clone())
                        } else {
                            CommittedOutputLengthToolEffect::None
                        },
                    }),
                },
                retry_event: None,
            })
        } else {
            completion
        };
        let completion = if matches!(
            response.output_length_disposition,
            tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
        ) {
            Some(AgentPublishCompletion::OutputLengthContinuation {
                batch_parent: self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|agent| agent.identity.head)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                response: Box::new(response.clone()),
                assistant_text: assistant_text.clone(),
                retry_event: None,
            })
        } else {
            completion
        };
        let output_length_terminal = matches!(
            response.output_length_disposition,
            tau_proto::OutputLengthDisposition::ContinuationTerminal { .. }
        );
        let completion = if output_length_terminal && !final_status_challenged {
            Some(AgentPublishCompletion::GatedFinal {
                batch_parent: self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|agent| agent.identity.head)
                    .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                disposition: GatedFinalDisposition::Accept {
                    terminal: Box::new(CommittedGatedFinal {
                        response: response.clone(),
                        response_contains_compaction,
                        input_tokens,
                        context_size_alerts: context_size_alerts.clone(),
                        is_non_tool_ext_query,
                        source: source.cloned(),
                        tool_effect: if requested_tool_calls {
                            CommittedOutputLengthToolEffect::Dispatch(normalized_tool_calls.clone())
                        } else {
                            CommittedOutputLengthToolEffect::None
                        },
                    }),
                },
                retry_event: None,
            })
        } else {
            completion
        };
        let notify_watchers_after_commit = completion.is_none()
            && !requested_tool_calls
            && !matches!(
                response.originator,
                tau_proto::PromptOriginator::Extension { .. }
            )
            && self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| !agent.turn.lifecycle_notification_only_turn)
            && successful
            && assistant_text.is_some();
        self.publish_finished_response_for_agent(
            &cid,
            source,
            &response,
            completion,
            notify_watchers_after_commit,
        );
        if !requested_tool_calls {
            self.clear_prompt_tool_snapshot(&response.agent_prompt_id);
        }
        if final_status_gated
            || commit_gated_terminal
            || matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
            )
            || output_length_terminal
        {
            return Ok(());
        }
        if response_contains_compaction {
            self.clear_agent_context_usage(&cid);
        } else if successful {
            self.queue_crossed_context_size_alerts_for_prompt(
                &cid,
                &response.agent_prompt_id,
                input_tokens,
                &context_size_alerts,
            );
        }
        if response.recovery_disposition
            != tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
            && self.handle_finished_response_side_conversation(
                &cid,
                FinishedSideConversation {
                    response: &response,
                    requested_tool_calls,
                    is_non_tool_ext_query,
                    assistant_text: assistant_text.as_deref(),
                    tool_call_count: tool_calls.len(),
                },
                &mut normalized_tool_calls,
                source,
            )
        {
            return Ok(());
        }
        if requested_tool_calls {
            self.dispatch_finished_response_tool_calls(&cid, normalized_tool_calls, source)?;
        } else {
            self.complete_finished_response_without_tool_calls(
                &cid,
                &response,
                assistant_text.as_deref(),
            );
        }

        Ok(())
    }

    /// Captures immutable, content-free native context-limit evidence
    /// immediately before provider dispatch.
    pub(super) fn prompt_context_limit_snapshot(
        &self,
        cid: &AgentId,
        model: &ModelId,
        operation: tau_proto::PromptOperation,
    ) -> PromptContextLimitSnapshot {
        let advertised_context_window = self
            .provider_runtime
            .model_info
            .get(model)
            .map(|info| info.context_window)
            .filter(|window| *window > tau_proto::TokenCount::ZERO);
        let transcript_delta_bytes = self.agent_runtime.agent_registry.agents.get(cid).map_or(
            Some(tau_proto::ByteCount::ZERO),
            |agent| {
                self.transcript_growth_since(
                    agent.identity.agent_id.as_deref(),
                    agent.identity.head,
                    agent.execution.context_usage_head,
                )
                .serialized_bytes
            },
        );
        let role_compaction = self
            .config
            .available_roles
            .get(&self.role_name_for_agent_id(cid))
            .and_then(|role| role.inference_compaction.or(role.compaction))
            .unwrap_or(path_tau_config_settings::RoleCompaction::ProviderDefault);
        let (compaction_threshold, compaction_policy) = match role_compaction {
            path_tau_config_settings::RoleCompaction::Threshold(value) => (
                Some(tau_proto::TokenCount::new(value)),
                tau_proto::ContextLimitCompactionPolicy::Threshold,
            ),
            path_tau_config_settings::RoleCompaction::ProviderDefault => (
                self.provider_runtime
                    .model_info
                    .get(model)
                    .and_then(|info| info.standalone_compaction_threshold),
                tau_proto::ContextLimitCompactionPolicy::ProviderDefault,
            ),
            path_tau_config_settings::RoleCompaction::Disabled => {
                (None, tau_proto::ContextLimitCompactionPolicy::Disabled)
            }
        };
        PromptContextLimitSnapshot {
            model: model.clone(),
            operation,
            transcript_delta_bytes,
            advertised_context_window,
            compaction_threshold,
            compaction_policy,
        }
    }

    pub(super) fn transcript_growth_since(
        &self,
        agent_id: Option<&str>,
        head: Option<NodeId>,
        usage_head: Option<NodeId>,
    ) -> TranscriptGrowth {
        agent_id
            .and_then(|id| self.session_runtime.agent_store.agent(id))
            .map_or(
                TranscriptGrowth {
                    serialized_bytes: Some(tau_proto::ByteCount::ZERO),
                },
                |tree| {
                    let ids = tree.branch_node_ids_from(head);
                    let first = usage_head
                        .and_then(|baseline| ids.iter().position(|id| *id == baseline))
                        .map_or(0, |index| index.saturating_add(1));
                    let entries = ids[first..]
                        .iter()
                        .filter_map(|id| tree.node(*id))
                        .map(|node| &node.entry)
                        .collect::<Vec<_>>();
                    TranscriptGrowth {
                        serialized_bytes: transcript_growth(entries).serialized_bytes,
                    }
                },
            )
    }

    pub(super) fn attach_context_limit_telemetry(
        &mut self,
        response: &mut ProviderResponseFinished,
    ) {
        let snapshot = self
            .prompt_coordination
            .prompt_runtime
            .context_limits
            .remove(&response.agent_prompt_id);
        if response.failure_kind != Some(tau_proto::ProviderFailureKind::ContextWindowExceeded) {
            return;
        }
        let Some(snapshot) = snapshot else {
            return;
        };
        let provider_input_tokens = response
            .usage
            .as_ref()
            .map(|usage| tau_proto::TokenCount::new(usage.prompt_sent_tokens));
        let observation =
            context_limit_observation(provider_input_tokens, snapshot.advertised_context_window);
        response.context_limit_telemetry = Some(tau_proto::ContextLimitTelemetry {
            model: snapshot.model,
            operation: snapshot.operation,
            transcript_delta_bytes: snapshot.transcript_delta_bytes,
            advertised_context_window: snapshot.advertised_context_window,
            provider_input_tokens,
            compaction_threshold: snapshot.compaction_threshold,
            compaction_policy: snapshot.compaction_policy,
            recovery_eligible: false,
            action: tau_proto::ContextLimitAction::Terminal,
            observation,
        });
    }

    /// Classify one terminal exhaustively at the reactive-recovery family
    /// boundary without applying terminal effects.
    pub(super) fn classify_reactive_context_recovery(
        &self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        source: Option<&tau_proto::ConnectionId>,
    ) -> ProviderTerminalPlan {
        if response.failure_kind != Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
            || response.stop_reason != ProviderStopReason::Error
            || !response.output_items.is_empty()
            || self
                .prompt_coordination
                .prompt_runtime
                .semantic_output
                .contains(&response.agent_prompt_id)
            || self
                .prompt_coordination
                .prompt_runtime
                .operations
                .get(&response.agent_prompt_id)
                .map(|operation| operation.0)
                != Some(tau_proto::PromptOperation::Inference)
        {
            return ProviderTerminalPlan::Other;
        }
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return ProviderTerminalPlan::Other;
        };
        let Some(tree) = self.session_runtime.agent_store.agent(agent_id.as_str()) else {
            return ProviderTerminalPlan::Other;
        };
        let Some(tau_core::InferenceDispatchRecovery::DispatchUncertain(checkpoint)) =
            tree.inference_dispatch_recovery()
        else {
            return ProviderTerminalPlan::Other;
        };
        let Some(model) = checkpoint.model.clone() else {
            return ProviderTerminalPlan::Other;
        };
        let current_head = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.head)
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let selected_or_continuation_model_matches = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| {
                self.model_for_agent_role(agent).as_ref() == Some(&model)
                    || agent
                        .turn
                        .output_length_continuation
                        .owns_prompt_model(&response.agent_prompt_id, &model)
            });
        if checkpoint.activation_cut.is_none() {
            return ProviderTerminalPlan::Other;
        }
        if checkpoint.transaction_id.is_some()
            || checkpoint.operation != Some(tau_proto::PromptOperation::Inference)
            || checkpoint.agent_prompt_id != response.agent_prompt_id
            || self
                .prompt_coordination
                .prompt_runtime
                .models
                .get(&response.agent_prompt_id)
                != Some(&model)
            || !selected_or_continuation_model_matches
            || !tree.is_ancestor_head(checkpoint.through, current_head)
            || self.durable_recovery_blocks_automatic(agent_id.as_str(), &model, current_head)
            || !self
                .provider_runtime
                .model_info
                .get(&model)
                .is_some_and(|info| info.supports_standalone_compaction)
        {
            return ProviderTerminalPlan::Other;
        }
        let role_name = self.role_name_for_agent_id(cid);
        if self
            .config
            .available_roles
            .get(&role_name)
            .and_then(|role| role.inference_compaction.or(role.compaction))
            == Some(path_tau_config_settings::RoleCompaction::Disabled)
        {
            return ProviderTerminalPlan::Other;
        }

        ProviderTerminalPlan::ReactiveContextRecovery(Box::new(ReactiveContextRecoveryPlan {
            checkpoint,
            source: source.cloned(),
        }))
    }

    /// Execute the narrow eager bundle and enqueue the exact canonical
    /// candidate for one classified provider-terminal plan.
    pub(super) fn execute_provider_terminal_plan(
        &mut self,
        cid: &AgentId,
        response: &mut ProviderResponseFinished,
        plan: ProviderTerminalPlan,
    ) -> bool {
        let plan = match plan {
            ProviderTerminalPlan::ReactiveContextRecovery(plan) => plan,
            ProviderTerminalPlan::Other => return false,
        };
        let ReactiveContextRecoveryPlan { checkpoint, source } = *plan;
        response.recovery_disposition =
            tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned;
        if let Some(telemetry) = response.context_limit_telemetry.as_mut() {
            telemetry.recovery_eligible = true;
            telemetry.action = tau_proto::ContextLimitAction::ReactiveCompactionPlanned;
        }
        let input_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_sent_tokens);
        let cached_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_cached_tokens);
        let output_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.response_received_tokens);
        self.attach_finished_response_usage(response, input_tokens, cached_tokens, output_tokens);
        self.add_finished_response_estimated_cost(cid, response, source.as_ref());
        self.discard_finished_response_prompt_tracking(&response.agent_prompt_id);
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid)
            && agent.dispatch.in_flight_prompt.as_ref() == Some(&response.agent_prompt_id)
        {
            agent.dispatch.in_flight_prompt = None;
        }
        self.publish_event_for_agent_with_completion(
            cid,
            source.as_ref(),
            Event::ProviderResponseFinished(response.clone()),
            Some(AgentPublishCompletion::ReactiveContextRecovery {
                reducer: CommittedReactiveContextRecovery {
                    checkpoint,
                    source: source.clone(),
                },
                retry_event: None,
            }),
            false,
        );
        true
    }

    /// Reconciles durable planned recoveries after provider discovery makes
    /// model capability authoritative.
    pub(super) fn reconcile_pending_context_recoveries(&mut self, absence_is_authoritative: bool) {
        let pending = self
            .agent_runtime
            .agent_registry
            .agents
            .iter()
            .filter_map(|(cid, agent)| match &agent.dispatch.activation_dispatch {
                path_crate_agent::ActivationDispatchState::ContextRecoveryPending {
                    checkpoint,
                } => Some((cid.clone(), checkpoint.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (cid, checkpoint) in pending {
            let Some(model) = checkpoint.model.as_ref() else {
                self.terminalize_replay_blocked_context_recovery(
                    &cid,
                    &checkpoint,
                    tau_proto::StandaloneCompactionFailureReason::StaleBranch,
                );
                continue;
            };
            if !self.provider_runtime.model_info.contains_key(model) && !absence_is_authoritative {
                continue;
            }
            let capability_matches = self
                .provider_runtime
                .model_info
                .get(model)
                .is_some_and(|info| info.supports_standalone_compaction);
            let selected_or_continuation_model_matches = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .is_some_and(|agent| {
                    self.model_for_agent_role(agent).as_ref() == Some(model)
                        || agent
                            .turn
                            .output_length_continuation
                            .owns_prompt_model(&checkpoint.agent_prompt_id, model)
                });
            let policy_allows = self
                .config
                .available_roles
                .get(&self.role_name_for_agent_id(&cid))
                .and_then(|role| role.inference_compaction.or(role.compaction))
                != Some(path_tau_config_settings::RoleCompaction::Disabled);
            let branch_matches = checkpoint.activation_cut.is_some()
                && self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(&cid)
                    .and_then(|agent| agent.identity.agent_id.as_deref())
                    .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                    .is_some_and(|tree| {
                        tree.is_ancestor_head(
                            checkpoint.through,
                            self.agent_runtime
                                .agent_registry
                                .agents
                                .get(&cid)
                                .and_then(|agent| agent.identity.head)
                                .map_or(AgentHead::Root, AgentHead::Node),
                        )
                    });
            let matching_failure_suppresses = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .and_then(|agent| {
                    let agent_id = agent.identity.agent_id.as_deref()?;
                    let current_head = agent.identity.head.map_or(AgentHead::Root, AgentHead::Node);
                    Some(self.durable_recovery_blocks_automatic(agent_id, model, current_head))
                })
                .unwrap_or(true);
            if selected_or_continuation_model_matches
                && capability_matches
                && policy_allows
                && branch_matches
                && !matching_failure_suppresses
            {
                self.start_reactive_compaction_for_checkpoint(&cid, &checkpoint, None);
            } else {
                self.terminalize_replay_blocked_context_recovery(
                    &cid,
                    &checkpoint,
                    tau_proto::StandaloneCompactionFailureReason::StaleBranch,
                );
            }
        }
    }

    /// Claims and categorically fails an unclaimed recovery without dispatching
    /// remote work when replay-time authority checks no longer match.
    pub(super) fn terminalize_replay_blocked_context_recovery(
        &mut self,
        cid: &AgentId,
        checkpoint: &tau_proto::AgentInferenceDispatchStarted,
        reason: tau_proto::StandaloneCompactionFailureReason,
    ) {
        let Some((agent_id, model, cut, originator, next)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| {
                Some((
                    agent.identity.agent_id.clone()?,
                    checkpoint.model.clone()?,
                    checkpoint.activation_cut?,
                    agent.identity.originator.clone(),
                    agent.dispatch.next_prompt_index,
                ))
            })
        else {
            return;
        };
        let transaction_id = tau_proto::CompactionTransactionId::parse(format!("ct-{next}"))
            .expect("generated compaction transaction id is valid");
        let compact_prompt_id = tau_proto::AgentPromptId::parse(format!("ap-{agent_id}-{next}"))
            .expect("known-safe AgentPromptId must be valid");
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
        }
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.activation_dispatch =
                path_crate_agent::ActivationDispatchState::ContextRecoveryClaimPending {
                    checkpoint: checkpoint.clone(),
                    transaction_id: transaction_id.clone(),
                };
        }
        self.prompt_coordination
            .compaction_runtime
            .suppress_start_for_queued_terminal(
                crate::parse_agent_id(&agent_id),
                transaction_id.clone(),
            );
        let failure = tau_proto::AgentStandaloneCompactionFailed {
            agent_id: crate::parse_agent_id(&agent_id),
            transaction_id: transaction_id.clone(),
            cut,
            reason,
            resume_through: Some(checkpoint.through),
            context_retreat: None,
        };
        self.publish_event_for_agent_with_completion(
            cid,
            None,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: crate::parse_agent_id(&agent_id),
                transaction_id: transaction_id.clone(),
                compact_prompt_id,
                cut,
                resume_through: Some(checkpoint.through),
                model,
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator,
                supersedes: None,
                trigger: tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                    failed_agent_prompt_id: checkpoint.agent_prompt_id.clone(),
                },
            }),
            Some(AgentPublishCompletion::ReactiveContextRecoveryStart {
                checkpoint: checkpoint.clone(),
                failure_after_commit: Some(Box::new(failure)),
                retry_event: None,
            }),
            false,
        );
        self.emit_info_important(&format!(
            "context recovery for restored agent `{cid}` is blocked by changed model, capability, policy, or branch; retry explicitly"
        ));
    }

    /// Publishes the unique durable compaction claim for a planned recovery.
    pub(super) fn start_reactive_compaction_for_checkpoint(
        &mut self,
        cid: &AgentId,
        checkpoint: &tau_proto::AgentInferenceDispatchStarted,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let Some(model) = checkpoint.model.clone() else {
            return;
        };
        let Some(activation_cut) = checkpoint.activation_cut else {
            return;
        };
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return;
        };
        let next = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map_or(0, |agent| agent.dispatch.next_prompt_index);
        let provisional_cut = self
            .session_runtime
            .agent_store
            .agent(agent_id.as_str())
            .and_then(|tree| tree.reactive_compaction_target(&checkpoint.agent_prompt_id))
            .unwrap_or(activation_cut);
        let prefix_budget = self
            .provider_runtime
            .model_info
            .get(&model)
            .and_then(|info| info.standalone_compaction_prefix_budget);
        let fitting_cut = if provisional_cut == tau_proto::AgentHead::Root {
            // Reactive recovery retains the established root-cut transaction:
            // the activating input remains exact suffix and the compact request
            // contains only fixed provider/system surface. Later inference may
            // still reject one oversized indivisible activating item.
            Some(tau_proto::AgentHead::Root)
        } else if let Some(prefix_budget) = prefix_budget {
            self.fitting_automatic_compaction_cut(&agent_id, provisional_cut, None, prefix_budget)
        } else {
            Some(provisional_cut)
        };
        let cut = fitting_cut.unwrap_or(provisional_cut);
        let transaction_id = tau_proto::CompactionTransactionId::parse(format!("ct-{next}"))
            .expect("generated compaction transaction id is valid");
        let compact_prompt_id = tau_proto::AgentPromptId::parse(format!("ap-{agent_id}-{next}"))
            .expect("known-safe AgentPromptId must be valid");
        let originator = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map_or_else(PromptOriginator::default, |agent| {
                agent.identity.originator.clone()
            });
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
            agent.dispatch.activation_dispatch =
                path_crate_agent::ActivationDispatchState::ContextRecoveryClaimPending {
                    checkpoint: checkpoint.clone(),
                    transaction_id: transaction_id.clone(),
                };
        }
        let trigger = fitting_cut.map_or_else(
            || tau_proto::StandaloneCompactionTrigger::ReactivePreflightFailure {
                failed_agent_prompt_id: checkpoint.agent_prompt_id.clone(),
                reason: tau_proto::StandaloneCompactionFailureReason::PrefixTooLarge,
            },
            |_| tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                failed_agent_prompt_id: checkpoint.agent_prompt_id.clone(),
            },
        );
        self.publish_event_for_agent_with_completion(
            cid,
            source,
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: crate::parse_agent_id(&agent_id),
                transaction_id: transaction_id.clone(),
                compact_prompt_id,
                cut,
                resume_through: Some(checkpoint.through),
                model,
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator,
                supersedes: None,
                trigger,
            }),
            Some(AgentPublishCompletion::ReactiveContextRecoveryStart {
                checkpoint: checkpoint.clone(),
                failure_after_commit: None,
                retry_event: None,
            }),
            false,
        );
    }

    /// Append the exact retreat successor pre-minted by a committed typed
    /// context rejection. The retained completion retries the same immutable
    /// event after append rejection; replay calls this only while the successor
    /// remains absent from the core projection.
    pub(super) fn start_context_retreat_from_plan(
        &mut self,
        cid: &AgentId,
        failed: &tau_proto::AgentStandaloneCompactionFailed,
        plan: tau_proto::ContextRetreatPlan,
    ) {
        let event =
            Event::AgentStandaloneCompactionStarted(tau_proto::AgentStandaloneCompactionStarted {
                agent_id: failed.agent_id.clone(),
                transaction_id: plan.transaction_id,
                compact_prompt_id: plan.compact_prompt_id,
                cut: plan.cut,
                resume_through: plan.resume_through,
                model: plan.model,
                operation: tau_proto::PromptOperation::StandaloneCompaction,
                originator: plan.originator,
                supersedes: Some(failed.transaction_id.clone()),
                trigger: tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
                    failed_transaction_id: failed.transaction_id.clone(),
                    roll_through: plan.roll_through,
                },
            });
        self.publish_event_for_agent_with_completion(
            cid,
            None,
            event,
            Some(AgentPublishCompletion::RollingCompactionStart { retry_event: None }),
            false,
        );
    }

    /// Classify a standalone terminal before it changes any context or cache
    /// state, including the complete durable-boundary validation.
    pub(super) fn classify_standalone_compaction_terminal(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
    ) -> StandaloneCompactionTerminal {
        if response.error.is_some() || response.failure_kind.is_some() {
            if response.failure_kind == Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
                && response.stop_reason == ProviderStopReason::Error
                && response.output_items.is_empty()
            {
                return StandaloneCompactionTerminal::Rejected(
                    StandaloneCompactionRejection::ContextWindowExceeded,
                );
            }
            return StandaloneCompactionTerminal::Rejected(
                StandaloneCompactionRejection::ProviderError,
            );
        }
        if response.stop_reason != ProviderStopReason::EndTurn {
            return StandaloneCompactionTerminal::Rejected(
                StandaloneCompactionRejection::InvalidStop,
            );
        }
        let replacement_window = match self.local_summary_compaction_window(&response.output_items)
        {
            Ok(Some(window)) => window,
            Ok(None) => {
                let Ok(window) =
                    tau_proto::ValidatedCompactionWindow::new(response.output_items.clone())
                else {
                    return StandaloneCompactionTerminal::Rejected(
                        StandaloneCompactionRejection::InvalidWindow,
                    );
                };
                window
            }
            Err(()) => {
                return StandaloneCompactionTerminal::Rejected(
                    StandaloneCompactionRejection::InvalidWindow,
                );
            }
        };
        if !self.standalone_compaction_boundary_is_valid(cid, response, &replacement_window) {
            return StandaloneCompactionTerminal::Rejected(
                StandaloneCompactionRejection::InvalidWindow,
            );
        }
        StandaloneCompactionTerminal::Accepted(replacement_window)
    }

    /// Converts a local narrative into its exact synthetic user checkpoint.
    /// Other providers retain their exact output window unchanged.
    pub(super) fn local_summary_compaction_window(
        &self,
        output_items: &[ContextItem],
    ) -> Result<Option<tau_proto::ValidatedCompactionWindow>, ()> {
        compaction_supplement::compose(output_items)
    }

    /// Checks the complete core boundary contract without appending it.
    pub(super) fn standalone_compaction_boundary_is_valid(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        replacement_window: &tau_proto::ValidatedCompactionWindow,
    ) -> bool {
        let Some((agent_id, parent, boundary)) =
            self.standalone_compaction_boundary(cid, response, replacement_window)
        else {
            return false;
        };
        self.session_runtime
            .agent_store
            .validate_agent_event_at(
                &agent_id,
                None,
                parent,
                &boundary,
                tau_proto::UnixMicros::now(),
            )
            .is_ok()
    }

    /// Materializes the exact boundary that a validated standalone terminal
    /// would append at the current agent head.
    pub(super) fn standalone_compaction_boundary(
        &self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        replacement_window: &tau_proto::ValidatedCompactionWindow,
    ) -> Option<(String, tau_core::AgentEventParent, Event)> {
        let agent = self.agent_runtime.agent_registry.agents.get(cid)?;
        let (transaction_id, cut, model, compact_prompt_id) =
            match &agent.dispatch.activation_dispatch {
                path_crate_agent::ActivationDispatchState::Running {
                    id,
                    cut,
                    model,
                    compact_prompt_id,
                    ..
                } => (id.clone(), *cut, model.clone(), compact_prompt_id.clone()),
                _ => return None,
            };
        let agent_id = agent.identity.agent_id.clone()?;
        let suffix_end = agent
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let parent = agent.identity.head.map_or(
            tau_core::AgentEventParent::Root,
            tau_core::AgentEventParent::Under,
        );
        Some((
            agent_id,
            parent,
            Event::AgentCompacted(tau_proto::AgentCompacted {
                original_input_tokens: response
                    .usage
                    .as_ref()
                    .map(|usage| tau_proto::TokenCount::new(usage.prompt_sent_tokens)),
                compaction_output_tokens: response
                    .usage
                    .as_ref()
                    .map(|usage| usage.response_received_tokens)
                    .map(tau_proto::TokenCount::new),
                compact_prompt_id: Some(compact_prompt_id),
                model: Some(model),
                operation: Some(tau_proto::PromptOperation::StandaloneCompaction),
                agent_id: response.agent_id.clone(),
                transaction_id: Some(transaction_id),
                cut: Some(cut),
                suffix_end: Some(suffix_end),
                replacement_window: replacement_window.items().to_vec(),
            }),
        ))
    }

    pub(super) fn accept_standalone_compaction(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        replacement_window: tau_proto::ValidatedCompactionWindow,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let Some((_, parent, boundary)) =
            self.standalone_compaction_boundary(cid, response, &replacement_window)
        else {
            self.emit_info("ignoring standalone compaction response without an active transaction");
            return;
        };
        if !self.standalone_compaction_response_matches_current_branch(cid, response) {
            self.fail_standalone_compaction(
                cid,
                response,
                tau_proto::StandaloneCompactionFailureReason::StaleBranch,
                source,
            );
            return;
        }
        self.publish_event_for_agent_with_completion(
            cid,
            source,
            boundary,
            Some(AgentPublishCompletion::OwedCompactionFact {
                batch_parent: match parent {
                    tau_core::AgentEventParent::Root => tau_proto::AgentHead::Root,
                    tau_core::AgentEventParent::Under(node) => tau_proto::AgentHead::Node(node),
                    tau_core::AgentEventParent::InheritHead => {
                        unreachable!("standalone boundary always captures an explicit parent")
                    }
                },
                retry_event: None,
            }),
            false,
        );
    }

    pub(super) fn standalone_compaction_response_matches_current_branch(
        &self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
    ) -> bool {
        let Some((resume_through, model, branch_generation, compact_prompt_id)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| match &agent.dispatch.activation_dispatch {
                path_crate_agent::ActivationDispatchState::Running {
                    resume_through,
                    model,
                    branch_generation,
                    compact_prompt_id,
                    ..
                } => Some((
                    *resume_through,
                    model,
                    *branch_generation,
                    compact_prompt_id,
                )),
                _ => None,
            })
        else {
            return false;
        };
        if compact_prompt_id != &response.agent_prompt_id {
            return false;
        }
        let suffix_head = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.head);
        let branch_matches = resume_through.is_none_or(|resume| {
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.identity.agent_id.as_deref())
                .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
                .is_some_and(|tree| {
                    tree.is_ancestor_head(
                        resume,
                        suffix_head.map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node),
                    )
                })
        });
        let operation_matches = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| self.model_for_agent_role(agent))
            .is_some_and(|prompt_model| prompt_model == *model);
        let branch_generation_matches = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| agent.identity.branch_generation == branch_generation);
        branch_matches && branch_generation_matches && operation_matches
    }

    pub(super) fn reject_standalone_compaction(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        rejection: StandaloneCompactionRejection,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        match rejection {
            StandaloneCompactionRejection::ProviderError => self.emit_info(&format!(
                "provider failed standalone compaction for agent_prompt_id={}",
                response.agent_prompt_id
            )),
            StandaloneCompactionRejection::ContextWindowExceeded => self.emit_info(&format!(
                "provider context-rejected standalone compaction for agent_prompt_id={}",
                response.agent_prompt_id
            )),
            StandaloneCompactionRejection::InvalidStop => self.emit_info(&format!(
                "provider returned a non-terminal standalone compaction stop for agent_prompt_id={}",
                response.agent_prompt_id
            )),
            StandaloneCompactionRejection::InvalidWindow => self.emit_info(&format!(
                "provider returned an invalid standalone compaction window for agent_prompt_id={}",
                response.agent_prompt_id
            )),
        }
        if matches!(
            rejection,
            StandaloneCompactionRejection::ContextWindowExceeded
        ) {
            self.publish_event_for_agent_with_completion(
                cid,
                source,
                Event::ProviderResponseFinished(response.clone()),
                Some(AgentPublishCompletion::StandaloneContextRejection {
                    response: Box::new(response.clone()),
                    source: source.cloned(),
                    retry_event: None,
                }),
                false,
            );
            return;
        }
        self.fail_standalone_compaction(cid, response, rejection.durable_reason(), source);
    }

    pub(super) fn fail_standalone_compaction(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        reason: tau_proto::StandaloneCompactionFailureReason,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let transaction = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| match &agent.dispatch.activation_dispatch {
                path_crate_agent::ActivationDispatchState::Running {
                    id,
                    cut,
                    resume_through,
                    compact_prompt_id,
                    ..
                } if compact_prompt_id == &response.agent_prompt_id => {
                    Some((id.clone(), *cut, *resume_through))
                }
                _ => None,
            });
        let Some((transaction_id, cut, resume_through)) = transaction else {
            return;
        };
        let automatic_context_recovery = self
            .session_runtime
            .agent_store
            .agent(response.agent_id.as_str())
            .and_then(|tree| match tree.standalone_compaction_recovery() {
                Some(tau_core::StandaloneCompactionRecovery::Interrupted(started))
                | Some(tau_core::StandaloneCompactionRecovery::RejectedAwaitingFailure {
                    started,
                    ..
                }) if started.transaction_id == transaction_id => Some(matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                        | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                        | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                        | tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat { .. }
                        | tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                )),
                _ => None,
            })
            .unwrap_or(false);
        let automatic_context_irreducible = automatic_context_recovery
            && self
                .previous_useful_compaction_cut(
                    response.agent_id.as_str(),
                    self.selected_head_for_agent(cid).unwrap_or(cut),
                    cut,
                )
                .is_none();
        let retreat_plan = if reason
            == tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
        {
            let agent_id = response.agent_id.as_str();
            let started = self
                .session_runtime
                .agent_store
                .agent(agent_id)
                .and_then(|tree| match tree.standalone_compaction_recovery() {
                    Some(tau_core::StandaloneCompactionRecovery::Interrupted(started))
                    | Some(tau_core::StandaloneCompactionRecovery::RejectedAwaitingFailure {
                        started,
                        ..
                    }) if started.transaction_id == transaction_id => Some(started),
                    _ => None,
                });
            started.and_then(|started| {
                let automatic = matches!(
                    started.trigger,
                    tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence { .. }
                        | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. }
                        | tau_proto::StandaloneCompactionTrigger::AutomaticContinuation { .. }
                        | tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat { .. }
                        | tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow { .. }
                );
                automatic.then_some(started).and_then(|started| {
                    let active_head = self.selected_head_for_agent(cid).unwrap_or(started.cut);
                    let predecessor =
                        self.previous_useful_compaction_cut(agent_id, active_head, started.cut)?;
                    let next = self
                        .agent_runtime
                        .agent_registry
                        .agents
                        .get(cid)?
                        .dispatch
                        .next_prompt_index;
                    let transaction_id =
                        tau_proto::CompactionTransactionId::parse(format!("ct-{next}")).ok()?;
                    let compact_prompt_id =
                        tau_proto::AgentPromptId::parse(format!("ap-{agent_id}-{next}")).ok()?;
                    Some(tau_proto::ContextRetreatPlan {
                        transaction_id,
                        compact_prompt_id,
                        cut: predecessor,
                        roll_through: match &started.trigger {
                            tau_proto::StandaloneCompactionTrigger::AutomaticThresholdEvidence {
                                ..
                            }
                            | tau_proto::StandaloneCompactionTrigger::AutomaticPolicy { .. } => {
                                started.cut
                            }
                            tau_proto::StandaloneCompactionTrigger::AutomaticContextRetreat {
                                roll_through,
                                ..
                            } => *roll_through,
                            tau_proto::StandaloneCompactionTrigger::AutomaticContinuation {
                                previous_transaction_id,
                            } => self
                                .session_runtime
                                .agent_store
                                .agent(agent_id)?
                                .reactive_compaction_progress(previous_transaction_id)
                                .and_then(|progress| match progress {
                                    tau_core::ReactiveCompactionProgress::NeedsContinuation {
                                        target_cut,
                                    } => Some(target_cut),
                                    tau_core::ReactiveCompactionProgress::ReachedTargetCut => None,
                                })?,
                            tau_proto::StandaloneCompactionTrigger::ReactiveContextOverflow {
                                failed_agent_prompt_id,
                            } => self
                                .session_runtime
                                .agent_store
                                .agent(agent_id)?
                                .reactive_compaction_target(failed_agent_prompt_id)?,
                            _ => return None,
                        },
                        model: started.model,
                        originator: started.originator,
                        resume_through: started.resume_through,
                    })
                })
            })
        } else {
            None
        };
        if retreat_plan.is_some()
            && let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid)
        {
            agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
        }
        // Rejection retains neither cache evidence nor context-recovery authority,
        // but it must release the prompt-local snapshots allocated for dispatch.
        self.provider_runtime
            .cache_residency
            .drop_prompt(&response.agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .context_size_alerts
            .remove(&response.agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .compaction_policies
            .remove(&response.agent_prompt_id);
        self.clear_finished_response_prompt_route(&response.agent_prompt_id);
        self.clear_prompt_tool_snapshot(&response.agent_prompt_id);
        self.emit_info_important(&format!(
            "standalone compaction failed for agent `{cid}` ({reason:?}); retry with :compact, switch model/role, or rewind"
        ));
        let batch_parent = self
            .selected_head_for_agent(cid)
            .unwrap_or(tau_proto::AgentHead::Root);
        self.publish_event_for_agent_with_completion(
            cid,
            source,
            Event::AgentStandaloneCompactionFailed(tau_proto::AgentStandaloneCompactionFailed {
                agent_id: response.agent_id.clone(),
                transaction_id,
                cut,
                reason: if reason
                    == tau_proto::StandaloneCompactionFailureReason::ContextWindowExceeded
                    && retreat_plan.is_none()
                    && automatic_context_irreducible
                {
                    tau_proto::StandaloneCompactionFailureReason::ContextIrreducible
                } else {
                    reason
                },
                resume_through,
                context_retreat: retreat_plan,
            }),
            Some(AgentPublishCompletion::OwedCompactionFact {
                batch_parent,
                retry_event: None,
            }),
            false,
        );
    }

    pub(super) fn clear_malformed_repetition_output(
        &mut self,
        response: &mut ProviderResponseFinished,
    ) {
        if response.stop_reason == ProviderStopReason::RepetitionDetected
            && !response.output_items.is_empty()
        {
            self.emit_info(&format!(
                "provider response {} used repetition_detected with output items; clearing malformed output",
                response.agent_prompt_id
            ));
            response.output_items.clear();
        }
    }

    pub(super) fn discard_finished_response_if_canceled(
        &mut self,
        agent_prompt_id: &AgentPromptId,
    ) -> bool {
        if self
            .prompt_coordination
            .canceled_prompts
            .remove(agent_prompt_id)
        {
            self.discard_finished_response_prompt_tracking(agent_prompt_id);
            return true;
        }
        false
    }

    pub(super) fn discard_finished_response_prompt_tracking(
        &mut self,
        agent_prompt_id: &AgentPromptId,
    ) {
        self.provider_runtime
            .cache_residency
            .drop_prompt(agent_prompt_id);
        self.remember_ephemeral_provider_prompt(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .context_limits
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .context_size_alerts
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .compaction_policies
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .agents
            .remove(agent_prompt_id.as_str());
        self.provider_runtime
            .pending_prompts
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .models
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .semantic_output
            .remove(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .operations
            .remove(agent_prompt_id);
        self.clear_prompt_tool_snapshot(agent_prompt_id);
    }

    /// Terminalize a live ordinary prompt whose tool-bearing response cannot
    /// acquire the AgentTree's sole foreground round.
    pub(super) fn terminalize_global_round_rejected_prompt(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let marked_owner = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .and_then(|tree| tree.marked_inference_through(&response.agent_prompt_id))
            .is_some();
        if let Some((session_id, originator)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|agent| {
                (
                    agent.identity.session_id.clone(),
                    agent.identity.originator.clone(),
                )
            })
        {
            if marked_owner {
                self.prompt_coordination
                    .prompt_runtime
                    .pending_stale_provider_responses
                    .insert(
                        response.agent_prompt_id.clone(),
                        PendingStaleProviderResponse {
                            response: response.clone(),
                        },
                    );
            }
            self.publish_prompt_terminated_from(
                session_id,
                response.agent_prompt_id.clone(),
                AgentPromptTerminationReason::Canceled,
                originator,
                None,
                source,
            );
        }
        if marked_owner {
            return;
        }
        self.discard_finished_response_prompt_tracking(&response.agent_prompt_id);
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            if agent.dispatch.in_flight_prompt.as_ref() == Some(&response.agent_prompt_id) {
                agent.dispatch.in_flight_prompt = None;
            }
            if agent.dispatch.last_prompt_id.as_ref() == Some(&response.agent_prompt_id) {
                agent.dispatch.last_prompt_id = None;
            }
            if matches!(
                &agent.dispatch.activation_dispatch,
                crate::agent::ActivationDispatchState::DispatchUncertain {
                    agent_prompt_id,
                    ..
                } if agent_prompt_id == &response.agent_prompt_id
            ) {
                agent.dispatch.activation_dispatch =
                    path_crate_agent::ActivationDispatchState::None;
            }
        }
        self.set_agent_turn_state(cid, AgentTurnState::Idle);
        self.try_advance_queue();
    }

    pub(super) fn clear_finished_response_prompt_route(&mut self, agent_prompt_id: &AgentPromptId) {
        self.remember_ephemeral_provider_prompt(agent_prompt_id);
        self.prompt_coordination
            .prompt_runtime
            .agents
            .remove(agent_prompt_id.as_str());
        self.provider_runtime
            .pending_prompts
            .remove(agent_prompt_id);
    }

    pub(super) fn remember_ephemeral_provider_prompt(&mut self, agent_prompt_id: &AgentPromptId) {
        if self
            .prompt_coordination
            .prompt_runtime
            .agents
            .get(agent_prompt_id)
            .and_then(|cid| self.agent_runtime.agent_registry.agents.get(cid))
            .is_some_and(|agent| agent.identity.persistence.is_ephemeral())
        {
            self.prompt_coordination
                .prompt_runtime
                .ephemeral_provider_prompts
                .insert(agent_prompt_id.clone());
        }
    }

    /// Preserve debug-suppression classification before an ephemeral runtime
    /// owner disappears while provider prompts remain correlated.
    pub(super) fn tombstone_ephemeral_provider_prompts_for_agent(&mut self, cid: &AgentId) {
        if !self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| agent.identity.persistence.is_ephemeral())
        {
            return;
        }
        self.prompt_coordination
            .prompt_runtime
            .ephemeral_provider_prompts
            .extend(
                self.prompt_coordination
                    .prompt_runtime
                    .agents
                    .iter()
                    .filter_map(|(prompt_id, owner)| (owner == cid).then_some(prompt_id.clone())),
            );
    }

    pub(super) fn update_finished_response_context_usage(
        &mut self,
        response_cid: Option<&AgentId>,
        agent_prompt_id: &AgentPromptId,
        input_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        // Per-conversation usage: separate from the global tracker because side
        // agents shouldn't clobber the user's status bar, but generic agent
        // stats still need their context usage.
        if let Some(cid) = response_cid {
            let usage_model = self
                .prompt_coordination
                .prompt_runtime
                .models
                .get(agent_prompt_id)
                .cloned();
            self.update_agent_context_usage(
                cid,
                Some(agent_prompt_id),
                usage_model.as_ref(),
                input_tokens,
                cached_tokens,
                source,
            );
        }
    }

    /// Queue each enabled named context-size alert once while usage remains
    /// above its threshold. Alerts ride the current tool round or dispatch
    /// after the finished turn through the ordinary internal-prompt queue.
    #[cfg(test)]
    pub(super) fn queue_crossed_context_size_alerts(
        &mut self,
        cid: &AgentId,
        input_tokens: Option<u64>,
        alerts: &BTreeMap<String, tau_config::settings::ContextSizeAlert>,
    ) {
        let prompt_id =
            tau_proto::AgentPromptId::parse("ap-test-alert").expect("known-safe test prompt id");
        self.queue_crossed_context_size_alerts_for_prompt(cid, &prompt_id, input_tokens, alerts);
    }

    pub(super) fn queue_crossed_context_size_alerts_for_prompt(
        &mut self,
        cid: &AgentId,
        agent_prompt_id: &AgentPromptId,
        input_tokens: Option<u64>,
        alerts: &BTreeMap<String, tau_config::settings::ContextSizeAlert>,
    ) {
        let Some(input_tokens) = input_tokens else {
            return;
        };
        let status_available = self
            .prompt_coordination
            .prompt_runtime
            .tool_specs
            .get(agent_prompt_id)
            .map_or_else(
                || {
                    self.agent_runtime
                        .agent_registry
                        .agents
                        .get(cid)
                        .is_some_and(|agent| agent.turn.terminal_status_was_available)
                },
                |specs| {
                    specs
                        .iter()
                        .any(|spec| self.tool_model_visible_name(spec).as_str() == "status")
                },
            );
        let logical_status = self.agent_runtime.agent_registry.agents.get(cid).map_or(
            tau_proto::AgentWorkStatusPhase::Working,
            |agent| {
                if status_available {
                    agent.turn.work_status.phase()
                } else {
                    tau_proto::AgentWorkStatusPhase::Working
                }
            },
        );
        let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) else {
            return;
        };
        agent.execution.fired_context_size_alerts.retain(|name| {
            alerts.get(name).is_some_and(|alert| {
                alert.enable
                    && matches!(
                        alert.when.at,
                        path_tau_config_settings::ContextPolicyPoint::AfterResponse
                            | path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished
                    )
                    && alert
                        .threshold
                        .is_exceeded_by(tau_proto::TokenCount::new(input_tokens))
            })
        });
        for (name, alert) in alerts {
            if alert.enable
                && alert.when.at == path_tau_config_settings::ContextPolicyPoint::AfterResponse
                && alert
                    .when
                    .statuses
                    .as_ref()
                    .is_none_or(|statuses| statuses.contains(&logical_status))
                && alert
                    .threshold
                    .is_exceeded_by(tau_proto::TokenCount::new(input_tokens))
                && agent
                    .execution
                    .fired_context_size_alerts
                    .insert(name.clone())
            {
                agent
                    .dispatch
                    .pending_prompts
                    .push_back(PendingPrompt::context_size_alert(alert.message.clone()));
            }
        }
    }

    /// Queues successful-response notices whose lifecycle selector owns the
    /// just-committed outer-turn finish.
    pub(super) fn queue_outer_turn_finished_context_size_alerts(
        &mut self,
        cid: &AgentId,
        outer_turn_id: &tau_proto::AgentOuterTurnId,
    ) {
        let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) else {
            return;
        };
        if !agent.turn.terminal_notice_eligible
            || agent.turn.terminal_notice_outer_turn_id.as_ref() != Some(outer_turn_id)
        {
            return;
        }
        let alerts = std::mem::take(&mut agent.turn.terminal_context_size_alerts);
        agent.turn.terminal_notice_eligible = false;
        agent.turn.terminal_notice_outer_turn_id = None;
        let Some(input_tokens) = agent.execution.context_input_tokens else {
            return;
        };
        let logical_status = if agent.turn.terminal_status_was_available {
            agent.turn.work_status.phase()
        } else {
            tau_proto::AgentWorkStatusPhase::Done
        };
        for (name, alert) in alerts {
            if alert.enable
                && alert.when.at == path_tau_config_settings::ContextPolicyPoint::OuterTurnFinished
                && alert
                    .when
                    .statuses
                    .as_ref()
                    .is_none_or(|statuses| statuses.contains(&logical_status))
                && alert
                    .threshold
                    .is_exceeded_by(tau_proto::TokenCount::new(input_tokens))
                && agent.execution.fired_context_size_alerts.insert(name)
            {
                agent
                    .dispatch
                    .pending_prompts
                    .push_back(PendingPrompt::context_size_alert(alert.message));
            }
        }
    }

    pub(super) fn emit_duplicate_finished_response_notice(
        &mut self,
        agent_prompt_id: &AgentPromptId,
    ) {
        if self.provider_prompt_targets_ephemeral(agent_prompt_id) {
            return;
        }
        // Dedupe: under at-least-once delivery the agent may resend a
        // finished-response after a reconnect. The first delivery
        // removed the entry from `prompt_agents`; later ones
        // must be ignored rather than falling back to another
        // session route, which would silently misroute the duplicate.
        self.emit_info(&format!(
            "discarding duplicate agent response for agent_prompt_id={agent_prompt_id}"
        ));
    }

    pub(super) fn assign_finished_response_agent_id(
        &mut self,
        cid: &AgentId,
        response: &mut ProviderResponseFinished,
    ) -> bool {
        let Some(agent_id) = self.target_agent_id_for_agent(cid) else {
            if !self.provider_prompt_targets_ephemeral(&response.agent_prompt_id) {
                self.emit_info(&format!(
                    "discarding agent response after owner unload for agent_prompt_id={}",
                    response.agent_prompt_id
                ));
            }
            self.discard_finished_response_prompt_tracking(&response.agent_prompt_id);
            return false;
        };
        response.agent_id = crate::parse_agent_id(&agent_id);
        true
    }

    pub(super) fn discard_finished_response_if_stale(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        source: Option<&tau_proto::ConnectionId>,
    ) -> bool {
        if !self.is_finished_response_stale(cid, &response.agent_prompt_id) {
            return false;
        }
        let marked_owner = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .and_then(|tree| tree.marked_inference_through(&response.agent_prompt_id))
            .is_some();
        if let Some((session_id, originator)) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|conv| {
                (
                    conv.identity.session_id.clone(),
                    conv.identity.originator.clone(),
                )
            })
        {
            if marked_owner {
                self.prompt_coordination
                    .prompt_runtime
                    .pending_stale_provider_responses
                    .insert(
                        response.agent_prompt_id.clone(),
                        PendingStaleProviderResponse {
                            response: response.clone(),
                        },
                    );
            }
            self.publish_prompt_terminated_from(
                session_id,
                response.agent_prompt_id.clone(),
                AgentPromptTerminationReason::Stale,
                originator,
                None,
                source,
            );
        }
        self.emit_info(&format!(
            "discarding stale agent response for agent_prompt_id={}",
            response.agent_prompt_id
        ));
        if !marked_owner {
            self.discard_finished_response_prompt_tracking(&response.agent_prompt_id);
        }
        true
    }

    pub(super) fn is_finished_response_stale(
        &self,
        cid: &AgentId,
        agent_prompt_id: &AgentPromptId,
    ) -> bool {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|conv| {
                conv.dispatch
                    .last_prompt_id
                    .as_ref()
                    .is_some_and(|last| last != agent_prompt_id)
                    || conv
                        .dispatch
                        .in_flight_prompt
                        .as_ref()
                        .is_some_and(|in_flight| in_flight != agent_prompt_id)
            })
    }

    pub(super) fn attach_finished_response_usage(
        &mut self,
        response: &mut ProviderResponseFinished,
        input_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        let reported_cache_read_ceiling = response
            .usage
            .as_ref()
            .and_then(|usage| usage.prompt_cache_read_ceiling_tokens);
        // Save the model that ran this turn before the
        // `prompt_models` entry is consumed below — we'll need it
        // again to anchor the stateful-chain state, and re-reading
        // `selected_model` later would lie if the user switched
        // models mid-turn.
        let turn_model = self
            .prompt_coordination
            .prompt_runtime
            .models
            .remove(&response.agent_prompt_id);
        if let Some(ref model) = turn_model
            && (input_tokens.is_some() || cached_tokens.is_some() || output_tokens.is_some())
        {
            let sent_tokens = input_tokens.unwrap_or(0);
            let cached_tokens = cached_tokens.unwrap_or(0);
            let received_tokens = output_tokens.unwrap_or(0);
            let cache = response
                .usage
                .as_ref()
                .and_then(|usage| usage.cache.as_deref())
                .map(|cache| {
                    let mut cache = *cache;
                    cache.read_tokens = Some(cache.read_tokens.unwrap_or(cached_tokens));
                    Box::new(cache)
                });
            let cached_tokens = cache
                .as_deref()
                .and_then(|cache| cache.read_tokens)
                .unwrap_or(cached_tokens);
            let cache_read_ceiling = validate_cache_read_ceiling(
                sent_tokens,
                cached_tokens,
                reported_cache_read_ceiling,
            );
            if let Some(rejected_ceiling) = reported_cache_read_ceiling
                && cache_read_ceiling.is_none()
            {
                tracing::warn!(
                    target: "tau_harness",
                    agent_prompt_id = %response.agent_prompt_id,
                    prompt_sent_tokens = sent_tokens,
                    prompt_cached_tokens = cached_tokens,
                    prompt_cache_read_ceiling_tokens = rejected_ceiling,
                    "discarding invalid provider cache-read ceiling"
                );
            }
            self.session_runtime
                .current_session_state
                .token_usage
                .add_sent(model, sent_tokens, cached_tokens);
            self.session_runtime
                .current_session_state
                .token_usage
                .add_received(model, received_tokens);
            response.usage = Some(ProviderTokenUsage {
                model: Some(model.clone()),
                prompt_sent_tokens: sent_tokens,
                prompt_cached_tokens: cached_tokens,
                prompt_cache_read_ceiling_tokens: cache_read_ceiling,
                cache,
                response_received_tokens: received_tokens,
                stats: self
                    .session_runtime
                    .current_session_state
                    .token_usage
                    .clone(),
            });
        }
    }

    pub(super) fn add_finished_response_estimated_cost(
        &mut self,
        cid: &AgentId,
        response: &mut ProviderResponseFinished,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let captured_rates = self
            .prompt_coordination
            .prompt_runtime
            .estimated_cost_rates
            .remove(&response.agent_prompt_id);
        let Some(usage) = response.usage.as_ref() else {
            response.estimated_api_cost_rates = None;
            response.estimated_api_cost_increment = None;
            self.emit_agent_stats_updated_from(cid, source);
            return;
        };
        let rates = captured_rates.unwrap_or_else(|| {
            tracing::warn!(
                target: "tau_harness",
                agent_prompt_id = %response.agent_prompt_id,
                model = ?usage.model,
                "accepted provider response has no dispatch pricing snapshot; \
                 using estimated API cost fallback"
            );
            tau_proto::ESTIMATED_API_COST_FALLBACK
        });
        let increment = tau_proto::EstimatedApiCost::for_usage(usage, rates);
        response.estimated_api_cost_rates = Some(rates);
        response.estimated_api_cost_increment = Some(increment);
        self.add_estimated_cost_increment(cid, increment, source);
    }

    /// Accounts for one accepted response increment and publishes affected live
    /// snapshots.
    pub(super) fn add_estimated_cost_increment(
        &mut self,
        cid: &AgentId,
        increment: tau_proto::EstimatedApiCost,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let changed_agents = self.agent_runtime.agent_registry.cost_ledger.add_increment(
            cid,
            increment,
            &self.agent_runtime.agent_registry.creator_topology,
        );
        for changed_agent_id in changed_agents {
            if self
                .agent_runtime
                .agent_registry
                .agents
                .contains_key(&changed_agent_id)
            {
                self.emit_agent_stats_updated_from(&changed_agent_id, source);
            }
        }
    }

    pub(super) fn attach_finished_response_compaction_usage(
        &self,
        response: &mut ProviderResponseFinished,
        input_tokens: Option<u64>,
    ) {
        response.compaction_original_input_tokens =
            input_tokens.or(response.compaction_original_input_tokens);
        response.compaction_output_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.response_received_tokens)
            .or(response.compaction_output_tokens);
    }

    pub(super) fn reconcile_finished_response_tool_call_stop(
        &mut self,
        response: &ProviderResponseFinished,
        tool_calls: &[AgentToolCall],
    ) -> (bool, bool) {
        let mut requested_tool_calls = response_requests_tool_calls(response);
        if requested_tool_calls && tool_calls.is_empty() {
            self.emit_info(&format!(
                "agent response {} reported tool calls but contained none; treating it as end_turn",
                response.agent_prompt_id
            ));
            requested_tool_calls = false;
        }
        let tool_calls_with_non_tool_stop = !requested_tool_calls && !tool_calls.is_empty();
        if tool_calls_with_non_tool_stop {
            requested_tool_calls = true;
        }
        (requested_tool_calls, tool_calls_with_non_tool_stop)
    }

    pub(super) fn is_non_tool_extension_query(&self, cid: &AgentId) -> bool {
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|conv| {
                if Self::is_peer_entrypoint_agent(conv) {
                    return false;
                }
                matches!(
                    conv.identity.originator,
                    tau_proto::PromptOriginator::Extension { .. }
                ) && conv.identity.parent_tool_call_id.is_none()
                    && !conv.identity.restored_tool_backed_start
            })
    }

    /// Identify the durable lifecycle purpose assigned by peer auto-start.
    pub(super) fn is_peer_entrypoint_agent(agent: &Agent) -> bool {
        agent.identity.peer_entrypoint_endpoint
    }

    pub(super) fn normalize_finished_response_tool_calls(
        &mut self,
        response: &mut ProviderResponseFinished,
        tool_calls: &mut Vec<AgentToolCall>,
        is_non_tool_ext_query: bool,
        tool_calls_with_non_tool_stop: bool,
    ) -> NormalizedFinishedToolCalls {
        let mut normalization = FinishedToolCallNormalization::new(
            response,
            self.known_tool_call_ids(),
            is_non_tool_ext_query,
            tool_calls_with_non_tool_stop,
        );
        let calls = tool_calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                self.normalize_finished_response_tool_call(
                    response,
                    index,
                    call,
                    &mut normalization,
                )
            })
            .collect::<Vec<_>>();
        Self::rewrite_finished_response_tool_call_items(response, &calls);
        *tool_calls = calls.iter().map(|entry| entry.call.clone()).collect();
        NormalizedFinishedToolCalls {
            invalid_errors: normalization.invalid_errors,
            calls,
        }
    }

    pub(super) fn normalize_finished_response_tool_call(
        &mut self,
        response: &ProviderResponseFinished,
        index: usize,
        call: &AgentToolCall,
        normalization: &mut FinishedToolCallNormalization,
    ) -> NormalizedFinishedToolCall {
        let mut call = call.clone();
        normalization.normalize_call_id(index, &mut call);
        self.prompt_coordination
            .prompt_runtime
            .tool_call_prompts
            .insert(call.id.clone(), response.agent_prompt_id.clone());
        let background_support = self.resolve_tool_background_support(call.name.as_str());
        let turn_categories = self
            .resolve_enabled_tool_spec_for_prompt(&call.name, &response.agent_prompt_id)
            .map_or_else(ToolTurnCategories::default, |spec| {
                ToolTurnCategories::from_tags(&spec.tags)
            });
        NormalizedFinishedToolCall {
            call,
            background_support,
            turn_categories,
        }
    }

    pub(super) fn rewrite_finished_response_tool_call_items(
        response: &mut ProviderResponseFinished,
        normalized_calls: &[NormalizedFinishedToolCall],
    ) {
        let mut normalized_calls_iter = normalized_calls.iter();
        response.output_items = response
            .output_items
            .drain(..)
            .map(|item| match item {
                ContextItem::ToolCall(original_call) => {
                    let entry = normalized_calls_iter
                        .next()
                        .expect("tool-call normalization count should match output items");
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: entry.call.id.clone(),
                        name: entry.call.name.clone(),
                        tool_type: entry.call.tool_type,
                        arguments: entry.call.arguments.clone(),
                        raw_arguments_json: original_call.raw_arguments_json,
                        responses_envelope: original_call.responses_envelope,
                    })
                }
                item => item,
            })
            .collect();
    }

    pub(super) fn publish_finished_response_for_agent(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
        response: &ProviderResponseFinished,
        completion: Option<AgentPublishCompletion>,
        notify_watchers: bool,
    ) {
        // Publish via the owning agent's branch — when text is
        // present the AgentTree fold appends an assistant response as a
        // child of `tree.head`, so an unsnapped publish would land on
        // whichever branch happened to be at `tree.head` (e.g. after
        // a sibling side conv's teardown touched another branch).
        // `publish_for_agent` snaps and updates `c.head`.
        self.publish_event_for_agent_with_completion(
            cid,
            source,
            Event::ProviderResponseFinished(response.clone()),
            completion,
            notify_watchers,
        );
        self.clear_finished_response_prompt_route(&response.agent_prompt_id);
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            conv.dispatch.in_flight_prompt = None;
        }
    }

    pub(super) fn handle_finished_response_side_conversation(
        &mut self,
        cid: &AgentId,
        side: FinishedSideConversation<'_>,
        normalized_tool_calls: &mut NormalizedFinishedToolCalls,
        source: Option<&tau_proto::ConnectionId>,
    ) -> bool {
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(Self::is_peer_entrypoint_agent)
        {
            return false;
        }
        let Some(active_originator) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .map(|agent| &agent.identity.originator)
        else {
            return false;
        };
        // A tool completion can synchronously dispatch another prompt while the
        // delegate's terminal StartAgentResult is being published. The prompt
        // retains the old extension originator, but the delegate is detached
        // before its response arrives. Do not treat that stale response as a
        // second completion of the already-finished start request.
        if active_originator != &side.response.originator {
            return false;
        }
        let Some((name, query_id)) = Self::finished_response_side_originator(
            active_originator,
            side.requested_tool_calls,
            side.is_non_tool_ext_query,
        ) else {
            return false;
        };

        if !side.requested_tool_calls {
            self.clear_prompt_tool_snapshot(&side.response.agent_prompt_id);
        }
        if side.requested_tool_calls {
            self.reject_finished_side_conversation_tool_calls(cid, normalized_tool_calls, source);
        }
        if self.finished_side_conversation_continues_for_pending_message_wake(
            cid,
            side.response,
            side.requested_tool_calls,
            side.is_non_tool_ext_query,
        ) {
            self.dispatch_prompt_after_publish_idle(cid);
            return true;
        }

        let error = Self::finished_side_conversation_error(
            side.response,
            side.is_non_tool_ext_query,
            side.requested_tool_calls,
            side.assistant_text,
            side.tool_call_count,
        );
        let result = tau_proto::StartAgentResult {
            query_id: query_id.clone(),
            text: side.assistant_text.unwrap_or_default().to_owned(),
            error,
        };
        self.deliver_finished_side_conversation_result(cid, &name, &query_id, result, source);
        self.complete_finished_side_conversation(cid, Some(&side.response.agent_prompt_id));
        true
    }

    /// Return whether a side-conversation terminal will continue in the same
    /// outer turn to process a pending agent-message wake.
    fn finished_side_conversation_continues_for_pending_message_wake(
        &self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        requested_tool_calls: bool,
        is_non_tool_ext_query: bool,
    ) -> bool {
        let Some(agent) = self.agent_runtime.agent_registry.agents.get(cid) else {
            return false;
        };
        !Self::is_peer_entrypoint_agent(agent)
            && agent.identity.originator == response.originator
            && Self::finished_response_side_originator(
                &agent.identity.originator,
                requested_tool_calls,
                is_non_tool_ext_query,
            )
            .is_some()
            && self.has_pending_agent_message_wake(cid)
    }

    pub(super) fn finished_response_side_originator(
        originator: &PromptOriginator,
        requested_tool_calls: bool,
        is_non_tool_ext_query: bool,
    ) -> Option<(ExtensionName, String)> {
        if let tau_proto::PromptOriginator::Extension { name, query_id } = originator
            && (!requested_tool_calls || is_non_tool_ext_query)
        {
            return Some((name.clone(), query_id.clone()));
        }
        None
    }

    pub(super) fn reject_finished_side_conversation_tool_calls(
        &mut self,
        cid: &AgentId,
        normalized_tool_calls: &mut NormalizedFinishedToolCalls,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let remaining_calls: Vec<ToolCallId> = normalized_tool_calls
            .calls
            .iter()
            .map(|entry| entry.call.id.clone())
            .collect();
        self.register_finished_response_pending_tools(&normalized_tool_calls.calls);
        self.set_agent_turn_state(cid, AgentTurnState::ToolsRunning { remaining_calls });
        for entry in &normalized_tool_calls.calls {
            let message = normalized_tool_calls
                .invalid_errors
                .remove(&entry.call.id)
                .unwrap_or_else(|| format!("refusing to execute tool call `{}`", entry.call.name));
            self.reject_agent_tool_call_before_dispatch_inner(
                cid,
                &entry.call,
                entry.call.name.clone(),
                message,
                false,
                source,
            );
        }
    }

    pub(super) fn finished_side_conversation_error(
        response: &ProviderResponseFinished,
        is_non_tool_ext_query: bool,
        requested_tool_calls: bool,
        _assistant_text: Option<&str>,
        tool_call_count: usize,
    ) -> Option<String> {
        if is_non_tool_ext_query && requested_tool_calls {
            Some(format!(
                "non-tool extension query attempted to call {tool_call_count} tool(s); refusing to execute"
            ))
        } else if matches!(
            response.stop_reason,
            ProviderStopReason::Length
                | ProviderStopReason::Error
                | ProviderStopReason::RepetitionDetected
        ) {
            Some(format!(
                "provider failure: {}",
                response
                    .failure_kind
                    .unwrap_or(tau_proto::ProviderFailureKind::Unknown)
                    .as_str()
            ))
        } else {
            None
        }
    }

    pub(super) fn deliver_finished_side_conversation_result(
        &mut self,
        cid: &AgentId,
        name: &ExtensionName,
        query_id: &str,
        result: tau_proto::StartAgentResult,
        result_source: Option<&tau_proto::ConnectionId>,
    ) {
        let source = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|c| c.identity.source_connection.clone());
        if let Some(source) = source {
            if &source == harness_connection_id() {
                self.publish_event(
                    Some(crate::harness::harness_connection_id()),
                    Event::StartAgentResult(result),
                );
            } else {
                let _ = self.runtime_io.bus.send_to(
                    &source,
                    result_source,
                    HarnessOutputMessage::deliver(Event::StartAgentResult(result)),
                );
            }
        } else {
            let agent_id = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.identity.agent_id.clone())
                .unwrap_or_else(|| cid.to_string());
            self.agent_runtime
                .agent_watch
                .pending_unload_reasons
                .insert(
                    agent_id.clone(),
                    tau_proto::AgentWatchLifecycleReason::RestoredDelegationRouteLost,
                );
            tracing::error!(
                target: "tau_harness::agent_lifecycle",
                %agent_id,
                %query_id,
                extension = %name,
                reason = "no_source_connection",
                action = "unload",
                "start-agent result route lost"
            );
            self.emit_harness_failure(&format!(
                "agent_id={agent_id} query_id={query_id} extension={name} \
                 reason=no_source_connection action=unload"
            ));
        }
    }

    /// Completes extension-originated work after a terminal standalone
    /// compaction failure, using only a safe categorical error.
    pub(super) fn complete_failed_compaction_side_conversation(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
    ) -> bool {
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(Self::is_peer_entrypoint_agent)
        {
            return false;
        }
        let Some((name, query_id)) =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| {
                    if let PromptOriginator::Extension { name, query_id } =
                        &agent.identity.originator
                    {
                        Some((name.clone(), query_id.clone()))
                    } else {
                        None
                    }
                })
        else {
            return false;
        };
        self.deliver_finished_side_conversation_result(
            cid,
            &name,
            &query_id,
            tau_proto::StartAgentResult {
                query_id: query_id.clone(),
                text: String::new(),
                error: Some("provider failure: compaction".to_owned()),
            },
            source,
        );
        self.complete_finished_side_conversation(cid, None);
        true
    }

    pub(super) fn complete_finished_side_conversation(
        &mut self,
        cid: &AgentId,
        completed_prompt_id: Option<&AgentPromptId>,
    ) {
        let keep_parented_conversation = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|conv| {
                conv.identity.parent_tool_call_id.is_some()
                    || conv.identity.parent_agent_id.is_some()
                    || conv.identity.restored_tool_backed_start
            });
        let replacement_prompt_in_flight = keep_parented_conversation
            && self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|conv| conv.dispatch.in_flight_prompt.as_ref())
                .is_some_and(|prompt_id| Some(prompt_id) != completed_prompt_id);
        let replacement_tool_terminal_in_flight = keep_parented_conversation
            && self
                .tool_routing
                .tool_runtime
                .tool_agents
                .values()
                .any(|owner| owner == cid);
        // Release before removing or detaching the side agent so
        // queued descendants can still resolve their parent agent
        // while starting. Active descendants keep their own copied state. Result
        // delivery can synchronously dispatch a replacement prompt, so do not
        // overwrite that prompt's running state while detaching the old request.
        if !replacement_prompt_in_flight && !replacement_tool_terminal_in_flight {
            self.set_agent_turn_state(cid, AgentTurnState::Idle);
        }
        self.release_start_agent_request(cid);
        if keep_parented_conversation {
            self.detach_completed_parented_start_agent(cid);
        } else {
            self.remove_agent_expected(cid);
        }
        self.try_advance_queue();
    }

    pub(super) fn dispatch_finished_response_tool_calls(
        &mut self,
        cid: &AgentId,
        mut normalized_tool_calls: NormalizedFinishedToolCalls,
        source: Option<&tau_proto::ConnectionId>,
    ) -> Result<(), HarnessError> {
        // Tool calls to execute — agent stays busy. After all
        // tools complete, maybe_complete_agent_turn drains any
        // prompts queued via `pending_prompts` (publishing one
        // `AgentPromptSteered` each, which folds them as
        // `UserMessage` entries onto this agent's branch)
        // and sends a new prompt with the results plus those
        // steering messages.
        // Malformed provider call ids were normalized before the assistant
        // response was published. Keep them in the turn as synthetic
        // rejected calls so the next model prompt sees a matched
        // tool-call/tool-error pair instead of the harness returning an
        // event-loop error or overwriting duplicate map entries.
        let remaining_calls: Vec<ToolCallId> = normalized_tool_calls
            .calls
            .iter()
            .map(|entry| entry.call.id.clone())
            .collect();
        self.register_finished_response_pending_tools(&normalized_tool_calls.calls);
        self.set_agent_turn_state(cid, AgentTurnState::ToolsRunning { remaining_calls });
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|conv| conv.dispatch.pending_cancel.is_some())
        {
            self.apply_pending_cancel_for_agent(cid);
            return Ok(());
        }
        // Queue well-formed tool calls and turn malformed calls into
        // model-visible errors. The turn machine preserves provider order
        // for calls that are safe to dispatch.
        for entry in normalized_tool_calls.calls {
            let call = entry.call;
            if let Some(message) = normalized_tool_calls.invalid_errors.remove(&call.id) {
                self.reject_agent_tool_call_before_dispatch_from(
                    cid,
                    &call,
                    call.name.clone(),
                    message,
                    source,
                );
            } else {
                self.tool_routing.tool_runtime.tool_turn.push_from(
                    cid.clone(),
                    call,
                    entry.background_support,
                    source.cloned(),
                    entry.turn_categories,
                );
            }
        }
        self.drain_pending_tool_invocations()
    }

    pub(super) fn register_finished_response_pending_tools(
        &mut self,
        normalized_calls: &[NormalizedFinishedToolCall],
    ) {
        for entry in normalized_calls {
            self.tool_routing.tool_runtime.pending_tools.insert(
                entry.call.id.clone(),
                PendingTool {
                    name: entry.call.name.clone(),
                    internal_name: entry.call.name.clone(),
                    tool_type: entry.call.tool_type,
                    allows_provider_image: false,
                },
            );
        }
        self.extend_cache_refresh_tool_window(
            normalized_calls.iter().map(|entry| entry.call.id.clone()),
        );
    }

    pub(super) fn extend_cache_refresh_tool_window(
        &mut self,
        call_ids: impl IntoIterator<Item = ToolCallId>,
    ) {
        self.provider_runtime
            .cache_refresh_tool_window_calls
            .extend(call_ids);
        if !self
            .provider_runtime
            .cache_refresh_tool_window_calls
            .is_empty()
        {
            self.provider_runtime.cache_residency.open_tool_window();
        }
    }

    /// Derive the current reasoning-only run's replay-safe continuation plan.
    pub(super) fn derive_output_length_continuation(
        &mut self,
        cid: &AgentId,
        response: &mut ProviderResponseFinished,
        operation: tau_proto::PromptOperation,
        requested_tool_calls: bool,
    ) {
        response.output_length_disposition = tau_proto::OutputLengthDisposition::None;
        let lineage_owner =
            self.agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| {
                    self.session_runtime
                        .agent_store
                        .agent(agent.identity.agent_id.as_deref()?)
                        .and_then(|tree| {
                            tree.output_length_lineage_owner_for_prompt(&response.agent_prompt_id)
                        })
                })
                .filter(|owner| {
                    self.agent_runtime.agent_registry.agents.get(cid).is_some_and(|agent| {
                    matches!(
                        &agent.turn.output_length_continuation,
                        path_crate_agent::OutputLengthContinuationState::Active(continuation)
                            if continuation.plan.owner == *owner
                    )
                })
                });
        if let Some(owner) = lineage_owner {
            let outcome = if response.stop_reason == ProviderStopReason::Length {
                tau_proto::OutputLengthContinuationOutcome::Incomplete
            } else if response.error.is_some()
                || response.failure_kind.is_some()
                || matches!(
                    response.stop_reason,
                    ProviderStopReason::Error | ProviderStopReason::RepetitionDetected
                )
            {
                tau_proto::OutputLengthContinuationOutcome::Failed
            } else {
                tau_proto::OutputLengthContinuationOutcome::Completed
            };
            response.output_length_disposition =
                tau_proto::OutputLengthDisposition::ContinuationTerminal {
                    outer_turn_id: owner.outer_turn_id,
                    source_agent_prompt_id: owner.source_agent_prompt_id,
                    ordinal: owner.ordinal,
                    outcome,
                    // The finish bit must reflect the actual post-suppression
                    // tool continuation. A ToolCalls stop with zero calls is
                    // reconciled to end_turn and owes its finish; an EndTurn
                    // with calls dispatches them and owes none.
                    outer_turn_finish_owed: !requested_tool_calls,
                };
            return;
        }
        let replay_safe_adapter = response
            .backend
            .as_ref()
            .is_some_and(|backend| backend.kind == tau_proto::ProviderBackendKind::ChatCompletions);
        let ordinary_user_conversation = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| agent.identity.originator.is_user());
        if operation != tau_proto::PromptOperation::Inference
            || !ordinary_user_conversation
            || !replay_safe_adapter
            || response.stop_reason != ProviderStopReason::Length
            || response.error.is_some()
            || response.failure_kind.is_some()
            || requested_tool_calls
            || response
                .output_items
                .iter()
                .any(|item| matches!(item, ContextItem::Message(_) | ContextItem::ToolCall(_)))
            || !response.output_items.iter().any(|item| {
                matches!(
                    item,
                    ContextItem::ReasoningText(tau_proto::ReasoningTextItem {
                        kind: tau_proto::ReasoningTextKind::Full,
                        text,
                    }) if !text.is_empty()
                )
            })
        {
            return;
        }
        let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) else {
            return;
        };
        let Some(outer_turn_id) = agent.turn.outer_turn.active_id().cloned() else {
            return;
        };
        if agent.turn.output_length_continuation.outer_turn_id() == Some(&outer_turn_id) {
            return;
        }
        let Some(agent_id) = agent.identity.agent_id.as_deref() else {
            return;
        };
        let successor_agent_prompt_id = tau_proto::AgentPromptId::parse(format!(
            "ap-{agent_id}-{}",
            agent.dispatch.next_prompt_index
        ))
        .expect("known-safe AgentPromptId must be valid");
        agent.dispatch.next_prompt_index = agent.dispatch.next_prompt_index.saturating_add(1);
        let owner = tau_proto::OutputLengthContinuationOwner {
            source_agent_prompt_id: response.agent_prompt_id.clone(),
            outer_turn_id: outer_turn_id.clone(),
            ordinal: 1,
        };
        let source_checkpoint = self
            .session_runtime
            .agent_store
            .agent(agent_id)
            .and_then(|tree| tree.marked_inference_checkpoint(&response.agent_prompt_id))
            .cloned();
        let Some(source_checkpoint) = source_checkpoint else {
            return;
        };
        let (Some(model), Some(operation), Some(activation_cut)) = (
            source_checkpoint.model,
            source_checkpoint.operation,
            source_checkpoint.activation_cut,
        ) else {
            return;
        };
        agent.turn.output_length_continuation =
            path_crate_agent::OutputLengthContinuationState::Planned(
                path_crate_agent::OutputLengthContinuationPlan {
                    agent_prompt_id: successor_agent_prompt_id.clone(),
                    owner,
                    dispatch: path_crate_agent::InferenceDispatchOwnership {
                        model,
                        operation,
                        activation_cut,
                    },
                },
            );
        response.output_length_disposition =
            tau_proto::OutputLengthDisposition::ContinuationPlanned {
                outer_turn_id,
                successor_agent_prompt_id,
                ordinal: 1,
                limit: 1,
            };
    }

    pub(super) fn complete_finished_response_without_tool_calls(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
        assistant_text: Option<&str>,
    ) {
        self.clear_prompt_tool_snapshot(&response.agent_prompt_id);
        self.project_committed_terminal_incomplete(cid, response);
        if matches!(
            response.output_length_disposition,
            tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
        ) {
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent
                    .dispatch
                    .pending_prompts
                    .push_back(PendingPrompt::output_length_continuation());
                // This is an inference round boundary, not an outer-turn
                // running-to-idle transition. Keep lifecycle and the committed
                // continuation reservation active while the steer is folded.
                agent.turn.turn_state = AgentTurnState::Idle;
            }
            let completion = AgentPublishCompletion::OutputLengthSteer {
                batch_parent: self
                    .selected_head_for_agent(cid)
                    .unwrap_or(tau_proto::AgentHead::Root),
                retry_event: None,
            };
            self.fold_pending_prompts_as_steered_with_completion(cid, Some(completion));
            self.dispatch_activation_after_publish_idle(cid);
            return;
        }
        if response.stop_reason == ProviderStopReason::RepetitionDetected {
            self.handle_loop_guard_trigger(
                cid,
                "provider-repetition-detected".to_owned(),
                "provider detected a tight exact stream repetition".to_owned(),
            );
        } else {
            self.record_assistant_loop_signature(cid, assistant_text);
        }
        self.set_agent_turn_state(cid, AgentTurnState::Idle);
        if matches!(
            response.output_length_disposition,
            tau_proto::OutputLengthDisposition::ContinuationTerminal {
                outcome: tau_proto::OutputLengthContinuationOutcome::Cancelled,
                ..
            }
        ) {
            return;
        }
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|conv| {
                conv.dispatch
                    .pending_prompts
                    .iter()
                    .any(PendingPrompt::is_loop_guard)
            })
        {
            self.fold_pending_prompts_as_steered(cid);
            self.dispatch_prompt_after_publish_idle(cid);
            return;
        }
        // No tool calls — this agent's turn is done. Drain
        // any queued prompts (on this or other agents) that
        // are now eligible to dispatch.
        self.try_advance_queue();
    }

    /// Publish sticky incomplete state only from the canonical post-commit
    /// response after interception and cancellation arbitration finish.
    pub(super) fn project_committed_terminal_incomplete(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
    ) {
        if response.stop_reason != ProviderStopReason::Length
            || matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
            )
            || self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .is_some_and(|agent| agent.turn.lifecycle_notification_only_turn)
        {
            return;
        }
        let selected_terminal = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent_id| self.session_runtime.agent_store.agent(agent_id))
            .and_then(tau_core::AgentTree::output_length_terminal_incomplete)
            .is_some_and(|terminal| terminal.agent_prompt_id == response.agent_prompt_id);
        if !selected_terminal {
            return;
        }
        let Some(public_id) = self.ensure_agent_id_for_agent(cid) else {
            return;
        };
        self.update_agent_watch_provider_status(
            &public_id,
            tau_proto::AgentWatchProviderStatusNotification {
                session_id: self.session_runtime.current_session_id.clone(),
                subscription_id: String::new(),
                turn_generation: self
                    .agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .map_or(tau_proto::AgentOuterTurnGeneration::initial(), |agent| {
                        agent.turn.turn_generation
                    }),
                agent_prompt_id: response.agent_prompt_id.clone(),
                state: tau_proto::AgentWatchProviderState::TerminalIncomplete {
                    category: tau_proto::AgentWatchProviderCategory::OutputLength,
                    attempt: response.provider_attempt.get(),
                },
                initial: false,
            },
        );
    }

    /// Apply the common successful-terminal gate before ordinary or delegated
    /// completion can project the candidate response.
    pub(super) fn apply_final_status_response_gate(
        &mut self,
        cid: &AgentId,
        response: &ProviderResponseFinished,
    ) -> Option<crate::agent::FinalStatusDecision> {
        let successful = response.error.is_none()
            && response.failure_kind.is_none()
            && !matches!(
                response.stop_reason,
                ProviderStopReason::Length
                    | ProviderStopReason::Error
                    | ProviderStopReason::RepetitionDetected
            );
        let status_was_available = self
            .prompt_coordination
            .prompt_runtime
            .tool_specs
            .get(&response.agent_prompt_id)
            .is_some_and(|specs| {
                specs
                    .iter()
                    .any(|spec| self.tool_model_visible_name(spec).as_str() == "status")
            });
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.turn.terminal_status_was_available = status_was_available;
        }
        self.agent_runtime
            .agent_registry
            .agents
            .get(cid)?
            .turn
            .work_status
            .decide_final(FinalStatusInput {
                successful,
                status_was_available,
            })
    }

    /// Perform ordinary or delegated completion only after an accepted gated
    /// final crossed its semantic append boundary.
    pub(super) fn complete_committed_gated_final(
        &mut self,
        cid: &AgentId,
        terminal: CommittedGatedFinal,
    ) {
        let CommittedGatedFinal {
            response,
            response_contains_compaction,
            input_tokens,
            context_size_alerts,
            is_non_tool_ext_query,
            source,
            tool_effect,
        } = terminal;
        let committed_user_cancellation = response.error.as_deref() == Some("cancelled")
            && self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|agent| agent.dispatch.pending_cancel.as_ref())
                .is_some_and(|pending| {
                    pending.agent_prompt_id.as_ref() == Some(&response.agent_prompt_id)
                });
        if committed_user_cancellation {
            self.prompt_coordination
                .canceled_prompts
                .insert(response.agent_prompt_id.clone());
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent.dispatch.pending_cancel = None;
                agent.dispatch.pending_prompts.clear();
                agent.turn.terminal_notice_eligible = false;
            }
        }
        let (requested_tool_calls, mut normalized_tool_calls) = match tool_effect {
            CommittedOutputLengthToolEffect::None => {
                (false, NormalizedFinishedToolCalls::default())
            }
            CommittedOutputLengthToolEffect::Dispatch(calls) => (true, calls),
        };
        let successful = response.error.is_none()
            && response.failure_kind.is_none()
            && !matches!(
                response.stop_reason,
                ProviderStopReason::Length
                    | ProviderStopReason::Error
                    | ProviderStopReason::RepetitionDetected
            );
        if response_contains_compaction {
            self.clear_agent_context_usage(cid);
        } else if successful {
            self.queue_crossed_context_size_alerts_for_prompt(
                cid,
                &response.agent_prompt_id,
                input_tokens,
                &context_size_alerts,
            );
        }
        let assistant_text = assistant_text_from_output_items(&response.output_items);
        let notify_watchers = !matches!(
            response.originator,
            tau_proto::PromptOriginator::Extension { .. }
        ) && self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| !agent.turn.lifecycle_notification_only_turn);
        if response.recovery_disposition
            != tau_proto::ContextRecoveryDisposition::ReactiveCompactionPlanned
            && self.handle_finished_response_side_conversation(
                cid,
                FinishedSideConversation {
                    response: &response,
                    requested_tool_calls,
                    is_non_tool_ext_query,
                    assistant_text: assistant_text.as_deref(),
                    tool_call_count: normalized_tool_calls.calls.len(),
                },
                &mut normalized_tool_calls,
                source.as_ref(),
            )
        {
            return;
        }
        if notify_watchers
            && !requested_tool_calls
            && successful
            && let Some(message) = assistant_text.clone()
        {
            self.notify_agent_watchers_about_response(cid, message);
        }
        if requested_tool_calls {
            if let Err(error) = self.dispatch_finished_response_tool_calls(
                cid,
                normalized_tool_calls,
                source.as_ref(),
            ) {
                self.emit_harness_failure(&format!(
                    "failed to dispatch committed output-length successor tools: {error}"
                ));
                self.terminalize_owned_dispatch_error(cid, error.to_string());
            }
        } else {
            self.complete_finished_response_without_tool_calls(
                cid,
                &response,
                assistant_text.as_deref(),
            );
        }
    }

    /// Dispatch a challenged candidate as an inner continuation without closing
    /// its durable outer turn or changing its runtime generation.
    pub(super) fn continue_after_gated_final_challenge(&mut self, cid: &AgentId) {
        if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            agent.turn.turn_state = AgentTurnState::Idle;
        }
        self.fold_pending_prompts_as_steered(cid);
        self.dispatch_activation_after_publish_idle(cid);
    }

    pub(super) fn notify_work_status_transition(&mut self, cid: &AgentId) {
        let Some(agent_id) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.clone())
        else {
            return;
        };
        for watcher_id in self.watchers_for_agent(&agent_id) {
            self.notify_agent_watcher_work_status(&watcher_id, &agent_id, false);
        }
        self.emit_agent_stats_updated(cid);
    }

    pub(super) fn known_tool_call_ids(&self) -> HashSet<ToolCallId> {
        let mut ids: HashSet<ToolCallId> = self
            .tool_routing
            .tool_runtime
            .tool_agents
            .keys()
            .chain(self.tool_routing.tool_runtime.pending_tools.keys())
            .chain(self.tool_routing.tool_runtime.completed_tool_calls.iter())
            .cloned()
            .collect();
        for tree in self.session_runtime.agent_store.agents() {
            for node in tree.nodes() {
                let tau_core::AgentEntry::AssistantResponse { output_items, .. } = &node.entry
                else {
                    continue;
                };
                ids.extend(output_items.iter().filter_map(|item| match item {
                    ContextItem::ToolCall(call) => Some(call.call_id.clone()),
                    _ => None,
                }));
            }
        }
        ids
    }

    /// Update one agent's `context_input_tokens` /
    /// `context_percent_used` from a finished agent response. Mirrors
    /// `update_context_usage` but scoped to a single conversation —
    /// the global tracker is intentionally only fed by user-agent
    /// turns so the status bar stays stable while side agents run.
    pub(super) fn update_agent_context_usage(
        &mut self,
        cid: &AgentId,
        agent_prompt_id: Option<&tau_proto::AgentPromptId>,
        model: Option<&ModelId>,
        input_tokens: Option<u64>,
        cached_tokens: Option<u64>,
        source: Option<&tau_proto::ConnectionId>,
    ) {
        let context_window =
            model.and_then(|m| context_window_for_model(&self.provider_runtime.model_info, m));
        let percent_used = match (context_window, input_tokens) {
            (Some(w), Some(tokens)) => Some(context_percent_used(tokens, w)),
            _ => None,
        };
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            conv.execution.context_input_tokens = input_tokens;
            conv.execution.context_cached_tokens = cached_tokens;
            conv.execution.context_usage_head = conv.identity.head;
            conv.execution.context_usage_model = model.cloned();
            conv.execution.context_usage_prompt_id = agent_prompt_id.cloned();
            conv.execution.context_percent_used = percent_used;
        }
        self.publish_event(
            source,
            Event::HarnessAgentContextUsageChanged(HarnessAgentContextUsageChanged {
                agent_id: cid.clone(),
                input_tokens,
                cached_tokens,
                context_window,
                percent_used,
            }),
        );
        self.emit_agent_stats_updated_from(cid, source);
    }

    pub(super) fn clear_agent_context_usage(&mut self, cid: &AgentId) {
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            conv.execution.context_input_tokens = None;
            conv.execution.context_usage_head = None;
            conv.execution.context_usage_model = None;
            conv.execution.context_usage_prompt_id = None;
            conv.execution.context_cached_tokens = None;
            conv.execution.context_percent_used = None;
            conv.execution.fired_context_size_alerts.clear();
            conv.dispatch
                .pending_prompts
                .retain(|prompt| !prompt.is_context_size_alert());
        }
    }

    /// Returns whether the provider usage baseline belongs to the selected
    /// transcript branch.
    pub(super) fn context_usage_baseline_applies(&self, conv: &Agent) -> bool {
        let Some(agent_id) = conv.identity.agent_id.as_deref() else {
            return false;
        };
        let baseline = conv
            .execution
            .context_usage_head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        let current_head = conv
            .identity
            .head
            .map_or(tau_proto::AgentHead::Root, tau_proto::AgentHead::Node);
        self.session_runtime
            .agent_store
            .agent(agent_id)
            .is_some_and(|tree| tree.contains_head_ancestry(baseline, current_head))
    }

    /// Reconciles provider usage with the selected durable branch and publishes
    /// the complete live context and stats projections.
    pub(super) fn reconcile_agent_context_usage_for_selected_branch(&mut self, cid: &AgentId) {
        let derived = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| {
                self.agent_context_usage_at(conv.identity.agent_id.as_deref()?, conv.identity.head)
            });
        let retained_root = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|conv| {
                (conv.execution.context_usage_head.is_none()
                    && self.context_usage_baseline_applies(conv))
                .then(|| {
                    Some((
                        conv.execution.context_usage_model.clone()?,
                        conv.execution.context_input_tokens?,
                        conv.execution.context_cached_tokens.unwrap_or_default(),
                        None,
                        conv.execution.context_usage_prompt_id.clone()?,
                    ))
                })
                .flatten()
            });
        let restored = derived.or(retained_root).filter(|(model, ..)| {
            let current_model = self
                .agent_runtime
                .agent_registry
                .agents
                .get(cid)
                .and_then(|conv| self.model_for_agent_role(conv));
            (current_model.is_none() && !self.provider_runtime.model_info.contains_key(model))
                || current_model.as_ref() == Some(model)
        });
        self.clear_agent_context_usage(cid);
        let (model, input_tokens, cached_tokens, usage_head, usage_prompt_id) = restored
            .map(|(model, input, cached, head, prompt_id)| {
                (
                    Some(model),
                    Some(input),
                    Some(cached),
                    head,
                    Some(prompt_id),
                )
            })
            .unwrap_or((None, None, None, None, None));
        let context_window = model
            .as_ref()
            .and_then(|model| context_window_for_model(&self.provider_runtime.model_info, model));
        let percent_used = context_window
            .zip(input_tokens)
            .map(|(window, tokens)| context_percent_used(tokens, window));
        if let Some(conv) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
            conv.execution.context_input_tokens = input_tokens;
            conv.execution.context_cached_tokens = cached_tokens;
            conv.execution.context_usage_head = usage_head;
            conv.execution.context_usage_model = model;
            conv.execution.context_usage_prompt_id = usage_prompt_id;
            conv.execution.context_percent_used = percent_used;
        }
        self.publish_event(
            None,
            Event::HarnessAgentContextUsageChanged(HarnessAgentContextUsageChanged {
                agent_id: cid.clone(),
                input_tokens,
                cached_tokens,
                context_window,
                percent_used,
            }),
        );
        self.emit_agent_stats_updated(cid);
    }
}

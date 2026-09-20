//! Complete ordinary terminal shaping before accounting or lifecycle effects.

use super::*;

/// Deferred ordinary-terminal effects and classification accompanying the
/// already-enriched response. Preparation changes only caller-owned values.
pub(super) struct PreparedOrdinaryProviderTerminal {
    /// Whether the reconciled terminal owns tool dispatch.
    pub(super) requested_tool_calls: bool,
    /// Diagnostic owed for a tool-call stop with no actual calls.
    pub(super) missing_tool_calls: bool,
    /// Normalized declarations and their prompt-frozen execution policies.
    pub(super) normalized_tool_calls: NormalizedFinishedToolCalls,
    /// Declaration identity allocated only when the terminal contains calls.
    pub(super) declaration: Option<tau_proto::ObservationId>,
    /// Continuation ownership and cursor to install at the old effect boundary.
    pub(super) output_length: prepared_output_length_continuation::PreparedOutputLengthContinuation,
    /// Final-status gate decided against the prepared response.
    pub(super) final_status_plan: ProviderTerminalPlan,
    /// Prompt-frozen availability to install before outer-turn policy effects.
    pub(super) status_was_available: Option<bool>,
    /// Whether the final-status gate challenges this response.
    pub(super) final_status_challenged: bool,
    /// Whether the side conversation retains a pending-message wake.
    pub(super) continues_for_pending_message_wake: bool,
    /// Frozen non-tool query classification used by later reducers.
    pub(super) is_non_tool_ext_query: bool,
    /// Ordinary success classification used by watcher and turn reducers.
    pub(super) successful: bool,
    /// Exact eager decision and diagnostic, without cursor allocation.
    pub(super) automatic:
        prepared_automatic_compaction_decision::PreparedAutomaticCompactionDecision,
}

impl Harness {
    /// Attach all remaining ordinary canonical fields before terminal effects.
    /// The response must already carry normalized accounting and telemetry.
    pub(super) fn prepare_ordinary_provider_terminal(
        &self,
        cid: &AgentId,
        response: &mut ProviderResponseFinished,
        projection: &mut terminal_response_projection::TerminalResponseProjection,
    ) -> PreparedOrdinaryProviderTerminal {
        let missing_tool_calls =
            response_requests_tool_calls(response) && projection.tool_calls.is_empty();
        let tool_calls_with_non_tool_stop =
            !response_requests_tool_calls(response) && !projection.tool_calls.is_empty();
        let mut requested_tool_calls = !projection.tool_calls.is_empty();
        // Length-stopped declarations are incomplete output, never executable.
        if response.stop_reason == ProviderStopReason::Length && requested_tool_calls {
            requested_tool_calls = false;
            projection.tool_calls.clear();
        }
        let operation = self
            .prompt_coordination
            .prompt_runtime
            .operations
            .get(&response.agent_prompt_id)
            .copied()
            .unwrap_or_default();
        let output_length = self.prepare_output_length_continuation(
            cid,
            response,
            operation.0,
            requested_tool_calls,
        );
        response.output_length_disposition = output_length.disposition.clone();
        let input_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_sent_tokens);
        if projection.contains_compaction {
            self.attach_finished_response_compaction_usage(response, input_tokens);
        }
        let is_non_tool_prompt_surface = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(Self::agent_uses_non_tool_prompt_surface);
        let is_non_tool_ext_query = self.is_non_tool_extension_query(cid);
        let declaration = requested_tool_calls.then(tau_proto::ObservationId::random);
        let normalized_tool_calls =
            declaration.map_or_else(NormalizedFinishedToolCalls::default, |declaration| {
                self.prepare_finished_response_tool_calls(
                    response,
                    &mut projection.tool_calls,
                    is_non_tool_prompt_surface,
                    tool_calls_with_non_tool_stop,
                    declaration,
                )
            });
        let successful = response.error.is_none()
            && response.failure_kind.is_none()
            && !matches!(
                response.stop_reason,
                ProviderStopReason::Length
                    | ProviderStopReason::Error
                    | ProviderStopReason::RepetitionDetected
            );
        let (final_status_plan, status_was_available) =
            self.prepare_final_status_provider_terminal(cid, response, requested_tool_calls);
        let final_status_challenged = matches!(
            final_status_plan,
            ProviderTerminalPlan::FinalStatusGated(FinalStatusGatedPlan::Challenge { .. })
        );
        response.final_status_disposition = if final_status_challenged {
            tau_proto::FinalStatusDisposition::Challenged
        } else {
            tau_proto::FinalStatusDisposition::Accepted
        };
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
                cid,
                response,
                requested_tool_calls,
                is_non_tool_ext_query,
            );
        let eager_decision_eligible = !final_status_challenged
            && !requested_tool_calls
            && !projection.contains_compaction
            && !continues_for_pending_message_wake
            && response.failure_kind != Some(tau_proto::ProviderFailureKind::ContextWindowExceeded)
            && response.recovery_disposition == tau_proto::ContextRecoveryDisposition::None
            && !matches!(
                response.output_length_disposition,
                tau_proto::OutputLengthDisposition::ContinuationPlanned { .. }
            );
        let model = response
            .usage
            .as_ref()
            .and_then(|usage| usage.model.clone())
            .or_else(|| {
                self.prompt_coordination
                    .prompt_runtime
                    .models
                    .get(&response.agent_prompt_id)
                    .cloned()
            });
        let automatic = if eager_decision_eligible && let Some(model) = model {
            self.prepare_automatic_compaction_decision_after_terminal(
                cid,
                model,
                input_tokens
                    .filter(|tokens| *tokens > 0)
                    .map(tau_proto::TokenCount::new),
                Some(response.agent_prompt_id.clone()),
                self.prompt_coordination
                    .prompt_runtime
                    .compaction_policies
                    .get(&response.agent_prompt_id)
                    .unwrap_or(&BTreeMap::new()),
                terminal_policy_projection::TerminalPolicyProjection {
                    status_was_available,
                    next_prompt_index: output_length.next_prompt_index,
                },
            )
        } else {
            prepared_automatic_compaction_decision::PreparedAutomaticCompactionDecision::default()
        };
        response.automatic_compaction_decision = automatic.decision.clone();
        PreparedOrdinaryProviderTerminal {
            requested_tool_calls,
            missing_tool_calls,
            normalized_tool_calls,
            declaration,
            output_length,
            final_status_plan,
            status_was_available,
            final_status_challenged,
            continues_for_pending_message_wake,
            is_non_tool_ext_query,
            successful,
            automatic,
        }
    }
}

//! Runtime custody of bounded output until its canonical observation commits.

use std::collections::VecDeque;
use std::io;

use super::*;
use crate::agent::PendingMessageWakeSource;

/// Each slot bounds live text, metadata, pending publication and final context.
const MAX_OUTPUT_STREAMS_PER_AGENT: usize = 8;
const MAX_OUTPUT_STREAMS_TOTAL: usize = 64;

/// One opted-in invocation's immutable output publication lane.
#[derive(Default)]
pub(super) struct ToolOutputLane {
    /// Reports accepted from this configured invocation, for exact retry
    /// checks.
    reports: Vec<tau_proto::ToolOutputReport>,
    /// Canonical updates awaiting semantic acceptance, in producer order.
    pending: VecDeque<tau_proto::ToolOutputUpdate>,
    /// Whether the head is inside ordinary publication/interception.
    publishing: bool,
    /// Approved head retained across semantic admission rejection.
    rejected: Option<interception::OwnedPublication>,
    /// UTF-8 payload bytes admitted into this lane.
    bytes: usize,
    /// Terminal held behind earlier output, retaining canonical source
    /// metadata.
    terminal: Option<HeldToolOutputTerminal>,
    /// A terminal report fences later output even before its append commits.
    terminal_started: bool,
}

/// Original publication continuation held behind earlier accepted reports.
struct HeldToolOutputTerminal {
    /// Configured producer whose report initiated publication, if external.
    source: Option<tau_proto::ConnectionId>,
    /// Bounded canonical terminal waiting behind earlier source frames.
    event: Event,
    /// Continuation resumed only after this terminal commits.
    completion: Option<AgentPublishCompletion>,
    /// Whether normal watcher notification accompanies the committed terminal.
    notify_watchers: bool,
}

impl Harness {
    /// Presentation follows the frozen declaration even if execution admission
    /// fails, no frame arrives, or restart repair has no live output lane.
    pub(super) fn tool_call_streams_output(&self, cid: &AgentId, call: &ToolCallId) -> bool {
        self.persisted_tool_call_ref(cid, call)
            .is_some_and(|reference| {
                self.agent_runtime
                    .agent_registry
                    .agents
                    .get(cid)
                    .and_then(|agent| agent.identity.agent_id.as_deref())
                    .and_then(|agent| self.session_runtime.agent_store.agent(agent))
                    .is_some_and(|tree| tree.tool_output_is_streamed(reference))
            })
    }

    /// Reserve a whole invocation's capture/context allowance before tool
    /// effects.
    pub(super) fn admit_tool_output_stream(&mut self, cid: &AgentId, call: &ToolCallId) -> bool {
        self.release_materialized_tool_output(cid);
        if self
            .agent_runtime
            .agent_registry
            .agents
            .values()
            .any(|agent| agent.dispatch.restored_tool_output_debt)
        {
            return false;
        }
        let Some(reference) = self.persisted_tool_call_ref(cid, call) else {
            return false;
        };
        let runtime = &mut self.tool_routing.tool_runtime;
        let owned = runtime
            .output_residencies
            .values()
            .filter(|owner| *owner == cid)
            .count();
        if runtime.output_residencies.len() >= MAX_OUTPUT_STREAMS_TOTAL
            || MAX_OUTPUT_STREAMS_PER_AGENT <= owned
            || runtime.output_residencies.contains_key(&reference)
            || runtime.output_lanes.contains_key(call)
        {
            return false;
        }
        runtime.output_residencies.insert(reference, cid.clone());
        runtime
            .output_lanes
            .insert(call.clone(), ToolOutputLane::default());
        true
    }

    /// Reclaim bounded custody, not historical transcript storage. This runs
    /// after canonical folds that can materialize deferred source occurrences.
    pub(super) fn release_materialized_tool_output(&mut self, cid: &AgentId) {
        let Some(tree) = self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .and_then(|agent| agent.identity.agent_id.as_deref())
            .and_then(|agent| self.session_runtime.agent_store.agent(agent))
        else {
            return;
        };
        self.tool_routing
            .tool_runtime
            .output_residencies
            .retain(|call, owner| owner != cid || tree.tool_output_needs_residency(*call));
        if self
            .agent_runtime
            .agent_registry
            .agents
            .get(cid)
            .is_some_and(|agent| agent.dispatch.restored_tool_output_debt)
        {
            let debt = tree.pending_tool_output_residencies().next().is_some();
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(cid) {
                agent.dispatch.restored_tool_output_debt = debt;
            }
        }
    }

    /// Restore output custody before terminal repair can dispatch new work.
    pub(super) fn restore_tool_output_residencies(&mut self, session: &SessionId) {
        for cid in self.restored_agent_ids(session) {
            let Some(tree) = self
                .agent_runtime
                .agent_registry
                .agents
                .get(&cid)
                .and_then(|agent| agent.identity.agent_id.as_deref())
                .and_then(|agent| self.session_runtime.agent_store.agent(agent))
            else {
                continue;
            };
            let debt = tree.pending_tool_output_residencies().next().is_some();
            if let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) {
                agent.dispatch.restored_tool_output_debt = debt;
            }
        }
    }

    /// Queue a payload-free, immediately ready activation after canonical
    /// append.
    pub(super) fn activate_tool_output(
        &mut self,
        update: &tau_proto::ToolOutputUpdate,
        outcome: &tau_core::AgentAppendOutcome,
    ) {
        let Some(cid) = self
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(&update.output.call_id)
            .cloned()
        else {
            return;
        };
        let activation = tau_proto::ObservationId::random();
        let Some(agent) = self.agent_runtime.agent_registry.agents.get_mut(&cid) else {
            return;
        };
        if agent.dispatch.terminating {
            return;
        }
        if agent
            .dispatch
            .pending_message_wakes
            .iter()
            .any(|wake| wake.source.durable_event_seq() == outcome.seq)
        {
            return;
        }
        agent
            .dispatch
            .pending_message_wakes
            .push_back(crate::agent::PendingMessageWake {
                source: PendingMessageWakeSource::ToolOutput {
                    durable_event_seq: outcome.seq,
                    terminal: false,
                },
                node_id: outcome.folded_node_id,
                activation_observation: Some(activation),
                source_observation: Some(outcome.observation_id),
                delivery_schedule: None,
            });
        self.append_activation_queued(
            &cid,
            activation,
            tau_proto::ActivationKind::BackgroundCompletion,
            Some(outcome.observation_id),
            Some(update.call),
        );
        if !self.suppresses_tool_update_wakes(&cid) {
            self.activate_waits_for(&cid, activation);
        }
        self.try_advance_queue();
    }
    /// Validate the configured generation, route, opt-in, sequence and bounds.
    pub(super) fn process_committed_tool_output_report(
        &mut self,
        peer: &interception::PeerPublicationContext,
        report: &tau_proto::ToolOutputReport,
    ) {
        let Some(extension) = peer.extension.as_ref() else {
            return;
        };
        let current = self
            .extensions
            .entries
            .get(&extension.source)
            .is_some_and(|entry| {
                entry.instance_id == extension.instance_id
                    && entry.name == extension.publisher
                    && matches!(
                        entry.kind,
                        ClientKind::Tool | ClientKind::Core | ClientKind::Provider
                    )
                    && entry.state != ExtensionState::Disconnected
            });
        if !current
            || self
                .tool_routing
                .tool_runtime
                .pending_tool_providers
                .get(&report.call_id)
                != Some(&extension.source)
            || report.text.len() > tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES
            || report.sequence >= tau_proto::MAX_TOOL_OUTPUT_UPDATES
        {
            return;
        }
        let Some(call) = self.wait_tool_call_ref(&report.call_id) else {
            return;
        };
        let Some(tool) = self
            .tool_routing
            .tool_runtime
            .pending_tools
            .get(&report.call_id)
        else {
            return;
        };
        let mut report = report.clone();
        // Display names are invocation metadata, never producer payload
        // authority.
        report.tool_name = tool.name.clone();
        let Some(cid) = self
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(&report.call_id)
            .cloned()
        else {
            return;
        };
        let Some(lane) = self
            .tool_routing
            .tool_runtime
            .output_lanes
            .get_mut(&report.call_id)
        else {
            return;
        };
        if report.sequence < lane.reports.len() as u64 {
            // Identical retransmission is idempotent; a conflict never replaces
            // history.
            if lane.reports.get(report.sequence as usize) != Some(&report) {
                tracing::warn!(call_id = %report.call_id, "conflicting tool output sequence");
            }
            return;
        }
        if report.sequence != lane.reports.len() as u64
            || lane.terminal_started
            || lane.bytes.saturating_add(report.text.len()) > tau_proto::MAX_TOOL_OUTPUT_LIVE_BYTES
            || (report.text.is_empty() && report.omitted_bytes == 0)
        {
            tracing::warn!(call_id = %report.call_id, "tool output sequence or budget rejected");
            return;
        }
        lane.bytes += report.text.len();
        lane.reports.push(report.clone());
        lane.pending.push_back(tau_proto::ToolOutputUpdate {
            call,
            output: report.clone(),
        });
        self.drive_tool_output_lane(&cid, &report.call_id);
    }

    /// Offer only the head; a terminal cannot overtake rejected or parked
    /// output.
    pub(super) fn drive_tool_output_lane(&mut self, cid: &AgentId, call_id: &ToolCallId) {
        let Some(lane) = self.tool_routing.tool_runtime.output_lanes.get_mut(call_id) else {
            return;
        };
        if lane.publishing {
            return;
        }
        if let Some(approved) = lane.rejected.take() {
            lane.publishing = true;
            self.commit_approved_tool_output_retry(cid, approved);
        } else if let Some(update) = lane.pending.front().cloned() {
            lane.publishing = true;
            self.publish_for_agent(cid, Event::ToolOutputUpdated(update));
        } else if let Some(terminal) = lane.terminal.take() {
            self.publish_event_for_agent_with_completion(
                cid,
                terminal.source.as_ref(),
                terminal.event,
                terminal.completion,
                terminal.notify_watchers,
            );
        }
    }

    /// Keep the canonical publication owner on rollback-safe admission failure.
    pub(super) fn retain_rejected_tool_output(
        &mut self,
        event: &Event,
        semantic_parent: tau_core::AgentEventParent,
    ) -> bool {
        let Event::ToolOutputUpdated(update) = event else {
            return false;
        };
        if let Some(lane) = self
            .tool_routing
            .tool_runtime
            .output_lanes
            .get_mut(&update.output.call_id)
        {
            lane.publishing = false;
            lane.rejected = Some(interception::OwnedPublication {
                approved_event: Box::new(event.clone()),
                semantic_parent,
                owning_branch: interception::OwnedPublicationBranch::SemanticParent,
                retry_policy:
                    interception::OwnedPublicationRetryPolicy::ApprovedEventWithoutInterception,
            });
            return true;
        }
        false
    }

    /// Retry retained stream heads through ordinary publication, without tool
    /// effects.
    pub(super) fn retry_tool_outputs(&mut self, cid: &AgentId) {
        let calls: Vec<_> = self
            .tool_routing
            .tool_runtime
            .output_lanes
            .keys()
            .filter(|call| self.tool_routing.tool_runtime.tool_agents.get(*call) == Some(cid))
            .cloned()
            .collect();
        for call in calls {
            self.drive_tool_output_lane(cid, &call);
        }
    }

    /// Stamp foreground stream receipts and hold terminals behind accepted
    /// output custody.
    pub(super) fn prepare_tool_output_terminal(
        &mut self,
        cid: &AgentId,
        source: Option<&tau_proto::ConnectionId>,
        event: &mut Event,
        completion: &Option<AgentPublishCompletion>,
        notify_watchers: bool,
    ) -> bool {
        let call = match event {
            Event::ProviderToolResult(result) if result.kind == ToolResultKind::Final => {
                if self.tool_call_streams_output(cid, &result.call_id) {
                    result.presentation = tau_proto::ToolResultPresentation::ToolStreamReceipt;
                    capture_result(
                        &mut result.result,
                        &mut result.captured_output,
                        &mut result.display,
                    );
                    result.provider_content.clear();
                }
                &result.call_id
            }
            Event::ProviderToolError(error) => {
                if self.tool_call_streams_output(cid, &error.call_id) {
                    error.presentation = tau_proto::ToolResultPresentation::ToolStreamReceipt;
                    capture_error(
                        &mut error.message,
                        &mut error.details,
                        &mut error.captured_output,
                        &mut error.display,
                    );
                }
                &error.call_id
            }
            Event::ToolCancelled(cancelled) => {
                if self.tool_call_streams_output(cid, &cancelled.call_id) {
                    cancelled.presentation = tau_proto::ToolResultPresentation::ToolStreamReceipt;
                    capture_cancelled(cancelled);
                }
                &cancelled.call_id
            }
            Event::ToolBackgroundResult(result) => {
                if self.tool_call_streams_output(cid, &result.call_id) {
                    capture_result(
                        &mut result.result,
                        &mut result.captured_output,
                        &mut result.display,
                    );
                }
                &result.call_id
            }
            Event::ToolBackgroundError(error) => {
                if self.tool_call_streams_output(cid, &error.call_id) {
                    capture_error(
                        &mut error.message,
                        &mut error.details,
                        &mut error.captured_output,
                        &mut error.display,
                    );
                }
                &error.call_id
            }
            _ => return false,
        };
        let Some(lane) = self.tool_routing.tool_runtime.output_lanes.get_mut(call) else {
            return false;
        };
        lane.terminal_started = true;
        if lane.pending.is_empty() {
            return false;
        }
        if lane.terminal.is_none() {
            lane.terminal = Some(HeldToolOutputTerminal {
                source: source.cloned(),
                event: event.clone(),
                completion: completion.clone(),
                notify_watchers,
            });
        }
        true
    }

    /// Include rejected output in existing capacity/frontier retry scheduling.
    pub(super) fn retained_tool_output_agents(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.tool_routing
            .tool_runtime
            .output_lanes
            .iter()
            .filter(|(_, lane)| lane.rejected.is_some())
            .filter_map(|(call, _)| {
                self.tool_routing
                    .tool_runtime
                    .tool_agents
                    .get(call)
                    .cloned()
            })
    }

    /// Canonical acceptance, not report arrival, releases the next stream item.
    pub(super) fn finish_tool_output_update(&mut self, update: &tau_proto::ToolOutputUpdate) {
        let call_id = &update.output.call_id;
        let Some(cid) = self
            .tool_routing
            .tool_runtime
            .tool_agents
            .get(call_id)
            .cloned()
        else {
            return;
        };
        let Some(lane) = self.tool_routing.tool_runtime.output_lanes.get_mut(call_id) else {
            return;
        };
        if lane.pending.front() != Some(update) {
            return;
        }
        lane.pending.pop_front();
        lane.publishing = false;
        self.drive_tool_output_lane(&cid, call_id);
    }
}

/// Preserve available cancellation diagnostics before terminal publication.
pub(super) fn capture_cancelled(cancelled: &mut tau_proto::ToolCancelled) {
    if cancelled.captured_output.is_none() {
        cancelled.captured_output = Some(match cancelled.output.take() {
            Some(text) => tau_proto::ToolFinalOutput::capture(
                &tau_proto::CborValue::Text(text),
                tau_proto::ProviderToolResultStatus::Cancelled {
                    reason: "tool cancelled",
                },
            ),
            None => tau_proto::ToolFinalOutput::capture(
                &tau_proto::CborValue::Null,
                tau_proto::ProviderToolResultStatus::Cancelled {
                    reason: "final capture unavailable",
                },
            ),
        });
    }
    cancelled.output = None;
    bound_terminal_display(&mut cancelled.display);
    ensure_capture_display(
        &mut cancelled.display,
        cancelled.captured_output.as_ref(),
        true,
    );
}

/// Keep coherent process status for trace consumers without retaining arbitrary
/// producer data a second time beside the bounded source capture.
fn process_metadata(value: &CborValue, error: bool) -> CborValue {
    let outcome = if error {
        tau_proto::ShellProcessOutcome::from_error_details(value)
    } else {
        tau_proto::ShellProcessOutcome::from_result(value)
    };
    let CborValue::Map(entries) = value else {
        return CborValue::Null;
    };
    if outcome.is_none() {
        return CborValue::Null;
    }
    CborValue::Map(entries.iter().filter(|(key, _)| matches!(key,
        CborValue::Text(key) if matches!(key.as_str(), "status" | "signal" | "timed_out" | "termination_reason")
    )).cloned().collect())
}

fn capture_result(
    result: &mut CborValue,
    captured: &mut Option<tau_proto::ToolFinalOutput>,
    display: &mut Option<tau_proto::ToolUseState>,
) {
    if captured.is_none() {
        *captured = Some(tau_proto::ToolFinalOutput::capture(
            result,
            tau_proto::ProviderToolResultStatus::Success,
        ));
        *result = process_metadata(result, false);
    }
    bound_terminal_display(display);
    ensure_capture_display(display, captured.as_ref(), false);
}

fn capture_error(
    message: &mut String,
    details: &mut Option<CborValue>,
    captured: &mut Option<tau_proto::ToolFinalOutput>,
    display: &mut Option<tau_proto::ToolUseState>,
) {
    if captured.is_none() {
        *captured = Some(tau_proto::ToolFinalOutput::capture(
            details.as_ref().unwrap_or(&CborValue::Null),
            tau_proto::ProviderToolResultStatus::Error { message },
        ));
        *details = details
            .as_ref()
            .map(|value| process_metadata(value, true))
            .filter(|value| *value != CborValue::Null);
        if message.len() > 1024 {
            let mut end = 1000;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push_str(" [diagnostic truncated]");
        }
    }
    bound_terminal_display(display);
    ensure_capture_display(display, captured.as_ref(), true);
}

fn ensure_capture_display(
    display: &mut Option<tau_proto::ToolUseState>,
    captured: Option<&tau_proto::ToolFinalOutput>,
    error: bool,
) {
    if display.is_none()
        && let Some(output) = captured
    {
        *display = Some(tau_proto::ToolUseState {
            status: if error {
                tau_proto::ToolUseStatus::Error
            } else {
                tau_proto::ToolUseStatus::Success
            },
            status_text: if error { "failed" } else { "ok" }.to_owned(),
            payload: Some(tau_proto::ToolUsePayload::Text {
                text: output.text.clone(),
            }),
            ..Default::default()
        });
    }
}

/// UI state is a separate bounded snapshot, not a second unbounded source.
fn bound_terminal_display(display: &mut Option<tau_proto::ToolUseState>) {
    /// Remaining encoded allowance for a terminal UI snapshot.
    struct Limit(
        /// Bytes still available to the encoder.
        usize,
    );
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("display allowance exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if display
        .as_ref()
        .is_some_and(|state| ciborium::into_writer(state, &mut Limit(8 * 1024)).is_err())
    {
        *display = None;
    }
}
